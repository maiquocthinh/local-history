//! Diff view — builds the left/right line arrays from the engine's structured
//! hunks and renders them side by side (a single `ScrollArea::show_rows` keeps
//! both panes' scrolling in sync).

use std::collections::BTreeSet;

use eframe::egui;
use history::Hunk;
use rust_i18n::t;

/// Background of deleted lines (left side of a change).
const DEL_BG: egui::Color32 = egui::Color32::from_rgb(60, 25, 25);
/// Background of added lines (right side of a change).
const ADD_BG: egui::Color32 = egui::Color32::from_rgb(25, 60, 30);
const NUM_COLOR: egui::Color32 = egui::Color32::from_rgb(130, 130, 130);
const TEXT_COLOR: egui::Color32 = egui::Color32::from_rgb(215, 215, 215);
const SEP_STROKE: egui::Stroke =
    egui::Stroke { width: 1.0, color: egui::Color32::from_rgb(70, 70, 70) };
const DIFF_FONT_SIZE: f32 = 13.0;
/// Width of the line-number gutter.
const GUTTER: f32 = 56.0;

/// Monospace font for diff text (no new embedded font — egui's built-in monospace).
fn diff_font() -> egui::FontId {
    egui::FontId::monospace(DIFF_FONT_SIZE)
}

fn num_font() -> egui::FontId {
    egui::FontId::monospace(DIFF_FONT_SIZE - 1.0)
}

/// One line rendered in a diff pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    /// Line content verbatim, including the terminator (stripped when rendering).
    pub text: String,
    /// 1-based line number within the pane's content; `None` for filler rows
    /// that pad a length mismatch between the two sides.
    pub no: Option<usize>,
    /// Deleted line (appears on the left side of a change).
    pub del: bool,
    /// Added line (appears on the right side of a change).
    pub add: bool,
}

impl Line {
    fn context(text: &str, no: usize) -> Self {
        Line { text: text.to_string(), no: Some(no), del: false, add: false }
    }
    fn del(text: &str, no: usize) -> Self {
        Line { text: text.to_string(), no: Some(no), del: true, add: false }
    }
    fn add(text: &str, no: usize) -> Self {
        Line { text: text.to_string(), no: Some(no), del: false, add: true }
    }
    /// Filler row padding a length mismatch — neutral, never colored.
    fn filler() -> Self {
        Line { text: String::new(), no: None, del: false, add: false }
    }
}

/// Builds the two line arrays (left = base/old, right = target/new) from hunks,
/// following `model::Hunk` splice semantics: a hunk replaces
/// `base[old_start-1 .. old_start-1+old_lines]` with
/// `target[new_start-1 .. new_start-1+new_lines]`, and hunks arrive with
/// ascending `old_start` (the engine's canonical output). Context lines pair
/// 1:1; within a change region sides pair in order and the shorter side gets
/// filler rows (empty text, `no = None`) so both panes stay row-aligned.
pub fn apply_hunks_to_sides(
    base_lines: &[&str],
    target_lines: &[&str],
    hunks: &[Hunk],
) -> (Vec<Line>, Vec<Line>) {
    let mut left: Vec<Line> = Vec::new();
    let mut right: Vec<Line> = Vec::new();
    let push_context = |left: &mut Vec<Line>,
                        right: &mut Vec<Line>,
                        base: &str,
                        base_no: usize,
                        target: &str,
                        target_no: usize| {
        left.push(Line::context(base, base_no));
        right.push(Line::context(target, target_no));
    };

    let mut oi = 0usize; // base lines consumed
    let mut ni = 0usize; // target lines consumed
    for h in hunks {
        let s = (h.old_start as usize).saturating_sub(1).min(base_lines.len());
        // context between the previous hunk and this one
        let ctx = s.saturating_sub(oi);
        for i in 0..ctx {
            let b = base_lines.get(oi + i).copied().unwrap_or("");
            let t = target_lines.get(ni + i).copied().unwrap_or("");
            push_context(&mut left, &mut right, b, oi + i + 1, t, ni + i + 1);
        }
        ni += ctx; // oi is left alone — it jumps straight to s + del_n after the hunk

        // change region: del on the left (only i < old_lines), add on the right
        // (only i < new_lines); the shorter side is padded and the side not
        // participating in the hunk consumes no lines.
        let del_n = h.old_lines as usize;
        let add_n = h.new_lines as usize;
        for i in 0..del_n.max(add_n) {
            match base_lines.get(s + i).filter(|_| i < del_n) {
                Some(b) => left.push(Line::del(b, s + i + 1)),
                None => left.push(Line::filler()),
            }
            match target_lines.get(ni + i).filter(|_| i < add_n) {
                Some(t) => right.push(Line::add(t, ni + i + 1)),
                None => right.push(Line::filler()),
            }
        }
        oi = s + del_n;
        ni += add_n;
    }
    // context after the last hunk
    let tail = base_lines.len().saturating_sub(oi);
    for i in 0..tail {
        let b = base_lines.get(oi + i).copied().unwrap_or("");
        let t = target_lines.get(ni + i).copied().unwrap_or("");
        push_context(&mut left, &mut right, b, oi + i + 1, t, ni + i + 1);
    }
    (left, right)
}

