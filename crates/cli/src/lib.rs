//! CLI `lh` — command-line interface for local-history (M3).
//!
//! All logic lives in the lib (not just main.rs) so tests run in-process — the same pattern
//! as the `daemon` crate. One `cmd_*` function per subcommand; helpers (tables, time, size,
//! workspace resolution) have their own unit tests.
//!
//! Output conventions:
//! - tables/text go through the rust-i18n layer (crates/cli/locales/en.yml + vi.yml); the
//!   locale is chosen by `--lang` (en|vi) > env `LH_LANG` > OS locale (starting with "vi"
//!   → vi, otherwise en);
//! - `--json`: serde_json objects/arrays with English keys on stdout (error values are
//!   still rendered in the chosen locale);
//! - errors: stderr + exit 1 (with `--json` → `{"error": "..."}` on stdout for machines);
//!   diffing binary content → exit 2 (scripts distinguish "has history but binary").
//!
//! No extra deps beyond i18n: time/size/table formatting is hand-rolled (the `time`/`chrono`
//! engines are not deps of this crate).

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use clap::{Parser, Subcommand};
use rust_i18n::t;
use serde_json::json;

// Load translations from crates/cli/locales (embedded at compile time); missing keys fall back to en.
rust_i18n::i18n!("locales", fallback = "en");

use history::{EventKind, LocalHistory, RetentionPolicy, Version, WorkspaceId};

/// Result type shared by all fallible helpers in this crate: every failure becomes a `CliError`.
pub type Result<T> = std::result::Result<T, CliError>;

// ---------- errors ----------

/// CLI error — printed directly to the user; `exit_code` decides the process exit code.
#[derive(Debug)]
pub enum CliError {
    /// Propagated from the `history` engine.
    History(history::Error),
    /// Content that cannot be diffed because it is binary — exit code 2.
    Binary(String),
    /// CLI-level failure carrying an already user-ready message.
    Msg(String),
}

impl CliError {
    /// Process exit code: 2 for binary-content failures, 1 for every other error.
    pub fn exit_code(&self) -> i32 {
        match self {
            CliError::Binary(_) => 2,
            CliError::History(history::Error::BinaryContent(_)) => 2,
            _ => 1,
        }
    }
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CliError::History(e) => write!(f, "{e}"),
            CliError::Binary(s) => write!(f, "{s}"),
            CliError::Msg(s) => write!(f, "{s}"),
        }
    }
}

impl std::error::Error for CliError {}

impl From<history::Error> for CliError {
    fn from(e: history::Error) -> Self {
        CliError::History(e)
    }
}

impl From<std::io::Error> for CliError {
    fn from(e: std::io::Error) -> Self {
        CliError::Msg(t!("err.io", e = e).to_string())
    }
}

// ---------- command definitions (clap derive) ----------

/// Root command of `lh`: global flags plus the subcommand to run.
#[derive(Debug, Parser)]
#[command(
    name = "lh",
    version,
    about = "local-history — lịch sử thay đổi file độc lập VCS",
    subcommand_required = true,
    arg_required_else_help = true
)]
pub struct Cli {
    /// Emit JSON instead of tables (English keys, written to stdout)
    #[arg(long, global = true)]
    pub json: bool,

    /// Override the store root (default: env LOCAL_HISTORY_DIR or %LOCALAPPDATA%\local-history)
    #[arg(long, global = true, value_name = "PATH")]
    pub store: Option<PathBuf>,

    /// Output language: en | vi (default: env LH_LANG, or the OS locale)
    #[arg(long, global = true, value_name = "LANG")]
    pub lang: Option<String>,

    #[command(subcommand)]
    pub command: Command,
}

/// One variant per `lh` subcommand.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Daemon status plus a workspace overview
    Status,
    /// Timeline of one file
    History {
        /// File path (absolute or relative)
        file: PathBuf,
    },
    /// Print a version's content to stdout (or write it to a file with -o)
    Show {
        file: PathBuf,
        /// Version seq, or `latest` for the newest entry
        #[arg(long)]
        at: String,
        /// Write to a file instead of stdout
        #[arg(short = 'o', long = "output", value_name = "FILE")]
        output: Option<PathBuf>,
    },
    /// Unified diff between two versions (`b` = seq, or `current` = on-disk content)
    Diff {
        file: PathBuf,
        /// Seq of the left side
        a: String,
        /// Seq of the right side, or `current`
        b: String,
    },
    /// Restore a file to a version's content (always records pre_revert before overwriting)
    Revert {
        file: PathBuf,
        /// Seq of the version to restore
        #[arg(long)]
        at: String,
        /// Skip the confirmation prompt
        #[arg(long)]
        yes: bool,
    },
    /// Attach a semantic label (creates a kind=label version from current on-disk content)
    Label {
        file: PathBuf,
        /// Label text (multiple words allowed; joined with spaces)
        #[arg(required = true)]
        text: Vec<String>,
    },
    /// Prune old versions per the retention policy (default: 5 working days, 50 versions/file)
    Gc {
        /// Preview only; delete nothing
        #[arg(long)]
        dry_run: bool,
    },
    /// Workspaces registered in the store
    Workspaces,
    /// Launch the GUI (lh-gui) detached — returns immediately
    Ui {
        /// Open the timeline for this file (optional)
        file: Option<PathBuf>,
    },
}

