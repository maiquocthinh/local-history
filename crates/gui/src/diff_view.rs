//! Diff view — builds the left/right line arrays from the engine's structured
//! hunks and renders them side by side (a single `ScrollArea::show_rows` keeps
//! both panes' scrolling in sync).

use crate::icons;
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
    /// that pad a length mismatch between the two sides (and for wrap
    /// continuation rows — see [`wrap_lines`]).
    pub no: Option<usize>,
    /// Deleted line (appears on the left side of a change).
    pub del: bool,
    /// Added line (appears on the right side of a change).
    pub add: bool,
    /// Soft-wrap continuation segment: joins the previous row's logical line
    /// when copying (no inserted newline between the two segments).
    pub cont: bool,
}

impl Line {
    fn context(text: &str, no: usize) -> Self {
        Line { text: text.to_string(), no: Some(no), del: false, add: false, cont: false }
    }
    fn del(text: &str, no: usize) -> Self {
        Line { text: text.to_string(), no: Some(no), del: true, add: false, cont: false }
    }
    fn add(text: &str, no: usize) -> Self {
        Line { text: text.to_string(), no: Some(no), del: false, add: true, cont: false }
    }
    /// Filler row padding a length mismatch — neutral, never colored.
    fn filler() -> Self {
        Line { text: String::new(), no: None, del: false, add: false, cont: false }
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
// Shared drawing helpers — the single-file diff (`show_diff`) and the batch
// view blocks (app.rs) both render side-by-side through these, so the grid
// looks and measures identically everywhere.
// ---------------------------------------------------------------------------

/// Measured grid geometry of the side-by-side diff, shared by `show_diff` and
/// the batch view blocks.
#[derive(Debug, Clone, Copy)]
pub struct DiffLayout {
    /// Row height (font row height + padding).
    pub row_h: f32,
    /// Width of one monospace char (drives the caret→column mapping).
    pub char_w: f32,
    /// Width of one cell (gutter + text) — exactly half the available width.
    pub cell_w: f32,
    /// Display chars per row before a line soft-wraps into a continuation row.
    pub wrap_cols: usize,
}

/// Measures the side-by-side grid: cells are EXACTLY half the available
/// width (never wider — long lines soft-wrap instead of scrolling the grid
/// horizontally, which used to drag the line-number gutters out of view).
pub fn measure_layout(ui: &mut egui::Ui) -> DiffLayout {
    let font = diff_font();
    let row_h = ui.fonts_mut(|f| f.row_height(&font)) + 4.0;
    let char_w = ui.fonts_mut(|f| f.glyph_width(&font, '0')).max(0.5);
    let avail = ui.available_width() - 20.0;
    let cell_w = (avail / 2.0).max(120.0);
    let wrap_cols =
        (((cell_w - GUTTER - 8.0) / char_w).floor() as usize).max(20);
    DiffLayout { row_h, char_w, cell_w, wrap_cols }
}

/// Soft-wraps every line longer than `cols` display chars into continuation
/// rows, keeping the uniform row-height grid intact (no horizontal scrolling
/// anywhere). The FIRST segment keeps the line number; continuation rows get
/// `cont = true` (copy logic joins them without inserted newlines) and
/// `no = None`. Segments carry no terminator — `line_content` semantics are
/// unchanged. Tabs count as one char here but render as four spaces, so a
/// tab-heavy wrapped line may exceed the cell by a few chars (clipped by the
/// cell painter).
pub fn wrap_lines(lines: &[Line], cols: usize) -> Vec<Line> {
    let mut out = Vec::with_capacity(lines.len());
    for l in lines {
        let content: Vec<char> = line_content(l).chars().collect();
        if content.len() <= cols {
            out.push(l.clone());
            continue;
        }
        let chunks = content.chunks(cols).count();
        for (idx, chunk) in content.chunks(cols).enumerate() {
            let mut seg = Line {
                text: chunk.iter().collect(),
                no: if idx == 0 { l.no } else { None },
                del: l.del,
                add: l.add,
                cont: idx > 0,
            };
            if idx == chunks - 1 && l.text.ends_with('\n') {
                seg.text.push('\n'); // keep the terminator on the last segment
            }
            out.push(seg);
        }
    }
    out
}

/// A display copy of the model whose rows are soft-wrapped to `cols` — the
/// selection/copy layer operates on the SAME wrapped rows the painter draws,
/// so carets, highlight and copied text stay consistent.
fn wrap_model(model: &DiffModel, cols: usize) -> DiffModel {
    let mut w = model.clone();
    w.left = wrap_lines(&model.left, cols);
    w.right = wrap_lines(&model.right, cols);
    w
}

/// Status text shown above the panes: an explicit note, or the "identical"
/// message when there are zero hunks. Shared by `show_diff` and the batch view.
pub fn note_text(model: &DiffModel) -> Option<String> {
    model
        .note
        .clone()
        .or_else(|| model.identical.then(|| t!("diff.identical").to_string()))
}

/// Draws the two pane headings (half width each, truncated) + the separator
/// under them.
pub fn draw_pane_headings(ui: &mut egui::Ui, left_title: &str, right_title: &str) {
    ui.horizontal(|ui| {
        let w = (ui.available_width() - 20.0) / 2.0;
        ui.add_sized(
            [w, 18.0],
            egui::Label::new(egui::RichText::new(left_title).monospace().weak())
                .wrap_mode(egui::TextWrapMode::Truncate),
        );
        ui.add_sized(
            [w, 18.0],
            egui::Label::new(egui::RichText::new(right_title).monospace().weak())
                .wrap_mode(egui::TextWrapMode::Truncate),
        );
    });
    ui.separator();
}

/// Draws one row of the side-by-side grid: the left (old) and the right (new)
/// cell, LEFT TO RIGHT — MUST be called inside a `ui.horizontal` (two
/// sequential allocations in a vertical layout would stack them). `fill_*` is
/// the selected char range of each cell (single-file diff only; the batch view
/// passes `None`/`None` — no char-level selection there yet). Returns both cell
/// responses for the drag/click handling in `show_diff`.
pub fn draw_row_pair(
    ui: &mut egui::Ui,
    layout: &DiffLayout,
    left: Option<&Line>,
    right: Option<&Line>,
    fill_left: Option<(usize, usize)>,
    fill_right: Option<(usize, usize)>,
) -> (egui::Response, egui::Response) {
    let rl = draw_cell(ui, layout.cell_w, layout.row_h, layout.char_w, left, fill_left);
    let rr = draw_cell(ui, layout.cell_w, layout.row_h, layout.char_w, right, fill_right);
    (rl, rr)
}

// ---------------------------------------------------------------------------
// Character-level selection (drag / shift+click, editor style) + raw-text copy
// ---------------------------------------------------------------------------

/// Which pane a caret belongs to. Enum order = display order of the panes (the
/// left cell is drawn before the right cell of a row).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Pane {
    /// Old side (left cell of a diff row).
    Left,
    /// New side (right cell of a diff row).
    Right,
}

/// A caret position inside the diff grid: which pane, which row, which char
/// column. Field order defines the display order (row-major, left before
/// right, columns last) — `Ord` on `CaretPos` is exactly the order the
/// highlight and copied text follow. Do not swap the fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct CaretPos {
    /// Row index in the diff grid (both panes share the same row indexes).
    pub row: usize,
    /// Pane the caret is in.
    pub pane: Pane,
    /// Char column within the line, `0..=len` (terminator stripped; it may
    /// rest one past the last char = the end-of-line caret).
    pub col: usize,
}

