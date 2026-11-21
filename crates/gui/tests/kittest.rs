//! Functional UI tests — drive the REAL `App` headlessly through
//! `egui_kittest` (AccessKit-based; no renderer: the snapshot/wgpu features
//! stay OFF, so this never touches the GPU stack).
//!
//! Determinism:
//! - locale is pinned to English by pre-writing `gui-state.json` with
//!   `"lang": "en"` BEFORE `App::new` runs — every asserted label is English;
//! - content assertions use technical strings (file names, `seq N`, kind
//!   values like "external") which are never translated;
//! - every test seeds its own temp store through the `history` engine API
//!   (no daemon, no network, no real window).
//!
//! The `probe_accessibility_tree` test dumps every labelled node — run it with
//! `cargo test -p gui --test kittest probe -- --nocapture` when a query comes
//! up empty and an assertion needs a new anchor.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use eframe::egui;
use egui_kittest::kittest::{By, NodeT, Queryable};
use egui_kittest::Harness;
use gui::app::App;
use history::{EventKind, LocalHistory, WorkspaceId};
use tempfile::TempDir;

/// Serializes every test in this binary: `App::new` sets the PROCESS-GLOBAL
/// rust-i18n locale, so the vi-locale test running concurrently would flip
/// the strings under the en tests' feet. Held for the whole test body.
static LOCALE_LOCK: Mutex<()> = Mutex::new(());

fn locale_guard() -> MutexGuard<'static, ()> {
    LOCALE_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ---------------------------------------------------------------------------
// Seeding — records inside the auto-batch window (shrunk via set_batch_policy)
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
/// wave spaced >500ms apart (batch #2) — two auto batches for the activity tab.
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
/// Builds the harness around the real `App`. The store's gui-state.json pins
/// the locale first so `App::new` resolves it deterministically (the
/// process-global locale makes parallel tests safe: every test pins its own).
/// `lang != "en"` also switches missing glyphs to tofu: the bundled fonts
/// (Ubuntu-Light) have no Vietnamese glyphs and kittest defaults to Panic —
/// the a11y labels are plain strings, so tofu does not affect the asserts.
fn make_harness_lang(seed: &Seed, initial_file: Option<&Path>, lang: &str) -> Harness<'static, App> {
    let st = gui::state::GuiState { lang: Some(lang.into()), ..Default::default() };
    gui::state::save_state(&gui::state::state_path(&seed.store_root), &st).unwrap();
    let store_root = seed.store_root.clone();
    let file = initial_file.map(PathBuf::from);
    let is_en = lang == "en";
    let builder = Harness::builder()
        .with_size(egui::Vec2::new(1280.0, 800.0))
        // unnamed input widgets exist on purpose (the diff grid cells and the
        // icon-only buttons) — the strict a11y naming check would reject them
        .with_accessibility_check(false);
    // the bundled fonts (Ubuntu-Light) have no Vietnamese glyphs and kittest
    // panics on missing glyphs by default — tofu keeps the vi test rendering
    // (a11y labels are plain strings, glyphs don't matter)
    let builder = if is_en { builder } else { builder.allow_missing_glyphs() };
    builder
        .with_context_setup(move |ctx| {
            ctx.set_theme(egui::Theme::Dark);
            gui::icons::install(ctx);
        })
        .build_eframe(move |_cc| App::new(store_root, file))
}

/// English-locale harness (every pre-existing test pins "en").
fn make_harness(seed: &Seed, initial_file: Option<&Path>) -> Harness<'static, App> {
    make_harness_lang(seed, initial_file, "en")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Dumps EVERY node (role + label + rect, rectless nodes marked) — the anchor
/// catalog for the other tests. Run:
/// `cargo test -p gui --test kittest probe -- --nocapture`.
#[test]
fn probe_accessibility_tree() {
    let _locale = locale_guard();
    let s = seed_versions(3);
    let mut h = make_harness(&s, Some(&s.notes));
    h.run();
    select_notes(&mut h, &s);
    h.run();

    // Node::rect() panics on rectless nodes (the root) — silence the hook and
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
        "accessibility tree is empty — the app did not render"
    );
}

