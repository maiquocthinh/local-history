//! Tests GC / retention (M1 bước 5).
//!
//! Tự chế tạo dữ liệu bằng SQL + ghi blob zstd trực tiếp lên disk — KHÔNG phụ thuộc
//! record/reconstruct (đang làm song song). Mỗi test dùng store root + workspace root riêng
//! (tempdir) nên chạy song song không đụng nhau.
//!
//! Mốc thời gian cố định: now = Thứ 6 2026-11-06 15:00 UTC, policy working_days = 1
//! → cutoff = Thứ 5 2026-11-05 00:00 UTC (khớp với unit test của `working_day_cutoff`).
//! Chuỗi 60 version cách nhau 2h, version index i (0-based) có timestamp
//! `cutoff - 80h + i*2h` → i < 40 cũ hơn cutoff, i = 40 trúng đúng cutoff.

use std::path::{Path, PathBuf};

use history::{FileId, LocalHistory, RetentionPolicy, WorkspaceId};
use rusqlite::Connection;
use time::Month;

const HOUR: i64 = 3_600_000_000; // micros

/// unix micros UTC của ngày-giờ cụ thể.
fn micros_at(y: i32, m: Month, d: u8, h: u8, mi: u8) -> i64 {
    time::Date::from_calendar_date(y, m, d)
        .unwrap()
        .with_time(time::Time::from_hms(h, mi, 0).unwrap())
        .assume_utc()
        .unix_timestamp_nanos() as i64 / 1000
}

/// Cutoff kỳ vọng: Thứ 5 2026-11-05 00:00 UTC (= now Thứ 6 15:00, lùi 1 ngày làm việc).
fn cutoff_thu() -> i64 {
    micros_at(2026, Month::November, 5, 0, 0)
}

/// now cố định: Thứ 6 2026-11-06 15:00 UTC.
fn now_fri() -> i64 {
    micros_at(2026, Month::November, 6, 15, 0)
}

/// (seq, timestamp_micros, kind, storage, size) — hàng dữ liệu giả cho `versions`.
type Row = (i64, i64, &'static str, &'static str, i64);

/// 60 version cách nhau 2h quanh cutoff (xem header module).
/// `storage_of(index)` quyết định full/delta cho version có index 0-based.
fn rows60(storage_of: impl Fn(i64) -> &'static str) -> Vec<Row> {
    (0..60i64)
        .map(|i| {
            let ts = cutoff_thu() - 80 * HOUR + i * 2 * HOUR;
            (i + 1, ts, "external", storage_of(i), 1000)
        })
        .collect()
}

struct Fabricated {
    ws: WorkspaceId,
    #[allow(dead_code)]
    file_id: FileId,
    /// (seq, version_id, storage) theo thứ tự seq tăng dần.
    versions: Vec<(i64, i64, &'static str)>,
}

/// Đăng ký workspace, INSERT file + versions bằng connection SQL riêng (WAL cho phép
/// nhiều connection cùng lúc), và GHI blob zstd thật vào
/// `root/workspaces/{ws:016x}/blobs/v{id}-{storage}.zst`.
fn fabricate(store: &LocalHistory, ws_root: &Path, rel_path: &str, rows: &[Row]) -> Fabricated {
    let ws = store.register_workspace(ws_root).unwrap();
    let conn = Connection::open(store.root().join("index.db")).unwrap();
    conn.busy_timeout(std::time::Duration::from_secs(5)).unwrap();

    let max_seq = rows.iter().map(|r| r.0).max().unwrap_or(0);
    conn.execute(
        "INSERT INTO files (workspace_id, rel_path, current_seq) VALUES (?1, ?2, ?3)",
        rusqlite::params![ws, rel_path, max_seq],
    )
    .unwrap();
    let file_id = conn.last_insert_rowid();

    let mut versions = Vec::new();
    for &(seq, ts, kind, storage, size) in rows {
        let base_seq = if storage == "delta" { Some(seq - 1) } else { None };
        conn.execute(
            "INSERT INTO versions
             (file_id, seq, timestamp, kind, label, storage, base_seq, size, content_hash)
             VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?6, ?7, ?8)",
            rusqlite::params![file_id, seq, ts, kind, storage, base_seq, size, format!("h{seq}")],
        )
        .unwrap();
        let vid = conn.last_insert_rowid();
        versions.push((seq, vid, storage));

        // blob thật: nén nội dung giả rồi ghi đúng layout của store
        let dir = store
            .root()
            .join("workspaces")
            .join(format!("{:016x}", ws as u64))
            .join("blobs");
        std::fs::create_dir_all(&dir).unwrap();
        let content = zstd::bulk::compress(format!("content-{seq}").as_bytes(), 3).unwrap();
        std::fs::write(dir.join(format!("v{vid}-{storage}.zst")), &content).unwrap();
    }
    Fabricated { ws, file_id, versions }
}

