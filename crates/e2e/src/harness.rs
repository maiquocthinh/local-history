//! Process-level harness for the `lh-e2e` runner.
//!
//! One [`Harness`] per scenario:
//! - a fresh store + workspace temp directories (`tempfile`),
//! - one real daemon process: `lh daemon --dir <ws...>` with env
//!   `LOCAL_HISTORY_DIR=<store>`, `LH_BATCH_WINDOW_MS=300`,
//!   `LH_BATCH_TIMEOUT_MS=400` (the daemon's tuning hook — keeps batch waves
//!   deterministic in hundreds of milliseconds),
//! - readiness poll: `daemon.json` exists AND `lh status --json` reports
//!   `daemon.alive == true` with every workspace registered (deadline 10s,
//!   200ms interval — no unbounded sleeps),
//! - teardown: `Child::kill` on the daemon, then the temp dirs are removed.
//!
//! A daemon dying in the middle of a scenario turns every subsequent harness
//! call into a clear failure (`assert_daemon_alive`), which is exactly what a
//! dead daemon must be: a scenario FAIL, never a hang.
//!
//! Cross-platform by construction: `std::process::Command`, `PathBuf`,
//! `Child::kill` — the binary is `lh.exe` on Windows and `lh` elsewhere.

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde::de::DeserializeOwned;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tempfile::TempDir;

/// Batch window override passed to every spawned daemon (shrunken so waves are
/// deterministic — the daemon reads `LH_BATCH_WINDOW_MS` before opening the store).
pub const BATCH_WINDOW_MS: &str = "300";
/// Batch timeout override (>= window; the daemon clamps it anyway).
pub const BATCH_TIMEOUT_MS: &str = "400";

/// Daemon readiness deadline (also reused for spawn/respawn in scenarios).
pub const READY_DEADLINE_MS: u64 = 10_000;
/// Poll interval for readiness/history/status waits.
pub const POLL_INTERVAL_MS: u64 = 200;

/// Error prefix used when the harness daemon died mid-scenario — wait helpers
/// treat it as terminal and fail the scenario immediately.
pub const DAEMON_DIED_PREFIX: &str = "daemon died mid-scenario";

/// Every daemon spawned by a scenario is registered here, so the runner can
/// kill the daemons of scenarios that hit the hard deadline (their threads
/// cannot be stopped, but their processes can).
static LIVE_DAEMONS: Mutex<Vec<Arc<Mutex<Option<Child>>>>> = Mutex::new(Vec::new());

/// Context handed to every scenario function.
#[derive(Clone)]
pub struct Ctx {
    /// Path of the `lh` binary under test.
    pub lh: PathBuf,
}

/// Scenario outcome: `Ok` carries a short success note, `Err` the failure reason.
pub type Res = Result<String, String>;

/// One finished `lh` invocation.
#[derive(Debug, Clone)]
pub struct CliOut {
    /// Process exit code (`-1` when terminated by a signal).
    pub code: i32,
    /// Raw stdout (lossy UTF-8).
    pub stdout: String,
    /// Raw stderr (lossy UTF-8).
    pub stderr: String,
}

impl CliOut {
    /// Parse the (trimmed) stdout as JSON.
    pub fn json(&self) -> Result<Value, String> {
        serde_json::from_str(self.stdout.trim()).map_err(|e| {
            format!(
                "stdout is not valid JSON (exit {}): {e} — stdout: {}",
                self.code,
                truncate(&self.stdout, 600)
            )
        })
    }
}

/// Locate the `lh` binary: `--lh PATH` > env `LH_BIN` > auto-detect
/// `target/debug/lh(.exe)` relative to the workspace root (derived from this
/// crate's `CARGO_MANIFEST_DIR` at compile time). Missing binary is a clear
/// error that tells the user how to build it.
pub fn locate_lh(explicit: Option<&str>) -> Result<PathBuf, String> {
    let name = format!("lh{}", std::env::consts::EXE_SUFFIX);
    if let Some(p) = explicit {
        let path = PathBuf::from(p);
        if !path.exists() {
            return Err(format!("--lh {}: file does not exist", path.display()));
        }
        return Ok(path);
    }
    if let Ok(p) = std::env::var("LH_BIN") {
        let path = PathBuf::from(&p);
        if !path.exists() {
            return Err(format!("LH_BIN={p}: file does not exist"));
        }
        return Ok(path);
    }
    // crates/e2e -> crates -> workspace root
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .ok_or_else(|| "cannot derive the workspace root from CARGO_MANIFEST_DIR".to_string())?;
    let candidate = root.join("target").join("debug").join(&name);
    if candidate.exists() {
        return Ok(candidate);
    }
    Err(format!(
        "could not locate the lh binary (tried {}). Build it first: cargo build -p cli -p daemon — or pass --lh PATH / set LH_BIN.",
        candidate.display()
    ))
}