/// Selects `notes.txt` in the file list (the same flow a user performs; the
/// `--file` auto-open contract resolves asynchronously, so tests click the
/// row explicitly).
fn select_notes(h: &mut Harness<App>, seed: &Seed) {
    let name = seed.notes.file_name().unwrap().to_string_lossy().to_string();
    // the row is not unique on every platform (an exactly-labelled node can
    // repeat) — tree order puts the file-list row first, take that one
    let row = h
        .query_all(By::new().label(&name))
        .next()
        .unwrap_or_else(|| panic!("file row `{name}` not found in the tree"));
    row.click();
    h.run();
}

/// Rect of the LEFT pane's diff heading. `draw_pane_headings` exposes the
/// title as a Label whose ACCESSKIT VALUE carries the text ("seq N Â· kind Â·
/// HH:MM") — the label attribute is empty. Timeline rows are a different
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
    let _locale = locale_guard();
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
    // its content lines are painter-drawn (NOT in the a11y tree) — the pane
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
    let _locale = locale_guard();
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
    let _locale = locale_guard();
    let s = seed_batches();
    let mut h = make_harness(&s, None);
    h.run();

    // two auto batches were seeded (direct engine-level assertion)
    assert_eq!(h.state().batches.len(), 2, "seed must produce 2 batches");

    h.get_by_label("Activity").click();
    h.run();

    // batch rows are PAINTER-DRAWN (allocate + painter text) — intentionally
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
    // the a11y tree, like every custom-drawn cell) — their correctness is
    // covered by the pure builder tests in app.rs; here only the title and
    // the revert action are assertable headlessly.
}

