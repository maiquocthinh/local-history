//! Integration test daemon in-process (M2): spawn `run_with_root` trên thread riêng, store +
//! workspace riêng bằng tempdir cho từng test → không đụng store thật, chạy song song an toàn.
//!
//! Chống flakiness: sleep warm-up trước khi ghi file (watcher cần nhịp sẵn sàng), mọi poll
//! vòng 200ms với timeout lớn 8s — máy chậm/CI vẫn pass.

use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use history::{EventKind, LocalHistory};

const POLL: Duration = Duration::from_millis(200);
const TIMEOUT: Duration = Duration::from_secs(8);
const WARMUP: Duration = Duration::from_millis(1000);

/// Daemon thread + cờ stop. Drop đảm bảo dừng + join ngay cả khi assert fail giữa chừng
/// (nhả lock + fs handle trước khi TempDir dọn dẹp).
struct TestDaemon {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<daemon::Result<()>>>,
}

impl TestDaemon {
    fn spawn(store: &Path, ws: &Path) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let (store, ws) = (store.to_path_buf(), ws.to_path_buf());
        let stop2 = stop.clone();
        let handle = thread::spawn(move || daemon::run_with_root(&store, &[ws], stop2));
        Self { stop, handle: Some(handle) }
    }

    /// Set stop + join; trả kết quả run_with_root (lock thả khi hàm đó return).
    fn stop_and_join(mut self) -> daemon::Result<()> {
        self.stop.store(true, Ordering::Relaxed);
        self.handle.take().unwrap().join().unwrap()
    }
}

impl Drop for TestDaemon {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Poll `check` mỗi 200ms tới khi true; quá 8s → panic kèm mô tả.
fn wait_for(desc: &str, mut check: impl FnMut() -> bool) {
    let start = Instant::now();
    loop {
        if check() {
            return;
        }
        assert!(start.elapsed() < TIMEOUT, "timeout chờ: {desc}");
        thread::sleep(POLL);
    }
}

/// Đảm bảo `check` KHÔNG bao giờ true trong suốt `window`.
fn ensure_absent(desc: &str, window: Duration, mut check: impl FnMut() -> bool) {
    let start = Instant::now();
    while start.elapsed() < window {
        assert!(!check(), "{desc}");
        thread::sleep(POLL);
    }
}

/// Canonicalize + strip `\\?\` — khớp cách daemon chuẩn hóa trước khi ghi vào daemon.json.
fn canon_str(p: &Path) -> String {
    let c = fs::canonicalize(p).unwrap();
    let s = c.to_string_lossy().into_owned();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        s
    }
}

#[test]
fn external_change_recorded() {
    let store = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    let engine = LocalHistory::open(store.path()).unwrap();
    let ws_id = engine.register_workspace(ws.path()).unwrap();
    let _daemon = TestDaemon::spawn(store.path(), ws.path());

    // Warm-up: watcher cần một nhịp để sẵn sàng (tránh miss event viết ngay sau spawn).
    thread::sleep(WARMUP);

    let file = ws.path().join("a.txt");
    fs::write(&file, b"v1").unwrap();
    wait_for("a.txt có version đầu", || {
        engine.history(ws_id, &file).is_ok_and(|v| !v.is_empty())
    });
    let first = engine.history(ws_id, &file).unwrap();
    assert_eq!(first[0].kind, EventKind::External, "kind version đầu phải là external");

    // Sửa lại → version thứ 2 (debounce gom các event thô của 1 lần ghi thành 1 version).
    fs::write(&file, b"v2").unwrap();
    wait_for("a.txt có version thứ 2", || {
        engine.history(ws_id, &file).is_ok_and(|v| v.len() >= 2)
    });
    let all = engine.history(ws_id, &file).unwrap();
    assert!(all.iter().all(|v| v.kind == EventKind::External), "tất cả kind phải external");
}

