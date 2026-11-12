//! Auto-batch (lazy lifecycle, plan M6 §3.2) and the query API (plan M6 §3.3) — milestone B1.
//!
//! Tests use a short policy (200 ms window / 400 ms timeout) on the REAL clock — no injected
//! clock, per plan M6 §3.2.

use std::path::Path;
use std::thread::sleep;
use std::time::Duration;

use history::{EventKind, LocalHistory, VersionId, WorkspaceId};

fn ms(m: u64) -> Duration {
    Duration::from_millis(m)
}

/// Store + one workspace with the short test policy; the store root doubles as ws root.
fn setup() -> (tempfile::TempDir, LocalHistory, WorkspaceId) {
    let dir = tempfile::tempdir().unwrap();
    let store = LocalHistory::open(dir.path()).unwrap();
    store.set_batch_policy(200, 400);
    let ws = store.register_workspace(dir.path()).unwrap();
    (dir, store, ws)
}

fn record(store: &LocalHistory, ws: WorkspaceId, file: &Path, content: &str) -> VersionId {
    store
        .record(ws, file, content.as_bytes(), EventKind::External, None)
        .unwrap()
        .expect("distinct contents never dedup")
}

/// 3 records 50 ms apart (window 200 ms) land in ONE batch with correct counts.
#[test]
fn sliding_window_groups_records_into_one_batch() {
    let (dir, store, ws) = setup();
    let f = dir.path().join("w.txt");
    for i in 0..3 {
        record(&store, ws, &f, &format!("content {i}"));
        if i < 2 {
            sleep(ms(50));
        }
    }
    let batches = store.batches(ws, 100).unwrap();
    assert_eq!(batches.len(), 1);
    let b = &batches[0];
    assert_eq!(b.kind, "auto");
    assert_eq!(b.label, None);
    assert_eq!(b.version_count, 3);
    assert_eq!(b.file_count, 1);
    assert!(b.started_at <= b.ended_at);
    assert_eq!(b.ws_id, ws);
}

/// Two waves separated by 400 ms (> window 200 ms) form TWO batches, newest first.
#[test]
fn two_waves_separated_by_window_form_two_batches() {
    let (dir, store, ws) = setup();
    let f = dir.path().join("w.txt");
    record(&store, ws, &f, "wave1-a");
    sleep(ms(50));
    record(&store, ws, &f, "wave1-b");
    sleep(ms(400));
    record(&store, ws, &f, "wave2-a");
    sleep(ms(50));
    record(&store, ws, &f, "wave2-b");
    let batches = store.batches(ws, 100).unwrap();
    assert_eq!(batches.len(), 2);
    assert_eq!(batches[0].version_count, 2);
    assert_eq!(batches[1].version_count, 2);
    assert!(batches[0].started_at > batches[1].started_at, "newest first");
}

/// Lazy close (plan M6 §3.2): no timer thread — a record past the window closes the old
/// batch (`closed_at` stamped) and starts a new one; `ended_at` tracks version timestamps.
#[test]
fn lazy_close_sets_closed_at_and_starts_new_batch() {
    let (dir, store, ws) = setup();
    let f = dir.path().join("lazy.txt");
    let id1 = record(&store, ws, &f, "one");
    sleep(ms(250)); // > window
    let id2 = record(&store, ws, &f, "two");
    let batches = store.batches(ws, 10).unwrap();
    assert_eq!(batches.len(), 2);
    // newest first: id2 in the new batch, id1 in the lazily closed one
    let map = store.batch_map_for_file(ws, &f).unwrap();
    assert_eq!(map.len(), 2);
    assert_eq!(map[&id1], batches[1].id);
    assert_eq!(map[&id2], batches[0].id);
    // ended_at == timestamp of the batch's newest version (architecture §10.1)
    let hist = store.history(ws, &f).unwrap();
    assert_eq!(hist[0].id, id2);
    assert_eq!(hist[1].id, id1);
    assert_eq!(batches[0].ended_at, hist[0].timestamp_micros);
    assert_eq!(batches[1].ended_at, hist[1].timestamp_micros);
    // the old batch was closed by the next record, not by a timer
    let conn = rusqlite::Connection::open(store.root().join("index.db")).unwrap();
    let closed_old: Option<i64> = conn
        .query_row(
            "SELECT closed_at FROM batches WHERE id = ?1",
            [batches[1].id],
            |r| r.get(0),
        )
        .unwrap();
    assert!(closed_old.is_some(), "old batch must be lazily closed");
    let closed_new: Option<i64> = conn
        .query_row(
            "SELECT closed_at FROM batches WHERE id = ?1",
            [batches[0].id],
            |r| r.get(0),
        )
        .unwrap();
    assert!(closed_new.is_none(), "new batch must still be open");
}

