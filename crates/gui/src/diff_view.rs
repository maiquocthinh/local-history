//! Diff view — dựng 2 mảng dòng trái/phải từ hunks có cấu trúc của engine và render
//! side-by-side (một `ScrollArea::show_rows` duy nhất ⇒ scroll 2 pane tự đồng bộ).

use eframe::egui;
use history::Hunk;
use rust_i18n::t;

/// Cột diff: nền đỏ nhạt cho dòng bị xóa, xanh nhạt cho dòng thêm (theo đặc tả).
const DEL_BG: egui::Color32 = egui::Color32::from_rgb(60, 25, 25);
const ADD_BG: egui::Color32 = egui::Color32::from_rgb(25, 60, 30);
const NUM_COLOR: egui::Color32 = egui::Color32::from_rgb(130, 130, 130);
const TEXT_COLOR: egui::Color32 = egui::Color32::from_rgb(215, 215, 215);
const SEP_STROKE: egui::Stroke =
    egui::Stroke { width: 1.0, color: egui::Color32::from_rgb(70, 70, 70) };
const DIFF_FONT_SIZE: f32 = 13.0;
/// Bề rộng cột số dòng.
const GUTTER: f32 = 56.0;

/// Font monospace dùng cho diff (không nhúng font mới — dùng font mặc định của egui).
fn diff_font() -> egui::FontId {
    egui::FontId::monospace(DIFF_FONT_SIZE)
}

fn num_font() -> egui::FontId {
    egui::FontId::monospace(DIFF_FONT_SIZE - 1.0)
}

/// Một dòng hiển thị trong pane diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    /// Nội dung dòng nguyên bản (gồm terminator — bỏ khi hiển thị).
    pub text: String,
    /// Số dòng 1-based trên nội dung của pane; None = dòng đệm khi 2 bên lệch độ dài.
    pub no: Option<usize>,
    /// Dòng bị xóa (chỉ có ở bên trái của thay đổi).
    pub del: bool,
    /// Dòng được thêm (chỉ có ở bên phải của thay đổi).
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
    /// Dòng đệm (lấp hàng khi 2 bên lệch độ dài) — trung tính, không tô màu.
    fn filler() -> Self {
        Line { text: String::new(), no: None, del: false, add: false }
    }
}

/// Dựng 2 mảng dòng (trái = base/old, phải = target/new) từ hunks, theo đúng ngữ nghĩa
/// splice của `model::Hunk`: hunk thay `base[old_start-1 .. old_start-1+old_lines]` bằng
/// `target[new_start-1 .. new_start-1+new_lines]`, các hunk tăng dần `old_start` (đầu ra
/// chuẩn của engine). Dòng context ghép cặp 1-1; vùng thay đổi ghép theo thứ tự, bên nào
/// ngắn hơn nhận dòng đệm (text rỗng, `no = None`) để 2 pane thẳng hàng theo hàng.
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

    let mut oi = 0usize; // dòng base đã duyệt
    let mut ni = 0usize; // dòng target đã duyệt
    for h in hunks {
        let s = (h.old_start as usize).saturating_sub(1).min(base_lines.len());
        // context giữa hunk trước và hunk này
        let ctx = s.saturating_sub(oi);
        for i in 0..ctx {
            let b = base_lines.get(oi + i).copied().unwrap_or("");
            let t = target_lines.get(ni + i).copied().unwrap_or("");
            push_context(&mut left, &mut right, b, oi + i + 1, t, ni + i + 1);
        }
        ni += ctx; // oi nhảy thẳng tới cuối vùng del của hunk (s + del_n)

        // vùng thay đổi: del ở trái (đúng i < old_lines), add ở phải (đúng i < new_lines),
        // đệm cho bên ngắn hơn — KHÔNG tiêu dòng của bên không tham gia hunk.
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
    // context sau hunk cuối
    let tail = base_lines.len().saturating_sub(oi);
    for i in 0..tail {
        let b = base_lines.get(oi + i).copied().unwrap_or("");
        let t = target_lines.get(ni + i).copied().unwrap_or("");
        push_context(&mut left, &mut right, b, oi + i + 1, t, ni + i + 1);
    }
    (left, right)
}

/// Model diff hoàn chỉnh cho khung hiển thị (do `app` dựng từ engine + disk).
#[derive(Debug, Clone, Default)]
pub struct DiffModel {
    pub left_title: String,
    pub right_title: String,
    pub left: Vec<Line>,
    pub right: Vec<Line>,
    /// Nội dung binary — không diff được.
    pub binary: bool,
    /// Thông báo trạng thái (chưa chọn file, version đầu tiên...).
    pub note: Option<String>,
    /// Hai bên giống hệt nhau (0 hunk).
    pub identical: bool,
    /// Số ký tự dòng dài nhất (để tính bề rộng ngang của cell).
    pub max_chars: usize,
}

impl DiffModel {
    pub fn empty_note(note: impl Into<String>) -> Self {
        DiffModel { note: Some(note.into()), ..Default::default() }
    }

    pub fn binary_note() -> Self {
        DiffModel {
            binary: true,
            note: Some(t!("diff.binary").to_string()),
            ..Default::default()
        }
    }

