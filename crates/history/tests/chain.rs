//! Integration test cho chuỗi version: record 40 lần sửa ngẫu nhiên → reconstruct khớp,
//! dedup, chính sách full/delta, delete marker, revert flow, label, diff.

use std::fs;

use history::{Error, EventKind, Hunk, LocalHistory, Storage, WorkspaceId};

fn setup() -> (tempfile::TempDir, WorkspaceId, LocalHistory) {
    let dir = tempfile::tempdir().unwrap();
    let store = LocalHistory::open(dir.path()).unwrap();
    let ws = store.register_workspace(dir.path()).unwrap();
    (dir, ws, store)
}

/// LCG cố định (không thêm dep rand): seed giống nhau → chuỗi test deterministic.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Nội dung text ngẫu nhiên gồm `lines` dòng, mang `salt` để luôn khác nội dung trước đó.
fn text_of_lines(rng: &mut Lcg, lines: usize, salt: u32) -> Vec<u8> {
    let mut s = String::new();
    for i in 0..lines {
        s.push_str(&format!(
            "line {salt}.{i} = {} {}\n",
            rng.below(10_000),
            rng.below(10_000)
        ));
    }
    s.into_bytes()
}

/// Sửa nhỏ 1-3 dòng ngẫu nhiên; dòng mới mang token `salt` duy nhất → luôn đổi nội dung.
fn edit_lines(rng: &mut Lcg, content: Vec<u8>, salt: u32) -> Vec<u8> {
    let text = String::from_utf8(content).unwrap();
    let mut lines: Vec<String> = text.split_inclusive('\n').map(str::to_string).collect();
    if lines.is_empty() {
        lines.push(String::new());
    }
    let n = 1 + rng.below(3) as usize;
    for _ in 0..n {
        let idx = rng.below(lines.len() as u64) as usize;
        lines[idx] = format!("edited {salt}: {}\n", rng.below(1_000_000));
    }
    lines.concat().into_bytes()
}

/// 40 lần sửa ngẫu nhiên → mọi version (kể cả giữa chain delta) tái tạo đúng nội dung gốc.
#[test]
fn roundtrip_40_edits() {
    let (_dir, ws, store) = setup();
    let file = _dir.path().join("story.txt");
    let mut rng = Lcg(0x9E37_79B9_7F4A_7C15);

    let mut content = text_of_lines(&mut rng, 60, 0);
    let mut expected: Vec<Vec<u8>> = Vec::new();
    let mut ids: Vec<i64> = Vec::new();
    for i in 0..40u32 {
        // Đôi lần thay cả file, còn lại sửa nhỏ vài dòng
        content = if i % 9 == 4 {
            let lines = 3 + rng.below(30) as usize;
            text_of_lines(&mut rng, lines, i)
        } else {
            edit_lines(&mut rng, content, i)
        };
        expected.push(content.clone());
        let id = store
            .record(ws, &file, &content, EventKind::External, None)
            .unwrap()
            .unwrap_or_else(|| panic!("lần sửa {i} không được record (dedup?)"));
        ids.push(id);
    }

    assert_eq!(expected.len(), 40);
    assert_eq!(ids.len(), 40);
    // Mọi version tái tạo đúng nội dung (chẵn/lẻ/giữa đều nằm trong vòng lặp)
    for (k, id) in ids.iter().enumerate() {
        assert_eq!(store.content(*id).unwrap(), expected[k], "version {k} (id {id}) sai nội dung");
    }
    let last = *ids.last().unwrap();
    assert_eq!(store.content(last).unwrap(), expected[39]);

    // history() đủ 40 version, seq giảm dần, thứ tự khớp ids
    let hist = store.history(ws, &file).unwrap();
    assert_eq!(hist.len(), 40);
    for w in hist.windows(2) {
        assert!(w[0].seq > w[1].seq, "seq phải giảm dần");
    }
    assert_eq!(hist[0].seq, 40);
    assert_eq!(hist[39].seq, 1);
    for (i, v) in hist.iter().enumerate() {
        assert_eq!(v.id, ids[39 - i]);
    }
}