/// Main entry point — returns the exit code (main.rs only calls `std::process::exit` with it).
pub fn run(cli: Cli) -> i32 {
    // set_locale first — every string below goes through the i18n layer in this locale.
    // An invalid `--lang` still sets the locale first so the error is rendered in the
    // user's language, then fails with a clear message (exit 1).
    rust_i18n::set_locale(&lang_choice(cli.lang.as_deref()));

    let result = match parse_lang(cli.lang.as_deref().unwrap_or("en")) {
        Ok(_) => dispatch(&cli),
        Err(e) => Err(e),
    };
    report(cli.json, result)
}

/// Report the outcome: ok → exit 0; on error, JSON mode prints `{"error": ...}` to stdout
/// (machine-readable on the same channel), text mode prints `lh: <error>` to stderr; the
/// exit code comes from `CliError::exit_code`.
fn report(json_mode: bool, result: Result<()>) -> i32 {
    match result {
        Ok(()) => 0,
        Err(e) => {
            if json_mode {
                println!("{}", json!({ "error": e.to_string() }));
            } else {
                eprintln!("lh: {e}");
            }
            e.exit_code()
        }
    }
}

/// Locale for this run: valid `--lang` > env `LH_LANG` > OS locale.
/// An invalid `--lang` falls back to auto (the error is reported after the locale is set,
/// see `run`).
fn lang_choice(flag: Option<&str>) -> String {
    flag.and_then(normalize_lang).unwrap_or_else(auto_lang)
}

/// Normalize to "en"|"vi" (trimmed, case-insensitive); anything else → None.
fn normalize_lang(s: &str) -> Option<String> {
    match s.trim().to_ascii_lowercase().as_str() {
        "en" => Some("en".into()),
        "vi" => Some("vi".into()),
        _ => None,
    }
}

/// Parse a `--lang`/`LH_LANG` value into "en"|"vi"; invalid → clear `err.bad_lang` error
/// (message rendered in the current locale).
fn parse_lang(s: &str) -> Result<String> {
    normalize_lang(s).ok_or_else(|| CliError::Msg(t!("err.bad_lang", v = s).to_string()))
}

/// Auto locale: env `LH_LANG` if it is en|vi, otherwise the OS locale (`sys_locale`
/// starting with "vi" → vi, otherwise en).
fn auto_lang() -> String {
    if let Ok(v) = std::env::var("LH_LANG")
        && let Some(l) = normalize_lang(&v)
    {
        return l;
    }
    match sys_locale::get_locale() {
        Some(l) if l.to_ascii_lowercase().starts_with("vi") => "vi".into(),
        _ => "en".into(),
    }
}

fn dispatch(cli: &Cli) -> Result<()> {
    match &cli.command {
        Command::Status => cmd_status(cli),
        Command::History { file } => cmd_history(cli, file),
        Command::Show { file, at, output } => cmd_show(cli, file, at, output.as_deref()),
        Command::Diff { file, a, b } => cmd_diff(cli, file, a, b),
        Command::Revert { file, at, yes } => cmd_revert(cli, file, at, *yes),
        Command::Label { file, text } => cmd_label(cli, file, text),
        Command::Gc { dry_run } => cmd_gc(cli, *dry_run),
        Command::Workspaces => cmd_workspaces(cli),
        Command::Ui { file } => cmd_ui(cli, file.as_deref()),
    }
}

// ---------- store helpers ----------

/// Store root: `--store` override, otherwise the engine default (env `LOCAL_HISTORY_DIR`
/// or `%LOCALAPPDATA%\local-history`).
fn store_root(cli: &Cli) -> Result<PathBuf> {
    match &cli.store {
        Some(p) => Ok(p.clone()),
        None => Ok(LocalHistory::default_root()?),
    }
}

/// Open the store READ-ONLY. Missing index.db → clear error (read commands never create a store).
fn open_read(root: &Path) -> Result<LocalHistory> {
    if !root.join("index.db").exists() {
        return Err(CliError::Msg(
            t!("err.store_empty", path = root.display()).to_string(),
        ));
    }
    Ok(LocalHistory::open_read_only(root)?)
}

// ---------- commands ----------

/// Per-workspace overview: (workspace id, root, file count, total version count).
type WsOverview = Vec<(WorkspaceId, PathBuf, usize, i64)>;

fn ws_overview(store: &LocalHistory) -> Result<WsOverview> {
    let mut out = Vec::new();
    for (ws, root) in store.workspaces()? {
        let files = store.list_files(ws, None)?;
        let versions: i64 = files.iter().map(|f| f.version_count).sum();
        out.push((ws, root, files.len(), versions));
    }
    Ok(out)
}

fn ws_json(w: &(WorkspaceId, PathBuf, usize, i64)) -> serde_json::Value {
    json!({
        "id": w.0,
        "root": w.1.display().to_string(),
        "files": w.2,
        "versions": w.3,
    })
}

fn ws_rows(overview: &WsOverview) -> Vec<Vec<String>> {
    overview
        .iter()
        .map(|(id, root, files, versions)| {
            vec![
                id.to_string(),
                root.display().to_string(),
                files.to_string(),
                versions.to_string(),
            ]
        })
        .collect()
}

/// Workspace table header (shared by `lh status` and `lh workspaces`) — localized.
fn ws_header() -> Vec<String> {
    vec![
        t!("ws.col.id").to_string(),
        t!("ws.col.root").to_string(),
        t!("ws.col.files").to_string(),
        t!("ws.col.versions").to_string(),
    ]
}