/// Character-level selection of the diff view, working like a text editor: an
/// anchor (where the drag started / the shift+click origin) and a cursor (the
/// moving end). Reset whenever the diff pair changes (`app::diff_panel` does
/// this when its cached diff key goes stale); carets left over from an older,
/// larger pair are tolerated at paint/copy time (clamped or skipped, never
/// panic).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TextSelection {
    /// Selection origin (drag start / shift+click origin); `None` = unset.
    pub anchor: Option<CaretPos>,
    /// The other end (current drag position / plain-click caret).
    pub cursor: Option<CaretPos>,
}

impl TextSelection {
    /// Whether nothing is visibly selected: either end unset, or anchor and
    /// cursor collapsed onto the same caret (a plain click).
    pub fn is_empty(&self) -> bool {
        match (self.anchor, self.cursor) {
            (Some(a), Some(c)) => a == c,
            _ => true,
        }
    }

    /// The two carets in display order (`(from, to)`), or `None` when either
    /// end is unset. A backwards drag (anchor > cursor) is swapped here, so
    /// paint and copy never care about the drag direction.
    pub fn normalized(&self) -> Option<(CaretPos, CaretPos)> {
        let (a, c) = (self.anchor?, self.cursor?);
        Some(if a <= c { (a, c) } else { (c, a) })
    }

    /// Clears the selection (both ends unset).
    pub fn reset(&mut self) {
        self.anchor = None;
        self.cursor = None;
    }
}

/// Char column of the caret under a pointer x inside a cell's text area. Pure
/// so the clamp rules are unit-testable: `col = ((x - text_left) / char_w)
/// .round()` clamped to `[0, max_col]` — left of the text start clamps to 0,
/// past the end of the line to `max_col` (the end-of-line caret). The float →
/// usize cast saturates, so negatives round into 0.
pub fn caret_from_pointer(
    pointer_x: f32,
    text_left: f32,
    char_w: f32,
    max_col: usize,
    row: usize,
    pane: Pane,
) -> CaretPos {
    let rel = (pointer_x - text_left) / char_w.max(1.0e-6);
    let col = (rel.round() as usize).min(max_col);
    CaretPos { row, pane, col }
}

/// A line's text with the terminator stripped — the form caret columns,
/// painting and copying all work on.
fn line_content(l: &Line) -> &str {
    let t = l.text.strip_suffix('\n').unwrap_or(&l.text);
    t.strip_suffix('\r').unwrap_or(t)
}

/// Char count of [`line_content`] — the caret column space of a line. Tabs
/// count as one char: columns are indexes into the verbatim text (copies keep
/// tabs, the display expands them to 4 spaces).
fn line_chars(l: &Line) -> usize {
    line_content(l).chars().count()
}

/// Selected char range of one cell under the normalized (from, to) carets, or
/// `None` when the cell is outside the selection. `len` = the cell's char
/// count (0 for filler/missing cells). Editor convention: the `from` cell
/// keeps its tail `[from.col .. len)`, the `to` cell its head `[0 .. to.col)`,
/// and cells in between are taken full-length — but ONLY for a cross-pane
/// selection; a same-pane selection never touches the other pane, even cells
/// that sit "between" its ends in row-major order (left row N sits between
/// right row N-1 and right row N).
fn cell_selected_cols(
    from: CaretPos,
    to: CaretPos,
    row: usize,
    pane: Pane,
    len: usize,
) -> Option<(usize, usize)> {
    if from.pane == to.pane && pane != from.pane {
        return None;
    }
    let cell = (row, pane);
    let from_cell = (from.row, from.pane);
    let to_cell = (to.row, to.pane);
    if from_cell == to_cell {
        // both carets in one cell — normalized() already ordered the columns
        if cell == from_cell {
            return Some((from.col.min(len), to.col.min(len)));
        }
        return None;
    }
    if cell == from_cell {
        return Some((from.col.min(len), len));
    }
    if cell == to_cell {
        return Some((0, to.col.min(len)));
    }
    if from_cell < cell && cell < to_cell {
        return Some((0, len));
    }
    None
}