/// Complete diff model for the viewer, built by `app` from engine + disk data.
#[derive(Debug, Clone, Default)]
pub struct DiffModel {
    /// Heading of the left pane.
    pub left_title: String,
    /// Heading of the right pane.
    pub right_title: String,
    /// Left (old) pane lines.
    pub left: Vec<Line>,
    /// Right (new) pane lines.
    pub right: Vec<Line>,
    /// Content is binary and cannot be diffed.
    pub binary: bool,
    /// Status message (no file selected, first version, ...).
    pub note: Option<String>,
    /// Both sides are identical (zero hunks).
    pub identical: bool,
    /// Character count of the longest line (drives the horizontal cell width).
    pub max_chars: usize,
}

impl DiffModel {
    /// Model that only shows a status note, with empty panes.
    pub fn empty_note(note: impl Into<String>) -> Self {
        DiffModel { note: Some(note.into()), ..Default::default() }
    }

    /// Model for binary content: shows the "binary" note only.
    pub fn binary_note() -> Self {
        DiffModel {
            binary: true,
            note: Some(t!("diff.binary").to_string()),
            ..Default::default()
        }
    }

    /// Recomputes `max_chars` from both sides (drives the horizontal cell width).
    pub fn compute_max_chars(&mut self) {
        self.max_chars = self
            .left
            .iter()
            .chain(self.right.iter())
            .map(|l| l.text.chars().count())
            .max()
            .unwrap_or(0);
    }

    /// Finishes a freshly built model by computing `max_chars`.
    pub fn finished(mut self) -> Self {
        self.compute_max_chars();
        self
    }
}

// ---------------------------------------------------------------------------
// Line selection (click / ctrl+click / shift+click) + raw-text copy
// ---------------------------------------------------------------------------

// ponytail: no drag-select of text WITHIN a diff line (char-level selection) — selection is line-level only.

/// Which pane a selected diff line belongs to. Enum order = display order of
/// the panes (the left cell is drawn before the right cell of a row).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Pane {
    /// Old side (left cell of a diff row).
    Left,
    /// New side (right cell of a diff row).
    Right,
}

/// One selected diff line. Field order defines the sort order: `row` FIRST ⇒ a
/// `BTreeSet<SelLine>` iterates in DISPLAY order (row-major, left before
/// right) — exactly the order copied text uses. Do not swap the fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct SelLine {
    /// Row index in the diff grid (both panes share the same row indexes).
    pub row: usize,
    /// Pane the line was clicked in.
    pub pane: Pane,
}

/// Line-level selection of the diff view: the highlighted rows plus the anchor
/// for shift+click ranges. Reset to empty whenever the diff pair changes
/// (`app::diff_panel` does this when its cached diff key goes stale); rows left
/// over from an older, longer pair are also tolerated at copy time (skipped,
/// never panic).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiffSelection {
    /// Selected lines, iterated in display order regardless of click order.
    pub lines: BTreeSet<SelLine>,
    /// Last plain/ctrl-clicked line; the fixed origin of shift+click ranges
    /// (shift+click does NOT move it, so repeated shift+clicks keep extending
    /// from the same origin).
    pub anchor: Option<SelLine>,
}