fn cmd_status(cli: &Cli) -> Result<()> {
    let root = store_root(cli)?;
    let daemon_status = daemon::read_status(&root);

    // No db yet (nothing recorded) → treat as empty instead of failing.
    let overview = if root.join("index.db").exists() {
        let store = open_read(&root)?;
        ws_overview(&store)?
    } else {
        Vec::new()
    };

    if cli.json {
        let daemon_obj = daemon_status.map(|d| {
            json!({
                "pid": d.pid,
                "alive": pid_alive(d.pid),
                "started_at": format_time(d.started_at_micros),
                "watching": d.workspaces,
            })
        });
        let ws_arr: Vec<_> = overview.iter().map(ws_json).collect();
        println!(
            "{}",
            json!({
                "store_root": root.display().to_string(),
                "daemon": daemon_obj,
                "workspaces": ws_arr,
                "last_gc": null,
            })
        );
        return Ok(());
    }

    println!("{}", t!("status.store_root", root = root.display()));
    match daemon_status {
        Some(d) => {
            let alive = pid_alive(d.pid);
            let state = if alive {
                t!("status.daemon_running").to_string()
            } else {
                t!("status.daemon_no_reply").to_string()
            };
            println!(
                "{}",
                t!(
                    "status.daemon_line",
                    state = state,
                    pid = d.pid,
                    time = format_time(d.started_at_micros)
                )
            );
            for w in &d.workspaces {
                println!("{}", t!("status.daemon_watch", ws = w));
            }
        }
        None => println!("{}", t!("status.daemon_stopped")),
    }
    if overview.is_empty() {
        println!("{}", t!("status.workspaces_none"));
    } else {
        println!("{}", t!("status.workspaces_label"));
        print_table(&ws_header(), &ws_rows(&overview));
    }
    println!("{}", t!("status.last_gc"));
    println!("{}", t!("status.tip"));
    Ok(())
}

fn version_json(v: &Version) -> serde_json::Value {
    json!({
        "id": v.id,
        "seq": v.seq,
        "time": format_time(v.timestamp_micros),
        "timestamp_micros": v.timestamp_micros,
        "kind": v.kind.as_str(),
        "label": v.label,
        "storage": v.storage.as_str(),
        "base_seq": v.base_seq,
        "size": v.size,
        "hash": v.content_hash,
    })
}

fn cmd_history(cli: &Cli, file: &Path) -> Result<()> {
    let root = store_root(cli)?;
    let store = open_read(&root)?;
    let (ws, ws_root) = resolve_ws_for_file(&store, file)?;
    let versions = store.history(ws, file)?;

    if cli.json {
        let arr: Vec<_> = versions.iter().map(version_json).collect();
        println!(
            "{}",
            json!({
                "file": file.display().to_string(),
                "workspace": ws_root.display().to_string(),
                "count": versions.len(),
                "versions": arr,
            })
        );
        return Ok(());
    }

    if versions.is_empty() {
        println!(
            "{}",
            t!("history.empty", file = file.display(), ws = ws_root.display())
        );
        return Ok(());
    }
    println!(
        "{}",
        t!("history.header", file = file.display(), ws = ws_root.display())
    );
    let rows: Vec<Vec<String>> = versions
        .iter()
        .map(|v| {
            vec![
                v.seq.to_string(),
                format_time(v.timestamp_micros),
                v.kind.as_str().to_string(),
                v.storage.as_str().to_string(),
                human_size(v.size),
                v.label.clone().unwrap_or_else(|| "-".into()),
            ]
        })
        .collect();
    print_table(
        &[
            t!("history.col.seq").to_string(),
            t!("history.col.time").to_string(),
            t!("history.col.kind").to_string(),
            t!("history.col.storage").to_string(),
            t!("history.col.size").to_string(),
            t!("history.col.label").to_string(),
        ],
        &rows,
    );
    Ok(())
}

fn cmd_show(cli: &Cli, file: &Path, at: &str, output: Option<&Path>) -> Result<()> {
    let root = store_root(cli)?;
    let store = open_read(&root)?;
    let (ws, _) = resolve_ws_for_file(&store, file)?;
    let versions = store.history(ws, file)?;
    let v = pick_version(&versions, at)?;
    let data = store.content(v.id)?;

    match output {
        Some(out) => {
            std::fs::write(out, &data)?;
            eprintln!(
                "{}",
                t!(
                    "show.wrote",
                    seq = v.seq,
                    bytes = data.len(),
                    path = out.display()
                )
            );
        }
        None => {
            // raw bytes to stdout (binary-safe) — write via stdout.lock()
            let stdout = std::io::stdout();
            let mut lock = stdout.lock();
            lock.write_all(&data)?;
            lock.flush()?;
        }
    }
    Ok(())
}

/// Map `BinaryContent` to `CliError::Binary` (clear message + exit 2); other errors pass through.
fn map_binary(r: history::Result<String>) -> Result<String> {
    match r {
        Ok(text) => Ok(text),
        Err(history::Error::BinaryContent(id)) => Err(CliError::Binary(
            t!("err.binary_content", id = id).to_string(),
        )),
        Err(e) => Err(e.into()),
    }
}

fn cmd_diff(cli: &Cli, file: &Path, a: &str, b: &str) -> Result<()> {
    let root = store_root(cli)?;
    let store = open_read(&root)?;
    let (ws, _) = resolve_ws_for_file(&store, file)?;
    let versions = store.history(ws, file)?;
    let va = pick_by_seq(&versions, a)?;

    let text = if b.eq_ignore_ascii_case("current") {
        let disk = std::fs::read(file).map_err(|e| {
            CliError::Msg(
                t!("err.read_current", file = file.display(), err = e).to_string(),
            )
        })?;
        map_binary(store.diff_text_content(&disk, va.id))?
    } else {
        let vb = pick_by_seq(&versions, b)?;
        map_binary(store.diff_text(va.id, vb.id))?
    };

    if cli.json {
        println!("{}", json!({ "a": a, "b": b, "diff": text }));
    } else {
        print!("{text}");
    }
    Ok(())
}