/// Records alternating between two workspaces inside the window never mix into one batch.
#[test]
fn batches_never_cross_workspaces() {
    let dir = tempfile::tempdir().unwrap();
    let store = LocalHistory::open(dir.path()).unwrap();
    store.set_batch_policy(200, 400);
    let ws1_dir = tempfile::tempdir().unwrap();
    let ws2_dir = tempfile::tempdir().unwrap();
    let ws1 = store.register_workspace(ws1_dir.path()).unwrap();
    let ws2 = store.register_workspace(ws2_dir.path()).unwrap();
    assert_ne!(ws1, ws2);
    let f1 = ws1_dir.path().join("a.txt");
    let f2 = ws2_dir.path().join("b.txt");
    let id1 = record(&store, ws1, &f1, "ws1 one");
    let id2 = record(&store, ws2, &f2, "ws2 one");
    let id3 = record(&store, ws1, &f1, "ws1 two");
    let batches1 = store.batches(ws1, 10).unwrap();
    let batches2 = store.batches(ws2, 10).unwrap();
    assert_eq!(batches1.len(), 1);
    assert_eq!(batches2.len(), 1);
    assert_eq!(batches1[0].ws_id, ws1);
    assert_eq!(batches2[0].ws_id, ws2);
    assert_eq!(batches1[0].version_count, 2);
    assert_eq!(batches2[0].version_count, 1);
    let map1 = store.batch_map_for_file(ws1, &f1).unwrap();
    let map2 = store.batch_map_for_file(ws2, &f2).unwrap();
    assert_eq!(map1[&id1], batches1[0].id);
    assert_eq!(map1[&id3], batches1[0].id);
    assert_eq!(map2[&id2], batches2[0].id);
}

/// A batch-layer failure must never fail the record: the version is committed, keeps
/// batch_id NULL and the chain is intact (plan M6 §3.2 — "record không bao giờ fail vì batch").
#[test]
fn record_survives_batch_layer_failure() {
    let (dir, store, ws) = setup();
    // Inject the failure: a trigger aborts every INSERT INTO batches while the table itself
    // (and the FK graph of versions.batch_id) stays intact.
    {
        let conn = rusqlite::Connection::open(store.root().join("index.db")).unwrap();
        conn.execute_batch(
            "CREATE TRIGGER fail_batches_insert BEFORE INSERT ON batches
             BEGIN SELECT RAISE(ABORT, 'injected batch failure'); END;",
        )
        .unwrap();
    }
    let f = dir.path().join("survivor.txt");
    let id1 = record(&store, ws, &f, "survivor");
    let id2 = record(&store, ws, &f, "survivor again");
    // the records themselves succeeded and the chain is intact
    let hist = store.history(ws, &f).unwrap();
    assert_eq!(hist.len(), 2);
    assert_eq!(hist[0].id, id2);
    assert_eq!(hist[1].id, id1);
    // the versions simply carry no batch
    assert!(store.batch_map_for_file(ws, &f).unwrap().is_empty());
    // reads of the batch layer are unaffected (they simply see nothing)
    assert!(store.batches(ws, 10).unwrap().is_empty());
}

/// batches() counts/limits, batch_entries() rows+ordering, batch_map_for_file() mapping.
#[test]
fn batch_queries_report_accurate_counts() {
    let (dir, store, ws) = setup();
    let a = dir.path().join("a.txt");
    let b = dir.path().join("sub").join("b.txt");
    // batch 1: two versions of a.txt
    let a1 = record(&store, ws, &a, "a one");
    sleep(ms(30));
    let a2 = record(&store, ws, &a, "a two");
    sleep(ms(400));
    // batch 2: sub/b.txt twice + a.txt once
    let b1 = record(&store, ws, &b, "b one");
    sleep(ms(30));
    let b2 = record(&store, ws, &b, "b two");
    let a3 = record(&store, ws, &a, "a three");

    let batches = store.batches(ws, 10).unwrap();
    assert_eq!(batches.len(), 2);
    assert_eq!(batches[0].version_count, 3);
    assert_eq!(batches[0].file_count, 2);
    assert_eq!(batches[1].version_count, 2);
    assert_eq!(batches[1].file_count, 1);
    assert_eq!(store.batches(ws, 1).unwrap().len(), 1, "limit respected");

    // entries ordered by rel_path then seq
    let entries = store.batch_entries(batches[0].id).unwrap();
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[0].rel_path, "a.txt");
    assert_eq!(entries[0].version.id, a3);
    assert_eq!(entries[1].rel_path, "sub/b.txt");
    assert_eq!(entries[1].version.id, b1);
    assert_eq!(entries[2].rel_path, "sub/b.txt");
    assert_eq!(entries[2].version.id, b2);

    // map covers exactly this file's versions, batch ids as recorded
    let map = store.batch_map_for_file(ws, &a).unwrap();
    assert_eq!(map.len(), 3);
    assert_eq!(map[&a1], batches[1].id);
    assert_eq!(map[&a2], batches[1].id);
    assert_eq!(map[&a3], batches[0].id);

    // unknown batch id → NotFound
    assert!(store.batch_entries(987_654).is_err());
}
