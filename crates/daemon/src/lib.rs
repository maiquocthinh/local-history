//! Daemon `lh-daemon` — watches workspace directories via fs events (notify) and snapshots
//! every file change into the `history` engine.
//!
//! Pipeline (docs/architecture.md §4.2):
//!   fs event → ignore filter (per component) → debounce map (path → instant)
//!   → path quiet ≥ DEBOUNCE_MS → read file → `record(..., External)` (dedup in engine)
//!
//! All logic lives in the lib (not main.rs) so tests can run in-process via `run_with_root`;
//! the binary entry point is `run_app` (args come in through its parameter — one source).

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
    // Test/tuning hook: shrink the auto-batch window/timeout via env (the
    // engine keeps its defaults; only the daemon applies the override). A
    // malformed value falls back to the default — it must never stop the
    // daemon from starting.
    let batch_window = env_u64("LH_BATCH_WINDOW_MS", 3_000);
    let batch_timeout = env_u64("LH_BATCH_TIMEOUT_MS", 5_000).max(batch_window);
    engine.set_batch_policy(batch_window, batch_timeout);
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

    // 6. Collector loop: new events → debounce map (renames take the pairing
    //    shortcut); on each wake, flush paths quiet ≥ DEBOUNCE_MS. Handled inline in
    //    this thread (kept simple); every record error is logged and ignored.
    let mut last_event: HashMap<String, Instant> = HashMap::new();
    let mut pending_from: Option<(String, Instant)> = None;
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
                if !paused
                    && let Collected::Rename(from, to) =
                        collect_event(&mut last_event, &mut pending_from, &event, &ws_entries)
                {
                    process_rename(&engine, &ws_entries, &from, &to);
                }
            }
            Ok(Err(e)) => log::warn!("watcher lỗi (bỏ qua): {e}"),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break, // watcher dropped
        }
        if paused {
            last_event.clear();
        } else {
            drain_ready(&mut last_event, &mut pending_from, debounce, &engine, &ws_entries);
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

/// true if ANY path component is in `IGNORE_DIRS`, or the file is an editor
/// temp (`*.tmp` — write-then-rename intermediates are noise, never history).
/// Used by the pre-debounce filter.
pub fn is_ignored(path: &Path) -> bool {
    if path.components().any(|c| {
        IGNORE_DIRS.iter().any(|d| c.as_os_str().eq_ignore_ascii_case(*d))
    }) {
        return true;
    }
    path.extension().map(|e| e.eq_ignore_ascii_case("tmp")).unwrap_or(false)
}

// ---------- binary entry point (`lh-daemon`; main.rs is a thin wrapper around `run_app`) ----------

/// Usage message printed for `--help` and for bad args (kept verbatim from the pre-lib
/// main.rs so the binary's output does not change).
const USAGE: &str =
    "cách dùng: lh-daemon [--tray] [--dir PATH]... (--dir lặp lại được; không có --dir → theo dõi thư mục hiện tại; --tray → chạy trong system tray thay vì console treo)";

/// Minimal stderr logger (no extra dependency) so the lib's logs (gc, warnings, ...) are
/// visible. Uses a static logger — the log crate here is built without the `alloc` feature,
/// so `set_boxed_logger` is unavailable. `run_app` installs it; in-process callers can
/// install their own instance the same way.
pub struct StderrLogger;

static STDERR_LOGGER: StderrLogger = StderrLogger;

impl log::Log for StderrLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::Level::Info
    }

    fn log(&self, record: &log::Record) {
        if self.enabled(record.metadata()) {
            eprintln!("[{}] {}", record.level(), record.args());
        }
    }

    fn flush(&self) {}
}