/// Record 2 lần cùng nội dung + cùng kind → lần 2 Ok(None), số version không đổi.
#[test]
fn dedup() {
    let (_dir, ws, store) = setup();
    let file = _dir.path().join("a.txt");
    let id1 = store
        .record(ws, &file, b"same\n", EventKind::External, None)
        .unwrap()
        .unwrap();
    let id2 = store
        .record(ws, &file, b"same\n", EventKind::External, None)
        .unwrap();
    assert!(id2.is_none());
    let hist = store.history(ws, &file).unwrap();
    assert_eq!(hist.len(), 1);
    assert_eq!(hist[0].id, id1);
}

/// Chính sách full/delta + binary detection.
#[test]
fn full_delta_policy() {
    let (_dir, ws, store) = setup();
    let file = _dir.path().join("big.txt");
    let mut rng = Lcg(42);

    // File lớn ~200KB, sửa nhỏ lẻ → các version giữa chain là Delta.
    let mut content = text_of_lines(&mut rng, 7500, 0);
    let mut ids = Vec::new();
    for i in 0..25u32 {
        content = edit_lines(&mut rng, content, i);
        let id = store
            .record(ws, &file, &content, EventKind::External, None)
            .unwrap()
            .unwrap();
        ids.push(id);
    }
    let hist = store.history(ws, &file).unwrap();
    assert_eq!(hist.len(), 25);
    let mut asc = hist;
    asc.sort_by_key(|v| v.seq);
    assert_eq!(asc[0].storage, Storage::Full, "version đầu là full");
    for v in &asc[1..21] {
        assert_eq!(v.storage, Storage::Delta, "seq {} phải là delta", v.seq);
    }
    // 20 delta liên tiếp → full xuất hiện ở GIỮA chain, rồi chain delta bắt đầu lại
    assert_eq!(asc[21].storage, Storage::Full, "chain 20 delta → full ở giữa");
    assert_eq!(asc[22].storage, Storage::Delta);

    // Nội dung có NUL trong 8KB đầu → full. Nội dung invalid UTF-8 → full.
    let bfile = _dir.path().join("bin.dat");
    store
        .record(ws, &bfile, b"plain text\n", EventKind::External, None)
        .unwrap()
        .unwrap();
    let nul_content = b"with\0nul byte inside\n".to_vec();
    let id_nul = store
        .record(ws, &bfile, &nul_content, EventKind::External, None)
        .unwrap()
        .unwrap();
    let bad_content = b"\xff\xfe bad utf8 \xc3\x28\n".to_vec();
    let id_bad = store
        .record(ws, &bfile, &bad_content, EventKind::External, None)
        .unwrap()
        .unwrap();
    for (id, want) in [(id_nul, &nul_content), (id_bad, &bad_content)] {
        let v = store
            .history(ws, &bfile)
            .unwrap()
            .into_iter()
            .find(|v| v.id == id)
            .unwrap();
        assert_eq!(v.storage, Storage::Full, "binary phải lưu full (id {id})");
        // Nội dung binary vẫn version đầy đủ, reconstruct được
        assert_eq!(store.content(id).unwrap(), *want);
    }
}

/// Delete marker: nội dung rỗng, version trước còn nguyên, chain tiếp tục bình thường.
#[test]
fn delete_marker() {
    let (_dir, ws, store) = setup();
    let file = _dir.path().join("gone.txt");
    let c1 = b"version mot\n".to_vec();
    let id1 = store
        .record(ws, &file, &c1, EventKind::External, None)
        .unwrap()
        .unwrap();

    let id_del = store.record(ws, &file, &[], EventKind::Delete, None).unwrap().unwrap();
    assert_eq!(store.content(id_del).unwrap(), b"".to_vec());
    // Version trước marker vẫn giữ nguyên nội dung
    assert_eq!(store.content(id1).unwrap(), c1);
    // Delete liên tiếp → dedup
    assert!(store.record(ws, &file, &[], EventKind::Delete, None).unwrap().is_none());

    // Ghi lại nội dung 2 → chain tiếp tục
    let c2 = b"version hai\nsau khi khoi phuc\n".to_vec();
    let id2 = store
        .record(ws, &file, &c2, EventKind::External, None)
        .unwrap()
        .unwrap();
    assert_eq!(store.content(id2).unwrap(), c2);

    let hist = store.history(ws, &file).unwrap();
    assert_eq!(hist.len(), 3);
    assert_eq!(hist[0].kind, EventKind::External);
    assert_eq!(hist[1].kind, EventKind::Delete);
    assert_eq!(hist[2].kind, EventKind::External);
}

