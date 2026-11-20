//! Functional UI tests â€” drive the REAL `App` headlessly through
//! `egui_kittest` (AccessKit-based; no renderer: the snapshot/wgpu features
//! stay OFF, so this never touches the GPU stack).
//!
//! Determinism:
//! - locale is pinned to English by pre-writing `gui-state.json` with
//!   `"lang": "en"` BEFORE `App::new` runs â€” every asserted label is English;
//! - content assertions use technical strings (file names, `seq N`, kind
//!   values like "external") which are never translated;
//! - every test seeds its own temp store through the `history` engine API
//!   (no daemon, no network, no real window).
//!
//! The `probe_accessibility_tree` test dumps every labelled node â€” run it with
//! `cargo test -p gui --test kittest probe -- --nocapture` when a query comes
//! up empty and an assertion needs a new anchor.

use std::path::{Path, PathBuf};

use eframe::egui;
use egui_kittest::kittest::{By, NodeT, Queryable};
use egui_kittest::Harness;
use gui::app::App;
use history::{EventKind, LocalHistory, WorkspaceId};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Seeding â€” records inside the auto-batch window (shrunk via set_batch_policy)
// form one batch each, so the activity tab has real batches to show.
// ---------------------------------------------------------------------------

/// A seeded temp store. The `_store`/`_ws` TempDirs are kept alive by the
/// caller via this struct (dropping them deletes data under a live harness).
struct Seed {
    _store: TempDir,
    _ws: TempDir,
    store_root: PathBuf,
    /// `notes.txt` inside the workspace (exists on disk AND in the store).
    notes: PathBuf,
}

fn open_seeded(store: &TempDir, ws: &TempDir, shrink_batches: bool) -> (LocalHistory, WorkspaceId) {
    let lh = LocalHistory::open(store.path()).unwrap();
    let ws_id = lh.register_workspace(ws.path()).unwrap();
    if shrink_batches {
        // e2e-speed batching: waves separated by >500ms form distinct batches
        lh.set_batch_policy(200, 400);
    }
    (lh, ws_id)
}

/// `notes.txt` with `n` external versions (content "line v1".."line vN") and
/// the matching file on disk.
fn seed_versions(n: usize) -> Seed {
    let store = TempDir::new().unwrap();
    let ws = TempDir::new().unwrap();
    let (lh, ws_id) = open_seeded(&store, &ws, false);
    let notes = ws.path().join("notes.txt");
    for i in 1..=n {
        let content = format!("line v{i}\nsecond line {i}\n");
        std::fs::write(&notes, &content).unwrap();
        lh.record(ws_id, &notes, content.as_bytes(), EventKind::External, None).unwrap();
    }
    let store_root = store.path().to_path_buf();
        Seed { _store: store, _ws: ws, store_root, notes }
}

/// 3 config files created in one wave (batch #1) then modified in a second
/// wave spaced >500ms apart (batch #2) â€” two auto batches for the activity tab.
fn seed_batches() -> Seed {
    let store = TempDir::new().unwrap();
    let ws = TempDir::new().unwrap();
    let (lh, ws_id) = open_seeded(&store, &ws, true);
    let mut files = Vec::new();
    for i in 1..=3 {
        let p = ws.path().join(format!("svc{i}.ini"));
        std::fs::write(&p, format!("app=svc{i}\nretries=3\nmode=normal\n")).unwrap();
        files.push(p);
    }
    for p in &files {
        let c = std::fs::read(p).unwrap();
        lh.record(ws_id, p, &c, EventKind::External, None).unwrap();
    }
    std::thread::sleep(std::time::Duration::from_millis(550));
    for (i, p) in files.iter().enumerate() {
        let content = format!("app=svc{}\nretries=10\nmode=turbo\n", i + 1);
        std::fs::write(p, &content).unwrap();
        lh.record(ws_id, p, content.as_bytes(), EventKind::External, None).unwrap();
    }
    let store_root = store.path().to_path_buf();
        Seed { _store: store, _ws: ws, store_root, notes: files[0].clone() }
}

// ---------------------------------------------------------------------------
// Harness factory
// ---------------------------------------------------------------------------