/// Đường blob — tự ghép (blob_path là pub(crate), test integration không gọi được).
fn blob_path(store_root: &Path, ws: WorkspaceId, vid: i64, storage: &str) -> PathBuf {
    store_root
        .join("workspaces")
        .join(format!("{:016x}", ws as u64))
        .join("blobs")
        .join(format!("v{vid}-{storage}.zst"))
}

fn blobs_dir(store_root: &Path, ws: WorkspaceId) -> PathBuf {
    store_root
        .join("workspaces")
        .join(format!("{:016x}", ws as u64))
        .join("blobs")
}

fn count_versions(conn: &Connection, ws: Option<WorkspaceId>) -> i64 {
    match ws {
        Some(w) => conn
            .query_row(
                "SELECT COUNT(*) FROM versions v JOIN files f ON f.id = v.file_id
                 WHERE f.workspace_id = ?1",
                [w],
                |r| r.get(0),
            )
            .unwrap(),
        None => conn
            .query_row("SELECT COUNT(*) FROM versions", [], |r| r.get(0))
            .unwrap(),
    }
}

// ----- test 3: gc end-to-end -----

#[test]
fn gc_end_to_end() {
    let store_dir = tempfile::tempdir().unwrap();
    let ws_dir = tempfile::tempdir().unwrap();
    let store = LocalHistory::open(store_dir.path()).unwrap();
    let rows = rows60(|i| if i % 5 == 0 { "full" } else { "delta" });
    let fab = fabricate(&store, ws_dir.path(), "src/main.rs", &rows);

    // dung lượng blob của 40 version sẽ bị xóa (đối chiếu bytes_freed)
    let expected_freed: u64 = fab
        .versions
        .iter()
        .take(40)
        .map(|&(_, vid, st)| {
            std::fs::metadata(blob_path(store.root(), fab.ws, vid, st))
                .unwrap()
                .len()
        })
        .sum();

    let report = store
        .gc(
            None,
            &RetentionPolicy { working_days: 1, min_versions: 10 },
            now_fri(),
            false,
        )
        .unwrap();

    assert_eq!(report.files_scanned, 1);
    assert_eq!(report.versions_deleted, 40);
    assert_eq!(report.orphan_blobs, 0);
    assert_eq!(report.bytes_freed, expected_freed);
    assert!(!report.dry_run);

    // db: còn đúng 20 version, seq 41..=60
    let conn = Connection::open(store.root().join("index.db")).unwrap();
    assert_eq!(count_versions(&conn, None), 20);
    let (min_seq, max_seq): (i64, i64) = conn
        .query_row("SELECT MIN(seq), MAX(seq) FROM versions", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!((min_seq, max_seq), (41, 60));

    // disk: blob version đã xóa biến mất, blob version còn lại còn nguyên
    for &(seq, vid, st) in &fab.versions {
        let p = blob_path(store.root(), fab.ws, vid, st);
        assert_eq!(p.exists(), seq > 40, "blob seq {seq} tồn tại sai");
    }
}

// ----- test 4: dry run không đụng db/disk -----

#[test]
fn gc_dry_run_leaves_everything_untouched() {
    let store_dir = tempfile::tempdir().unwrap();
    let ws_dir = tempfile::tempdir().unwrap();
    let store = LocalHistory::open(store_dir.path()).unwrap();
    let rows = rows60(|i| if i % 5 == 0 { "full" } else { "delta" });
    let fab = fabricate(&store, ws_dir.path(), "src/main.rs", &rows);

    let expected_freed: u64 = fab
        .versions
        .iter()
        .take(40)
        .map(|&(_, vid, st)| {
            std::fs::metadata(blob_path(store.root(), fab.ws, vid, st))
                .unwrap()
                .len()
        })
        .sum();

    let report = store
        .gc(
            None,
            &RetentionPolicy { working_days: 1, min_versions: 10 },
            now_fri(),
            true,
        )
        .unwrap();

    assert!(report.dry_run);
    assert_eq!(report.files_scanned, 1);
    assert_eq!(report.versions_deleted, 40); // số SẼ xóa
    assert_eq!(report.orphan_blobs, 0);
    assert_eq!(report.bytes_freed, expected_freed); // dung lượng SẼ giải phóng

    // db không đổi
    let conn = Connection::open(store.root().join("index.db")).unwrap();
    assert_eq!(count_versions(&conn, None), 60);
    // disk không đổi: mọi blob còn nguyên
    for &(_, vid, st) in &fab.versions {
        assert!(blob_path(store.root(), fab.ws, vid, st).exists());
    }
}

// ----- test 5: orphan sweep -----

#[test]
fn orphan_sweep_removes_blobs_without_rows() {
    let store_dir = tempfile::tempdir().unwrap();
    let ws_dir = tempfile::tempdir().unwrap();
    let store = LocalHistory::open(store_dir.path()).unwrap();
    // 3 version mới (sau cutoff) → phần retention không xóa gì
    let rows: Vec<Row> = (0..3i64)
        .map(|i| {
            let ts = now_fri() - (3 - i) * HOUR;
            (i + 1, ts, "external", if i == 0 { "full" } else { "delta" }, 100)
        })
        .collect();
    let fab = fabricate(&store, ws_dir.path(), "a.txt", &rows);

    // 2 blob mồ côi trong cùng blobs dir: id không có trong db + file .tmp rác
    let dir = blobs_dir(store.root(), fab.ws);
    std::fs::write(dir.join("v99999-full.zst"), b"orphan blob").unwrap();
    std::fs::write(dir.join("x.tmp"), b"temp garbage").unwrap();

    let report = store
        .gc(None, &RetentionPolicy::default(), now_fri(), false)
        .unwrap();

    assert_eq!(report.files_scanned, 1);
    assert_eq!(report.versions_deleted, 0);
    assert_eq!(report.orphan_blobs, 2);
    assert_eq!(
        report.bytes_freed,
        (b"orphan blob".len() + b"temp garbage".len()) as u64
    );

    // file mồ côi đã biến mất, blob thật còn nguyên
    assert!(!dir.join("v99999-full.zst").exists());
    assert!(!dir.join("x.tmp").exists());
    for &(_, vid, st) in &fab.versions {
        assert!(blob_path(store.root(), fab.ws, vid, st).exists());
    }
}

// ----- test 6: gc giới hạn trong 1 workspace -----

#[test]
fn gc_scoped_to_one_workspace() {
    let store_dir = tempfile::tempdir().unwrap();
    let ws1_dir = tempfile::tempdir().unwrap();
    let ws2_dir = tempfile::tempdir().unwrap();
    let store = LocalHistory::open(store_dir.path()).unwrap();
    let rows = rows60(|i| if i % 5 == 0 { "full" } else { "delta" });
    let fab1 = fabricate(&store, ws1_dir.path(), "one.txt", &rows);
    let fab2 = fabricate(&store, ws2_dir.path(), "two.txt", &rows);

    let report = store
        .gc(
            Some(fab1.ws),
            &RetentionPolicy { working_days: 1, min_versions: 10 },
            now_fri(),
            false,
        )
        .unwrap();

    assert_eq!(report.files_scanned, 1); // chỉ file của ws1 được quét
    assert_eq!(report.versions_deleted, 40);
    assert_eq!(report.orphan_blobs, 0);

    // ws1: còn 20; ws2: nguyên vẹn 60
    let conn = Connection::open(store.root().join("index.db")).unwrap();
    assert_eq!(count_versions(&conn, Some(fab1.ws)), 20);
    assert_eq!(count_versions(&conn, Some(fab2.ws)), 60);

    for &(seq, vid, st) in &fab1.versions {
        assert_eq!(
            blob_path(store.root(), fab1.ws, vid, st).exists(),
            seq > 40
        );
    }
    for &(_, vid, st) in &fab2.versions {
        assert!(blob_path(store.root(), fab2.ws, vid, st).exists());
    }
}
