//! Structured patch — the on-disk delta format (a JSON array of hunks, zstd-compressed at
//! the blob layer).
//!
//! Splice semantics: see `model::Hunk`. Hunk coordinates refer to the original base lines;
//! hunks are ordered by `old_start` and never overlap. Apply is a sequential scan that
//! concatenates bytes — line endings are never re-parsed.

use similar::{ChangeTag, DiffOp, TextDiff};

use crate::model::{Error, Hunk, Result};

/// Splits bytes into lines: cut after each `\n`, keeping the terminator in the line
/// (`abc\r\n` stays intact). Empty input → empty vec; a final line without `\n` still counts.
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

/// Half-open line span `(start, end)` an op occupies on the old side.
fn op_old_span(op: &DiffOp) -> (usize, usize) {
    match *op {
        DiffOp::Equal { old_index, len, .. } => (old_index, old_index + len),
        DiffOp::Delete { old_index, old_len, .. } => (old_index, old_index + old_len),
        DiffOp::Insert { old_index, .. } => (old_index, old_index),
        DiffOp::Replace { old_index, old_len, .. } => (old_index, old_index + old_len),
    }
}

/// Half-open line span `(start, end)` an op occupies on the new side.
fn op_new_span(op: &DiffOp) -> (usize, usize) {
    match *op {
        DiffOp::Equal { new_index, len, .. } => (new_index, new_index + len),
        DiffOp::Delete { new_index, .. } => (new_index, new_index),
        DiffOp::Insert { new_index, new_len, .. } => (new_index, new_index + new_len),
        DiffOp::Replace { new_index, new_len, .. } => (new_index, new_index + new_len),
    }
}

/// Builds hunks from two text contents (the caller — record — has already validated UTF-8).
///
/// Uses `similar::TextDiff::from_lines` + `grouped_ops(3)` (context 3): each group of ops
/// becomes one hunk, with coordinates referring to the original lines of `old`. `new_text`
/// comes from similar's own tokenization (lines keep terminators), so it is always
/// self-consistent with the op coordinates.
pub(crate) fn make_patch(old: &[u8], new: &[u8]) -> Result<Vec<Hunk>> {
    let old_str = std::str::from_utf8(old)
        .map_err(|_| Error::CorruptChain("make_patch: nội dung cũ không phải UTF-8".into()))?;
    let new_str = std::str::from_utf8(new)
        .map_err(|_| Error::CorruptChain("make_patch: nội dung mới không phải UTF-8".into()))?;

    let diff = TextDiff::from_lines(old_str, new_str);

    // New-side lines in order: every non-Delete change is one line of the new content,
    // using similar's own tokenization.
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

/// Applies hunks sequentially to `base`. Hunks must be ordered by `old_start`, must not
/// overlap, and their old regions must lie within base — otherwise `Error::CorruptChain`.
///
/// Because hunk coordinates refer to the original base lines (and hunks are disjoint),
/// a sequential scan with a "lines copied so far" cursor reproduces the per-hunk splice
/// exactly.
pub(crate) fn apply_patch(base: &[u8], hunks: &[Hunk]) -> Result<Vec<u8>> {
    let lines = split_lines(base);
    let mut out = Vec::with_capacity(base.len());
    let mut next_old: usize = 0; // first base line not yet copied
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

/// Serializes hunks to JSON (the step before zstd-compression into a delta blob).
pub(crate) fn encode_hunks(hunks: &[Hunk]) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec(hunks)?)
}

/// Parses hunks back from JSON bytes.
pub(crate) fn decode_hunks(bytes: &[u8]) -> Result<Vec<Hunk>> {
    Ok(serde_json::from_slice(bytes)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mandatory roundtrip: make_patch → JSON (the exact on-disk serialization path) →
    /// decode → apply_patch must reproduce the new content byte-exactly.
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
        // CRLF terminators stay intact in the line
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
        // Two edited regions more than 6 lines apart (2 * context) must yield >= 2 separate
        // hunks; original-line coordinates still apply correctly.
        let old: String = (1..=12).map(|i| format!("line {i}\n")).collect();
        let mut lines: Vec<String> = (1..=12).map(|i| format!("line {i}\n")).collect();
        lines[1] = "edited-2\n".to_string();
        lines[10] = "edited-11\n".to_string();
        let new = lines.concat();
        let hunks = make_patch(old.as_bytes(), new.as_bytes()).unwrap();
        assert!(hunks.len() >= 2, "hai vùng xa nhau phải ra >= 2 hunks (got {})", hunks.len());
        roundtrip(&old, &new);
        // Identical content → no hunks
        assert!(make_patch(old.as_bytes(), old.as_bytes()).unwrap().is_empty());
    }

    #[test]
    fn apply_rejects_corrupt_hunks() {
        // old_start not increasing
        let bad_order = vec![
            Hunk { old_start: 2, old_lines: 1, new_start: 2, new_lines: 1, new_text: "x\n".into() },
            Hunk { old_start: 2, old_lines: 1, new_start: 3, new_lines: 1, new_text: "y\n".into() },
        ];
        assert!(apply_patch(b"1\n2\n3\n", &bad_order).is_err());
        // old_start beyond the base line count
        let over = vec![Hunk {
            old_start: 9,
            old_lines: 1,
            new_start: 9,
            new_lines: 1,
            new_text: "x\n".into(),
        }];
        assert!(apply_patch(b"1\n2\n3\n", &over).is_err());
        // two hunks whose old regions overlap
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