#[test]
fn delete_recorded() {
    let store = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    let engine = LocalHistory::open(store.path()).unwrap();
    let ws_id = engine.register_workspace(ws.path()).unwrap();
    let _daemon = TestDaemon::spawn(store.path(), ws.path());
    thread::sleep(WARMUP);

    let file = ws.path().join("gone.txt");
    fs::write(&file, b"bye").unwrap();
    wait_for("gone.txt có version external", || {
        engine.history(ws_id, &file).is_ok_and(|v| !v.is_empty())
    });

    fs::remove_file(&file).unwrap();
    wait_for("gone.txt có marker delete", || {
        engine
            .history(ws_id, &file)
            .is_ok_and(|v| v.first().is_some_and(|x| x.kind == EventKind::Delete))
    });
}

#[test]
fn ignore_filtered() {
    let store = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    let engine = LocalHistory::open(store.path()).unwrap();
    let ws_id = engine.register_workspace(ws.path()).unwrap();

    // .git/config: tạo TRƯỚC khi spawn + viết lại sau warm-up — cả hai phải bị filter.
    let git_cfg = ws.path().join(".git").join("config");
    fs::create_dir_all(git_cfg.parent().unwrap()).unwrap();
    fs::write(&git_cfg, b"[core]").unwrap();

    let _daemon = TestDaemon::spawn(store.path(), ws.path());
    thread::sleep(WARMUP);
    fs::write(&git_cfg, b"[user]\n\tname = t\n").unwrap();

    // 2s (≫ debounce 400ms) mà vẫn rỗng ⇒ event đã bị loại, không tốn debounce slot.
    ensure_absent(".git/config không được ghi lịch sử", Duration::from_secs(2), || {
        engine.history(ws_id, &git_cfg).is_ok_and(|v| !v.is_empty())
    });

    // Sanity: file thường cùng lúc vẫn được ghi — watcher không chết, filter mới là lý do.
    let plain = ws.path().join("plain.txt");
    fs::write(&plain, b"ok").unwrap();
    wait_for("plain.txt được ghi bình thường", || {
        engine.history(ws_id, &plain).is_ok_and(|v| !v.is_empty())
    });
}

#[test]
fn lock_single_instance() {
    let store = tempfile::tempdir().unwrap();
    let ws_a = tempfile::tempdir().unwrap();
    let ws_b = tempfile::tempdir().unwrap();

    // Daemon #1 giữ lock. daemon.json ghi SAU khi chiếm lock → read_status Some ⇒ lock đã giữ.
    let daemon1 = TestDaemon::spawn(store.path(), ws_a.path());
    wait_for("daemon.json xuất hiện (daemon #1 giữ lock)", || {
        daemon::read_status(store.path()).is_some()
    });

    // Lần 2 trên cùng store (ws khác) → phải Err(Lock).
    let stop2 = Arc::new(AtomicBool::new(false));
    let err = daemon::run_with_root(store.path(), &[ws_b.path().to_path_buf()], stop2).unwrap_err();
    assert!(matches!(err, daemon::DaemonError::Lock), "phải là lỗi lock, nhận: {err:?}");

    // Daemon #1 không bị ảnh hưởng; lock chỉ thả khi nó dừng (guard drop lúc run_with_root trả).
    assert!(daemon::read_status(store.path()).is_some(), "daemon #1 phải vẫn đang chạy");
    daemon1.stop_and_join().unwrap();
    assert!(daemon::read_status(store.path()).is_none(), "daemon #1 dừng phải dọn daemon.json");
}

#[test]
fn status_file() {
    let store = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    let daemon_t = TestDaemon::spawn(store.path(), ws.path());

    wait_for("daemon.json xuất hiện", || daemon::read_status(store.path()).is_some());
    let status = daemon::read_status(store.path()).unwrap();
    assert!(status.pid > 0);
    assert_eq!(status.pid, std::process::id(), "in-process: pid phải là pid của process test");
    let want = canon_str(ws.path()).to_lowercase();
    assert!(
        status.workspaces.iter().any(|w| w.to_lowercase() == want),
        "workspaces {:?} phải chứa ws {}",
        status.workspaces,
        want
    );

    // Dừng → daemon.json bị dọn.
    daemon_t.stop_and_join().unwrap();
    assert!(
        daemon::read_status(store.path()).is_none(),
        "daemon.json phải bị xóa sau khi dừng"
    );
}