impl DiffSelection {
    /// Whether the given cell is selected.
    pub fn contains(&self, pane: Pane, row: usize) -> bool {
        self.lines.contains(&SelLine { row, pane })
    }
}

/// Mouse modifiers of a click on a diff row (a plain struct so the click rules
/// are unit-testable without a real event system).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ClickMods {
    /// Ctrl held: toggle the clicked line in/out of the selection.
    pub ctrl: bool,
    /// Shift held: select the display-order range from the anchor to the click.
    pub shift: bool,
}

/// Applies one click on a diff row:
/// plain click = select only that line (anchor moves); ctrl+click = toggle that
/// line (anchor moves); shift+click = select the display-order range from the
/// anchor to the click, REPLACING the selection (anchor stays; a missing anchor
/// behaves like a plain click).
pub fn apply_click(sel: &mut DiffSelection, clicked: SelLine, mods: ClickMods) {
    if mods.ctrl {
        if !sel.lines.remove(&clicked) {
            sel.lines.insert(clicked);
        }
        sel.anchor = Some(clicked);
    } else if mods.shift {
        let start = sel.anchor.unwrap_or(clicked);
        let (lo, hi) = if start <= clicked { (start, clicked) } else { (clicked, start) };
        let mut lines = BTreeSet::new();
        let mut cur = lo;
        loop {
            lines.insert(cur);
            if cur >= hi {
                break;
            }
            cur = next_display_line(cur);
        }
        sel.lines = lines;
    } else {
        *sel = DiffSelection { lines: BTreeSet::from([clicked]), anchor: Some(clicked) };
    }
}

/// Next line in display order (row-major, left before right): the right cell of
/// the same row, else the left cell of the next row. Rows beyond the model are
/// harmless — they are filtered out at copy time — so no bound check is needed.
fn next_display_line(l: SelLine) -> SelLine {
    match l.pane {
        Pane::Left => SelLine { row: l.row, pane: Pane::Right },
        Pane::Right => SelLine { row: l.row + 1, pane: Pane::Left },
    }
}

/// Raw text of the selected lines: content verbatim (tabs kept), line numbers
/// and terminators stripped, joined with `\n` in display order. Rows out of
/// range (a stale selection right after the diff pair changed) are skipped —
/// this never panics.
pub fn selection_text(model: &DiffModel, sel: &DiffSelection) -> String {
    let mut out = String::new();
    for s in &sel.lines {
        let pane = match s.pane {
            Pane::Left => &model.left,
            Pane::Right => &model.right,
        };
        let Some(line) = pane.get(s.row) else { continue };
        if !out.is_empty() {
            out.push('\n');
        }
        let t = line.text.strip_suffix('\n').unwrap_or(&line.text);
        let t = t.strip_suffix('\r').unwrap_or(t);
        out.push_str(t);
    }
    out
}

