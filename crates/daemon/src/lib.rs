//! Daemon `lh-daemon` — watches workspace directories via fs events (notify) and snapshots
//! every file change into the `history` engine.
//!
//! Pipeline (docs/architecture.md §4.2):
//!   fs event → ignore filter (per component) → debounce map (path → instant)
//!   → path quiet ≥ DEBOUNCE_MS → read file → `record(..., External)` (dedup in engine)
//!
//! All logic lives in the lib (not main.rs) so tests can run in-process via `run_with_root`.

use std::collections::HashMap;
use std::fmt;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use notify::Watcher;
use serde::{Deserialize, Serialize};

use history::{EventKind, LocalHistory, RetentionPolicy, WorkspaceId};

// Load translations from crates/daemon/locales (baked in at compile time); a missing locale
// falls back to en. Used by the tray module (`--tray`, see tray.rs) for menu labels — the
// rest of the daemon output stays hardcoded Vietnamese for now.
rust_i18n::i18n!("locales", fallback = "en");

pub mod tray;

/// Auto-generated 32×32 RGBA bytes of the app icon (tools/gen_icon.py) — wired into the
/// tray icon by `tray::icon_rgba`.
mod tray_icon_data;

/// Directories skipped by the ignore filter — matched per path component, case-insensitive.
pub const IGNORE_DIRS: &[&str] =
    &[".git", ".hg", ".svn", "target", "node_modules", "dist", "build", ".zcode"];

/// Minimum quiet period a path must reach before it is processed (debounce).
pub const DEBOUNCE_MS: u64 = 400;

/// Collector wake-up interval: absorb new events / flush paths quiet long enough.
pub const COLLECTOR_INTERVAL_MS: u64 = 100;

/// Interval between periodic GC runs.
const GC_INTERVAL_SECS: u64 = 6 * 3600;

/// Crate-wide result type: every fallible operation returns this.
pub type Result<T> = std::result::Result<T, DaemonError>;

/// Daemon error — a manual enum (this crate has no thiserror).
#[derive(Debug)]
pub enum DaemonError {
    /// Watcher setup or event-stream failure (notify).
    Watcher(String),
    /// `daemon.lock` could not be acquired — another daemon is running on this store.
    Lock,
    /// Underlying I/O failure.
    Io(std::io::Error),
    /// Propagated engine (`history`) error.
    History(history::Error),
    /// Invalid argument or directory.
    Path(String),
    /// Tray mode (`--tray`): event loop / icon / menu failure.
    Tray(String),
}

impl fmt::Display for DaemonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DaemonError::Watcher(s) => write!(f, "watcher: {s}"),
            DaemonError::Lock => {
                write!(f, "daemon khác đang chạy trên store này (không lấy được daemon.lock)")
            }
            DaemonError::Io(e) => write!(f, "io: {e}"),
            DaemonError::History(e) => write!(f, "history: {e}"),
            DaemonError::Path(s) => write!(f, "path: {s}"),
            DaemonError::Tray(s) => write!(f, "tray: {s}"),
        }
    }
}

impl std::error::Error for DaemonError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            DaemonError::Io(e) => Some(e),
            DaemonError::History(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for DaemonError {
    fn from(e: std::io::Error) -> Self {
        DaemonError::Io(e)
    }
}

impl From<history::Error> for DaemonError {
    fn from(e: history::Error) -> Self {
        DaemonError::History(e)
    }
}

/// Daemon state written to `<store_root>/daemon.json` for the CLI/GUI to read (`lh status`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonStatus {
    /// Process id of the running daemon — lets consumers detect a stale status file.
    pub pid: u32,
    /// Normalized watch directories (canonicalized, `\\?\` prefix stripped).
    pub workspaces: Vec<String>,
    /// UTC start time as unix microseconds.
    pub started_at_micros: i64,
}