fn cmd_revert(cli: &Cli, file: &Path, at: &str, yes: bool) -> Result<()> {
    let root = store_root(cli)?;
    let store = LocalHistory::open(&root)?; // rw — revert writes both disk and index
    let (ws, _) = resolve_ws_for_file(&store, file)?;
    let versions = store.history(ws, file)?;
    let v = pick_by_seq(&versions, at)?;

    if !yes {
        println!(
            "{}",
            t!(
                "revert.plan",
                file = file.display(),
                seq = v.seq,
                kind = v.kind.as_str(),
                time = format_time(v.timestamp_micros),
                size = human_size(v.size)
            )
        );
        print!("{}", t!("revert.confirm", seq = v.seq));
        std::io::stdout().flush()?;
        let mut ans = String::new();
        std::io::stdin().read_line(&mut ans)?;
        let reply = ans.trim();
        if !(reply.eq_ignore_ascii_case("y") || reply.eq_ignore_ascii_case("yes")) {
            if cli.json {
                println!("{}", json!({ "reverted": false, "reason": "cancelled" }));
            } else {
                println!("{}", t!("revert.cancelled"));
            }
            return Ok(());
        }
    }

    store.revert(ws, file, v.id)?;
    // After revert: the newest PreRevert version (recorded when disk differed from the
    // target) is the safety snapshot the restore sits on. Disk already matched → the
    // engine writes nothing.
    let after = store.history(ws, file)?;
    let pre = after.iter().find(|x| x.kind == EventKind::PreRevert);
    if cli.json {
        println!(
            "{}",
            json!({
                "reverted": true,
                "target_seq": v.seq,
                "pre_revert_seq": pre.map(|p| p.seq),
            })
        );
    } else {
        match pre {
            Some(p) => println!("{}", t!("revert.done_pre", seq = p.seq)),
            None => println!("{}", t!("revert.done_noop", seq = v.seq)),
        }
    }
    Ok(())
}

fn cmd_label(cli: &Cli, file: &Path, text: &[String]) -> Result<()> {
    let text = text.join(" ");
    if text.trim().is_empty() {
        return Err(CliError::Msg(t!("err.empty_label").to_string()));
    }
    let root = store_root(cli)?;
    let store = LocalHistory::open(&root)?; // rw — add_label records a version
    let (ws, _) = resolve_ws_for_file(&store, file)?;
    let vid = store.add_label(ws, file, &text)?;
    let seq = store
        .history(ws, file)?
        .iter()
        .find(|v| v.id == vid)
        .map(|v| v.seq);
    if cli.json {
        println!(
            "{}",
            json!({ "version_id": vid, "seq": seq, "label": text })
        );
    } else {
        println!(
            "{}",
            t!(
                "label.done",
                text = text,
                id = vid,
                seq = seq.map_or_else(|| "-".into(), |s| s.to_string())
            )
        );
    }
    Ok(())
}

fn cmd_gc(cli: &Cli, dry_run: bool) -> Result<()> {
    let root = store_root(cli)?;
    let store = LocalHistory::open(&root)?;
    let report = if dry_run {
        store.gc(None, &RetentionPolicy::default(), now_micros(), true)?
    } else {
        store.gc_auto(None, &RetentionPolicy::default())?
    };

    if cli.json {
        println!(
            "{}",
            json!({
                "dry_run": report.dry_run,
                "files_scanned": report.files_scanned,
                "versions_deleted": report.versions_deleted,
                "bytes_freed": report.bytes_freed,
                "bytes_freed_human": human_size(report.bytes_freed),
                "orphan_blobs": report.orphan_blobs,
            })
        );
        return Ok(());
    }

    if report.dry_run {
        println!("{}", t!("gc.header_dry"));
    } else {
        println!("{}", t!("gc.header"));
    }
    println!("{}", t!("gc.files_scanned", n = report.files_scanned));
    println!(
        "{}",
        t!("gc.versions_deleted", n = report.versions_deleted)
    );
    println!(
        "{}",
        t!(
            "gc.bytes_freed",
            human = human_size(report.bytes_freed),
            bytes = report.bytes_freed
        )
    );
    println!("{}", t!("gc.orphan_blobs", n = report.orphan_blobs));
    Ok(())
}

fn cmd_workspaces(cli: &Cli) -> Result<()> {
    let root = store_root(cli)?;
    let store = open_read(&root)?;
    let overview = ws_overview(&store)?;

    if cli.json {
        let arr: Vec<_> = overview.iter().map(ws_json).collect();
        println!("{}", json!(arr));
        return Ok(());
    }

    if overview.is_empty() {
        println!("{}", t!("ws.none", root = root.display()));
        return Ok(());
    }
    println!("{}", t!("ws.header", root = root.display()));
    print_table(&ws_header(), &ws_rows(&overview));
    Ok(())
}

