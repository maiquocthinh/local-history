//! In-process daemon integration tests (M2): each test spawns `run_with_root` on its own
//! thread with its own store + workspace in a tempdir → the real store is never touched and
//! tests run safely in parallel.
//!
//! Flakiness guards: a warm-up sleep before writing files (the watcher needs a beat to be
//! ready), every poll loops every 200ms with a generous 8s timeout — slow machines/CI pass.

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

/// Daemon thread + stop flag. Drop guarantees stop + join even when an assert fails midway
/// (releasing the lock + fs handles before TempDir cleanup).
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

    /// Sets stop + joins; returns run_with_root's result (the lock releases when it returns).
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

/// Polls `check` every 200ms until true; past 8s → panics with `desc`.
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

/// Asserts `check` NEVER becomes true throughout `window`.
fn ensure_absent(desc: &str, window: Duration, mut check: impl FnMut() -> bool) {
    let start = Instant::now();
    while start.elapsed() < window {
        assert!(!check(), "{desc}");
        thread::sleep(POLL);
    }
}

/// Canonicalize + strip `\\?\` — matches how the daemon normalizes paths before writing
/// daemon.json.
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

    // Warm-up: the watcher needs a beat to become ready (avoid missing events written right
    // after spawn).
    thread::sleep(WARMUP);

    let file = ws.path().join("a.txt");
    fs::write(&file, b"v1").unwrap();
    wait_for("a.txt có version đầu", || {
        engine.history(ws_id, &file).is_ok_and(|v| !v.is_empty())
    });
    let first = engine.history(ws_id, &file).unwrap();
    assert_eq!(first[0].kind, EventKind::External, "kind version đầu phải là external");

    // Rewrite → 2nd version (the debounce collapses the raw events of one write into one version).
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

    // .git/config: created BEFORE spawn + rewritten after warm-up — both writes must be filtered.
    let git_cfg = ws.path().join(".git").join("config");
    fs::create_dir_all(git_cfg.parent().unwrap()).unwrap();
    fs::write(&git_cfg, b"[core]").unwrap();

    let _daemon = TestDaemon::spawn(store.path(), ws.path());
    thread::sleep(WARMUP);
    fs::write(&git_cfg, b"[user]\n\tname = t\n").unwrap();

    // Still empty after 2s (≫ the 400ms debounce) ⇒ the event was filtered, no debounce slot spent.
    ensure_absent(".git/config không được ghi lịch sử", Duration::from_secs(2), || {
        engine.history(ws_id, &git_cfg).is_ok_and(|v| !v.is_empty())
    });

    // Sanity: a plain file written at the same time is recorded — the watcher is alive, the
    // filter is the reason.
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

    // Daemon #1 holds the lock. daemon.json is written AFTER the lock is taken → read_status
    // Some ⇒ the lock is held.
    let daemon1 = TestDaemon::spawn(store.path(), ws_a.path());
    wait_for("daemon.json xuất hiện (daemon #1 giữ lock)", || {
        daemon::read_status(store.path()).is_some()
    });

    // A second run on the same store (different ws) → must be Err(Lock).
    let stop2 = Arc::new(AtomicBool::new(false));
    let err = daemon::run_with_root(store.path(), &[ws_b.path().to_path_buf()], stop2).unwrap_err();
    assert!(matches!(err, daemon::DaemonError::Lock), "phải là lỗi lock, nhận: {err:?}");

    // Daemon #1 is unaffected; the lock releases only when it stops (guard drops when
    // run_with_root returns).
    assert!(daemon::read_status(store.path()).is_some(), "daemon #1 phải vẫn đang chạy");
    daemon1.stop_and_join().unwrap();
    assert!(daemon::read_status(store.path()).is_none(), "daemon #1 dừng phải dọn daemon.json");
}

#[test]
fn pause_drops_events() {
    // Tray pause (run_with_root_paused): a write made while paused → NOT recorded; resume →
    // recorded again. The watcher must survive both phases.
    let store = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    let engine = LocalHistory::open(store.path()).unwrap();
    let ws_id = engine.register_workspace(ws.path()).unwrap();

    let pause = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));
    let (store_p, ws_p) = (store.path().to_path_buf(), ws.path().to_path_buf());
    let (pause_p, stop_p) = (pause.clone(), stop.clone());
    let handle =
        thread::spawn(move || daemon::run_with_root_paused(&store_p, &[ws_p], stop_p, pause_p));

    // Pause BEFORE the write — a change made while paused must never be recorded.
    pause.store(true, Ordering::Relaxed);
    thread::sleep(WARMUP);
    let file = ws.path().join("paused.txt");
    fs::write(&file, b"x").unwrap();
    // 3s ≫ debounce 400ms — nothing recorded, yet the watcher stays alive.
    ensure_absent("paused → nothing recorded", Duration::from_secs(3), || {
        engine.history(ws_id, &file).is_ok_and(|v| !v.is_empty())
    });

    // Resume → the next change is recorded as usual.
    pause.store(false, Ordering::Relaxed);
    fs::write(&file, b"after resume").unwrap();
    wait_for("resume → recorded again", || {
        engine.history(ws_id, &file).is_ok_and(|v| !v.is_empty())
    });
    let all = engine.history(ws_id, &file).unwrap();
    assert_eq!(
        all.len(),
        1,
        "the write made while paused (1 byte) must be dropped entirely — exactly 1 version left: {all:?}"
    );
    assert_eq!(all[0].size, 12, "recorded version must be the post-resume content (12 bytes)");
    assert_eq!(all[0].kind, EventKind::External);

    stop.store(true, Ordering::Relaxed);
    handle.join().unwrap().unwrap();
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

    // Stop → daemon.json is removed.
    daemon_t.stop_and_join().unwrap();
    assert!(
        daemon::read_status(store.path()).is_none(),
        "daemon.json phải bị xóa sau khi dừng"
    );
}