/// Parses the daemon CLI args and runs until stopped: `--tray` runs the tray (icon + menu on
/// the calling thread, watcher pipeline on a background thread), otherwise the blocking
/// console watcher (`--dir PATH` repeatable; without `--dir` the current directory is
/// watched). Returns the process exit code — 0 ok, 1 runtime error, 2 usage error — the
/// caller decides what to do with it; this function never terminates the process.
///
/// ONE args source (plan §3.1, review feedback #6): everything is parsed from the iterator
/// passed in, the lib never re-reads `env::args()` — safe for in-process callers.
///
/// The iterator holds the daemon args WITHOUT the program name (thin `main.rs` passes
/// `env::args().skip(1)`, the CLI `daemon` arm passes the raw clap args). They are
/// replayed verbatim by the tray self-update respawn (plan §3.4); restoring the `daemon`
/// subcommand for the multi-call `lh` image is `respawn_command`'s job, not the caller's.
pub fn run_app(args: impl Iterator<Item = String>) -> i32 {
    let _ = log::set_logger(&STDERR_LOGGER);
    log::set_max_level(log::LevelFilter::Info);

    let argv: Vec<String> = args.collect();
    // The daemon args AS PASSED IN — replayed verbatim to the respawned executable after
    // a tray-driven self-update (plan §3.4). Every caller already strips the program name
    // (thin `main.rs` passes `env::args().skip(1)`, the CLI `daemon` arm passes the raw
    // clap args), so nothing is cut here: slicing off a leading element would drop the
    // first real flag (`--tray`/`--dir`) from the respawn. ONE args source (review note
    // 6): taken from the iterator passed in, the lib never re-reads `env::args()`.
    let launch_args: Vec<String> = argv.clone();

    let mut watch_dirs: Vec<PathBuf> = Vec::new();
    let mut tray_mode = false;
    let mut it = argv.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--dir" => match it.next() {
                Some(p) => watch_dirs.push(PathBuf::from(p)),
                None => {
                    eprintln!("lh-daemon: --dir cần một PATH\n{USAGE}");
                    return 2;
                }
            },
            // Tray mode: same watcher pipeline, but no blocking console — icon + menu instead.
            "--tray" => tray_mode = true,
            "--help" | "-h" => {
                println!("{USAGE}");
                return 0;
            }
            other => {
                eprintln!("lh-daemon: không hiểu argument `{other}`\n{USAGE}");
                return 2;
            }
        }
    }
    if watch_dirs.is_empty() {
        match std::env::current_dir() {
            Ok(d) => watch_dirs.push(d),
            Err(e) => {
                eprintln!("lh-daemon: không lấy được thư mục hiện tại: {e}");
                return 2;
            }
        }
    }

    let store_root = match history::LocalHistory::default_root() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("lh-daemon: {e}");
            return 1;
        }
    };

    println!("lh-daemon {} (store root: {})", env!("CARGO_PKG_VERSION"), store_root.display());
    for d in &watch_dirs {
        println!("  watch: {}", d.display());
    }

    if tray_mode {
        // Tray mode: watcher pipeline on a background thread; the tray (icon + menu) runs on
        // the main thread — Windows requires the icon on the thread owning the event loop.
        let stop = Arc::new(AtomicBool::new(false));
        let pause = Arc::new(AtomicBool::new(false));
        let restart = Arc::new(AtomicBool::new(false));
        let update_busy = Arc::new(AtomicBool::new(false));
        let watch_stop = stop.clone();
        let watch_pause = pause.clone();
        let dirs = watch_dirs.clone();
        let store = store_root.clone();
        let spawned = std::thread::Builder::new().name("lh-daemon-watch".into()).spawn(move || {
            run_with_root_paused(&store, &dirs, watch_stop, watch_pause)
        });
        let watcher = match spawned {
            Ok(h) => h,
            Err(e) => {
                eprintln!("lh-daemon: không tạo được thread watcher: {e}");
                return 1;
            }
        };
        println!("chạy trong system tray — chuột phải icon để mở menu, Thoát để dừng.");
        match tray::run_tray(
            tray::TrayConfig {
                store_root: store_root.clone(),
                stop,
                pause,
                restart: restart.clone(),
                update_busy,
            },
            watcher,
        ) {
            Ok(()) => {
                // The tray update menu item succeeded: the watcher is already joined here
                // (store lock released, daemon.json cleaned) — respawn the updated
                // executable detached, replaying the original launch args (plan §3.4).
                if restart.load(Ordering::Relaxed) {
                    println!("cập nhật xong — khởi động lại daemon...");
                    return respawn_self(&launch_args);
                }
                println!("đã dừng.");
            }
            Err(e) => {
                eprintln!("lh-daemon: {e}");
                return 1;
            }
        }
        return 0;
    }

    // Ctrl+C: the OS default terminate — the lock releases when the process dies; a stale
    // daemon.json is left behind and later CLI runs check the pid (not handled here).
    println!("Ctrl+C (hoặc kill tiến trình) để dừng.");

    let stop = Arc::new(AtomicBool::new(false));
    if let Err(e) = run(&watch_dirs, stop) {
        eprintln!("lh-daemon: {e}");
        return 1;
    }
    println!("đã dừng.");
    0
}