/// Builds the harness around the real `App`. The store's gui-state.json pins
/// `lang: en` first so `App::new` resolves the locale deterministically (the
/// process-global locale makes parallel tests safe: every test pins "en").
fn make_harness(seed: &Seed, initial_file: Option<&Path>) -> Harness<'static, App> {
    let st = gui::state::GuiState { lang: Some("en".into()), ..Default::default() };
    gui::state::save_state(&gui::state::state_path(&seed.store_root), &st).unwrap();
    let store_root = seed.store_root.clone();
    let file = initial_file.map(PathBuf::from);
    Harness::builder()
        .with_size(egui::Vec2::new(1280.0, 800.0))
        // unnamed input widgets exist on purpose (the diff grid cells and the
        // icon-only buttons) â€” the strict a11y naming check would reject them
        .with_accessibility_check(false)
        .with_context_setup(|ctx| {
            ctx.set_theme(egui::Theme::Dark);
            gui::icons::install(ctx);
        })
        .build_eframe(move |_cc| App::new(store_root, file))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Dumps EVERY node (role + label + rect, rectless nodes marked) â€” the anchor
/// catalog for the other tests. Run:
/// `cargo test -p gui --test kittest probe -- --nocapture`.
#[test]
fn probe_accessibility_tree() {
    let s = seed_versions(3);
    let mut h = make_harness(&s, Some(&s.notes));
    h.run();
    select_notes(&mut h, &s);
    h.run();

    // Node::rect() panics on rectless nodes (the root) â€” silence the hook and
    // mark them instead of blowing up the dump
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    for n in h.query_all(By::new().predicate(|_| true)) {
        let ak = n.accesskit_node();
        let role = ak.role();
        let label = ak.label().map(|s| s.to_string()).unwrap_or_default();
        let value = n.value().unwrap_or_default();
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| n.rect()));
        let rect_str = match r {
            Ok(r) => format!("({:.0},{:.0} {:.0}x{:.0})", r.left(), r.top(), r.width(), r.height()),
            Err(_) => "(no rect)".to_string(),
        };
        println!("{role:?} {rect_str}  label={label:?} value={value:?}");
    }
    std::panic::set_hook(prev_hook);

    // sanity: the app rendered at least one labelled node
    assert!(
        h.query_all(By::new().role(egui::accesskit::Role::Button)).count()
            + h.query_all(By::new().role(egui::accesskit::Role::Label)).count()
            > 0,
        "accessibility tree is empty â€” the app did not render"
    );
}

/// Selects `notes.txt` in the file list (the same flow a user performs; the
/// `--file` auto-open contract resolves asynchronously, so tests click the
/// row explicitly).
fn select_notes(h: &mut Harness<App>, seed: &Seed) {
    let name = seed.notes.file_name().unwrap().to_string_lossy().to_string();
    h.get_by_label(&name).click();
    h.run();
}

/// Rect of the LEFT pane's diff heading. `draw_pane_headings` exposes the
/// title as a Label whose ACCESSKIT VALUE carries the text ("seq N Â· kind Â·
/// HH:MM") â€” the label attribute is empty. Timeline rows are a different
/// role (Unknown) with the text in the label, so role+value uniquely finds
/// the headings; the leftmost is the left pane's.
fn pane_heading(h: &Harness<App>, contains: &str) -> egui::Rect {
    h.query_all(By::new().role(egui::accesskit::Role::Label))
        .filter(|n| n.value().is_some_and(|v| v.contains(contains)))
        .map(|n| n.rect())
        .min_by_key(|r| (r.left() as i64, r.top() as i64))
        .expect("diff pane heading must exist")
}

/// Files tab: the seeded file appears in the list and its timeline shows the
/// version rows (`seq N Â· external Â· HH:MM`).
#[test]
fn files_tab_shows_file_and_timeline() {
    let s = seed_versions(3);
    let mut h = make_harness(&s, Some(&s.notes));
    h.run();
    select_notes(&mut h, &s);
    h.run();

    // file list row + timeline version rows; several labels legitimately
    // repeat (a row and a pane heading may share "seq N") â†’ count-based asserts
    assert!(h.query_all(By::new().label_contains("notes.txt")).count() >= 1);
    assert!(h.query_all(By::new().label_contains("seq 1")).count() >= 1, "timeline row seq 1");
    assert!(h.query_all(By::new().label_contains("seq 3")).count() >= 1, "timeline row seq 3");
    assert!(h.query_all(By::new().label_contains("external")).count() >= 1, "kind external");
    // the diff of the last two versions is open by default (mode Previous):
    // its content lines are painter-drawn (NOT in the a11y tree) â€” the pane
    // headings are the accessible anchor, both panes compare seq 2
    assert!(
        h.query_all(By::new().label_contains("seq 2")).count() >= 2,
        "left+right pane headings should both mention the compared version"
    );
}