/// Raw text of the selected cells: each in-range cell contributes its selected
/// substring (terminator stripped, tabs kept verbatim), joined with `\n`. Both
/// ends in one pane (the UI guarantees this via [`confine_cursor`]) copy ONLY
/// that pane's lines; differing panes (a stale selection right after the diff
/// pair changed) fall back to display order (row-major, left before right).
/// Rows/cols out of the model are skipped — this never panics. A collapsed
/// selection yields "".
pub fn selection_text(model: &DiffModel, sel: &TextSelection) -> String {
    match sel.normalized() {
        Some((from, to)) => selection_segments(model, from, to).join("\n"),
        None => String::new(),
    }
}

/// The selected substring of every in-range cell, in iteration order. Empty
/// ranges (filler rows, end-of-line carets) keep an empty segment so the join
/// preserves blank lines. Soft-wrap continuation rows (cont = true) whose
/// predecessor in the SAME pane was also selected join the previous segment
/// with NO inserted newline — a wrapped logical line copies back as one line.
fn selection_segments(model: &DiffModel, from: CaretPos, to: CaretPos) -> Vec<String> {
    // iterate only as far as the model goes; stale rows beyond it are skipped
    let max_row = model.left.len().max(model.right.len()).saturating_sub(1);
    let last_row = to.row.min(max_row);
    let mut out: Vec<String> = Vec::new();
    let mut last: Option<(Pane, usize)> = None;
    for row in from.row..=last_row {
        for pane in [Pane::Left, Pane::Right] {
            let lines = match pane {
                Pane::Left => &model.left,
                Pane::Right => &model.right,
            };
            let Some(line) = lines.get(row) else { continue };
            let Some((c0, c1)) = cell_selected_cols(from, to, row, pane, line_chars(line)) else {
                continue;
            };
            let t = line_content(line);
            let seg: String = t.chars().skip(c0).take(c1.saturating_sub(c0)).collect();
            let contiguous = line.cont && last == Some((pane, row.wrapping_sub(1)));
            if contiguous && let Some(prev) = out.last_mut() {
                prev.push_str(&seg);
            } else {
                out.push(seg);
            }
            last = Some((pane, row));
        }
    }
    out
}

/// Geometry of one visible cell, kept while drawing so a global pointer
/// position (reported by the drag owner's response) can be mapped back onto
/// its (row, pane, col) caret.
struct CellGeom<'a> {
    row: usize,
    pane: Pane,
    rect: egui::Rect,
    line: Option<&'a Line>,
}

/// Caret for a global pointer position: the cell containing the position wins
/// (y picks the row, x the pane); a pointer dragged vertically past the drawn
/// rows (outside the viewport) clamps to the nearest visible row. `None` when
/// no rows are visible.
fn caret_at_screen_pos(pos: egui::Pos2, cells: &[CellGeom<'_>], char_w: f32) -> Option<CaretPos> {
    if let Some(c) = cells.iter().find(|c| c.rect.contains(pos)) {
        return Some(cell_caret(c, pos.x, char_w));
    }
    let nearest = cells.iter().min_by(|a, b| {
        let da = (pos.y - a.rect.center().y).abs();
        let db = (pos.y - b.rect.center().y).abs();
        da.total_cmp(&db)
    })?;
    Some(cell_caret(nearest, pos.x, char_w))
}

/// Caret inside one cell: the column comes from the monospace char grid
/// ([`caret_from_pointer`]), clamped to the cell's own char count.
fn cell_caret(c: &CellGeom<'_>, pointer_x: f32, char_w: f32) -> CaretPos {
    let max_col = c.line.map(line_chars).unwrap_or(0);
    caret_from_pointer(pointer_x, c.rect.left() + GUTTER, char_w, max_col, c.row, c.pane)
}

/// What [`show_diff`] did this frame — the caller translates actions into
/// toasts; the engine-backed patch copy stays in app.rs (store access never
/// reaches this module).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffAction {
    /// The raw text of the selection was copied to the clipboard (Ctrl+C or
    /// the context-menu item); the value = non-empty lines copied.
    CopiedSelection(usize),
    /// One pane's full text was copied via the context menu; value = lines.
    CopiedPane(usize),
    /// The context-menu "copy diff (patch)" item was clicked — the caller
    /// fetches the ENGINE's unified diff itself.
    CopyUnified,
}

/// Whole-pane text: every line's content (terminator stripped) joined with
/// `\n` — the context-menu "copy left/right pane" payload. A filler row
/// contributes an empty segment (a blank line in the copy).
fn pane_text(model: &DiffModel, pane: Pane) -> String {
    let lines = match pane {
        Pane::Left => &model.left,
        Pane::Right => &model.right,
    };
    lines.iter().map(line_content).collect::<Vec<_>>().join("\n")
}