fn cmd_ui(cli: &Cli, file: Option<&Path>) -> Result<()> {
    let exe = std::env::current_exe()
        .map_err(|e| CliError::Msg(t!("err.current_exe", err = e).to_string()))?;
    let gui = exe
        .parent()
        .ok_or_else(|| CliError::Msg(t!("err.exe_dir").to_string()))?
        .join(format!("lh-gui{}", std::env::consts::EXE_SUFFIX));
    if !gui.exists() {
        return Err(CliError::Msg(
            t!("err.gui_missing", path = gui.display()).to_string(),
        ));
    }

    let mut cmd = std::process::Command::new(&gui);
    if let Some(f) = file {
        // GUI canonicalizes and resolves the workspace itself (contract: `lh-gui --file <path>`)
        cmd.args(["--file", &f.display().to_string()]);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // detached: no console, separate process group — the GUI outlives lh
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    let child = cmd
        .spawn()
        .map_err(|e| CliError::Msg(t!("err.spawn", path = gui.display(), err = e).to_string()))?;

    if cli.json {
        println!(
            "{}",
            json!({ "spawned": true, "pid": child.id(), "gui": gui.display().to_string() })
        );
    } else {
        println!("{}", t!("ui.opened", pid = child.id()));
    }
    Ok(())
}

// ---------- resolve file → workspace ----------

/// Resolve a file to (workspace id, workspace root): the workspace whose root is a prefix
/// of the file wins; with nested workspaces the LONGEST root (most specific) wins.
/// Outside every workspace → clear error.
pub fn resolve_ws_for_file(store: &LocalHistory, file: &Path) -> Result<(WorkspaceId, PathBuf)> {
    let abs = normalized_abs(file)?;
    let mut best: Option<(usize, WorkspaceId, PathBuf)> = None;
    for (ws, root) in store.workspaces()? {
        if under_root(&abs, &root) {
            let depth = root.components().count();
            if best.as_ref().is_none_or(|(d, _, _)| depth > *d) {
                best = Some((depth, ws, root));
            }
        }
    }
    best.map(|(_, ws, root)| (ws, root))
        .ok_or_else(|| {
            CliError::Msg(t!("err.outside_workspace", file = file.display()).to_string())
        })
}

/// Absolute normalized path: existing files → canonicalize (strip `\\?\`); missing files
/// (e.g. already deleted) → canonicalize the nearest surviving ancestor and re-append the
/// tail. Matches the engine's convention (`normalize_canonical`/`rel_path_against`) so the
/// prefix check against roots stored in the db is exact.
fn normalized_abs(file: &Path) -> Result<PathBuf> {
    let original = file.display().to_string();
    let file = if file.is_absolute() {
        file.to_path_buf()
    } else {
        let cwd = std::env::current_dir()
            .map_err(|e| CliError::Msg(t!("err.cwd", err = e).to_string()))?;
        cwd.join(file)
    };
    if file.exists() {
        return canonical_lossy(&file);
    }
    let mut existing = file;
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    loop {
        let Some(name) = existing.file_name().map(|n| n.to_os_string()) else {
            return Err(CliError::Msg(
                t!("err.invalid_path", path = original).to_string(),
            ));
        };
        let Some(parent) = existing.parent() else {
            return Err(CliError::Msg(
                t!("err.invalid_path", path = original).to_string(),
            ));
        };
        tail.push(name);
        existing = parent.to_path_buf();
        if existing.exists() {
            break;
        }
    }
    let mut abs = canonical_lossy(&existing)?;
    for comp in tail.iter().rev() {
        abs.push(comp);
    }
    Ok(abs)
}

/// Canonicalize and strip the Windows `\\?\` prefix (UNC `\\?\UNC\...` → `\\...`).
fn canonical_lossy(p: &Path) -> Result<PathBuf> {
    let c = p.canonicalize().map_err(|e| {
        CliError::Msg(t!("err.canonicalize", path = p.display(), err = e).to_string())
    })?;
    let s = c.to_string_lossy();
    Ok(if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{rest}"))
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        c
    })
}

/// Is `path` strictly INSIDE `root` (component-wise; case-insensitive on Windows)?
fn under_root(path: &Path, root: &Path) -> bool {
    let pc: Vec<_> = path.components().collect();
    let rc: Vec<_> = root.components().collect();
    pc.len() > rc.len()
        && pc[..rc.len()]
            .iter()
            .zip(rc.iter())
            .all(|(a, b)| {
                if cfg!(windows) {
                    a.as_os_str().eq_ignore_ascii_case(b.as_os_str())
                } else {
                    a == b
                }
            })
}

// ---------- version helpers ----------

/// Selector for a version in a file's history: the newest entry or an explicit seq.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum At {
    /// Newest version in the history.
    Latest,
    /// Version with this seq.
    Seq(i64),
}

/// Parse `--at`: `latest` (case-insensitive) or a numeric seq.
pub fn parse_at(s: &str) -> Result<At> {
    if s.eq_ignore_ascii_case("latest") {
        return Ok(At::Latest);
    }
    Ok(At::Seq(parse_seq(s)?))
}

/// Parse a seq: non-negative integer.
pub fn parse_seq(s: &str) -> Result<i64> {
    let n: i64 = s
        .trim()
        .parse()
        .map_err(|_| CliError::Msg(t!("err.bad_seq", s = s).to_string()))?;
    if n < 0 {
        return Err(CliError::Msg(t!("err.negative_seq", n = n).to_string()));
    }
    Ok(n)
}

/// Find a version by seq in a newest-first list. Missing → clear error including the seq range.
fn find_by_seq(versions: &[Version], seq: i64) -> Result<Version> {
    versions
        .iter()
        .find(|v| v.seq == seq)
        .cloned()
        .ok_or_else(|| {
            let (min, max) = match (versions.first(), versions.last()) {
                (Some(f), Some(l)) => (l.seq, f.seq),
                _ => (0, 0),
            };
            CliError::Msg(
                t!(
                    "err.seq_not_found",
                    seq = seq,
                    count = versions.len(),
                    min = min,
                    max = max
                )
                .to_string(),
            )
        })
}

