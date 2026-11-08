//! patch — structured patch: định dạng delta lưu trên disk (mảng hunks JSON, nén zstd ở tầng blob).
//!
//! Ngữ nghĩa splice xem `model::Hunk`: toạ độ hunk tính trên dòng GỐC của base; các hunk tăng
//! dần `old_start`, không chồng nhau; apply = duyệt tuần tự và nối bytes (không parse lại
//! line ending).

use similar::{ChangeTag, DiffOp, TextDiff};

use crate::model::{Error, Hunk, Result};

/// Chia bytes thành các dòng: cắt sau mỗi `\n`, terminator giữ nguyên trong dòng
/// (dòng `abc\r\n` nguyên vẹn). Input rỗng → vec rỗng; dòng cuối không có `\n` vẫn là 1 dòng.
pub(crate) fn split_lines(bytes: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut start = 0usize;
    for (i, b) in bytes.iter().enumerate() {
        if *b == b'\n' {
            out.push(&bytes[start..=i]);
            start = i + 1;
        }
    }
    if start < bytes.len() {
        out.push(&bytes[start..]);
    }
    out
}

/// Khoảng dòng (start, end — nửa mở) mà một op chiếm trên chuỗi old.
fn op_old_span(op: &DiffOp) -> (usize, usize) {
    match *op {
        DiffOp::Equal { old_index, len, .. } => (old_index, old_index + len),
        DiffOp::Delete { old_index, old_len, .. } => (old_index, old_index + old_len),
        DiffOp::Insert { old_index, .. } => (old_index, old_index),
        DiffOp::Replace { old_index, old_len, .. } => (old_index, old_index + old_len),
    }
}

/// Khoảng dòng mà một op chiếm trên chuỗi new.
fn op_new_span(op: &DiffOp) -> (usize, usize) {
    match *op {
        DiffOp::Equal { new_index, len, .. } => (new_index, new_index + len),
        DiffOp::Delete { new_index, .. } => (new_index, new_index),
        DiffOp::Insert { new_index, new_len, .. } => (new_index, new_index + new_len),
        DiffOp::Replace { new_index, new_len, .. } => (new_index, new_index + new_len),
    }
}

/// Sinh hunks từ hai nội dung text (caller — record — đã xác nhận UTF-8).
///
/// Dùng `similar::TextDiff::from_lines` + `grouped_ops(3)` (context 3): mỗi nhóm op → một hunk,
/// toạ độ tính trên dòng gốc của `old`. `new_text` lấy từ chính tokenization của similar
/// (dòng giữ terminator) nên luôn tự nhất quán với toạ độ op.
pub(crate) fn make_patch(old: &[u8], new: &[u8]) -> Result<Vec<Hunk>> {
    let old_str = std::str::from_utf8(old)
        .map_err(|_| Error::CorruptChain("make_patch: nội dung cũ không phải UTF-8".into()))?;
    let new_str = std::str::from_utf8(new)
        .map_err(|_| Error::CorruptChain("make_patch: nội dung mới không phải UTF-8".into()))?;

    let diff = TextDiff::from_lines(old_str, new_str);

    // Dòng phía "new" theo đúng tokenization của similar: mọi change không phải Delete
    // đều là một dòng của chuỗi mới, theo đúng thứ tự.
    let new_side: Vec<&str> = diff
        .iter_all_changes()
        .filter(|c| c.tag() != ChangeTag::Delete)
        .map(|c| c.value())
        .collect();

    let mut hunks = Vec::new();
    for group in diff.grouped_ops(3) {
        if group.is_empty() {
            continue;
        }
        let first = &group[0];
        let last = &group[group.len() - 1];
        let (old_first, _) = op_old_span(first);
        let (_, old_last) = op_old_span(last);
        let (new_first, _) = op_new_span(first);
        let (_, new_last) = op_new_span(last);

        let mut new_text = String::new();
        for line in &new_side[new_first..new_last] {
            new_text.push_str(line);
        }
        hunks.push(Hunk {
            old_start: old_first as u64 + 1,
            old_lines: (old_last - old_first) as u64,
            new_start: new_first as u64 + 1,
            new_lines: (new_last - new_first) as u64,
            new_text,
        });
    }
    Ok(hunks)
}

/// Áp tuần tự các hunks lên `base`. Hunks phải tăng dần `old_start`, không chồng nhau và
/// vùng old phải nằm trong base — vi phạm → `Error::CorruptChain`.
///
/// Vì toạ độ hunk tính trên dòng gốc của base (và các hunk rời rạc), áp tuần tự với một con
/// trỏ "dòng đã sao chép" cho kết quả đúng như splice từng hunk.
pub(crate) fn apply_patch(base: &[u8], hunks: &[Hunk]) -> Result<Vec<u8>> {
    let lines = split_lines(base);
    let mut out = Vec::with_capacity(base.len());
    let mut next_old: usize = 0; // dòng base đầu tiên chưa được sao chép
    let mut prev_start: u64 = 0;
    for (idx, h) in hunks.iter().enumerate() {
        if h.old_start <= prev_start {
            return Err(Error::CorruptChain(format!(
                "hunk {idx}: old_start {} không tăng dần (hunk trước: {prev_start})",
                h.old_start
            )));
        }
        prev_start = h.old_start;
        let s = (h.old_start - 1) as usize;
        let e = s
            .checked_add(h.old_lines as usize)
            .ok_or_else(|| Error::CorruptChain(format!("hunk {idx}: old_lines tràn số")))?;
        if e > lines.len() || s < next_old {
            return Err(Error::CorruptChain(format!(
                "hunk {idx}: vùng old [{s}..{e}] không hợp lệ (base có {} dòng, đã duyệt đến {next_old})",
                lines.len()
            )));
        }
        for line in &lines[next_old..s] {
            out.extend_from_slice(line);
        }
        out.extend_from_slice(h.new_text.as_bytes());
        next_old = e;
    }
    for line in &lines[next_old..] {
        out.extend_from_slice(line);
    }
    Ok(out)
}