/// Char count of one pane's line at `row` — the column space a cross-pane
/// caret is clamped onto (0 for a missing/filler row).
fn pane_row_len(model: &DiffModel, pane: Pane, row: usize) -> usize {
    let lines = match pane {
        Pane::Left => &model.left,
        Pane::Right => &model.right,
    };
    lines.get(row).map(line_chars).unwrap_or(0)
}

/// The moving end of a selection never leaves the anchor's pane (editor
/// convention — dragging across the gutter does not extend the selection
/// into the neighbouring pane). A caret in the other pane is remapped onto
/// the anchor's pane at the same row, column clamped to that row's end; a
/// caret already in the anchor's pane — or an unset anchor (defensive) —
/// passes through.
fn confine_cursor(sel: &TextSelection, model: &DiffModel, c: CaretPos) -> CaretPos {
    match sel.anchor {
        Some(a) if a.pane != c.pane => CaretPos {
            row: c.row,
            pane: a.pane,
            col: pane_row_len(model, a.pane, c.row),
        },
        _ => c,
    }
}

/// The context menu of a right-clicked diff cell (both panes share it).
/// Performs the copies directly on the clipboard and records what happened
/// into the caller's locals — the toast text differs per action, so the
/// caller ([`show_diff`]) turns them into a [`DiffAction`]. Items are styled
/// as flat full-width rows (the combo-box popup look), not floating buttons.
fn show_cell_menu(
    ui: &mut egui::Ui,
    model: &DiffModel,
    sel: &TextSelection,
    allow_unified: bool,
    copied_sel: &mut Option<usize>,
    copied_pane: &mut Option<usize>,
    want_unified: &mut bool,
) {
    // menu-item styling: tight stack, equal width, flat rows that highlight
    // on hover (SelectableLabel = egui's own dropdown-item look)
    ui.style_mut().spacing.item_spacing = egui::vec2(0.0, 0.0);
    ui.style_mut().spacing.button_padding = egui::vec2(10.0, 4.0);
    ui.set_min_width(200.0);
    let item = |ui: &mut egui::Ui, enabled: bool, text: String| -> bool {
        ui.add_enabled(enabled, egui::Button::selectable(false, text)).clicked()
    };

    if item(ui, !sel.is_empty(), format!("{} {}", icons::COPY, t!("diff.menu_copy_selection"))) {
        let text = selection_text(model, sel);
        let n = text.lines().filter(|l| !l.is_empty()).count();
        if n > 0 {
            ui.ctx().copy_text(text);
            *copied_sel = Some(n);
        }
        ui.close();
    }
    if item(ui, !model.left.is_empty(), format!("{} {}", icons::COPY, t!("diff.menu_copy_left"))) {
        ui.ctx().copy_text(pane_text(model, Pane::Left));
        *copied_pane = Some(model.left.len());
        ui.close();
    }
    if item(
        ui,
        !model.right.is_empty(),
        format!("{} {}", icons::COPY, t!("diff.menu_copy_right")),
    ) {
        ui.ctx().copy_text(pane_text(model, Pane::Right));
        *copied_pane = Some(model.right.len());
        ui.close();
    }
    // a thin divider with a little breathing room — not a fat gap block
    ui.add_space(3.0);
    ui.separator();
    ui.add_space(3.0);
    if item(ui, allow_unified, format!("{} {}", icons::COPY, t!("diff.copy"))) {
        *want_unified = true;
        ui.close();
    }
}