/// Runs the daemon until `stop` is set to true. Blocks the calling thread.
///
/// `store_root` is the data directory (tests pass a tempdir; production uses the engine's
/// default root, see `run`). `watch_dirs` are the workspace directories to watch (empty → error).
pub fn run_with_root(store_root: &Path, watch_dirs: &[PathBuf], stop: Arc<AtomicBool>) -> Result<()> {
    // No tray here: a fresh pause flag — the pipeline records everything as before.
    run_with_root_paused(store_root, watch_dirs, stop, Arc::new(AtomicBool::new(false)))
}

/// Same as `run_with_root` plus a `pause` flag for tray mode (`--tray`): true → every fs event
/// is dropped (nothing recorded) while the watcher keeps running and `stop` still works.
/// Without tray, call `run_with_root` — behavior there is unchanged.
pub fn run_with_root_paused(
    store_root: &Path,
    watch_dirs: &[PathBuf],
    stop: Arc<AtomicBool>,
    pause: Arc<AtomicBool>,
) -> Result<()> {
    if watch_dirs.is_empty() {
        return Err(DaemonError::Path("không có thư mục nào để theo dõi".into()));
    }

    // 1. Normalize watch dirs (canonicalize + strip `\\?\` — same convention as the engine),
    //    then open the store + register workspaces. Pairs (canonical root → ws_id) attribute
    //    each event back to its workspace.
    let mut dirs = Vec::with_capacity(watch_dirs.len());
    for d in watch_dirs {
        dirs.push(normalize_path(d).map_err(DaemonError::Path)?);
    }
    let engine = Arc::new(LocalHistory::open(store_root)?);
    let mut ws_entries: Vec<(PathBuf, WorkspaceId)> = Vec::with_capacity(dirs.len());
    for dir in &dirs {
        let ws = engine.register_workspace(dir)?;
        ws_entries.push((dir.clone(), ws));
    }

    // 2. Single-instance lock. The guard lives to the end of the function; if the process
    //    dies, the OS releases the lock. (The file stays empty, nothing is written into it
    //    → `truncate(false)` states that intent.)
    let lock_file = File::options()
        .create(true)
        .write(true)
        .truncate(false)
        .open(store_root.join("daemon.lock"))?;
    let mut lock = fd_lock::RwLock::new(lock_file);
    let _lock_guard = lock.try_write().map_err(|_| DaemonError::Lock)?;

    // 3. Status file. Written only AFTER the lock is held → a CLI that sees daemon.json knows
    //    a daemon is actually running.
    let status = DaemonStatus {
        pid: std::process::id(),
        workspaces: dirs.iter().map(|d| d.to_string_lossy().into_owned()).collect(),
        started_at_micros: now_micros(),
    };
    write_status(store_root, &status)?;
    log::info!("daemon start: pid={} store={} ws={:?}", status.pid, store_root.display(), dirs);

    // 4. Watcher — on failure, clean up the status file before returning (no stale daemon.json).
    let (tx, rx) = mpsc::channel();
    let mut watcher = match notify::recommended_watcher(tx) {
        Ok(w) => w,
        Err(e) => return Err(watcher_failed(store_root, DaemonError::Watcher(e.to_string()))),
    };
    for dir in &dirs {
        if let Err(e) = watcher.watch(dir, notify::RecursiveMode::Recursive) {
            return Err(watcher_failed(
                store_root,
                DaemonError::Watcher(format!("{}: {e}", dir.display())),
            ));
        }
    }

    // 5. GC at start + a periodic GC thread (daemon lifetime — no graceful stop needed).
    run_gc(&engine, "lúc start");
    let gc_engine = engine.clone();
    let gc_stop = stop.clone();
    std::thread::Builder::new().name("lh-daemon-gc".into()).spawn(move || {
        while !gc_stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_secs(GC_INTERVAL_SECS));
            if gc_stop.load(Ordering::Relaxed) {
                break;
            }
            run_gc(&gc_engine, "định kỳ");
        }
    })?;

    // 6. Collector loop: new events → debounce map; on each wake, flush paths quiet ≥
    //    DEBOUNCE_MS. Handled inline in this thread (kept simple); every record error is
    //    logged and ignored.
    let mut last_event: HashMap<String, Instant> = HashMap::new();
    let debounce = Duration::from_millis(DEBOUNCE_MS);
    let tick = Duration::from_millis(COLLECTOR_INTERVAL_MS);
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        // Tray pause (read once per tick): dropped events are not debounced at all, and the
        // debounce map is cleared each tick so nothing pending leaks through on resume.
        let paused = pause.load(Ordering::Relaxed);
        match rx.recv_timeout(tick) {
            Ok(Ok(event)) => {
                if !paused {
                    collect_event(&mut last_event, &event, &ws_entries);
                }
            }
            Ok(Err(e)) => log::warn!("watcher lỗi (bỏ qua): {e}"),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break, // watcher dropped
        }
        if paused {
            last_event.clear();
        } else {
            drain_ready(&mut last_event, debounce, &engine, &ws_entries);
        }
    }

    // 7. Clean up the status file (best effort). daemon.lock is left behind — the lock
    //    releases itself when the process dies.
    let _ = std::fs::remove_file(store_root.join("daemon.json"));
    log::info!("daemon dừng sạch");
    Ok(())
}

