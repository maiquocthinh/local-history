//! Integration tests for `LocalHistory::rename`: the whole version chain follows the file
//! to its new relative path (JetBrains-style), with one `Rename` version marking the move.

use std::fs;

use history::{Error, EventKind, LocalHistory, WorkspaceId};

fn setup() -> (tempfile::TempDir, WorkspaceId, LocalHistory) {
    let dir = tempfile::tempdir().unwrap();
    let store = LocalHistory::open(dir.path()).unwrap();
    let ws = store.register_workspace(dir.path()).unwrap();
    (dir, ws, store)
}

/// Builds a 3-version file (init + 2 externals) and returns its history.
fn three_versions(
    dir: &tempfile::TempDir,
    ws: WorkspaceId,
    store: &LocalHistory,
) -> Vec<history::Version> {
    let p = dir.path().join("a.txt");
    fs::write(&p, b"v1").unwrap();
    store.record(ws, &p, b"v1", EventKind::Save, None).unwrap();
    fs::write(&p, b"v2").unwrap();
    store.record(ws, &p, b"v2", EventKind::External, None).unwrap();
    fs::write(&p, b"v3").unwrap();
    store.record(ws, &p, b"v3", EventKind::External, None).unwrap();
    store.history(ws, &p).unwrap()
}

#[test]
fn rename_moves_the_whole_chain() {
    let (dir, ws, store) = setup();
    let a = dir.path().join("a.txt");
    let a_versions = three_versions(&dir, ws, &store);
    let files_before = store.list_files(ws, None).unwrap().len();

    // Disk rename first (the daemon calls rename() after the move happened).
    let b = dir.path().join("b.txt");
    fs::rename(&a, &b).unwrap();
    let vid = store.rename(ws, &a, &b).unwrap().expect("rename phải ghi version");

    // history() is newest-first (seq DESC): the newest = the Rename version itself.
    let b_versions = store.history(ws, &b).unwrap();
    assert_eq!(b_versions.len(), a_versions.len() + 1, "chain dài thêm đúng 1 (rename)");
    let newest = b_versions.first().unwrap();
    assert_eq!(newest.id, vid);
    assert_eq!(newest.kind, EventKind::Rename);
    assert_eq!(newest.label.as_deref(), Some("a.txt"), "label = rel path cũ");
    assert_eq!(newest.seq, b_versions[1].seq + 1, "seq tiếp mạch");
    // Old lineage intact under the new path (same rows: ids match 1:1, newest-first).
    assert_eq!(
        b_versions[1..].iter().map(|v| v.id).collect::<Vec<_>>(),
        a_versions.iter().map(|v| v.id).collect::<Vec<_>>(),
        "các version cũ phải nguyên vẹn (id, seq, content…)"
    );

    // (b) The old rel path is gone from the files list; the count is unchanged.
    let files = store.list_files(ws, None).unwrap();
    assert_eq!(files.len(), files_before, "rename không tạo file row mới");
    assert!(files.iter().all(|f| f.rel_path != "a.txt"), "path cũ phải biến mất");
    assert!(files.iter().any(|f| f.rel_path == "b.txt"), "path mới phải xuất hiện");
    assert!(store.history(ws, &a).unwrap().is_empty(), "path cũ không còn history");

    // inotify fires a metadata event right after the rename — same content, kind
    // External instead of Rename — and must dedup (no spurious duplicate row).
    store.record(ws, &b, b"v3", EventKind::External, None).unwrap();
    assert_eq!(
        store.history(ws, &b).unwrap().len(),
        a_versions.len() + 1,
        "metadata event sau rename phải dedup"
    );
}

#[test]
fn rename_to_existing_path_errors() {
    let (dir, ws, store) = setup();
    let a = dir.path().join("a.txt");
    three_versions(&dir, ws, &store);

    // A tracked file already occupies the destination.
    let c = dir.path().join("c.txt");
    fs::write(&c, b"other").unwrap();
    store.record(ws, &c, b"other", EventKind::Save, None).unwrap();

    fs::rename(&a, &c).unwrap(); // disk-level overwrite
    let err = store.rename(ws, &a, &c).unwrap_err();
    assert!(
        matches!(err, Error::NotFound(_)),
        "đích đã có file row → phải lỗi, nhận: {err:?}"
    );
    // Both rows untouched.
    assert!(store.history(ws, &dir.path().join("a.txt")).unwrap().len() >= 3);
    assert!(!store.history(ws, &c).unwrap().is_empty());
}

#[test]
fn rename_outside_workspace_errors() {
    let (dir, ws, store) = setup();
    let a = dir.path().join("a.txt");
    three_versions(&dir, ws, &store);

    // Move the file OUT of the workspace, then try to record the rename.
    let outside = tempfile::tempdir().unwrap();
    let out = outside.path().join("moved.txt");
    fs::rename(&a, &out).unwrap();
    let err = store.rename(ws, &a, &out).unwrap_err();
    assert!(
        matches!(err, Error::OutsideWorkspace(_) | Error::NotFound(_)),
        "ngoài ws phải lỗi, nhận: {err:?}"
    );
    // The chain is untouched — a.txt still owns it.
    assert_eq!(store.history(ws, &a).unwrap().len(), 3);
}

#[test]
fn rename_then_record_continues_the_chain() {
    let (dir, ws, store) = setup();
    let a = dir.path().join("a.txt");
    let a_versions = three_versions(&dir, ws, &store);

    let b = dir.path().join("b.txt");
    fs::rename(&a, &b).unwrap();
    store.rename(ws, &a, &b).unwrap();

    // (e) A post-rename edit appends to the SAME chain; repeated identical content still
    // dedups against the now-latest version (kind + hash match).
    store.record(ws, &b, b"v4", EventKind::External, None).unwrap();
    let h = store.history(ws, &b).unwrap();
    assert_eq!(h.len(), a_versions.len() + 2, "sau rename ghi tiếp được lên chain");
    store.record(ws, &b, b"v4", EventKind::External, None).unwrap();
    assert_eq!(
        store.history(ws, &b).unwrap().len(),
        a_versions.len() + 2,
        "dedup vẫn chặn nội dung lặp"
    );
    let h = store.history(ws, &b).unwrap();
    assert_eq!(h.first().unwrap().kind, EventKind::External);
    // Reconstruct the newest — the delta chain must survive the rel_path swap.
    assert_eq!(
        store.content(h.first().unwrap().id).unwrap(),
        b"v4".to_vec(),
        "delta chain phải tái dựng được sau rename"
    );
}

#[test]
fn rename_same_path_is_noop() {
    let (dir, ws, store) = setup();
    let a = dir.path().join("a.txt");
    three_versions(&dir, ws, &store);
    assert!(store.rename(ws, &a, &a).unwrap().is_none(), "trùng path → Ok(None)");
    assert_eq!(store.history(ws, &a).unwrap().len(), 3, "không thêm version");
}