/// Renders the diff model into the center panel: two column headings +
/// side-by-side rows. Also handles char-level selection (drag / shift+click on
/// the diff cells, editor style — a selection stays inside the pane it
/// started in), Ctrl+C (`egui::Event::Copy` — egui-winit turns the Ctrl+C /
/// Ctrl+Insert chord into this event, so a `Key::C` press never fires) and a
/// right-click context menu with the copy actions. `allow_unified` enables
/// the "copy diff (patch)" menu item (the caller then copies the engine's
/// unified diff on [`DiffAction::CopyUnified`]); returns what was just done.
pub fn show_diff(
    ui: &mut egui::Ui,
    model: &DiffModel,
    sel: &mut TextSelection,
    allow_unified: bool,
) -> Option<DiffAction> {
    let total = model.left.len().max(model.right.len());
    if let Some(note) = note_text(model) {
        ui.vertical_centered(|ui| {
            ui.add_space(6.0);
            ui.label(egui::RichText::new(note).weak());
        });
    }
    if model.binary || total == 0 {
        return None;
    }

    // Grid geometry: cells fit the viewport exactly; long lines soft-wrap into
    // continuation rows (no horizontal scrolling — the gutters stay put).
    let layout = measure_layout(ui);
    let wrapped = wrap_model(model, layout.wrap_cols);
    let total = wrapped.left.len().max(wrapped.right.len());

    // Two column headings (same width split as the minimum content layout).
    draw_pane_headings(ui, &wrapped.left_title, &wrapped.right_title);

    // Events collected while drawing, processed after the row loop (the paint
    // of this frame already used the previous selection state; the update is
    // visible next frame — standard immediate-mode one-frame latency).
    let mut cells: Vec<CellGeom<'_>> = Vec::new();
    let mut drag_start_pos = None; // drag_started: anchor goes to the press position
    let mut drag_pos = None; // dragged/drag_stopped: cursor follows the pointer
    let mut click_pos = None; // clicked (press + release, no drag)
    let shift = ui.input(|i| i.modifiers.shift);
    // context-menu results (the copies happen inside show_cell_menu; the
    // caller only needs to know which toast to show)
    let mut copied_sel: Option<usize> = None;
    let mut copied_pane: Option<usize> = None;
    let mut want_unified = false;

    // ONE vertical ScrollArea walking row indexes → both columns in the same
    // row ⇒ scrolling stays in sync naturally; no horizontal axis at all.
    egui::ScrollArea::vertical()
        .id_salt("diff_scroll")
        .auto_shrink([false, false])
        .show_rows(ui, layout.row_h, total, |ui, rows| {
            for r in rows {
                let l = wrapped.left.get(r);
                let rt = wrapped.right.get(r);
                let norm = sel.normalized();
                let fill_l = norm.and_then(|(f, t)| {
                    cell_selected_cols(f, t, r, Pane::Left, l.map(line_chars).unwrap_or(0))
                });
                let fill_r = norm.and_then(|(f, t)| {
                    cell_selected_cols(f, t, r, Pane::Right, rt.map(line_chars).unwrap_or(0))
                });
                ui.horizontal(|ui| {
                    let (rl, rr) = draw_row_pair(ui, &layout, l, rt, fill_l, fill_r);
                    cells.push(CellGeom { row: r, pane: Pane::Left, rect: rl.rect, line: l });
                    cells.push(CellGeom { row: r, pane: Pane::Right, rect: rr.rect, line: rt });
                    if rl.drag_started() {
                        drag_start_pos = rl.interact_pointer_pos();
                    }
                    if rr.drag_started() {
                        drag_start_pos = rr.interact_pointer_pos();
                    }
                    // egui fires drag events only on the cell that owns the
                    // press, but the reported position is global: cells dragged
                    // OVER count — the caret maps onto whatever cell it falls in
                    if rl.dragged() || rl.drag_stopped() {
                        drag_pos = rl.interact_pointer_pos();
                    }
                    if rr.dragged() || rr.drag_stopped() {
                        drag_pos = rr.interact_pointer_pos();
                    }
                    if rl.clicked() {
                        click_pos = rl.interact_pointer_pos();
                    }
                    if rr.clicked() {
                        click_pos = rr.interact_pointer_pos();
                    }
                    // right-click on either cell opens the shared copy menu
                    rl.context_menu(|ui| {
                        show_cell_menu(
                            ui,
                            model,
                            sel,
                            allow_unified,
                            &mut copied_sel,
                            &mut copied_pane,
                            &mut want_unified,
                        )
                    });
                    rr.context_menu(|ui| {
                        show_cell_menu(
                            ui,
                            model,
                            sel,
                            allow_unified,
                            &mut copied_sel,
                            &mut copied_pane,
                            &mut want_unified,
                        )
                    });
                });
            }

            // drag start: anchor and cursor both go to the press position
            if let Some(pos) = drag_start_pos
                && let Some(c) = caret_at_screen_pos(pos, &cells, layout.char_w)
            {
                sel.anchor = Some(c);
                sel.cursor = Some(c);
                // starting a selection gives up any lingering keyboard focus
                // (e.g. the file-search TextEdit) so the diff view owns Ctrl+C
                ui.memory_mut(|mem| {
                    if let Some(f) = mem.focused() {
                        mem.surrender_focus(f);
                    }
                });
                ui.ctx().request_repaint();
            }
            // dragging: the cursor follows the pointer (confined to the
            // anchor's pane — a drag across the gutter never leaks there)
            if let Some(pos) = drag_pos
                && let Some(c) = caret_at_screen_pos(pos, &cells, layout.char_w)
            {
                sel.cursor = Some(confine_cursor(sel, model, c));
                ui.ctx().request_repaint();
            }
            // plain click: collapse the selection onto the click position;
            // shift+click: move only the cursor, keeping the anchor (editor
            // style — no anchor behaves like a plain click)
            if let Some(pos) = click_pos
                && let Some(c) = caret_at_screen_pos(pos, &cells, layout.char_w)
            {
                ui.memory_mut(|mem| {
                    if let Some(f) = mem.focused() {
                        mem.surrender_focus(f);
                    }
                });
                if shift && sel.anchor.is_some() {
                    sel.cursor = Some(confine_cursor(sel, model, c));
                } else {
                    sel.anchor = Some(c);
                    sel.cursor = Some(c);
                }
                ui.ctx().request_repaint();
            }
        });

    // Ctrl+C / Ctrl+Insert (the integration's "copy" command event): copy the
    // raw text of the selected cells. Skipped while some other widget owns
    // keyboard focus — a focused TextEdit handles Event::Copy itself (its
    // cursor selection wins; nothing is stolen from the modal/search boxes).
    if !sel.is_empty()
        && ui.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Copy)))
        && ui.memory(|m| m.focused().is_none())
    {
        let text = selection_text(&wrapped, sel);
        // every cell contributes exactly one line (terminators are stripped),
        // so the "{n} segment(s)" count = the non-empty lines of the text —
        // blank filler lines in the middle of a range are not segments
        let n = text.lines().filter(|l| !l.is_empty()).count();
        if n > 0 {
            ui.ctx().copy_text(text);
            return Some(DiffAction::CopiedSelection(n));
        }
    }
    // context-menu actions performed this frame (same one-frame latency as
    // the selection updates above)
    if let Some(n) = copied_sel {
        return Some(DiffAction::CopiedSelection(n));
    }
    if let Some(n) = copied_pane {
        return Some(DiffAction::CopiedPane(n));
    }
    if want_unified {
        return Some(DiffAction::CopyUnified);
    }
    None
}