/// Convenience for main.rs: runs with the engine's default store root (env `LOCAL_HISTORY_DIR`
/// or `%LOCALAPPDATA%\local-history`).
pub fn run(watch_dirs: &[PathBuf], stop: Arc<AtomicBool>) -> Result<()> {
    let root = LocalHistory::default_root()?;
    run_with_root(&root, watch_dirs, stop)
}

/// Reads the daemon status (`<store_root>/daemon.json`). Missing or corrupt file → `None`.
pub fn read_status(store_root: &Path) -> Option<DaemonStatus> {
    let data = std::fs::read_to_string(store_root.join("daemon.json")).ok()?;
    serde_json::from_str(&data).ok()
}

/// true if ANY path component is in `IGNORE_DIRS`. Used by the pre-debounce filter.
pub fn is_ignored(path: &Path) -> bool {
    path.components().any(|c| {
        IGNORE_DIRS.iter().any(|d| c.as_os_str().eq_ignore_ascii_case(*d))
    })
}

// ---------- internals ----------

/// Ignore filter applied RELATIVE to the watched root — avoids false positives when an
/// absolute path above the root happens to contain an `IGNORE_DIRS` name (e.g. running the
/// daemon inside `target/`).
fn is_ignored_under(path: &Path, root: &Path) -> bool {
    match path.strip_prefix(root) {
        Ok(rel) => is_ignored(rel),
        Err(_) => is_ignored(path),
    }
}

/// true if `path` lies INSIDE `dir` (component comparison, case-insensitive on Windows).
fn under_dir(path: &Path, dir: &Path) -> bool {
    let pc: Vec<_> = path.components().collect();
    let dc: Vec<_> = dir.components().collect();
    pc.len() > dc.len()
        && pc[..dc.len()].iter().zip(dc.iter()).all(|(a, b)| {
            if cfg!(windows) {
                a.as_os_str().eq_ignore_ascii_case(b.as_os_str())
            } else {
                a == b
            }
        })
}

/// Canonicalize + strip the Windows `\\?\` prefix (UNC `\\?\UNC\...` → `\\...`).
fn normalize_path(p: &Path) -> std::result::Result<PathBuf, String> {
    let canonical = p.canonicalize().map_err(|e| format!("{}: {e}", p.display()))?;
    let s = canonical.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        Ok(PathBuf::from(format!(r"\\{rest}")))
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        Ok(PathBuf::from(rest))
    } else {
        Ok(canonical)
    }
}

/// Current UTC time as unix microseconds.
fn now_micros() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
}