/// Resolve an `--at` value to a concrete version (`latest` = newest in the history).
fn pick_version(versions: &[Version], at: &str) -> Result<Version> {
    if versions.is_empty() {
        return Err(CliError::Msg(t!("err.no_versions").to_string()));
    }
    match parse_at(at)? {
        At::Latest => Ok(versions[0].clone()),
        At::Seq(n) => find_by_seq(versions, n),
    }
}

/// Like `pick_version` but accepts ONLY a seq (used for both sides of `lh diff`).
fn pick_by_seq(versions: &[Version], s: &str) -> Result<Version> {
    if versions.is_empty() {
        return Err(CliError::Msg(t!("err.no_versions").to_string()));
    }
    let seq = parse_seq(s)?;
    find_by_seq(versions, seq)
}

// ---------- helpers: time / size / table / pid ----------

/// Unix micros → `YYYY-MM-DD HH:MM:SS` (UTC — hand-rolled because `time`/`chrono` are not
/// deps of this crate). civil-from-days (Howard Hinnant) — correct for pre-1970 dates,
/// leap years and the century rule (2100 is not a leap year).
pub fn format_time(micros: i64) -> String {
    let secs = micros.div_euclid(1_000_000);
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400); // seconds of day [0, 86399]
    let (y, m, d) = civil_from_days(days);
    let (hh, mm, ss) = (sod / 3600, (sod % 3600) / 60, sod % 60);
    format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}:{ss:02}")
}

/// Days since 1970-01-01 → (year, month, day). Reference: Howard Hinnant,
/// "chrono-Compatible Low-Level Date Algorithms" (adapted to div_euclid for negatives).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468; // shift to the 0000-03-01 epoch
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097); // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (y, m as u32, d as u32)
}

/// Human-readable size: B / KB / MB / GB (1 KB = 1024 B).
pub fn human_size(bytes: u64) -> String {
    const K: f64 = 1024.0;
    let b = bytes as f64;
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", b / K)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{:.1} MB", b / (K * K))
    } else {
        format!("{:.1} GB", b / (K * K * K))
    }
}

/// Left-pad a table cell — counted in CHARACTERS (accented labels are multi-byte), not bytes.
fn pad(s: &str, width: usize) -> String {
    let n = s.chars().count();
    if n >= width {
        s.to_string()
    } else {
        format!("{s}{}", " ".repeat(width - n))
    }
}

/// Minimal table: header + rule + body, left-aligned, two-space gaps. No deps.
/// The header is already localized — widths account for it (a longer header widens the column).
fn print_table(header: &[String], rows: &[Vec<String>]) {
    let ncol = header.len();
    let mut widths: Vec<usize> = header.iter().map(|h| h.chars().count()).collect();
    for r in rows {
        for (i, c) in r.iter().enumerate().take(ncol) {
            widths[i] = widths[i].max(c.chars().count());
        }
    }
    let render = |cells: &[String]| -> String {
        let mut line = String::new();
        for (i, cell) in cells.iter().enumerate() {
            line.push_str(&pad(cell, widths[i]));
            if i + 1 < cells.len() {
                line.push_str("  ");
            }
        }
        line.trim_end().to_string()
    };
    let head_cells: Vec<String> = header.to_vec();
    println!("{}", render(&head_cells));
    let sep: Vec<String> = widths.iter().map(|w| "-".repeat(*w)).collect();
    println!("{}", sep.join("  "));
    for r in rows {
        println!("{}", render(r));
    }
}

/// Is the pid alive? Windows: `tasklist /FI "PID eq <pid>" /NH /FO CSV` — parse the pid
/// column; an "INFO: ..." line (no such process) has a single column → no match. Other
/// OSes: /proc. Command failure → false (conservative: report "not responding").
#[cfg(windows)]
fn pid_alive(pid: u32) -> bool {
    let out = match std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
        .output()
    {
        Ok(o) => o,
        Err(_) => return false,
    };
    let s = String::from_utf8_lossy(&out.stdout);
    s.lines().any(|l| {
        l.split(',')
            .nth(1)
            .is_some_and(|f| f.trim_matches('"') == pid.to_string())
    })
}

#[cfg(not(windows))]
fn pid_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

/// Current unix micros (for gc dry-run) — same convention as engine/daemon.
fn now_micros() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
}

// ---------- tests ----------

#[cfg(test)]
mod tests {
    use super::*;

    // ----- format_time -----

    #[test]
    fn format_time_epoch() {
        assert_eq!(format_time(0), "1970-01-01 00:00:00");
    }

    #[test]
    fn format_time_negative_micros() {
        // one microsecond before the epoch
        assert_eq!(format_time(-1), "1969-12-31 23:59:59");
    }

    #[test]
    fn format_time_known_value() {
        // 1700000000s = 2023-11-14 22:13:20 UTC (public reference value)
        assert_eq!(format_time(1_700_000_000_000_000), "2023-11-14 22:13:20");
    }

    #[test]
    fn format_time_leap_day() {
        // 2024 is a leap year: Feb 29 exists
        assert_eq!(format_time(1_709_164_800_000_000), "2024-02-29 00:00:00");
    }

    #[test]
    fn format_time_century_non_leap() {
        // 2100 is NOT a leap year (div 100 but not div 400): Feb 28 → Mar 01, no Feb 29
        let feb28: i64 = 4_107_456_000_000_000;
        assert_eq!(format_time(feb28), "2100-02-28 00:00:00");
        assert_eq!(format_time(feb28 + 86_400_000_000), "2100-03-01 00:00:00");
    }