/// Respawn the current executable detached with `args` — the restart half of the tray
/// self-update (plan §3.4). Called only after `run_tray` returned, i.e. the watcher was
/// joined first: `daemon.lock` is released and `daemon.json` cleaned, so the fresh
/// process can take over the store. Returns the process exit code: 0 spawned, 1 failure
/// (message already printed).
fn respawn_self(args: &[String]) -> i32 {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("lh-daemon: không lấy được current_exe để khởi động lại: {e}");
            return 1;
        }
    };
    let mut cmd = respawn_command(&exe, args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    match cmd.spawn() {
        Ok(child) => {
            println!("đã khởi động lại daemon (pid {})", child.id());
            0
        }
        Err(e) => {
            eprintln!("lh-daemon: spawn {} lỗi khi khởi động lại: {e}", exe.display());
            1
        }
    }
}

/// The respawn `Command` for `exe` + the daemon `args` as passed to `run_app` — pure, so
/// the command line shape is unit-testable without spawning anything. When the running
/// image is the multi-call `lh` CLI the `daemon` subcommand must be replayed, otherwise
/// the child would come up as the GUI (`lh --tray ...`) or die on a clap usage error
/// (`lh --dir ws`); the standalone `lh-daemon` binary takes the args as-is.
fn respawn_command(exe: &Path, args: &[String]) -> std::process::Command {
    let mut cmd = std::process::Command::new(exe);
    if is_multi_call_cli(exe) {
        cmd.arg("daemon");
    }
    cmd.args(args);
    cmd
}