/// Right-click on the diff area opens the copy context menu (all four items,
/// regardless of enabled state).
#[test]
fn diff_context_menu_lists_copy_actions() {
    let _locale = locale_guard();
    let s = seed_versions(3);
    let mut h = make_harness(&s, Some(&s.notes));
    h.run();
    select_notes(&mut h, &s);
    h.run();

    // anchor: the first diff row below the left pane's heading — Previous
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
    let _locale = locale_guard();
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

/// Seeded rename: `notes.txt` gets 2 external versions, then the file is
/// renamed on disk to `note-final.txt` and `engine.rename` moves the chain.
fn seed_rename() -> Seed {
    let store = TempDir::new().unwrap();
    let ws = TempDir::new().unwrap();
    let (lh, ws_id) = open_seeded(&store, &ws, false);
    let notes = ws.path().join("notes.txt");
    for i in 1..=2 {
        let content = format!("line v{i}\n");
        std::fs::write(&notes, &content).unwrap();
        lh.record(ws_id, &notes, content.as_bytes(), EventKind::External, None).unwrap();
    }
    let final_p = ws.path().join("note-final.txt");
    std::fs::rename(&notes, &final_p).unwrap();
    lh.rename(ws_id, &notes, &final_p).unwrap().expect("seed rename must record");
    let store_root = store.path().to_path_buf();
    Seed { _store: store, _ws: ws, store_root, notes: final_p }
}

/// Rename version: the timeline row (its a11y label is the row tooltip)
/// carries the `rename` kind AND the `renamed from <old path>` line; the
/// diff heading names the same version (`seq N · rename`).
#[test]
fn rename_row_shows_kind_and_old_path() {
    let _locale = locale_guard();
    let s = seed_rename();
    let mut h = make_harness(&s, Some(&s.notes));
    h.run();
    select_notes(&mut h, &s);
    h.run();

    // timeline row label = "seq 3 · rename · hash …\nrenamed from notes.txt"
    assert!(
        h.query_all(By::new().label_contains("renamed from notes.txt")).count() >= 1,
        "the rename row must carry the old path in its tooltip/label"
    );
    assert!(
        h.query_all(By::new().label_contains("· rename ·")).count() >= 1,
        "the rename row must show the rename kind"
    );
    // the diff pane heading names the selected (newest) version as rename
    assert!(
        h.query_all(By::new().role(egui::accesskit::Role::Label))
            .filter(|n| n.value().is_some_and(|v| v.contains("· rename ·")))
            .count()
            >= 1,
        "the diff heading must name the rename version"
    );
}

/// Revert from the GUI: select seq 1 in the timeline, click
/// "Revert to this version", confirm in the modal ("Revert" button) — the
/// file on disk goes back to v1 byte-exact.
#[test]
fn revert_from_gui_restores_disk_content() {
    let _locale = locale_guard();
    let s = seed_versions(3);
    let mut h = make_harness(&s, Some(&s.notes));
    h.run();
    select_notes(&mut h, &s);
    h.run();

    // click the timeline row of seq 1 (role Unknown rows carry the tooltip as
    // their label; the file-list row and pane headings have different roles)
    let row = h
        .query_all(By::new().label_contains("seq 1"))
        .find(|n| n.accesskit_node().role() == egui::accesskit::Role::Unknown)
        .unwrap_or_else(|| panic!("timeline row `seq 1` not found in the tree"));
    row.click();
    h.run();

    h.get_by_label_contains("Revert to this version").click();
    h.run();
    // the confirm modal's action button is exactly "Revert" (btn.do_revert)
    let confirm = h
        .query_all(By::new().label("Revert"))
        .next()
        .unwrap_or_else(|| panic!("confirm modal `Revert` button not found"));
    confirm.click();
    h.run();

    let disk = std::fs::read(&s.notes).unwrap();
    assert_eq!(
        disk,
        b"line v1\nsecond line 1\n".to_vec(),
        "GUI revert must restore seq 1's content on disk"
    );
}

/// Whole-batch revert from the GUI: the confirm modal's action restores every
/// file of the modification batch to its pre-batch content on disk.
#[test]
fn revert_batch_via_gui_restores_files() {
    let _locale = locale_guard();
    let s = seed_batches();
    let mut h = make_harness(&s, None);
    h.run();
    h.get_by_label("Activity").click();
    h.run();

    // select the modification batch — the FIRST row in the Activity tab is the
    // newest batch (batches() is newest-first), and the user clicks that row
    let batch = h.state().batches.first().unwrap().clone();
    h.state_mut().selected_batch = Some(batch.id);
    h.state_mut().next_poll = None;
    h.run();

    // open the confirm modal directly (a row click would set the same field),
    // then click the modal's action button — both the batch view and the
    // modal carry "Revert whole batch", so pick the LOWEST one on screen (the
    // modal is vertically centered, the view button sits in the panel top)
    h.state_mut().confirm_revert_batch = Some(batch.clone());
    h.run();
    let nodes: Vec<_> = h
        .query_all(By::new().label_contains("Revert whole batch"))
        .collect();
    assert!(nodes.len() >= 2, "modal + view `Revert whole batch` buttons not found");
    let modal_btn = nodes
        .iter()
        .max_by_key(|n| n.rect().top() as i64)
        .unwrap();
    modal_btn.click();
    h.run();
    h.run();
    assert!(
        h.state().confirm_revert_batch.is_none(),
        "clicking the modal button must close the modal (revert executed)"
    );

    // every seeded file is back to its pre-batch content on disk
    for i in 1..=3 {
        let p = s._ws.path().join(format!("svc{i}.ini"));
        let disk = std::fs::read_to_string(&p).unwrap();
        assert_eq!(
            disk,
            format!("app=svc{i}\nretries=3\nmode=normal\n"),
            "batch revert must restore {p:?} to its pre-batch content"
        );
    }
}

/// Set Label from the GUI: select seq 1, open the label modal, submit — the
/// version carries the label in the app state (set_label edits in place).
#[test]
fn label_via_gui_sets_version_label() {
    let _locale = locale_guard();
    let s = seed_versions(3);
    let mut h = make_harness(&s, Some(&s.notes));
    h.run();
    select_notes(&mut h, &s);
    h.run();

    // select seq 1 in the timeline
    let row = h
        .query_all(By::new().label_contains("seq 1"))
        .find(|n| n.accesskit_node().role() == egui::accesskit::Role::Unknown)
        .unwrap_or_else(|| panic!("timeline row `seq 1` not found in the tree"));
    row.click();
    h.run();

    // open the label modal (two "Set label" buttons may exist — either opens
    // the same modal for the selected version), fill the draft, confirm
    h.query_all(By::new().label_contains("Set label"))
        .next()
        .unwrap_or_else(|| panic!("`Set label` button not found"))
        .click();
    h.run();
    h.state_mut().label_draft = "before deploy".into();
    h.query_all(By::new().label("OK"))
        .next()
        .unwrap_or_else(|| panic!("modal `OK` button not found"))
        .click();
    h.run();
    h.run();

    let labeled = h
        .state()
        .versions
        .iter()
        .find(|v| v.seq == 1)
        .and_then(|v| v.label.clone())
        .unwrap_or_else(|| panic!("seq 1 not in the loaded versions"));
    assert_eq!(labeled, "before deploy", "set_label must reach the version");
}

/// Kind filter hides everything but the selected kind; the label filter keeps
/// only versions whose user label contains the query.
#[test]
fn kind_and_label_filters_narrow_the_timeline() {
    let _locale = locale_guard();
    // --- kind filter on a rename chain (1 rename + 2 external versions) ---
    let s = seed_rename();
    let mut h = make_harness(&s, Some(&s.notes));
    h.run();
    select_notes(&mut h, &s);
    h.run();

    // timeline rows are role-Unknown nodes whose label (the tooltip) starts
    // with "seq N ·" — pane headings are role Label, file rows have no "seq"
    let visible_rows = |h: &Harness<App>| {
        h.query_all(By::new().label_contains("seq "))
            .filter(|n| n.accesskit_node().role() == egui::accesskit::Role::Unknown)
            .count()
    };
    assert_eq!(visible_rows(&h), 3, "unfiltered: rename + 2 external rows");

    h.state_mut().kind_filter = Some(EventKind::Rename);
    h.run();
    assert_eq!(visible_rows(&h), 1, "kind=rename leaves exactly one row");
    let external_rows = h
        .query_all(By::new().label_contains("· external ·"))
        .filter(|n| n.accesskit_node().role() == egui::accesskit::Role::Unknown)
        .count();
    assert_eq!(external_rows, 0, "external rows must be hidden by the kind filter");
    h.state_mut().kind_filter = None;
    h.run();

    // --- label filter: set the label through the GUI's own modal flow ---
    // (an external engine write is not picked up by the ro connection inside
    // one test — the GUI's own submit_label goes through its rw connection)
    let seq1_id = h
        .state()
        .versions
        .iter()
        .find(|v| v.seq == 1)
        .map(|v| v.id)
        .unwrap_or_else(|| panic!("seq 1 not in the loaded versions"));
    h.state_mut().selected_version = Some(seq1_id);
    h.run();
    h.query_all(By::new().label_contains("Set label"))
        .next()
        .unwrap_or_else(|| panic!("`Set label` button not found"))
        .click();
    h.run();
    h.state_mut().label_draft = "before deploy".into();
    h.query_all(By::new().label("OK"))
        .next()
        .unwrap_or_else(|| panic!("modal `OK` button not found"))
        .click();
    h.run();
    h.run();
    assert_eq!(visible_rows(&h), 3, "unfiltered after labeling: all rows still visible");

    h.state_mut().label_filter = "deploy".into();
    h.run();
    h.run();
    assert_eq!(visible_rows(&h), 1, "label filter keeps only seq 1");
    h.state_mut().label_filter = "nomatch-anywhere".into();
    h.run();
    assert_eq!(visible_rows(&h), 0, "a label that matches nothing hides every row");
}

/// Vietnamese locale: the pinned gui-state.json switches every chrome label
/// (tabs "Tệp" / "Hoạt động") — the same App, no recompilation.
#[test]
fn vi_locale_translates_the_chrome() {
    let _locale = locale_guard();
    let s = seed_versions(3);
    let mut h = make_harness_lang(&s, None, "vi");
    h.run();

    assert!(h.query_all(By::new().label("Tệp")).count() >= 1, "tab Files in Vietnamese");
    assert!(
        h.query_all(By::new().label("Hoạt động")).count() >= 1,
        "tab Activity in Vietnamese"
    );
    // the English labels must be gone from the tab strip
    assert_eq!(h.query_all(By::new().label("Files")).count(), 0);
}