/// Renders the diff model into the center panel: two column headings +
/// side-by-side rows. Also handles diff-row click selection and Ctrl+C
/// (`egui::Event::Copy` — egui-winit turns the Ctrl+C / Ctrl+Insert chord into
/// this event, so a `Key::C` press never fires): returns `Some(n)` when the raw
/// text of the `n` selected lines was just copied to the clipboard (the caller
/// shows the toast).
pub fn show_diff(ui: &mut egui::Ui, model: &DiffModel, sel: &mut DiffSelection) -> Option<usize> {
    let total = model.left.len().max(model.right.len());
    let note = model
        .note
        .clone()
        .or_else(|| model.identical.then(|| t!("diff.identical").to_string()));
    if let Some(note) = note {
        ui.vertical_centered(|ui| {
            ui.add_space(6.0);
            ui.label(egui::RichText::new(note).weak());
        });
    }
    if model.binary || total == 0 {
        return None;
    }

    // Cell width: at least half the available width, stretched by the longest line.
    let font = diff_font();
    let row_h = ui.fonts_mut(|f| f.row_height(&font)) + 4.0;
    let char_w = ui.fonts_mut(|f| f.glyph_width(&font, '0')).max(0.5);
    let avail = ui.available_width() - 20.0;
    let min_cell = (avail / 2.0).max(120.0);
    let content_w = GUTTER + 8.0 + model.max_chars as f32 * char_w;
    let cell_w = min_cell.max(content_w);

    // Two column headings (same width split as the minimum content layout).
    ui.horizontal(|ui| {
        let w = (ui.available_width() - 20.0) / 2.0;
        ui.add_sized(
            [w, 18.0],
            egui::Label::new(egui::RichText::new(&model.left_title).monospace().weak())
                .wrap_mode(egui::TextWrapMode::Truncate),
        );
        ui.add_sized(
            [w, 18.0],
            egui::Label::new(egui::RichText::new(&model.right_title).monospace().weak())
                .wrap_mode(egui::TextWrapMode::Truncate),
        );
    });
    ui.separator();

    // ONE ScrollArea walking row indexes → both columns in the same row ⇒
    // scrolling stays in sync naturally.
    egui::ScrollArea::both()
        .id_salt("diff_scroll")
        .auto_shrink([false, false])
        .show_rows(ui, row_h, total, |ui, rows| {
            for r in rows {
                let l = model.left.get(r);
                let rt = model.right.get(r);
                let m = click_mods(ui);
                ui.horizontal(|ui| {
                    let rl = draw_cell(ui, cell_w, row_h, l, sel.contains(Pane::Left, r));
                    let rr = draw_cell(ui, cell_w, row_h, rt, sel.contains(Pane::Right, r));
                    if rl.clicked() {
                        // clicking a diff row gives up any lingering keyboard
                        // focus (e.g. the file-search TextEdit) so the diff
                        // view owns Ctrl+C from here on
                        ui.memory_mut(|mem| {
                            if let Some(f) = mem.focused() {
                                mem.surrender_focus(f);
                            }
                        });
                        apply_click(sel, SelLine { row: r, pane: Pane::Left }, m);
                    }
                    if rr.clicked() {
                        ui.memory_mut(|mem| {
                            if let Some(f) = mem.focused() {
                                mem.surrender_focus(f);
                            }
                        });
                        apply_click(sel, SelLine { row: r, pane: Pane::Right }, m);
                    }
                });
            }
        });

    // Ctrl+C / Ctrl+Insert (the integration's "copy" command event): copy the
    // raw text of the selected lines. Skipped while some other widget owns
    // keyboard focus — a focused TextEdit handles Event::Copy itself (its
    // cursor selection wins; nothing is stolen from the modal/search boxes).
    if !sel.lines.is_empty()
        && ui.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Copy)))
        && ui.memory(|m| m.focused().is_none())
    {
        let text = selection_text(model, sel);
        if !text.is_empty() {
            let n = text.lines().count();
            ui.ctx().copy_text(text);
            return Some(n);
        }
    }
    None
}

/// Click modifiers of the current frame, read once per row.
fn click_mods(ui: &egui::Ui) -> ClickMods {
    ui.input(|i| ClickMods { ctrl: i.modifiers.ctrl, shift: i.modifiers.shift })
}

/// Draws one cell (the left or right half of a diff row). `selected` adds the
/// selection highlight on top of the del/add tint; the returned response is
/// used for click handling.
fn draw_cell(ui: &mut egui::Ui, w: f32, h: f32, line: Option<&Line>, selected: bool) -> egui::Response {
    // raw Sense::CLICK (not Sense::click()) — cells must NOT be focusable, or
    // clicking one would steal keyboard focus and block the Ctrl+C handler.
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, h), egui::Sense::CLICK);
    let bg = line.and_then(|l| {
        if l.del {
            Some(DEL_BG)
        } else if l.add {
            Some(ADD_BG)
        } else {
            None
        }
    });
    if let Some(bg) = bg {
        ui.painter().rect_filled(rect, 0.0, bg);
    }
    if selected {
        ui.painter().rect_filled(
            rect,
            0.0,
            ui.visuals().selection.bg_fill.gamma_multiply(0.6),
        );
    }
    ui.painter().vline(rect.left(), rect.y_range(), SEP_STROKE);
    let Some(l) = line else { return resp };
    let p = ui.painter_at(rect);
    let y = rect.center().y;
    if let Some(no) = l.no {
        p.text(
            egui::pos2(rect.left() + GUTTER - 8.0, y),
            egui::Align2::RIGHT_CENTER,
            format!("{no}"),
            num_font(),
            NUM_COLOR,
        );
    }
    // Strip the terminator for display; tabs become 4 spaces (monospace).
    let t = l.text.strip_suffix('\n').unwrap_or(&l.text);
    let t = t.strip_suffix('\r').unwrap_or(t);
    let t = t.replace('\t', "    ");
    p.text(
        egui::pos2(rect.left() + GUTTER, y),
        egui::Align2::LEFT_CENTER,
        t,
        diff_font(),
        TEXT_COLOR,
    );
    resp
}