fn run_gc(engine: &LocalHistory, when: &str) {
    match engine.gc_auto(None, &RetentionPolicy::default()) {
        Ok(r) => log::info!("gc {when}: {r:?}"),
        Err(e) => log::warn!("gc {when} lỗi (bỏ qua): {e}"),
    }
}

/// Watcher setup failed: remove daemon.json (best effort), then return the error.
fn watcher_failed(store_root: &Path, e: DaemonError) -> DaemonError {
    let _ = std::fs::remove_file(store_root.join("daemon.json"));
    e
}

fn write_status(store_root: &Path, status: &DaemonStatus) -> Result<()> {
    let data = serde_json::to_string_pretty(status)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(store_root.join("daemon.json"), data)?;
    Ok(())
}

/// Absorbs one event: only Create/Modify/Remove are kept (Access/Other dropped); the path
/// must pass the ignore filter and belong to a registered watch dir. A new event on the same
/// path resets its debounce clock.
fn collect_event(
    last_event: &mut HashMap<String, Instant>,
    event: &notify::Event,
    ws_entries: &[(PathBuf, WorkspaceId)],
) {
    if !matches!(
        event.kind,
        notify::EventKind::Create(_) | notify::EventKind::Modify(_) | notify::EventKind::Remove(_)
    ) {
        return;
    }
    for path in &event.paths {
        for (root, _) in ws_entries {
            if under_dir(path, root) {
                if !is_ignored_under(path, root) {
                    last_event.insert(path.to_string_lossy().into_owned(), Instant::now());
                }
                break;
            }
        }
    }
}

/// Flushes paths quiet ≥ `debounce` for processing — inline in the collector thread (kept simple).
fn drain_ready(
    last_event: &mut HashMap<String, Instant>,
    debounce: Duration,
    engine: &LocalHistory,
    ws_entries: &[(PathBuf, WorkspaceId)],
) {
    let now = Instant::now();
    let ready: Vec<String> = last_event
        .iter()
        .filter(|(_, t)| now.duration_since(**t) >= debounce)
        .map(|(p, _)| p.clone())
        .collect();
    for path in ready {
        last_event.remove(&path);
        process_one(engine, ws_entries, &path);
    }
}

/// Processes one path after its debounce. NEVER panics — every error is logged and ignored
/// (the daemon must survive half-written / write-locked files).
fn process_one(engine: &LocalHistory, ws_entries: &[(PathBuf, WorkspaceId)], path_str: &str) {
    let path = PathBuf::from(path_str);
    let Some(&ws) = ws_entries
        .iter()
        .find(|(root, _)| under_dir(&path, root))
        .map(|(_, ws)| ws)
    else {
        return;
    };

    if path.exists() {
        if !path.is_file() {
            return; // newly created directory — only files matter
        }
        // Write-locked → read fails → skip; a later event (if the file changed again) records it.
        match std::fs::read(&path) {
            Ok(content) => {
                match engine.record(ws, &path, &content, EventKind::External, None) {
                    Ok(Some(_)) => log::info!("đã record (external): {path_str}"),
                    Ok(None) => {} // deduped in the engine — no redundant version
                    Err(e) => log::warn!("record {path_str} lỗi (bỏ qua): {e}"),
                }
            }
            Err(e) => log::warn!("đọc {path_str} lỗi (bỏ qua): {e}"),
        }
    } else {
        // File vanished: write the delete marker only if the file has a row in the workspace's
        // files table (avoids noise for temp dirs / files never tracked).
        match engine.history(ws, &path) {
            Ok(versions) if !versions.is_empty() => {
                match engine.record(ws, &path, &[], EventKind::Delete, None) {
                    Ok(_) => log::info!("đã ghi marker delete: {path_str}"),
                    Err(e) => log::warn!("record delete {path_str} lỗi (bỏ qua): {e}"),
                }
            }
            Ok(_) => {} // never had history — skip
            Err(e) => log::warn!("history {path_str} lỗi (bỏ qua): {e}"),
        }
    }
}