/// Run one `lh` invocation against an optional store and capture all output.
fn run_lh(lh: &Path, store: Option<&Path>, args: &[&str]) -> Result<CliOut, String> {
    let mut cmd = Command::new(lh);
    if let Some(s) = store {
        cmd.arg("--store").arg(s);
    }
    cmd.arg("--lang").arg("en");
    for a in args {
        cmd.arg(a);
    }
    cmd.env("LH_LANG", "en");
    if let Some(s) = store {
        cmd.env("LOCAL_HISTORY_DIR", s);
    }
    let out = cmd
        .output()
        .map_err(|e| format!("failed to spawn {}: {e}", lh.display()))?;
    Ok(CliOut {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

/// Lock a mutex, tolerating poisoning (a panic elsewhere must not cascade).
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Kill a registered daemon (and unregister it from [`LIVE_DAEMONS`]).
fn kill_handle(handle: &Arc<Mutex<Option<Child>>>) {
    lock(&LIVE_DAEMONS).retain(|h| !Arc::ptr_eq(h, handle));
    if let Some(mut child) = lock(handle).take() {
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// Kill every daemon still registered (used after a scenario deadline hit).
pub fn kill_all_live_daemons() {
    let handles: Vec<_> = std::mem::take(&mut *lock(&LIVE_DAEMONS));
    for h in &handles {
        if let Some(mut child) = lock(h).take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Spawn the daemon process with the shrunken batch window and register it.
fn spawn_daemon_process(
    lh: &Path,
    store: &Path,
    ws_roots: &[&Path],
    log: &Path,
) -> Result<Arc<Mutex<Option<Child>>>, String> {
    let out_file = File::create(log).map_err(|e| format!("failed to create {log:?}: {e}"))?;
    let err_file = out_file
        .try_clone()
        .map_err(|e| format!("failed to clone the daemon log handle: {e}"))?;
    let mut cmd = Command::new(lh);
    cmd.arg("daemon");
    for w in ws_roots {
        cmd.arg("--dir").arg(w);
    }
    cmd.env("LOCAL_HISTORY_DIR", store);
    cmd.env("LH_BATCH_WINDOW_MS", BATCH_WINDOW_MS);
    cmd.env("LH_BATCH_TIMEOUT_MS", BATCH_TIMEOUT_MS);
    cmd.env("LH_LANG", "en");
    cmd.stdout(Stdio::from(out_file));
    cmd.stderr(Stdio::from(err_file));
    let child = cmd
        .spawn()
        .map_err(|e| format!("failed to spawn daemon {}: {e}", lh.display()))?;
    let handle = Arc::new(Mutex::new(Some(child)));
    lock(&LIVE_DAEMONS).push(handle.clone());
    Ok(handle)
}

/// Wait until the daemon is ready: `daemon.json` exists AND `lh status --json`
/// reports `daemon.alive == true` with all workspaces registered. A daemon
/// that exits during startup fails immediately with its log tail.
fn wait_ready(
    lh: &Path,
    store: &Path,
    ws_count: usize,
    handle: &Arc<Mutex<Option<Child>>>,
    log: &Path,
) -> Result<(), String> {
    let start = Instant::now();
    loop {
        let mut guard = lock(handle);
        let died = guard.as_mut().and_then(|c| c.try_wait().ok().flatten());
        drop(guard);
        if let Some(status) = died {
            return Err(format!(
                "{DAEMON_DIED_PREFIX} during startup ({status})\n{}",
                log_tail(log)
            ));
        }
        if store.join("daemon.json").exists()
            && let Ok(j) = status_json_direct(lh, store)
        {
            let alive = j
                .get("daemon")
                .and_then(|d| d.get("alive"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let n_ws = j
                .get("workspaces")
                .and_then(Value::as_array)
                .map(|a| a.len())
                .unwrap_or(0);
            if alive && n_ws == ws_count {
                return Ok(());
            }
        }
        if start.elapsed() >= Duration::from_millis(READY_DEADLINE_MS) {
            return Err(format!(
                "daemon not ready within {READY_DEADLINE_MS}ms\n{}",
                log_tail(log)
            ));
        }
        std::thread::sleep(Duration::from_millis(POLL_INTERVAL_MS));
    }
}

/// `lh status --json` without a harness (used by the readiness poll).
fn status_json_direct(lh: &Path, store: &Path) -> Result<Value, String> {
    let out = run_lh(lh, Some(store), &["--json", "status"])?;
    if out.code != 0 {
        return Err(format!("lh status exited {}: {}", out.code, out.stderr.trim()));
    }
    out.json()
}

/// Per-scenario harness: its own store, its own workspace(s), its own daemon.
pub struct Harness {
    /// Path of the `lh` binary under test.
    pub lh: PathBuf,
    /// Store root temp dir (removed on drop — unless `LH_E2E_KEEP=1`).
    pub store: TempDir,
    /// Workspace root temp dirs (removed on drop — unless `LH_E2E_KEEP=1`).
    pub ws: Vec<TempDir>,
    /// stdout+stderr log of the current daemon, inside the store dir.
    pub daemon_log: PathBuf,
    daemon: Option<Arc<Mutex<Option<Child>>>>,
    /// `LH_E2E_KEEP=1`: on drop the temp dirs are NOT deleted (artifact
    /// retention for debugging a failed scenario) and their paths are printed.
    keep: bool,
}

impl Harness {
    /// Create the temp dirs, spawn the daemon over every workspace and wait
    /// for readiness. `workspaces` >= 1.
    pub fn new(lh: &Path, workspaces: usize) -> Result<Harness, String> {
        if workspaces == 0 {
            return Err("Harness::new needs at least one workspace".into());
        }
        let store = TempDir::new().map_err(|e| format!("failed to create store tempdir: {e}"))?;
        let mut ws = Vec::with_capacity(workspaces);
        for i in 0..workspaces {
            ws.push(
                TempDir::new()
                    .map_err(|e| format!("failed to create workspace tempdir {i}: {e}"))?,
            );
        }
        let log = store.path().join("daemon.log");
        let ws_refs: Vec<&Path> = ws.iter().map(|w| w.path()).collect();
        let handle = spawn_daemon_process(lh, store.path(), &ws_refs, &log)?;
        if let Err(e) = wait_ready(lh, store.path(), workspaces, &handle, &log) {
            kill_handle(&handle);
            return Err(e);
        }
        Ok(Harness {
            lh: lh.to_path_buf(),
            store,
            ws,
            daemon_log: log,
            daemon: Some(handle),
            keep: std::env::var("LH_E2E_KEEP").is_ok_and(|v| v == "1"),
        })
    }

    /// Workspace root `i` as an owned `PathBuf`.
    pub fn ws_path(&self, i: usize) -> PathBuf {
        self.ws[i].path().to_path_buf()
    }

    /// Is the harness daemon still running? (No handle -> trivially true.)
    pub fn daemon_alive(&self) -> bool {
        let Some(handle) = &self.daemon else {
            return true;
        };
        let mut guard = lock(handle);
        guard
            .as_mut()
            .and_then(|c| c.try_wait().ok().flatten())
            .is_none()
    }

    /// Fail fast when the harness daemon died mid-scenario.
    pub fn assert_daemon_alive(&self) -> Result<(), String> {
        let Some(handle) = &self.daemon else {
            return Ok(());
        };
        let mut guard = lock(handle);
        let died = guard.as_mut().and_then(|c| c.try_wait().ok().flatten());
        drop(guard);
        match died {
            None => Ok(()),
            Some(status) => Err(format!(
                "{DAEMON_DIED_PREFIX} (exit {status})\n{}",
                log_tail(&self.daemon_log)
            )),
        }
    }

    /// Kill the harness daemon (`Child::kill` + reap). Idempotent.
    pub fn kill_daemon(&mut self) -> Result<(), String> {
        if let Some(handle) = self.daemon.take() {
            kill_handle(&handle);
        }
        Ok(())
    }

    /// Spawn (again) the daemon over the same store/workspaces and wait for
    /// readiness — used by the resilience scenario.
    pub fn spawn_daemon(&mut self) -> Result<(), String> {
        if self.daemon.is_some() {
            self.kill_daemon()?;
        }
        let log = self.store.path().join("daemon.log");
        let ws_refs: Vec<&Path> = self.ws.iter().map(|w| w.path()).collect();
        let handle = spawn_daemon_process(&self.lh, self.store.path(), &ws_refs, &log)?;
        if let Err(e) = wait_ready(&self.lh, self.store.path(), self.ws.len(), &handle, &log) {
            kill_handle(&handle);
            return Err(e);
        }
        self.daemon = Some(handle);
        Ok(())
    }

    /// Run the CLI against this scenario's store (text mode, `--lang en`).
    pub fn cli(&self, args: &[&str]) -> Result<CliOut, String> {
        self.assert_daemon_alive()?;
        run_lh(&self.lh, Some(self.store.path()), args)
    }

    /// Run the CLI with `--json` (plus `--lang en` for stable English text).
    pub fn cli_json(&self, args: &[&str]) -> Result<CliOut, String> {
        let mut full: Vec<&str> = vec!["--json"];
        full.extend_from_slice(args);
        self.cli(&full)
    }

    /// `--json` run that must exit 0; returns the parsed stdout JSON.
    pub fn cli_json_value(&self, args: &[&str]) -> Result<Value, String> {
        let out = self.cli_json(args)?;
        if out.code != 0 {
            return Err(format!(
                "`lh {}` exited {} — stdout: {}",
                args.join(" "),
                out.code,
                truncate(&out.stdout, 600)
            ));
        }
        out.json()
    }

    /// `run_cli_json<T: DeserializeOwned>` — a `--json` run that must exit 0,
    /// parsed into `T`.
    pub fn cli_json_as<T: DeserializeOwned>(&self, args: &[&str]) -> Result<T, String> {
        let out = self.cli_json(args)?;
        if out.code != 0 {
            return Err(format!(
                "`lh {}` exited {} — stdout: {}",
                args.join(" "),
                out.code,
                truncate(&out.stdout, 600)
            ));
        }
        serde_json::from_str(out.stdout.trim())
            .map_err(|e| format!("`lh {}` did not return the expected JSON: {e}", args.join(" ")))
    }

    /// `lh status --json` for this store.
    pub fn status_json(&self) -> Result<Value, String> {
        self.assert_daemon_alive()?;
        self.cli_json_value(&["status"])
    }

    /// `lh history --json` for one file.
    pub fn history_json(&self, file: &Path) -> Result<Value, String> {
        self.assert_daemon_alive()?;
        let p = file.display().to_string();
        self.cli_json_value(&["history", &p])
    }

    /// `lh batches --json` for this store.
    pub fn batches_json(&self) -> Result<Value, String> {
        self.assert_daemon_alive()?;
        self.cli_json_value(&["batches"])
    }

    /// Poll `lh history --json` until `pred` holds. A dead daemon stops the
    /// poll immediately with its reason; otherwise a timeout error carries the
    /// last seen history for diagnosis.
    pub fn wait_history(
        &self,
        file: &Path,
        deadline_ms: u64,
        mut pred: impl FnMut(&Value) -> bool,
    ) -> Result<Value, String> {
        let mut last_json: Option<Value> = None;
        let mut last_err: Option<String> = None;
        let found = poll_until(deadline_ms, POLL_INTERVAL_MS, || {
            match self.history_json(file) {
                Ok(v) => {
                    if pred(&v) {
                        return Some(Ok(v));
                    }
                    last_json = Some(v);
                }
                Err(e) => {
                    if e.starts_with(DAEMON_DIED_PREFIX) {
                        return Some(Err(e));
                    }
                    last_err = Some(e);
                }
            }
            None
        });
        match found {
            Some(Ok(v)) => Ok(v),
            Some(Err(e)) => Err(e),
            None => Err(format!(
                "timeout ({deadline_ms}ms) waiting for history of {} — last error: {:?}, last history: {}",
                file.display(),
                last_err.unwrap_or_default(),
                last_json
                    .map(|v| truncate(&v.to_string(), 500))
                    .unwrap_or_else(|| "(none)".into()),
            )),
        }
    }
}

impl Drop for Harness {
    /// Teardown: kill the daemon, then the fields (store/ws temp dirs) drop —
    /// unless `LH_E2E_KEEP=1`, in which case the dirs are kept on disk for
    /// inspection and their paths printed to stderr.
    fn drop(&mut self) {
        let _ = self.kill_daemon();
        if self.keep {
            let _ = self.store.disable_cleanup(true);
            for w in &mut self.ws {
                let _ = w.disable_cleanup(true);
            }
            eprintln!(
                "LH_E2E_KEEP: store={} ws={:?} daemon_log={}",
                self.store.path().display(),
                self.ws.iter().map(|w| w.path().display().to_string()).collect::<Vec<_>>(),
                self.daemon_log.display()
            );
        }
    }
}

/// Poll `probe` every `interval_ms` until it returns `Some` or the deadline
/// passes. No unbounded sleeps anywhere.
pub fn poll_until<T>(
    deadline_ms: u64,
    interval_ms: u64,
    mut probe: impl FnMut() -> Option<T>,
) -> Option<T> {
    let start = Instant::now();
    loop {
        if let Some(v) = probe() {
            return Some(v);
        }
        if start.elapsed() >= Duration::from_millis(deadline_ms) {
            return None;
        }
        std::thread::sleep(Duration::from_millis(interval_ms));
    }
}

/// Spawn a second daemon on the same store (single-instance lock test) and
/// wait (deadline 10s) for it to exit; returns its exit code. It is NOT
/// registered as a harness daemon — it must terminate by itself.
pub fn spawn_second_daemon(h: &Harness) -> Result<i32, String> {
    let log = h.store.path().join("daemon-second.log");
    let out_file = File::create(&log).map_err(|e| format!("failed to create {log:?}: {e}"))?;
    let err_file = out_file
        .try_clone()
        .map_err(|e| format!("failed to clone the second-daemon log handle: {e}"))?;
    let mut cmd = Command::new(&h.lh);
    let ws = h.ws_path(0);
    cmd.arg("daemon").arg("--dir").arg(ws);
    cmd.env("LOCAL_HISTORY_DIR", h.store.path());
    cmd.env("LH_BATCH_WINDOW_MS", BATCH_WINDOW_MS);
    cmd.env("LH_BATCH_TIMEOUT_MS", BATCH_TIMEOUT_MS);
    cmd.env("LH_LANG", "en");
    cmd.stdout(Stdio::from(out_file));
    cmd.stderr(Stdio::from(err_file));
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to spawn the second daemon: {e}"))?;
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().map_err(|e| format!("try_wait: {e}"))? {
            return Ok(status.code().unwrap_or(-1));
        }
        if start.elapsed() >= Duration::from_secs(10) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "second daemon did not exit within 10s (single-instance lock seems to hang)\n{}",
                log_tail(&log)
            ));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

// ---------- small utilities shared with the scenarios ----------

/// sha256 of a byte slice, lowercase hex (manifest schema of the updater).
pub fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Canonicalize a path and strip the Windows `\\?\` prefix (same convention as
/// the engine/daemon), for string comparison of workspace roots.
pub fn canon_str(p: &Path) -> String {
    let c = p
        .canonicalize()
        .map(|c| c.to_string_lossy().into_owned())
        .unwrap_or_else(|_| p.to_string_lossy().into_owned());
    if let Some(rest) = c.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = c.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        c
    }
}

/// Keep the last `max_chars` characters of a string (diagnostics only).
pub fn truncate(s: &str, max_chars: usize) -> String {
    let n = s.chars().count();
    if n <= max_chars {
        return s.to_string();
    }
    let tail: String = s.chars().skip(n - max_chars).collect();
    format!("[...] {tail}")
}

/// Last part of the daemon log, for failure messages.
pub fn log_tail(log: &Path) -> String {
    match fs::read_to_string(log) {
        Ok(s) => truncate(s.trim(), 1200),
        Err(_) => "(no daemon log)".into(),
    }
}

// ---------- serde_json::Value field readers with clear errors ----------

fn bad_field(v: &Value, key: &str, ty: &str) -> String {
    format!(
        "field `{key}` ({ty}) missing or invalid in {}",
        truncate(&v.to_string(), 400)
    )
}

/// Read a `u64` field.
pub fn v_u64(v: &Value, key: &str) -> Result<u64, String> {
    v.get(key).and_then(Value::as_u64).ok_or_else(|| bad_field(v, key, "u64"))
}

/// Read an `i64` field.
pub fn v_i64(v: &Value, key: &str) -> Result<i64, String> {
    v.get(key).and_then(Value::as_i64).ok_or_else(|| bad_field(v, key, "i64"))
}

/// Read a `bool` field.
pub fn v_bool(v: &Value, key: &str) -> Result<bool, String> {
    v.get(key).and_then(Value::as_bool).ok_or_else(|| bad_field(v, key, "bool"))
}

/// Read a `&str` field.
pub fn v_str<'a>(v: &'a Value, key: &str) -> Result<&'a str, String> {
    v.get(key).and_then(Value::as_str).ok_or_else(|| bad_field(v, key, "str"))
}

/// Read an array field.
pub fn v_arr<'a>(v: &'a Value, key: &str) -> Result<&'a Vec<Value>, String> {
    v.get(key).and_then(Value::as_array).ok_or_else(|| bad_field(v, key, "array"))
}