    // ----- human_size -----

    #[test]
    fn human_size_bytes() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1023), "1023 B");
    }

    #[test]
    fn human_size_kb_mb_gb() {
        assert_eq!(human_size(1024), "1.0 KB");
        assert_eq!(human_size(1536), "1.5 KB");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0 MB");
        assert_eq!(human_size(3 * 1024 * 1024 * 1024), "3.0 GB");
    }

    // ----- parse seq / latest -----

    #[test]
    fn parse_at_and_seq() {
        assert_eq!(parse_at("latest").unwrap(), At::Latest);
        assert_eq!(parse_at("LATEST").unwrap(), At::Latest);
        assert_eq!(parse_at("7").unwrap(), At::Seq(7));
        assert_eq!(parse_seq(" 42 ").unwrap(), 42);
        assert!(parse_at("abc").is_err());
        assert!(parse_at("").is_err());
        assert!(parse_at("-3").is_err());
        assert!(parse_seq("1.5").is_err());
    }

    // ----- resolve workspace -----

    #[test]
    fn under_root_matches_components() {
        let p = Path::new(r"D:\Work\Proj\a.txt");
        assert!(under_root(p, Path::new(r"D:\Work\Proj")));
        assert!(!under_root(p, Path::new(r"D:\Other")));
        // equal paths → not "inside"
        assert!(!under_root(Path::new(r"D:\Work\Proj"), Path::new(r"D:\Work\Proj")));
        // Windows: case-insensitive
        if cfg!(windows) {
            assert!(under_root(p, Path::new(r"D:\WORK\proj")));
        }
    }

    #[test]
    fn resolve_ws_longest_prefix_and_errors_outside() {
        let store_dir = tempfile::tempdir().unwrap();
        let store = LocalHistory::open(store_dir.path()).unwrap();
        let ws_a = tempfile::tempdir().unwrap();
        let ws_b = tempfile::tempdir_in(ws_a.path()).unwrap(); // workspace nested inside ws_a
        let id_a = store.register_workspace(ws_a.path()).unwrap();
        let id_b = store.register_workspace(ws_b.path()).unwrap();

        // file inside the nested workspace → the longest (most specific) root wins
        let f_b = ws_b.path().join("x.txt");
        std::fs::write(&f_b, b"hi").unwrap();
        let (got, root_b) = resolve_ws_for_file(&store, &f_b).unwrap();
        assert_eq!(got, id_b);
        // assert on the normalized path (raw tempdir may differ from canonical via junctions)
        assert!(under_root(&normalized_abs(&f_b).unwrap(), &root_b));

        // file in the parent workspace (does not fall into the nested one)
        let f_a = ws_a.path().join("y.txt");
        std::fs::write(&f_a, b"hi").unwrap();
        let (got2, root_a) = resolve_ws_for_file(&store, &f_a).unwrap();
        assert_eq!(got2, id_a);
        assert!(under_root(&normalized_abs(&f_a).unwrap(), &root_a));

        // non-existent files still resolve (nearest surviving ancestor)
        let (got3, _) = resolve_ws_for_file(&store, &ws_a.path().join("gone").join("z.txt")).unwrap();
        assert_eq!(got3, id_a);

        // relative path → resolved against cwd (running inside ws_a)
        let old_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(ws_a.path()).unwrap();
        let got4 = resolve_ws_for_file(&store, Path::new("y.txt")).unwrap();
        std::env::set_current_dir(old_cwd).unwrap();
        assert_eq!(got4.0, id_a);

        // outside every workspace → clear error (assert on "workspace", stable in both locales)
        let outside = tempfile::tempdir().unwrap();
        let f_out = outside.path().join("q.txt");
        std::fs::write(&f_out, b"hi").unwrap();
        let err = resolve_ws_for_file(&store, &f_out).unwrap_err();
        assert!(err.to_string().contains("workspace"), "{err}");
    }

    // ----- find_by_seq -----

    #[test]
    fn find_by_seq_range_message() {
        let mk = |seq: i64| Version {
            id: seq,
            file_id: 1,
            seq,
            timestamp_micros: 0,
            kind: EventKind::External,
            label: None,
            storage: history::Storage::Full,
            base_seq: None,
            size: 0,
            content_hash: String::new(),
        };
        let versions = vec![mk(3), mk(2), mk(1)]; // newest first
        assert_eq!(find_by_seq(&versions, 2).unwrap().seq, 2);
        let err = find_by_seq(&versions, 9).unwrap_err().to_string();
        assert!(err.contains("seq 1..3"), "{err}");
    }

    // ----- i18n -----

    /// set_locale is process-global state — this lock keeps locale-switching tests from
    /// racing tests that read messages in the ambient locale.
    static LOCALE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn i18n_lookup_vi_and_en() {
        assert_eq!(
            t!("err.no_versions", locale = "vi"),
            "file chưa có version nào"
        );
        assert_eq!(t!("err.no_versions", locale = "en"), "file has no versions yet");
        assert_eq!(
            t!("history.header", locale = "vi", file = "a.txt", ws = "D:/ws"),
            "lịch sử của a.txt (ws: D:/ws)"
        );
        assert_eq!(
            t!("history.header", locale = "en", file = "a.txt", ws = "D:/ws"),
            "history of a.txt (ws: D:/ws)"
        );
        assert_eq!(
            t!("status.daemon_stopped", locale = "vi"),
            "daemon     : không chạy"
        );
        assert_eq!(
            t!("status.daemon_stopped", locale = "en"),
            "daemon     : not running"
        );
        // the confirm prompt keeps its trailing space — the user types right after "[y/N] "
        assert!(t!("revert.confirm", locale = "vi", seq = 3).ends_with("[y/N] "));
        assert!(t!("revert.confirm", locale = "en", seq = 3).ends_with("[y/N] "));
    }

    #[test]
    fn i18n_invalid_locale_falls_back_to_en() {
        let _guard = LOCALE_TEST_LOCK.lock().unwrap();
        // a locale absent from locales/*.yml → falls back to "en"
        rust_i18n::set_locale("fr");
        assert_eq!(t!("err.no_versions"), "file has no versions yet");
        // the real run() path: set_locale("vi") then look up WITHOUT naming the locale
        rust_i18n::set_locale("vi");
        assert_eq!(t!("err.no_versions"), "file chưa có version nào");
        rust_i18n::set_locale("en"); // restore the default for other tests
    }

    #[test]
    fn parse_lang_accepts_en_vi_rejects_other() {
        assert_eq!(parse_lang("en").unwrap(), "en");
        assert_eq!(parse_lang("VI").unwrap(), "vi");
        assert_eq!(parse_lang(" vi ").unwrap(), "vi");
        // lang_choice: a valid flag wins; an invalid one is reported later by parse_lang
        assert_eq!(lang_choice(Some("en")), "en");
        assert_eq!(lang_choice(Some("vi")), "vi");
        let err = parse_lang("fr").unwrap_err().to_string();
        // the "--lang" token appears in both locales — the assert is locale-independent
        assert!(err.contains("--lang"), "{err}");
    }

    #[test]
    fn i18n_yml_key_parity_and_cover_t_calls() {
        let en = yml_keys(include_str!("../locales/en.yml"));
        let vi = yml_keys(include_str!("../locales/vi.yml"));

        // no duplicate keys within one file (YAML silently swallows duplicates)
        for (name, keys) in [("en", &en), ("vi", &vi)] {
            let mut seen = std::collections::BTreeSet::new();
            for k in keys {
                assert!(seen.insert(k), "key `{k}` bị lặp trong {name}.yml");
            }
        }
        assert!(!en.is_empty(), "en.yml không có key nào");
        assert_eq!(
            en.len(),
            vi.len(),
            "số key en ({}) != số key vi ({})",
            en.len(),
            vi.len()
        );
        let only_en: Vec<&String> = en.iter().filter(|k| !vi.contains(*k)).collect();
        let only_vi: Vec<&String> = vi.iter().filter(|k| !en.contains(*k)).collect();
        assert!(
            only_en.is_empty() && only_vi.is_empty(),
            "2 file yml lệch tập key — chỉ có ở en: {only_en:?} · chỉ có ở vi: {only_vi:?}"
        );

        // every key used in translation calls within lib.rs must be defined in the yml
        // (no missed translation)
        for key in t_keys_in_source(include_str!("lib.rs")) {
            assert!(
                en.contains(&key),
                "key `{key}` được gọi trong lib.rs nhưng không có trong en.yml"
            );
        }
    }

    /// Minimal parser matching this crate's locales/*.yml format: one YAML tree per file
    /// (locale = file name), nested maps (namespace → group → key), double-quoted values
    /// kept on a single line. Returns full "a.b.c" keys in file order.
    fn yml_keys(yml: &str) -> Vec<String> {
        let mut keys = Vec::new();
        let mut stack: Vec<(usize, &str)> = Vec::new();
        for line in yml.lines() {
            let trimmed = line.trim_end();
            let body = trimmed.trim_start();
            if body.is_empty() || body.starts_with('#') {
                continue;
            }
            let indent = trimmed.len() - body.len();
            let Some((k, v)) = body.split_once(':') else {
                panic!("dòng YAML không ngờ (thiếu ':'): {line}");
            };
            let k = k.trim();
            let v = v.trim();
            while stack.last().is_some_and(|(i, _)| *i >= indent) {
                stack.pop();
            }
            if v.starts_with('"') {
                assert!(
                    v.len() >= 2 && v.ends_with('"'),
                    "value của `{k}` phải là chuỗi quoted trọn 1 dòng: {line}"
                );
                let path: Vec<&str> = stack.iter().map(|(_, s)| *s).chain([k]).collect();
                keys.push(path.join("."));
            } else {
                stack.push((indent, k));
            }
        }
        keys
    }

    /// Scan the source for keys passed to the translation macro (two tokens: "t" followed
    /// by "!(") — skip comment lines, accept only namespaced keys (alphanumerics/underscore/
    /// dash, containing a dot). Note: many standard macros (`assert!`, `print!`, `concat!`...)
    /// also contain that token pair — the prefix is built at runtime and the key filter
    /// is strict to avoid false positives.
    fn t_keys_in_source(src: &str) -> Vec<String> {
        let call_prefix = "t~!~(".replace('~', "");
        let mut keys = Vec::new();
        for (idx, _) in src.match_indices(call_prefix.as_str()) {
            let line_start = src[..idx].rfind('\n').map_or(0, |p| p + 1);
            if src[line_start..idx].trim_start().starts_with("//") {
                continue;
            }
            let rest = src[idx + call_prefix.len()..].trim_start();
            let Some(rest) = rest.strip_prefix('"') else {
                continue;
            };
            let Some(end) = rest.find('"') else {
                continue;
            };
            let key = &rest[..end];
            if !key.is_empty()
                && key.contains('.')
                && key
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
            {
                keys.push(key.to_string());
            }
        }
        keys
    }
}
