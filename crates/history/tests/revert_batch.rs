//! `revert_batch` edge cases — milestone B2 (plan M6 §3.4).
//!
//! Same short real-clock policy as tests/batch.rs (200 ms window / 400 ms timeout).

use std::path::Path;
use std::thread::sleep;
use std::time::Duration;

use history::{BatchSummary, EventKind, LocalHistory, VersionId, WorkspaceId};

fn ms(m: u64) -> Duration {
    Duration::from_millis(m)
}

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

fn latest_batch(store: &LocalHistory, ws: WorkspaceId) -> BatchSummary {
    store.batches(ws, 1).unwrap().into_iter().next().unwrap()
}

/// Edge 1 — file alive and unchanged since the batch: reverted to the pre-batch content via
/// `revert()`, so the pre_revert + restore snapshots are recorded.
#[test]
fn live_file_reverts_to_pre_batch_content() {
    let (dir, store, ws) = setup();
    let f = dir.path().join("live.txt");
    std::fs::write(&f, "v0").unwrap();
    record(&store, ws, &f, "v0"); // batch 1 = pre-batch state
    sleep(ms(250));
    record(&store, ws, &f, "v1"); // batch 2 = the batch being reverted
    std::fs::write(&f, "v1").unwrap(); // disk unchanged since the batch

    let batch = latest_batch(&store, ws);
    assert_eq!(batch.version_count, 1);
    let report = store.revert_batch(batch.id, false).unwrap();
    assert_eq!(report.reverted, 1);
    assert_eq!((report.restored, report.deleted, report.skipped), (0, 0, 0));
    assert!(report.errors.is_empty());
    assert_eq!(std::fs::read(&f).unwrap(), b"v0");
    // safety snapshots recorded by revert(): restore (newest) then pre_revert
    let hist = store.history(ws, &f).unwrap();
    assert_eq!(hist.len(), 4);
    assert_eq!(hist[0].kind, EventKind::Restore);
    assert_eq!(hist[1].kind, EventKind::PreRevert);
}

/// Edge 2 — file created within the batch, disk still identical: deleting it IS the
/// pre-batch state.
#[test]
fn created_in_batch_identical_file_is_deleted() {
    let (dir, store, ws) = setup();
    let f = dir.path().join("created.txt");
    std::fs::write(&f, "born").unwrap();
    record(&store, ws, &f, "born"); // created within the batch

    let batch = latest_batch(&store, ws);
    let report = store.revert_batch(batch.id, false).unwrap();
    assert_eq!(report.deleted, 1);
    assert_eq!((report.reverted, report.restored, report.skipped), (0, 0, 0));
    assert!(report.errors.is_empty());
    assert!(!f.exists());
}

/// Edge 2 continued — file created within the batch but edited afterwards: skip unless
/// force; force deletes it anyway.
#[test]
fn created_in_batch_modified_file_skips_then_force_deletes() {
    let (dir, store, ws) = setup();
    let f = dir.path().join("edited.txt");
    std::fs::write(&f, "born").unwrap();
    record(&store, ws, &f, "born");
    std::fs::write(&f, "user edit").unwrap(); // deliberate post-batch edit

    let batch = latest_batch(&store, ws);
    let report = store.revert_batch(batch.id, false).unwrap();
    assert_eq!(report.skipped, 1);
    assert!(report.errors.is_empty());
    assert_eq!(std::fs::read(&f).unwrap(), b"user edit");

    let report = store.revert_batch(batch.id, true).unwrap();
    assert_eq!(report.deleted, 1);
    assert!(!f.exists());
}

/// Edge 3 — file deleted within the batch (subtree removed from disk too): restored from
/// the pre-batch blob, missing parent directories recreated.
#[test]
fn file_deleted_in_batch_is_restored_with_parent_dirs() {
    let (dir, store, ws) = setup();
    let f = dir.path().join("sub").join("inner").join("deep.txt");
    std::fs::create_dir_all(f.parent().unwrap()).unwrap();
    std::fs::write(&f, "v0").unwrap();
    record(&store, ws, &f, "v0"); // batch 1 = pre-batch state
    sleep(ms(250));
    store
        .record(ws, &f, b"", EventKind::Delete, None)
        .unwrap()
        .unwrap(); // batch 2 = the delete marker
    std::fs::remove_dir_all(dir.path().join("sub")).unwrap();
    assert!(!f.exists());

    let batch = latest_batch(&store, ws);
    assert_eq!(batch.version_count, 1);
    let report = store.revert_batch(batch.id, false).unwrap();
    assert_eq!(report.restored, 1);
    assert!(report.errors.is_empty());
    assert_eq!(std::fs::read(&f).unwrap(), b"v0");
}