// ---------------------------------------------------------------------------
// Tests — building the two columns from hunks (pure logic)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn lines<'a>(v: &[&'a str]) -> Vec<&'a str> {
        v.to_vec()
    }

    fn hunk(old_start: u64, old_lines: u64, new_start: u64, new_lines: u64, new_text: &str) -> Hunk {
        Hunk {
            old_start,
            old_lines,
            new_start,
            new_lines,
            new_text: new_text.to_string(),
        }
    }

    fn texts(v: &[Line]) -> Vec<String> {
        v.iter().map(|l| l.text.clone()).collect()
    }

    #[test]
    fn modify_middle() {
        let base = lines(&["a\n", "b\n", "c\n"]);
        let target = lines(&["a\n", "B\n", "c\n"]);
        let hunks = [hunk(2, 1, 2, 1, "B\n")];
        let (l, r) = apply_hunks_to_sides(&base, &target, &hunks);
        assert_eq!(texts(&l), vec!["a\n", "b\n", "c\n"]);
        assert_eq!(texts(&r), vec!["a\n", "B\n", "c\n"]);
        assert!(l[1].del);
        assert!(r[1].add);
        assert!(!l[0].del);
        assert!(!r[2].add);
        assert_eq!(l[1].no, Some(2));
    }

    #[test]
    fn insert_head_and_tail() {
        // head insert: old_lines == 0, old_start == 1 — left gets a leading filler row
        let base = lines(&["b\n", "c\n"]);
        let target = lines(&["a\n", "b\n", "c\n"]);
        let (l, r) = apply_hunks_to_sides(&base, &target, &[hunk(1, 0, 1, 1, "a\n")]);
        assert_eq!(texts(&l), vec!["", "b\n", "c\n"]);
        assert_eq!(texts(&r), vec!["a\n", "b\n", "c\n"]);
        assert!(r[0].add);
        assert_eq!(l[0].no, None);
        assert_eq!(l[1].no, Some(1));
        assert_eq!(r[1].no, Some(2));

        // tail insert: old_start == len + 1, new line has no trailing newline
        let base = lines(&["a\n"]);
        let target = lines(&["a\n", "b"]);
        let (l, r) = apply_hunks_to_sides(&base, &target, &[hunk(2, 0, 2, 1, "b")]);
        assert_eq!(texts(&l), vec!["a\n", ""]); // left has a filler row
        assert_eq!(texts(&r), vec!["a\n", "b"]);
        assert!(r[1].add);
        assert_eq!(r[1].no, Some(2));
        assert_eq!(l[1].no, None);
    }

    #[test]
    fn delete_block() {
        let base = lines(&["a\n", "x\n", "y\n", "z\n", "b\n"]);
        let target = lines(&["a\n", "b\n"]);
        let hunks = [hunk(2, 3, 2, 0, "")];
        let (l, r) = apply_hunks_to_sides(&base, &target, &hunks);
        assert_eq!(texts(&l), vec!["a\n", "x\n", "y\n", "z\n", "b\n"]);
        assert_eq!(texts(&r), vec!["a\n", "", "", "", "b\n"]); // 3 filler rows on the right
        for line in l.iter().take(4).skip(1) {
            assert!(line.del);
        }
        assert!(r[1].no.is_none());
        assert_eq!(r[4].no, Some(2)); // "b\n" is target line 2
    }

    #[test]
    fn no_trailing_newline_edit() {
        let base = lines(&["a\n", "b"]);
        let target = lines(&["a\n", "B"]);
        let (l, r) = apply_hunks_to_sides(&base, &target, &[hunk(2, 1, 2, 1, "B")]);
        assert_eq!(texts(&l), vec!["a\n", "b"]);
        assert_eq!(texts(&r), vec!["a\n", "B"]);
        assert!(l[1].del);
        assert!(r[1].add);
        assert_eq!(l[1].text, "b"); // terminator preserved (absent here)
    }

    #[test]
    fn two_regions_padded_rows() {
        // Two change regions: the second adds one extra line ⇒ the left side
        // needs a filler row to keep the panes row-aligned.
        let base = lines(&["1\n", "2\n", "3\n", "4\n", "5\n", "6\n", "7\n", "8\n", "9\n", "10\n"]);
        let target = lines(&["1\n", "hai\n", "3\n", "4\n", "5\n", "6\n", "7\n", "8\n", "9\n", "mười\n", "mười một\n"]);
        let hunks = [
            hunk(2, 1, 2, 1, "hai\n"),
            hunk(10, 1, 10, 2, "mười\nmười một\n"),
        ];
        let (l, r) = apply_hunks_to_sides(&base, &target, &hunks);
        assert_eq!(l.len(), r.len(), "both panes must have equal row counts for synced scrolling");
        // the filler row is last: left None, right the second new line
        let last_l = l.last().unwrap();
        assert_eq!(last_l.no, None);
        assert_eq!(texts(&r)[r.len() - 1], "mười một\n");
    }

    #[test]
    fn identical_contents_no_hunks() {
        let base = lines(&["a\n", "b\n"]);
        let target = lines(&["a\n", "b\n"]);
        let (l, r) = apply_hunks_to_sides(&base, &target, &[]);
        assert_eq!(l.len(), 2);
        assert_eq!(r.len(), 2);
        assert!(l.iter().chain(r.iter()).all(|x| !x.del && !x.add));
    }

    #[test]
    fn empty_base_to_content() {
        let (l, r) = apply_hunks_to_sides(&[], &lines(&["hello\n", "world\n"]), &[hunk(1, 0, 1, 2, "hello\nworld\n")]);
        assert!(l.iter().all(|x| x.no.is_none()));
        assert_eq!(r.len(), 2);
        assert_eq!(texts(&r), vec!["hello\n", "world\n"]);
    }

    #[test]
    fn max_chars_counts() {
        let mut m = DiffModel {
            left: vec![Line::context("12345", 1)],
            right: vec![Line::context("ab", 1)],
            ..Default::default()
        };
        m.compute_max_chars();
        assert_eq!(m.max_chars, 5);
    }

    // ----- selection → text (click rules + raw copy) -----

    fn mods(ctrl: bool, shift: bool) -> ClickMods {
        ClickMods { ctrl, shift }
    }

    /// 3-row model: left = ["a", "old", filler], right = ["a", "new", "extra"].
    fn test_model() -> DiffModel {
        DiffModel {
            left_title: "left".into(),
            right_title: "right".into(),
            left: vec![Line::context("a\n", 1), Line::del("old\n", 2), Line::filler()],
            right: vec![Line::context("a\n", 1), Line::add("new\n", 2), Line::add("extra\n", 3)],
            binary: false,
            note: None,
            identical: false,
            max_chars: 5,
        }
    }

    #[test]
    fn plain_click_selects_one_line_and_copy_is_raw() {
        let m = test_model();
        let mut sel = DiffSelection::default();
        apply_click(&mut sel, SelLine { row: 1, pane: Pane::Left }, mods(false, false));
        assert_eq!(sel.lines.len(), 1);
        assert_eq!(sel.anchor, Some(SelLine { row: 1, pane: Pane::Left }));
        // raw text: no line number, no terminator, no decoration
        assert_eq!(selection_text(&m, &sel), "old");
        // a new plain click REPLACES the selection
        apply_click(&mut sel, SelLine { row: 0, pane: Pane::Right }, mods(false, false));
        assert_eq!(selection_text(&m, &sel), "a");
    }

    #[test]
    fn chaotic_ctrl_clicks_copy_in_display_order() {
        let m = test_model();
        let mut sel = DiffSelection::default();
        // deliberately scrambled click order — the copy must still be in
        // DISPLAY order (row-major, left before right at equal rows)
        apply_click(&mut sel, SelLine { row: 1, pane: Pane::Right }, mods(true, false));
        apply_click(&mut sel, SelLine { row: 0, pane: Pane::Left }, mods(true, false));
        apply_click(&mut sel, SelLine { row: 1, pane: Pane::Left }, mods(true, false));
        apply_click(&mut sel, SelLine { row: 0, pane: Pane::Right }, mods(true, false));
        // ctrl+click toggles: select a filler row (empty text), then drop it
        apply_click(&mut sel, SelLine { row: 2, pane: Pane::Left }, mods(true, false));
        apply_click(&mut sel, SelLine { row: 2, pane: Pane::Left }, mods(true, false));
        assert_eq!(selection_text(&m, &sel), "a\na\nold\nnew");
    }

    #[test]
    fn empty_selection_copies_nothing() {
        let m = test_model();
        assert_eq!(selection_text(&m, &DiffSelection::default()), "");
        // ctrl+clicking the only selected line empties the selection again
        let mut sel = DiffSelection::default();
        apply_click(&mut sel, SelLine { row: 0, pane: Pane::Left }, mods(false, false));
        apply_click(&mut sel, SelLine { row: 0, pane: Pane::Left }, mods(true, false));
        assert!(sel.lines.is_empty(), "ctrl+click must toggle the line off");
        assert_eq!(selection_text(&m, &sel), "");
    }

    #[test]
    fn two_panes_same_row_left_before_right() {
        let m = test_model();
        let mut sel = DiffSelection::default();
        apply_click(&mut sel, SelLine { row: 1, pane: Pane::Right }, mods(true, false));
        apply_click(&mut sel, SelLine { row: 1, pane: Pane::Left }, mods(true, false));
        assert_eq!(selection_text(&m, &sel), "old\nnew");
    }

    #[test]
    fn shift_click_selects_range_from_anchor() {
        let m = test_model();
        let mut sel = DiffSelection::default();
        // shift+click with no anchor behaves like a plain click
        apply_click(&mut sel, SelLine { row: 2, pane: Pane::Right }, mods(false, true));
        assert_eq!(selection_text(&m, &sel), "extra");
        // plain click sets the anchor
        apply_click(&mut sel, SelLine { row: 0, pane: Pane::Left }, mods(false, false));
        // shift+click replaces the selection with the display-order range
        apply_click(&mut sel, SelLine { row: 1, pane: Pane::Right }, mods(false, true));
        assert_eq!(selection_text(&m, &sel), "a\na\nold\nnew");
        // the anchor stays put: extending further still starts at the same origin
        apply_click(&mut sel, SelLine { row: 2, pane: Pane::Right }, mods(false, true));
        // the range includes the left filler row (an empty copied line)
        assert_eq!(selection_text(&m, &sel), "a\na\nold\nnew\n\nextra");
    }

    #[test]
    fn out_of_range_rows_are_skipped_never_panic() {
        let m = test_model(); // 3 rows per pane
        let mut sel = DiffSelection::default();
        // stale rows left over from an older, longer diff pair
        apply_click(&mut sel, SelLine { row: 10, pane: Pane::Left }, mods(true, false));
        apply_click(&mut sel, SelLine { row: 99, pane: Pane::Right }, mods(true, false));
        apply_click(&mut sel, SelLine { row: 0, pane: Pane::Left }, mods(true, false));
        assert_eq!(selection_text(&m, &sel), "a");
        // note-only model (empty panes) with the same selection — still no panic
        let empty = DiffModel::empty_note("gone");
        assert_eq!(selection_text(&empty, &sel), "");
    }

    #[test]
    fn pair_change_resets_selection() {
        // app::diff_panel resets to DiffSelection::default() when the diff key
        // changes; the reset selection is empty against any model.
        let m = test_model();
        let mut sel = DiffSelection::default();
        apply_click(&mut sel, SelLine { row: 1, pane: Pane::Left }, mods(false, false));
        assert_eq!(selection_text(&m, &sel), "old");
        let sel = DiffSelection::default(); // the reset applied by app on a stale key
        let shorter = DiffModel {
            left: vec![Line::context("x\n", 1)],
            right: vec![Line::context("y\n", 1)],
            ..m
        };
        assert_eq!(selection_text(&shorter, &sel), "");
    }
}