/// Serialize hunks → JSON bytes (bước trước khi nén zstd vào blob delta).
pub(crate) fn encode_hunks(hunks: &[Hunk]) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec(hunks)?)
}

/// Đọc lại hunks từ JSON bytes.
pub(crate) fn decode_hunks(bytes: &[u8]) -> Result<Vec<Hunk>> {
    Ok(serde_json::from_slice(bytes)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Roundtrip bắt buộc: make_patch → JSON (đúng đường serialize trên disk) → decode
    /// → apply_patch phải ra đúng nội dung mới, byte-exact.
    fn roundtrip(old: &str, new: &str) {
        let hunks = make_patch(old.as_bytes(), new.as_bytes()).unwrap();
        let json = encode_hunks(&hunks).unwrap();
        let decoded: Vec<Hunk> = decode_hunks(&json).unwrap();
        assert_eq!(decoded, hunks);
        let applied = apply_patch(old.as_bytes(), &decoded).unwrap();
        assert_eq!(applied, new.as_bytes(), "apply(make_patch) phải khớp nội dung mới");
    }

    #[test]
    fn split_lines_basics() {
        assert!(split_lines(b"").is_empty());
        assert_eq!(split_lines(b"abc"), vec![&b"abc"[..]]);
        assert_eq!(split_lines(b"abc\n"), vec![&b"abc\n"[..]]);
        assert_eq!(
            split_lines(b"a\nb\nc"),
            vec![&b"a\n"[..], &b"b\n"[..], &b"c"[..]]
        );
        // CRLF giữ nguyên terminator trong dòng
        assert_eq!(split_lines(b"a\r\nb\r\n"), vec![&b"a\r\n"[..], &b"b\r\n"[..]]);
    }

    #[test]
    fn roundtrip_no_trailing_newline() {
        roundtrip("a\nb\nc", "a\nB\nc");
    }

    #[test]
    fn roundtrip_crlf() {
        roundtrip("a\r\nb\r\nc\r\n", "a\r\nX\r\nc\r\n");
        roundtrip("a\r\nb\r\nc\r\n", "a\r\nb\r\nc\r\nd\r\n");
    }

    #[test]
    fn roundtrip_empty_to_content() {
        roundtrip("", "hello\nworld\n");
    }

    #[test]
    fn roundtrip_content_to_empty() {
        roundtrip("hello\nworld\n", "");
    }

    #[test]
    fn roundtrip_insert_head() {
        roundtrip("b\nc\n", "a\nb\nc\n");
    }

    #[test]
    fn roundtrip_append_tail() {
        roundtrip("a\nb\n", "a\nb\nc\n");
    }

    #[test]
    fn roundtrip_modify_middle() {
        roundtrip("l1\nl2\nl3\nl4\nl5\n", "l1\nl2\nl3-changed\nl4\nl5\n");
    }

    #[test]
    fn roundtrip_adjacent_blank_lines() {
        roundtrip("a\n\n\nb\n", "a\n\n\n\nB\n");
        roundtrip("a\n\nb\n", "a\n\n\nc\n\nb\n");
    }

    #[test]
    fn roundtrip_two_regions_and_identity() {
        // Hai vùng sửa cách nhau hơn 6 dòng (2 * context) → phải ra ≥ 2 hunks riêng,
        // toạ độ theo dòng gốc vẫn apply đúng.
        let old: String = (1..=12).map(|i| format!("line {i}\n")).collect();
        let mut lines: Vec<String> = (1..=12).map(|i| format!("line {i}\n")).collect();
        lines[1] = "edited-2\n".to_string();
        lines[10] = "edited-11\n".to_string();
        let new = lines.concat();
        let hunks = make_patch(old.as_bytes(), new.as_bytes()).unwrap();
        assert!(hunks.len() >= 2, "hai vùng xa nhau phải ra >= 2 hunks (got {})", hunks.len());
        roundtrip(&old, &new);
        // Nội dung giống hệt nhau → không hunk nào
        assert!(make_patch(old.as_bytes(), old.as_bytes()).unwrap().is_empty());
    }

    #[test]
    fn apply_rejects_corrupt_hunks() {
        // old_start không tăng dần
        let bad_order = vec![
            Hunk { old_start: 2, old_lines: 1, new_start: 2, new_lines: 1, new_text: "x\n".into() },
            Hunk { old_start: 2, old_lines: 1, new_start: 3, new_lines: 1, new_text: "y\n".into() },
        ];
        assert!(apply_patch(b"1\n2\n3\n", &bad_order).is_err());
        // old_start vượt số dòng của base
        let over = vec![Hunk {
            old_start: 9,
            old_lines: 1,
            new_start: 9,
            new_lines: 1,
            new_text: "x\n".into(),
        }];
        assert!(apply_patch(b"1\n2\n3\n", &over).is_err());
        // Hai hunk chồng vùng old
        let overlap = vec![
            Hunk { old_start: 1, old_lines: 2, new_start: 1, new_lines: 1, new_text: "x\n".into() },
            Hunk { old_start: 2, old_lines: 1, new_start: 3, new_lines: 1, new_text: "y\n".into() },
        ];
        assert!(apply_patch(b"1\n2\n3\n", &overlap).is_err());
    }

    #[test]
    fn apply_empty_hunks_returns_base() {
        assert_eq!(apply_patch(b"abc", &[]).unwrap(), b"abc".to_vec());
        assert_eq!(apply_patch(b"", &[]).unwrap(), b"".to_vec());
    }
}