/// Edge 4 — file edited after the batch (batch no longer matches disk): skipped unless
/// force; force overwrites with the pre-batch content.
#[test]
fn file_edited_after_batch_skips_then_force_overwrites() {
    let (dir, store, ws) = setup();
    let f = dir.path().join("flow.txt");
    std::fs::write(&f, "v0").unwrap();
    record(&store, ws, &f, "v0"); // batch 1 = pre
    sleep(ms(250));
    record(&store, ws, &f, "v1"); // batch 2 = the batch
    sleep(ms(250));
    std::fs::write(&f, "v2").unwrap();
    record(&store, ws, &f, "v2"); // batch 3 = user edited after the batch

    let b2 = &store.batches(ws, 3).unwrap()[1];
    assert_eq!(b2.version_count, 1);
    let report = store.revert_batch(b2.id, false).unwrap();
    assert_eq!(report.skipped, 1);
    assert!(report.errors.is_empty());
    assert_eq!(std::fs::read(&f).unwrap(), b"v2"); // user's edit kept

    let report = store.revert_batch(b2.id, true).unwrap();
    assert_eq!(report.reverted, 1);
    assert_eq!(std::fs::read(&f).unwrap(), b"v0"); // pre-batch content
}

/// Edge 5 — one failing file lands in report.errors and the other files are still reverted.
#[test]
fn one_failing_file_does_not_block_the_others() {
    let (dir, store, ws) = setup();
    let a = dir.path().join("broken.txt");
    let b = dir.path().join("healthy.txt");
    std::fs::write(&a, "a0").unwrap();
    std::fs::write(&b, "b0").unwrap();
    record(&store, ws, &a, "a0"); // batch 1 = pre-batch state for both
    record(&store, ws, &b, "b0");
    sleep(ms(250));
    record(&store, ws, &a, "a1"); // batch 2
    record(&store, ws, &b, "b1");
    std::fs::write(&a, "a1").unwrap();
    std::fs::write(&b, "b1").unwrap();

    // Sabotage exactly one file: remove the blob of broken.txt's pre-batch (seq 1, full)
    // version → content() fails for that file only.
    let hist = store.history(ws, &a).unwrap();
    let pre_id = hist.last().unwrap().id;
    let blob = store
        .root()
        .join("workspaces")
        .join(format!("{:016x}", ws as u64))
        .join("blobs")
        .join(format!("v{pre_id}-full.zst"));
    std::fs::remove_file(&blob).unwrap();

    let batch = latest_batch(&store, ws);
    let report = store.revert_batch(batch.id, false).unwrap();
    assert_eq!(report.reverted, 1, "healthy.txt must still be reverted");
    assert_eq!(report.errors.len(), 1);
    assert_eq!(report.errors[0].0, "broken.txt");
    assert_eq!(std::fs::read(&b).unwrap(), b"b0");
    assert_eq!(std::fs::read(&a).unwrap(), b"a1"); // untouched
}

/// Edge 7 — the pre-batch state itself is ABSENCE (the newest pre-batch version is a delete
/// marker) and the file is gone from disk: a no-op, never a fabricated 0-byte file (a
/// delete marker stores empty content) — with or without `force` (plan M6 §3.4: revert to
/// the state BEFORE the batch, which here is "file missing").
#[test]
fn pre_batch_absent_file_missing_on_disk_is_a_noop() {
    let (dir, store, ws) = setup();
    let f = dir.path().join("ghost.txt");
    std::fs::write(&f, "v0").unwrap();
    record(&store, ws, &f, "v0"); // batch 1: the last content the file ever had
    sleep(ms(250));
    store
        .record(ws, &f, b"", EventKind::Delete, None)
        .unwrap()
        .unwrap(); // batch 2: deleted BEFORE the batch being reverted
    std::fs::remove_file(&f).unwrap();
    sleep(ms(250));
    record(&store, ws, &f, "v1"); // batch 3: re-created and deleted again within the batch
    store
        .record(ws, &f, b"", EventKind::Delete, None)
        .unwrap()
        .unwrap();
    assert!(!f.exists());

    let batch = latest_batch(&store, ws);
    let report = store.revert_batch(batch.id, false).unwrap();
    assert_eq!(report.skipped, 1, "absent before the batch → nothing to restore");
    assert_eq!((report.reverted, report.restored, report.deleted), (0, 0, 0));
    assert!(report.errors.is_empty());
    assert!(!f.exists(), "no fabricated empty file on disk");

    // `force` must not fabricate content either: the pre-batch state is still absence
    let report = store.revert_batch(batch.id, true).unwrap();
    assert_eq!(report.skipped, 1);
    assert!(!f.exists());
}