/// True when `exe` is the multi-call `lh` CLI image (the respawn must replay the
/// `daemon` subcommand), false for the standalone `lh-daemon` binary and anything else.
/// Matched on the file stem, case-insensitively (Windows filenames are case-insensitive).
fn is_multi_call_cli(exe: &Path) -> bool {
    exe.file_stem().is_some_and(|s| s.eq_ignore_ascii_case("lh"))
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

/// u64 from an env var, or `default` when unset/unparsable — the e2e/tuning
/// hook for the batch policy; a malformed value never stops the daemon.
fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

/// Outcome of absorbing one watcher event.
enum Collected {
    /// Nothing to do (irrelevant kind, ignored path, or just a debounce clock reset).
    None,
    /// A paired rename inside one workspace — process immediately via `engine::rename`,
    /// bypassing the debounce (the per-path route would record a bogus delete + create).
    Rename(String, String),
}

/// true if the path belongs to a registered watch dir AND passes its ignore filter.
fn trackable(path: &Path, ws_entries: &[(PathBuf, WorkspaceId)]) -> bool {
    ws_entries
        .iter()
        .any(|(root, _)| under_dir(path, root) && !is_ignored_under(path, root))
}

/// Absorbs one event. Only Create/Modify/Remove are kept (Access/Other dropped); each path
/// must pass the ignore filter and belong to a registered watch dir. A new event on the same
/// path resets its debounce clock.
///
/// Renames get the pairing shortcut instead of the debounce: notify delivers either one
/// `Modify(Name(Both))` event carrying `[from, to]` (Windows) or separate
/// `Modify(Name(From))` + `Modify(Name(To))` events (Linux inotify). `From` stashes the old
/// path in `pending_from`; the matching `To` returns [`Collected::Rename`]. A `To` without a
/// stash is a plain appearance (moved in from outside) and goes through the debounce.
fn collect_event(
    last_event: &mut HashMap<String, Instant>,
    pending_from: &mut Option<(String, Instant)>,
    event: &notify::Event,
    ws_entries: &[(PathBuf, WorkspaceId)],
) -> Collected {
    if !matches!(
        event.kind,
        notify::EventKind::Create(_) | notify::EventKind::Modify(_) | notify::EventKind::Remove(_)
    ) {
        return Collected::None;
    }

    // One-event rename (Both), or the From leg of a two-event rename.
    if let notify::EventKind::Modify(notify::event::ModifyKind::Name(mode)) = &event.kind {
        match mode {
            notify::event::RenameMode::Both if event.paths.len() == 2 => {
                let (from, to) = (&event.paths[0], &event.paths[1]);
                if trackable(from, ws_entries) && trackable(to, ws_entries) {
                    last_event.remove(&from.to_string_lossy().into_owned());
                    last_event.remove(&to.to_string_lossy().into_owned());
                    *pending_from = None;
                    return Collected::Rename(
                        from.to_string_lossy().into_owned(),
                        to.to_string_lossy().into_owned(),
                    );
                }
                // Either leg untrackable → fall through to the plain debounce per path
                // (records the create / delete-marker side of a cross-boundary move).
            }
            notify::event::RenameMode::From => {
                let from = event.paths.first();
                match from.filter(|p| trackable(p, ws_entries)) {
                    Some(p) => {
                        *pending_from =
                            Some((p.to_string_lossy().into_owned(), Instant::now()));
                    }
                    None => *pending_from = None, // moved OUT of tracked space — drop any stash
                }
                return Collected::None;
            }
            _ => {} // RenameMode::To / Any → handled below or via the plain debounce
        }
    }

    // To leg of a two-event rename: pair with the stashed From if there is one.
    if matches!(
        event.kind,
        notify::EventKind::Modify(notify::event::ModifyKind::Name(notify::event::RenameMode::To))
    ) {
        let to = event
            .paths
            .iter()
            .find(|p| trackable(p, ws_entries))
            .map(|p| p.to_string_lossy().into_owned());
        if let Some(to) = to {
            if let Some((from, _)) = pending_from.take() {
                last_event.remove(&from);
                last_event.remove(&to);
                return Collected::Rename(from, to);
            }
            // Unpaired To = the file appeared (moved in from outside) → plain debounce.
            last_event.insert(to, Instant::now());
        }
        return Collected::None;
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
    Collected::None
}

/// Flushes paths quiet ≥ `debounce` for processing — inline in the collector thread (kept simple).
/// An unpaired `From` stash (the rename never got its `To`) expires after [`PAIR_TIMEOUT`]
/// and is processed as a vanished file — the move-out-of-workspace case.
fn drain_ready(
    last_event: &mut HashMap<String, Instant>,
    pending_from: &mut Option<(String, Instant)>,
    debounce: Duration,
    engine: &LocalHistory,
    ws_entries: &[(PathBuf, WorkspaceId)],
) {
    if let Some((from, at)) = pending_from
        && at.elapsed() >= PAIR_TIMEOUT
    {
        let from = from.clone();
        *pending_from = None;
        process_one(engine, ws_entries, &from); // gone → delete marker via process_one
    }
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

/// Window for the `From`→`To` pairing. inotify/ReadDirectoryChangesW emit the two legs
/// back-to-back; anything slower is treated as a plain delete + create.
const PAIR_TIMEOUT: Duration = Duration::from_secs(2);

/// Processes a paired rename: the whole version chain moves to the new path via
/// `engine::rename` (one version, kind=rename, label=old path). Moves across watch roots
/// fall back to the plain per-path handling; on engine failure both legs are processed the
/// plain way so nothing is silently lost. NEVER panics — same contract as `process_one`.
fn process_rename(engine: &LocalHistory, ws_entries: &[(PathBuf, WorkspaceId)], from: &str, to: &str) {
    let from_path = PathBuf::from(from);
    let to_path = PathBuf::from(to);
    let ws_of = |p: &Path| {
        ws_entries
            .iter()
            .find(|(root, _)| under_dir(p, root))
            .map(|(_, ws)| ws)
    };
    match (ws_of(&from_path), ws_of(&to_path)) {
        (Some(&ws), Some(&w2)) if ws == w2 => match engine.rename(ws, &from_path, &to_path) {
            Ok(Some(_)) => log::info!("đã record (rename): {from} → {to}"),
            Ok(None) => {} // engine deduped / no-op
            Err(e) => {
                log::warn!("rename {from} → {to} lỗi ({e}) — fallback ghi từng path");
                process_one(engine, ws_entries, from);
                process_one(engine, ws_entries, to);
            }
        },
        _ => {
            // across watch roots (or one leg unresolvable): plain delete + create semantics
            process_one(engine, ws_entries, from);
            process_one(engine, ws_entries, to);
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd_args(cmd: &std::process::Command) -> Vec<String> {
        cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect()
    }

    #[test]
    fn respawn_under_lh_restores_daemon_subcommand() {
        let cmd = respawn_command(
            Path::new(r"C:\bin\lh.exe"),
            &["--tray".into(), "--dir".into(), r"D:\ws".into()],
        );
        assert_eq!(cmd.get_program(), r"C:\bin\lh.exe");
        assert_eq!(cmd_args(&cmd), vec!["daemon", "--tray", "--dir", r"D:\ws"]);
    }

    #[test]
    fn is_ignored_skips_editor_temp_files_but_keeps_real_ones() {
        assert!(is_ignored(Path::new("ws/svc1.ini.tmp")));
        assert!(is_ignored(Path::new("ws/notes.TXT.tmp")));
        assert!(is_ignored(Path::new("ws/.git/config")));
        assert!(is_ignored(Path::new("ws/target/debug/lh.exe")));
        assert!(!is_ignored(Path::new("ws/src/main.rs")));
        assert!(!is_ignored(Path::new("ws/svc1.ini")));
    }

    #[test]
    fn respawn_standalone_daemon_keeps_args_asis() {
        let cmd = respawn_command(
            Path::new(r"C:\bin\lh-daemon.exe"),
            &["--tray".into(), "--dir".into(), r"D:\ws".into()],
        );
        assert_eq!(cmd.get_program(), r"C:\bin\lh-daemon.exe");
        assert_eq!(cmd_args(&cmd), vec!["--tray", "--dir", r"D:\ws"]);
    }

    #[test]
    fn multi_call_detection_is_case_insensitive_cross_platform() {
        assert!(is_multi_call_cli(Path::new(r"C:\bin\lh.exe")));
        assert!(is_multi_call_cli(Path::new(r"C:\bin\LH.EXE")));
        assert!(is_multi_call_cli(Path::new("/usr/local/bin/lh")));
        let cmd = respawn_command(Path::new(r"C:\bin\lh.exe"), &[]);
        assert_eq!(cmd_args(&cmd), vec!["daemon"]);
    }

    #[test]
    fn other_images_are_not_the_multi_call_cli() {
        assert!(!is_multi_call_cli(Path::new(r"C:\bin\lh-daemon.exe")));
        assert!(!is_multi_call_cli(Path::new(r"C:\bin\lh-gui.exe")));
        assert!(!is_multi_call_cli(Path::new(r"C:\bin\mytool.exe")));
        assert!(!is_multi_call_cli(Path::new("lh-daemon")));
    }
}