/// Revert: sinh `pre_revert` trước khi ghi đè, `restore` sau đó; revert lại khi disk đã
/// trùng target không tạo version mới.
#[test]
fn revert_flow() {
    let (_dir, ws, store) = setup();
    let file = _dir.path().join("code.txt");
    let v1 = b"fn a() {}\n".to_vec();
    let v2 = b"fn a() {}\nfn b() {}\n".to_vec();
    let v3 = b"fn a() {}\nfn b() {}\nfn c() {}\n".to_vec();
    fs::write(&file, &v1).unwrap();
    let id1 = store.record(ws, &file, &v1, EventKind::External, None).unwrap().unwrap();
    fs::write(&file, &v2).unwrap();
    let id2 = store.record(ws, &file, &v2, EventKind::External, None).unwrap().unwrap();
    fs::write(&file, &v3).unwrap();
    let _id3 = store.record(ws, &file, &v3, EventKind::External, None).unwrap().unwrap();

    store.revert(ws, &file, id1).unwrap();
    assert_eq!(fs::read(&file).unwrap(), v1);

    let hist = store.history(ws, &file).unwrap();
    let pre = hist.iter().find(|v| v.kind == EventKind::PreRevert).expect("thiếu pre_revert");
    let res = hist.iter().find(|v| v.kind == EventKind::Restore).expect("thiếu restore");
    assert_eq!(store.content(pre.id).unwrap(), v3);
    assert_eq!(store.content(res.id).unwrap(), v1);
    assert!(id2 < pre.id && pre.id < res.id);
    assert_eq!(hist[0].id, res.id, "version mới nhất là restore");

    // Revert lần nữa khi disk đã == target → Ok, KHÔNG tạo version mới
    let before = store.history(ws, &file).unwrap().len();
    store.revert(ws, &file, id1).unwrap();
    assert_eq!(store.history(ws, &file).unwrap().len(), before);

    // Version của file khác → NotFound
    let other = _dir.path().join("other.txt");
    fs::write(&other, b"other\n").unwrap();
    let oid = store.record(ws, &other, b"other\n", EventKind::External, None).unwrap().unwrap();
    assert!(matches!(store.revert(ws, &file, oid), Err(Error::NotFound(_))));
}

/// Label: add_label tạo version kind=Label; set_label đổi label version cũ; history phản ánh.
#[test]
fn labels() {
    let (_dir, ws, store) = setup();
    let file = _dir.path().join("doc.txt");
    fs::write(&file, b"noi dung\n").unwrap();
    let id_ext = store
        .record(ws, &file, b"noi dung\n", EventKind::External, None)
        .unwrap()
        .unwrap();

    let id_label = store.add_label(ws, &file, "before tests").unwrap();
    let hist = store.history(ws, &file).unwrap();
    assert_eq!(hist.len(), 2);
    let lv = hist.iter().find(|v| v.id == id_label).unwrap();
    assert_eq!(lv.kind, EventKind::Label);
    assert_eq!(lv.label.as_deref(), Some("before tests"));
    assert_eq!(store.content(id_label).unwrap(), b"noi dung\n".to_vec());

    // set_label đổi label của version cũ, không tạo version mới
    store.set_label(id_ext, Some("renamed-old")).unwrap();
    let hist = store.history(ws, &file).unwrap();
    assert_eq!(hist.len(), 2);
    assert_eq!(
        hist.iter().find(|v| v.id == id_ext).unwrap().label.as_deref(),
        Some("renamed-old")
    );
    // clear label
    store.set_label(id_label, None).unwrap();
    let hist = store.history(ws, &file).unwrap();
    assert_eq!(hist.iter().find(|v| v.id == id_label).unwrap().label, None);

    // Version lạ → NotFound; file không có trên disk → lỗi
    assert!(matches!(store.set_label(987_654, Some("x")), Err(Error::NotFound(_))));
    assert!(store.add_label(ws, &_dir.path().join("missing.txt"), "x").is_err());
}