/// Diff mode `CurrentDisk` with disk == latest version â†’ the "identical" note
/// renders. Drives the mode through the App state (the combo popup is brittle
/// in a headless harness); the panel reacts through its normal cache path.
#[test]
fn diff_mode_disk_shows_identical_note() {
    let s = seed_versions(2);
    let mut h = make_harness(&s, Some(&s.notes));
    h.run();
    select_notes(&mut h, &s);
    h.run();

    h.state_mut().diff_mode = gui::app::DiffMode::CurrentDisk;
    h.run();
    h.get_by_label_contains("identical");
}

/// Activity tab: batches appear, selecting one opens the whole-batch view
/// (per-file block headers + revert button + entry list).
#[test]
fn activity_tab_shows_batches_and_batch_view() {
    let s = seed_batches();
    let mut h = make_harness(&s, None);
    h.run();

    // two auto batches were seeded (direct engine-level assertion)
    assert_eq!(h.state().batches.len(), 2, "seed must produce 2 batches");

    h.get_by_label("Activity").click();
    h.run();

    // batch rows are PAINTER-DRAWN (allocate + painter text) â€” intentionally
    // not in the a11y tree; selecting a batch via the same field a row click
    // sets exercises the identical center-panel code path
    let id = h.state().batches.last().unwrap().id;
    h.state_mut().selected_batch = Some(id);
    h.state_mut().next_poll = None;
    h.run();

    // egui wraps text in parent node + TextRun child (both may carry the
    // text) â†’ presence asserts are count-based, not uniqueness-based
    assert!(h.query_all(By::new().label_contains("Batch #")).count() >= 1, "batch title");
    assert!(
        h.query_all(By::new().label_contains("Revert whole batch")).count() >= 1,
        "revert button"
    );
    // NOTE: the per-file block headers are painter-drawn (deliberately not in
    // the a11y tree, like every custom-drawn cell) â€” their correctness is
    // covered by the pure builder tests in app.rs; here only the title and
    // the revert action are assertable headlessly.
}

/// Right-click on the diff area opens the copy context menu (all four items,
/// regardless of enabled state).
#[test]
fn diff_context_menu_lists_copy_actions() {
    let s = seed_versions(3);
    let mut h = make_harness(&s, Some(&s.notes));
    h.run();
    select_notes(&mut h, &s);
    h.run();

    // anchor: the first diff row below the left pane's heading â€” Previous
    // mode puts the NEWEST version on the left (seq 3), so its heading marks
    // the left pane's column span and the rows start right below it
    let heading = pane_heading(&h, "seq 3");
    let pos = egui::pos2(heading.left() + heading.width() * 0.5, heading.bottom() + 12.0);

    h.event(egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Secondary,
        pressed: true,
        modifiers: egui::Modifiers::NONE,
    });
    h.step();
    h.event(egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Secondary,
        pressed: false,
        modifiers: egui::Modifiers::NONE,
    });
    h.run();

    assert!(h.query_all(By::new().label_contains("Copy selection")).count() >= 1, "menu: selection");
    assert!(h.query_all(By::new().label_contains("Copy left pane")).count() >= 1, "menu: left");
    assert!(h.query_all(By::new().label_contains("Copy right pane")).count() >= 1, "menu: right");
    assert!(h.query_all(By::new().label_contains("Copy diff (patch)")).count() >= 1, "menu: patch");
}

/// Drag-selection confinement: dragging downward inside the LEFT pane must
/// keep the whole selection in `Pane::Left` (the regression from the
/// cross-pane-leak bug), and it must not be empty.
#[test]
fn drag_selection_stays_in_one_pane() {
    let s = seed_versions(3);
    let mut h = make_harness(&s, Some(&s.notes));
    h.run();
    select_notes(&mut h, &s);
    h.run();

    // drag inside the left pane (Previous mode: newest seq 3 on the left),
    // starting just below its heading and crossing into the next row (rows
    // are ~20px; the intermediate move makes the drag unambiguous)
    let heading = pane_heading(&h, "seq 3");
    let x = heading.left() + heading.width() * 0.5;
    let y0 = heading.bottom() + 8.0;
    let y1 = y0 + 32.0;

    h.drag_at(egui::pos2(x, y0));
    h.step();
    h.hover_at(egui::pos2(x, y0 + 14.0));
    h.step();
    h.hover_at(egui::pos2(x, y1));
    h.step();
    h.drop_at(egui::pos2(x, y1));
    h.run();

    let sel = h.state().diff_selection.clone();
    assert!(!sel.is_empty(), "drag must produce a non-empty selection");
    let (from, to) = sel.normalized().expect("both ends set");
    assert_eq!(from.pane, to.pane, "selection must never span both panes");
    assert_eq!(from.pane, gui::diff_view::Pane::Left, "drag started in the left pane");
}