    /// Số ký tự của dòng dài nhất trên 2 bên (để cấp bề rộng ngang).
    pub fn compute_max_chars(&mut self) {
        self.max_chars = self
            .left
            .iter()
            .chain(self.right.iter())
            .map(|l| l.text.chars().count())
            .max()
            .unwrap_or(0);
    }

    /// Hoàn tất sau khi dựng: tính max_chars.
    pub fn finished(mut self) -> Self {
        self.compute_max_chars();
        self
    }
}

/// Render model diff vào panel giữa: tiêu đề 2 cột + các dòng side-by-side.
pub fn show_diff(ui: &mut egui::Ui, model: &DiffModel) {
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
        return;
    }

    // Tính bề rộng cell: tối thiểu nửa chiều rộng có sẵn, dãn theo dòng dài nhất.
    let font = diff_font();
    let row_h = ui.fonts_mut(|f| f.row_height(&font)) + 4.0;
    let char_w = ui.fonts_mut(|f| f.glyph_width(&font, '0')).max(0.5);
    let avail = ui.available_width() - 20.0;
    let min_cell = (avail / 2.0).max(120.0);
    let content_w = GUTTER + 8.0 + model.max_chars as f32 * char_w;
    let cell_w = min_cell.max(content_w);

    // Tiêu đề 2 cột (cùng cách chia bề rộng với phần nội dung tối thiểu).
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

    // MỘT ScrollArea duyệt index dòng → 2 cột trong cùng row ⇒ scroll đồng bộ tự nhiên.
    egui::ScrollArea::both()
        .id_salt("diff_scroll")
        .auto_shrink([false, false])
        .show_rows(ui, row_h, total, |ui, rows| {
            for r in rows {
                let l = model.left.get(r);
                let rt = model.right.get(r);
                ui.horizontal(|ui| {
                    draw_cell(ui, cell_w, row_h, l);
                    draw_cell(ui, cell_w, row_h, rt);
                });
            }
        });
}

/// Vẽ một cell (nửa trái hoặc phải của một hàng diff).
fn draw_cell(ui: &mut egui::Ui, w: f32, h: f32, line: Option<&Line>) {
    let (rect, _resp) = ui.allocate_exact_size(egui::vec2(w, h), egui::Sense::hover());
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
    ui.painter().vline(rect.left(), rect.y_range(), SEP_STROKE);
    let Some(l) = line else { return };
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
    // Bỏ terminator khi hiển thị; tab thay bằng 4 space (monospace).
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
}

// ---------------------------------------------------------------------------
// Tests — dựng 2 cột từ hunks (logic thuần)
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
        // Chèn đầu: old_lines == 0, old_start == 1 — trái có hàng đệm ở đầu
        let base = lines(&["b\n", "c\n"]);
        let target = lines(&["a\n", "b\n", "c\n"]);
        let (l, r) = apply_hunks_to_sides(&base, &target, &[hunk(1, 0, 1, 1, "a\n")]);
        assert_eq!(texts(&l), vec!["", "b\n", "c\n"]);
        assert_eq!(texts(&r), vec!["a\n", "b\n", "c\n"]);
        assert!(r[0].add);
        assert_eq!(l[0].no, None);
        assert_eq!(l[1].no, Some(1));
        assert_eq!(r[1].no, Some(2));

        // Chèn cuối: old_start == len + 1, dòng mới không kết newline
        let base = lines(&["a\n"]);
        let target = lines(&["a\n", "b"]);
        let (l, r) = apply_hunks_to_sides(&base, &target, &[hunk(2, 0, 2, 1, "b")]);
        assert_eq!(texts(&l), vec!["a\n", ""]); // bên trái có dòng đệm
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
        assert_eq!(texts(&r), vec!["a\n", "", "", "", "b\n"]); // 3 hàng đệm bên phải
        for line in l.iter().take(4).skip(1) {
            assert!(line.del);
        }
        assert!(r[1].no.is_none());
        assert_eq!(r[4].no, Some(2)); // "b\n" là dòng 2 của target
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
        assert_eq!(l[1].text, "b"); // giữ nguyên terminator (ở đây không có)
    }

    #[test]
    fn two_regions_padded_rows() {
        // Hai vùng sửa: vùng 2 có thêm 1 dòng mới ⇒ bên trái phải có dòng đệm
        // để 2 pane thẳng hàng.
        let base = lines(&["1\n", "2\n", "3\n", "4\n", "5\n", "6\n", "7\n", "8\n", "9\n", "10\n"]);
        let target = lines(&["1\n", "hai\n", "3\n", "4\n", "5\n", "6\n", "7\n", "8\n", "9\n", "mười\n", "mười một\n"]);
        let hunks = [
            hunk(2, 1, 2, 1, "hai\n"),
            hunk(10, 1, 10, 2, "mười\nmười một\n"),
        ];
        let (l, r) = apply_hunks_to_sides(&base, &target, &hunks);
        assert_eq!(l.len(), r.len(), "2 pane phải bằng hàng để scroll khớp");
        // hàng đệm nằm ở cuối: trái None, phải là dòng mới thứ hai
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
}