/// Áp hunks lên base bằng splice đúng ngữ nghĩa `model::Hunk` (dùng chỉ bằng API public).
fn apply_hunks_locally(base: &[u8], hunks: &[Hunk]) -> Vec<u8> {
    let mut lines: Vec<&[u8]> = Vec::new();
    let mut start = 0usize;
    for (i, b) in base.iter().enumerate() {
        if *b == b'\n' {
            lines.push(&base[start..=i]);
            start = i + 1;
        }
    }
    if start < base.len() {
        lines.push(&base[start..]);
    }
    let mut out = Vec::new();
    let mut next = 0usize;
    for h in hunks {
        let s = (h.old_start - 1) as usize;
        for line in &lines[next..s] {
            out.extend_from_slice(line);
        }
        out.extend_from_slice(h.new_text.as_bytes());
        next = s + h.old_lines as usize;
    }
    for line in &lines[next..] {
        out.extend_from_slice(line);
    }
    out
}

/// Diff text/hunks giữa 2 version và so với nội dung disk hiện tại.
#[test]
fn diff_version_and_content() {
    let (_dir, ws, store) = setup();
    let file = _dir.path().join("d.txt");
    let old = b"l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nl10\n".to_vec();
    fs::write(&file, &old).unwrap();
    let a = store.record(ws, &file, &old, EventKind::External, None).unwrap().unwrap();
    let newc = b"l1\nl2\nl3\nl4\nX5\nl6\nl7\nl8\nl9\nl10\n".to_vec();
    fs::write(&file, &newc).unwrap();
    let b = store.record(ws, &file, &newc, EventKind::External, None).unwrap().unwrap();

    // Unified diff: header + dòng -/+ đúng
    let text = store.diff_text(a, b).unwrap();
    assert!(text.contains(&format!("--- version {a}")), "header ---: {text}");
    assert!(text.contains(&format!("+++ version {b}")), "header +++: {text}");
    assert!(text.contains("-l5\n"), "thiếu dòng -l5: {text}");
    assert!(text.contains("+X5\n"), "thiếu dòng +X5: {text}");

    // Hunks áp lên nội dung a phải ra nội dung b
    let hunks = store.diff_lines(a, b).unwrap();
    assert!(!hunks.is_empty());
    assert_eq!(apply_hunks_locally(&old, &hunks), newc);
    // Không khác gì → rỗng
    assert!(store.diff_lines(a, a).unwrap().is_empty());
    assert_eq!(store.diff_text(a, a).unwrap(), "");

    // So nội dung disk hiện tại với version
    let disk_hunks = store.diff_lines_content(&newc, b).unwrap();
    assert!(disk_hunks.is_empty(), "disk == version b → không hunk");
    assert_eq!(store.diff_text_content(&newc, b).unwrap(), "");
    let older_hunks = store.diff_lines_content(&old, b).unwrap();
    assert_eq!(apply_hunks_locally(&old, &older_hunks), newc);
    let older_text = store.diff_text_content(&old, b).unwrap();
    assert!(older_text.contains("+X5\n"));

    // Version binary (invalid UTF-8) → BinaryContent(id) tương ứng
    let bin = b"\xff\xfe binary \xc3\x28\n".to_vec();
    let c = store.record(ws, &file, &bin, EventKind::External, None).unwrap().unwrap();
    assert!(matches!(store.diff_text(a, c), Err(Error::BinaryContent(id)) if id == c));
    assert!(matches!(
        store.diff_text_content(b"\xff\xfe bad\n", b),
        Err(Error::BinaryContent(-1))
    ));
    // NUL trong nội dung vẫn là UTF-8 hợp lệ → diff được (record lưu full nhưng không cản diff)
    let nul = b"bin\0ary\n".to_vec();
    let n = store.record(ws, &file, &nul, EventKind::External, None).unwrap().unwrap();
    assert!(!store.diff_lines(a, n).unwrap().is_empty());
}