/// Edge 8 — same pre-batch delete marker, but the file EXISTS on disk (re-created within
/// the batch): the pre-batch state is absence, so an unchanged file is REMOVED from disk —
/// never overwritten with the marker's empty content.
#[test]
fn pre_batch_absent_file_present_on_disk_is_deleted() {
    let (dir, store, ws) = setup();
    let f = dir.path().join("phoenix.txt");
    std::fs::write(&f, "v0").unwrap();
    record(&store, ws, &f, "v0"); // batch 1
    sleep(ms(250));
    store
        .record(ws, &f, b"", EventKind::Delete, None)
        .unwrap()
        .unwrap(); // batch 2: deleted before the batch being reverted
    std::fs::remove_file(&f).unwrap();
    sleep(ms(250));
    record(&store, ws, &f, "v1"); // batch 3: re-created within the batch...
    std::fs::write(&f, "v1").unwrap(); // ...and back on disk, unchanged since the batch

    let batch = latest_batch(&store, ws);
    let report = store.revert_batch(batch.id, false).unwrap();
    assert_eq!(report.deleted, 1, "removing it IS the pre-batch state");
    assert_eq!((report.reverted, report.restored, report.skipped), (0, 0, 0));
    assert!(report.errors.is_empty());
    assert!(!f.exists());
}

/// Edge 8 continued — the re-created file was edited after the batch: skipped unless
/// `force`; force removes it (its newer content differs from anything in the chain).
#[test]
fn pre_batch_absent_file_edited_after_batch_skips_then_force_deletes() {
    let (dir, store, ws) = setup();
    let f = dir.path().join("reborn.txt");
    std::fs::write(&f, "v0").unwrap();
    record(&store, ws, &f, "v0"); // batch 1
    sleep(ms(250));
    store
        .record(ws, &f, b"", EventKind::Delete, None)
        .unwrap()
        .unwrap(); // batch 2: deleted before the batch being reverted
    std::fs::remove_file(&f).unwrap();
    sleep(ms(250));
    record(&store, ws, &f, "v1"); // batch 3: re-created within the batch
    std::fs::write(&f, "user edit").unwrap(); // edited after the batch (not recorded)

    let batch = latest_batch(&store, ws);
    let report = store.revert_batch(batch.id, false).unwrap();
    assert_eq!(report.skipped, 1);
    assert!(report.errors.is_empty());
    assert_eq!(std::fs::read(&f).unwrap(), b"user edit");

    let report = store.revert_batch(batch.id, true).unwrap();
    assert_eq!(report.deleted, 1);
    assert!(!f.exists());
}

/// Edge 6 — one file with N versions in the same batch: the representative is the max-seq
/// version and the pre-batch version is min-seq − 1 (NOT the intermediate version).
#[test]
fn multiple_versions_in_one_batch_use_max_seq_representative() {
    let (dir, store, ws) = setup();
    let f = dir.path().join("multi.txt");
    std::fs::write(&f, "v0").unwrap();
    record(&store, ws, &f, "v0"); // batch 1 = pre
    sleep(ms(250));
    record(&store, ws, &f, "v1"); // batch 2: two versions in ONE batch
    sleep(ms(30));
    record(&store, ws, &f, "v2");
    std::fs::write(&f, "v2").unwrap();

    let batch = latest_batch(&store, ws);
    assert_eq!(batch.version_count, 2);
    assert_eq!(batch.file_count, 1);
    assert_eq!(store.batch_entries(batch.id).unwrap().len(), 2);

    let report = store.revert_batch(batch.id, false).unwrap();
    assert_eq!(report.reverted, 1);
    assert!(report.errors.is_empty());
    // reverted to min-seq − 1 ("v0"), not to the intermediate "v1"
    assert_eq!(std::fs::read(&f).unwrap(), b"v0");
}