/// Draws one cell (the left or right half of a diff row). `fill` = the selected
/// char range to highlight on top of the del/add tint: partial ranges are
/// painted by character on the monospace grid; a range covering the whole line
/// (middle rows, filler/empty cells, end-of-line) paints the whole text area
/// from the gutter to the cell edge — the line-number gutter is never tinted.
/// The returned response is used for the drag/click selection handling.
fn draw_cell(
    ui: &mut egui::Ui,
    w: f32,
    h: f32,
    char_w: f32,
    line: Option<&Line>,
    fill: Option<(usize, usize)>,
) -> egui::Response {
    // Sense::CLICK | Sense::DRAG — like Sense::click_and_drag() but WITHOUT
    // focusable: focusable cells would steal keyboard focus and block the
    // Ctrl+C handler; the drag sense is what enables char-level drag selection.
    let (rect, resp) =
        ui.allocate_exact_size(egui::vec2(w, h), egui::Sense::CLICK | egui::Sense::DRAG);
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
    if let Some((c0, c1)) = fill {
        let len = line.map(line_chars).unwrap_or(0);
        let x0 = rect.left() + GUTTER + c0 as f32 * char_w;
        let x1 = if c0 == 0 && c1 >= len {
            rect.right() // whole text area: middle rows, filler/empty cells, EOL
        } else {
            rect.left() + GUTTER + c1 as f32 * char_w
        };
        if x1 > x0 {
            ui.painter().rect_filled(
                egui::Rect::from_min_max(egui::pos2(x0, rect.top()), egui::pos2(x1, rect.bottom())),
                0.0,
                ui.visuals().selection.bg_fill.gamma_multiply(0.6),
            );
        }
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
    let t = line_content(l).replace('\t', "    ");
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

    // ----- char-level selection: caret mapping, normalized, raw copy -----

    /// 3-row model: left = ["alpha", "old line", filler],
    /// right = ["alpha", "new line", "extra"].
    fn test_model() -> DiffModel {
        DiffModel {
            left_title: "left".into(),
            right_title: "right".into(),
            left: vec![Line::context("alpha\n", 1), Line::del("old line\n", 2), Line::filler()],
            right: vec![
                Line::context("alpha\n", 1),
                Line::add("new line\n", 2),
                Line::add("extra\n", 3),
            ],
            binary: false,
            note: None,
            identical: false,
            max_chars: 8,
        }
    }

    fn caret(row: usize, pane: Pane, col: usize) -> CaretPos {
        CaretPos { row, pane, col }
    }

    fn sel_from_to(a: CaretPos, b: CaretPos) -> TextSelection {
        TextSelection { anchor: Some(a), cursor: Some(b) }
    }

    #[test]
    fn caret_from_pointer_clamps_to_line_bounds() {
        // far left of the text start → column 0
        assert_eq!(caret_from_pointer(-100.0, 0.0, 10.0, 5, 2, Pane::Left), caret(2, Pane::Left, 0));
        // inside the left half of the first char → still column 0
        assert_eq!(caret_from_pointer(4.0, 0.0, 10.0, 5, 0, Pane::Right), caret(0, Pane::Right, 0));
        // far right → clamped to the char count (end-of-line caret)
        assert_eq!(caret_from_pointer(1000.0, 0.0, 10.0, 5, 1, Pane::Left), caret(1, Pane::Left, 5));
    }

    #[test]
    fn caret_from_pointer_rounds_between_chars() {
        // 1.5 char widths → rounds to column 2 (the caret sits between chars)
        assert_eq!(caret_from_pointer(15.0, 0.0, 10.0, 9, 0, Pane::Left).col, 2);
        // 1.49 char widths → still column 1
        assert_eq!(caret_from_pointer(14.9, 0.0, 10.0, 9, 0, Pane::Left).col, 1);
        // exactly at the text origin → column 0
        assert_eq!(caret_from_pointer(0.0, 0.0, 10.0, 9, 0, Pane::Left).col, 0);
        // row and pane pass through untouched
        assert_eq!(caret_from_pointer(3.0, 0.0, 10.0, 9, 7, Pane::Right), caret(7, Pane::Right, 0));
    }

    #[test]
    fn normalized_orders_reversed_drags() {
        // anchor after cursor (a backwards drag) — display order comes out right
        let sel = sel_from_to(caret(2, Pane::Right, 4), caret(0, Pane::Left, 1));
        assert_eq!(sel.normalized(), Some((caret(0, Pane::Left, 1), caret(2, Pane::Right, 4))));
        // same row: left before right
        let sel = sel_from_to(caret(1, Pane::Right, 0), caret(1, Pane::Left, 3));
        assert_eq!(sel.normalized(), Some((caret(1, Pane::Left, 3), caret(1, Pane::Right, 0))));
        // either end unset → None
        assert_eq!(TextSelection::default().normalized(), None);
    }

    #[test]
    fn empty_selection_and_reset() {
        let mut sel = sel_from_to(caret(0, Pane::Left, 2), caret(1, Pane::Right, 0));
        assert!(!sel.is_empty());
        sel.reset();
        assert!(sel.is_empty());
        assert_eq!(sel, TextSelection::default());
        // collapsed: anchor == cursor (a plain click) — copies nothing
        let sel = sel_from_to(caret(0, Pane::Left, 2), caret(0, Pane::Left, 2));
        assert!(sel.is_empty());
        assert_eq!(selection_text(&test_model(), &sel), "");
    }

    #[test]
    fn same_cell_range_copies_substring() {
        let m = test_model();
        // "old line", columns 0..3
        let sel = sel_from_to(caret(1, Pane::Left, 0), caret(1, Pane::Left, 3));
        assert_eq!(selection_text(&m, &sel), "old");
        // reversed within the cell (normalized swaps the columns)
        let sel = sel_from_to(caret(1, Pane::Left, 3), caret(1, Pane::Left, 0));
        assert_eq!(selection_text(&m, &sel), "old");
        // past-the-end columns clamp to the line length
        let sel = sel_from_to(caret(1, Pane::Left, 5), caret(1, Pane::Left, 100));
        assert_eq!(selection_text(&m, &sel), "ine");
    }

    #[test]
    fn multi_row_range_takes_full_middle_cells() {
        let m = test_model();
        // from the middle of left row 0 to the middle of left row 1:
        // from-cell tail + full in-between LEFT cells + to-cell head — a
        // same-pane selection never includes the other pane's cells
        let sel = sel_from_to(caret(0, Pane::Left, 2), caret(1, Pane::Left, 3));
        assert_eq!(selection_text(&m, &sel), "pha\nold");
    }

    #[test]
    fn range_across_panes_follows_display_order() {
        let m = test_model();
        // left row 1 → right row 1 (same row, both panes)
        let sel = sel_from_to(caret(1, Pane::Left, 4), caret(1, Pane::Right, 3));
        assert_eq!(selection_text(&m, &sel), "line\nnew");
        // a drag across the pane boundary carries into the next row's left cell
        let sel = sel_from_to(caret(0, Pane::Right, 2), caret(1, Pane::Left, 3));
        assert_eq!(selection_text(&m, &sel), "pha\nold");
    }

    #[test]
    fn filler_rows_copy_as_blank_lines() {
        let m = test_model();
        // left row 2 is a filler: inside the range it contributes an empty line
        let sel = sel_from_to(caret(0, Pane::Left, 0), caret(2, Pane::Right, 2));
        assert_eq!(selection_text(&m, &sel), "alpha\nalpha\nold line\nnew line\n\nex");
    }

    #[test]
    fn out_of_range_rows_and_cols_are_skipped_never_panic() {
        let m = test_model(); // 3 rows per pane
        // stale rows from an older, longer pair: entirely beyond the model
        let sel = sel_from_to(caret(10, Pane::Left, 0), caret(12, Pane::Right, 4));
        assert_eq!(selection_text(&m, &sel), "");
        // columns past the end clamp; the from-cell at EOL copies nothing
        // (same-pane selection → right-pane cells only)
        let sel = sel_from_to(caret(1, Pane::Right, 100), caret(2, Pane::Right, 2));
        assert_eq!(selection_text(&m, &sel), "\nex");
        // a cursor row beyond the model extends the copy to the model's end
        let sel = sel_from_to(caret(2, Pane::Left, 0), caret(99, Pane::Right, 2));
        assert_eq!(selection_text(&m, &sel), "\nextra");
        // note-only model (empty panes) with any selection — still no panic
        let empty = DiffModel::empty_note("gone");
        let sel = sel_from_to(caret(0, Pane::Left, 0), caret(5, Pane::Right, 1));
        assert_eq!(selection_text(&empty, &sel), "");
    }

    #[test]
    fn tabs_are_kept_verbatim_in_copies() {
        let mut m = test_model();
        m.left[1] = Line::del("a\tb\n", 2);
        let sel = sel_from_to(caret(1, Pane::Left, 0), caret(1, Pane::Left, 3));
        assert_eq!(selection_text(&m, &sel), "a\tb");
    }

    #[test]
    fn pair_change_resets_selection() {
        // app::diff_panel resets to TextSelection::default() when the diff key
        // changes; the reset selection copies nothing against any model.
        let m = test_model();
        let mut sel = sel_from_to(caret(0, Pane::Left, 0), caret(1, Pane::Left, 2));
        assert!(!selection_text(&m, &sel).is_empty());
        sel.reset();
        let shorter = DiffModel {
            left: vec![Line::context("x\n", 1)],
            right: vec![Line::context("y\n", 1)],
            ..m
        };
        assert_eq!(selection_text(&shorter, &sel), "");
    }

    // ----- pane confinement: a drag never extends into the other pane -----

    #[test]
    fn cell_selected_cols_same_pane_never_takes_the_other_pane() {
        // same-pane selection spanning rows: the OTHER pane's cells are not
        // selected, even "in-between" ones in row-major order (the highlight
        // bug: left rows 1→2 must not highlight right row 1)
        let from = caret(0, Pane::Left, 0);
        let to = caret(1, Pane::Left, 8);
        assert_eq!(cell_selected_cols(from, to, 0, Pane::Left, 5), Some((0, 5)));
        assert_eq!(cell_selected_cols(from, to, 1, Pane::Left, 8), Some((0, 8)));
        assert_eq!(cell_selected_cols(from, to, 0, Pane::Right, 5), None);
        assert_eq!(cell_selected_cols(from, to, 1, Pane::Right, 8), None);
        // mirrored: right-pane rows 1→2 must not highlight left row 2 (the
        // cell that sits between right rows 1 and 2 in row-major order)
        let from = caret(1, Pane::Right, 0);
        let to = caret(2, Pane::Right, 4);
        assert_eq!(cell_selected_cols(from, to, 2, Pane::Left, 0), None);
        // a cross-pane (stale) selection keeps the row-major in-between rule
        let from = caret(0, Pane::Left, 0);
        let to = caret(2, Pane::Right, 2);
        assert_eq!(cell_selected_cols(from, to, 1, Pane::Left, 8), Some((0, 8)));
        assert_eq!(cell_selected_cols(from, to, 1, Pane::Right, 8), Some((0, 8)));
    }

    #[test]
    fn confine_cursor_stays_in_anchor_pane() {
        let m = test_model();
        let sel = sel_from_to(caret(0, Pane::Left, 0), caret(0, Pane::Left, 0));
        // pointer dragged over the RIGHT pane at row 1 → remapped onto the
        // anchor pane's end of that row ("old line" = 8 chars)
        let c = confine_cursor(&sel, &m, caret(1, Pane::Right, 3));
        assert_eq!(c, caret(1, Pane::Left, 8));
        // a caret already in the anchor's pane passes through untouched
        assert_eq!(confine_cursor(&sel, &m, caret(2, Pane::Left, 1)), caret(2, Pane::Left, 1));
        // the confined selection copies ONLY left-pane lines
        let sel = TextSelection { anchor: Some(caret(0, Pane::Left, 0)), cursor: Some(c) };
        assert_eq!(selection_text(&m, &sel), "alpha\nold line");
    }

    #[test]
    fn confine_cursor_right_anchor_maps_to_right_pane() {
        let m = test_model();
        let sel = sel_from_to(caret(1, Pane::Right, 0), caret(1, Pane::Right, 0));
        // dragged into the LEFT pane at row 2 → remapped onto the anchor
        // pane's own row 2 ("extra" = 5 chars), NOT the hovered pane's length
        assert_eq!(confine_cursor(&sel, &m, caret(2, Pane::Left, 5)), caret(2, Pane::Right, 5));
        // dragged into the LEFT pane at row 0 → right pane's row 0 end ("alpha")
        assert_eq!(confine_cursor(&sel, &m, caret(0, Pane::Left, 1)), caret(0, Pane::Right, 5));
    }

    #[test]
    fn confine_cursor_without_anchor_passes_through() {
        let m = test_model();
        assert_eq!(
            confine_cursor(&TextSelection::default(), &m, caret(1, Pane::Right, 3)),
            caret(1, Pane::Right, 3)
        );
    }

    #[test]
    fn pane_text_strips_terminators() {
        let m = test_model();
        // filler row contributes a blank line (trailing empty segment)
        assert_eq!(pane_text(&m, Pane::Left), "alpha\nold line\n");
        assert_eq!(pane_text(&m, Pane::Right), "alpha\nnew line\nextra");
        // note-only model (empty panes) → empty text
        assert_eq!(pane_text(&DiffModel::empty_note("gone"), Pane::Left), "");
    }

    #[test]
    fn pane_row_len_clamps_missing_rows() {
        let m = test_model();
        assert_eq!(pane_row_len(&m, Pane::Left, 1), 8); // "old line"
        assert_eq!(pane_row_len(&m, Pane::Left, 2), 0); // filler row
        assert_eq!(pane_row_len(&m, Pane::Left, 99), 0); // beyond the model
        assert_eq!(pane_row_len(&m, Pane::Right, 2), 5); // "extra"
    }

    // ----- soft wrap: long lines become continuation rows -----

    #[test]
    fn wrap_lines_passes_short_lines_through() {
        let lines = vec![Line::context("ab\n", 1), Line::filler()];
        let wrapped = wrap_lines(&lines, 80);
        assert_eq!(wrapped.len(), 2);
        assert_eq!(wrapped, lines, "lines within the column budget are untouched");
    }

    #[test]
    fn wrap_lines_splits_long_line_into_cont_segments() {
        // "abcdef\n" at 2 cols → "ab", "cd", "ef\n": first keeps the number,
        // continuations are marked cont + no=None, terminator rides the last
        let lines = vec![Line::context("abcdef\n", 7)];
        let wrapped = wrap_lines(&lines, 2);
        assert_eq!(wrapped.len(), 3);
        assert_eq!(wrapped[0].text, "ab");
        assert_eq!(wrapped[0].no, Some(7));
        assert!(!wrapped[0].cont);
        assert_eq!(wrapped[1].text, "cd");
        assert_eq!(wrapped[1].no, None);
        assert!(wrapped[1].cont);
        assert_eq!(wrapped[2].text, "ef\n");
        assert!(wrapped[2].cont);
        assert_eq!(line_content(&wrapped[2]), "ef");
        // del/add tint survives the split
        let lines = vec![Line::del("x y z\n", 1)];
        let wrapped = wrap_lines(&lines, 2);
        assert!(wrapped.iter().all(|l| l.del));
    }

    #[test]
    fn selection_across_wrapped_rows_joins_without_newlines() {
        // "abcdefghij" wrapped at 4 → "abcd"(cont=false) + "efgh"(cont) +
        // "ij"(cont): selecting the whole logical line copies it back as ONE
        // line, exactly what an editor copy does
        let mut m = DiffModel {
            left_title: "left".into(),
            right_title: "right".into(),
            left: wrap_lines(&[Line::context("abcdefghij\n", 1)], 4),
            right: vec![Line::context("abcdefghij\n", 1)],
            binary: false,
            note: None,
            identical: false,
            max_chars: 10,
        };
        m.compute_max_chars();
        assert_eq!(m.left.len(), 3, "one logical line → three wrapped rows");
        let sel = sel_from_to(caret(0, Pane::Left, 0), caret(2, Pane::Left, 2));
        assert_eq!(selection_text(&m, &sel), "abcdefghij");
        // a partial selection spanning the wrap boundary joins too
        let sel = sel_from_to(caret(1, Pane::Left, 1), caret(2, Pane::Left, 1));
        assert_eq!(selection_text(&m, &sel), "fghi");
    }
}
