//! CLI `lh` — giao diện dòng lệnh cho local-history (M3).
//!
//! Toàn bộ logic nằm ở lib (không chỉ main.rs) để test in-process — cùng pattern với crate
//! `daemon`. Mỗi lệnh một hàm `cmd_*`; helpers (bảng, thời gian, dung lượng, resolve
//! workspace) có unit test riêng.
//!
//! Quy ước đầu ra:
//! - bảng/text đi qua lớp i18n rust-i18n (crates/cli/locales/en.yml + vi.yml): locale chọn
//!   bởi `--lang` (en|vi) > env `LH_LANG` > locale hệ điều hành (bắt đầu bằng "vi" → vi,
//!   không → en);
//! - `--json`: object/mảng serde_json, key tiếng Anh, ra stdout (value error vẫn dịch theo
//!   locale);
//! - lỗi: stderr + exit 1 (có `--json` → `{"error": "..."}` ra stdout cho máy đọc);
//!   diff nội dung binary → exit 2 (script phân biệt "có lịch sử nhưng binary").
//!
//! Không thêm dep ngoài i18n: định dạng thời gian/dung lượng/bảng tự viết (engine
//! `time`/`chrono` không có trong deps của crate này).

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use clap::{Parser, Subcommand};
use rust_i18n::t;
use serde_json::json;

// Nạp bản dịch từ crates/cli/locales (nhúng lúc biên dịch); thiếu bản dịch → fallback en.
rust_i18n::i18n!("locales", fallback = "en");

use history::{EventKind, LocalHistory, RetentionPolicy, Version, WorkspaceId};

pub type Result<T> = std::result::Result<T, CliError>;

// ---------- lỗi ----------

/// Lỗi CLI — in thẳng cho người dùng; `exit_code` quyết định code thoát.
#[derive(Debug)]
pub enum CliError {
    /// Lỗi từ engine `history`.
    History(history::Error),
    /// Nội dung binary không diff được — exit 2.
    Binary(String),
    /// Lỗi nghiệp vụ CLI (message đã viết sẵn cho người dùng).
    Msg(String),
}

impl CliError {
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

// ---------- định nghĩa lệnh (clap derive) ----------

#[derive(Debug, Parser)]
#[command(
    name = "lh",
    version,
    about = "local-history — lịch sử thay đổi file độc lập VCS",
    subcommand_required = true,
    arg_required_else_help = true
)]
pub struct Cli {
    /// In JSON thay vì bảng (key tiếng Anh, ra stdout)
    #[arg(long, global = true)]
    pub json: bool,

    /// Override store root (mặc định: env LOCAL_HISTORY_DIR hoặc %LOCALAPPDATA%\local-history)
    #[arg(long, global = true, value_name = "PATH")]
    pub store: Option<PathBuf>,

    /// Ngôn ngữ output: en | vi (mặc định: env LH_LANG, hoặc theo hệ điều hành)
    #[arg(long, global = true, value_name = "LANG")]
    pub lang: Option<String>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Trạng thái daemon + danh sách workspace
    Status,
    /// Timeline của một file
    History {
        /// Đường dẫn file (tuyệt đối hoặc tương đối)
        file: PathBuf,
    },
    /// In nội dung một version ra stdout (hoặc ghi file với -o)
    Show {
        file: PathBuf,
        /// Seq của version, hoặc `latest` cho bản mới nhất
        #[arg(long)]
        at: String,
        /// Ghi ra file thay vì stdout
        #[arg(short = 'o', long = "output", value_name = "FILE")]
        output: Option<PathBuf>,
    },
    /// Unified diff giữa hai version (b = seq, hoặc `current` = nội dung disk hiện tại)
    Diff {
        file: PathBuf,
        /// Seq version bên trái
        a: String,
        /// Seq version bên phải, hoặc `current`
        b: String,
    },
    /// Khôi phục file về nội dung của một version (luôn sinh pre_revert trước khi ghi đè)
    Revert {
        file: PathBuf,
        /// Seq version cần khôi phục
        #[arg(long)]
        at: String,
        /// Bỏ qua câu hỏi xác nhận
        #[arg(long)]
        yes: bool,
    },
    /// Gắn label ngữ nghĩa (tạo version kind=label với nội dung hiện tại trên disk)
    Label {
        file: PathBuf,
        /// Nội dung label (nhiều từ được, ghép lại bằng khoảng trắng)
        #[arg(required = true)]
        text: Vec<String>,
    },
    /// Dọn version cũ theo retention (mặc định: 5 ngày làm việc, giữ 50 version/file)
    Gc {
        /// Chỉ xem trước, không xóa gì
        #[arg(long)]
        dry_run: bool,
    },
    /// Danh sách workspace trong store
    Workspaces,
    /// Mở GUI (lh-gui) detached — trả về ngay
    Ui {
        /// File mở thẳng timeline (tùy chọn)
        file: Option<PathBuf>,
    },
}

/// Điểm vào chính — trả exit code (main.rs chỉ `std::process::exit` với giá trị này).
pub fn run(cli: Cli) -> i32 {
    // set_locale NGAY ĐẦU run() — mọi chuỗi phía sau đi qua lớp i18n theo locale này.
    // `--lang` sai giá trị: vẫn set locale theo hệ thống trước để thông báo lỗi ra đúng
    // ngôn ngữ người dùng, rồi báo lỗi rõ (exit 1).
    rust_i18n::set_locale(&lang_choice(cli.lang.as_deref()));

    let result = match parse_lang(cli.lang.as_deref().unwrap_or("en")) {
        Ok(_) => dispatch(&cli),
        Err(e) => Err(e),
    };
    report(cli.json, result)
}

/// In kết quả: ok → exit 0; lỗi → json mode in `{"error": ...}` ra stdout (máy đọc được
/// cùng một kênh), text mode in `lh: <lỗi>` ra stderr; exit code theo `CliError::exit_code`.
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

/// Locale dùng cho phiên chạy: `--lang` hợp lệ > env `LH_LANG` > locale hệ điều hành.
/// `--lang` sai giá trị → rơi về auto (lỗi được báo sau khi locale đã set, xem `run`).
fn lang_choice(flag: Option<&str>) -> String {
    flag.and_then(normalize_lang).unwrap_or_else(auto_lang)
}

/// Chuẩn hóa "en"|"vi" (trim khoảng trắng, không phân biệt hoa/thường); khác → None.
fn normalize_lang(s: &str) -> Option<String> {
    match s.trim().to_ascii_lowercase().as_str() {
        "en" => Some("en".into()),
        "vi" => Some("vi".into()),
        _ => None,
    }
}

/// Parse giá trị `--lang`/`LH_LANG` → "en"|"vi"; sai → lỗi rõ `err.bad_lang`
/// (message được dựng theo locale hiện tại).
fn parse_lang(s: &str) -> Result<String> {
    normalize_lang(s).ok_or_else(|| CliError::Msg(t!("err.bad_lang", v = s).to_string()))
}

/// Locale tự động: env `LH_LANG` nếu là en|vi, nếu không theo hệ điều hành
/// (`sys_locale` bắt đầu bằng "vi" → vi, không → en).
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

/// Root store: `--store` override, nếu không thì mặc định của engine (env `LOCAL_HISTORY_DIR`
/// hoặc `%LOCALAPPDATA%\local-history`).
fn store_root(cli: &Cli) -> Result<PathBuf> {
    match &cli.store {
        Some(p) => Ok(p.clone()),
        None => Ok(LocalHistory::default_root()?),
    }
}

/// Mở store để ĐỌC. Chưa có index.db → lỗi rõ (không tự tạo store từ lệnh đọc).
fn open_read(root: &Path) -> Result<LocalHistory> {
    if !root.join("index.db").exists() {
        return Err(CliError::Msg(
            t!("err.store_empty", path = root.display()).to_string(),
        ));
    }
    Ok(LocalHistory::open_read_only(root)?)
}

// ---------- các lệnh ----------

/// Tổng quan một workspace: (ws_id, root, số file, tổng số version).
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

/// Header bảng workspace (dùng chung cho `lh status` và `lh workspaces`) — đi qua i18n.
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

    // Store chưa có db (chưa từng ghi) → coi như rỗng, không lỗi.
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
            // bytes thô ra stdout (binary-safe) — write trên stdout.lock()
            let stdout = std::io::stdout();
            let mut lock = stdout.lock();
            lock.write_all(&data)?;
            lock.flush()?;
        }
    }
    Ok(())
}

/// BinaryContent → CliError::Binary (message rõ + exit 2); lỗi khác truyền qua.
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
    let store = LocalHistory::open(&root)?; // rw — revert ghi cả disk lẫn index
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
        let t = ans.trim();
        if !(t.eq_ignore_ascii_case("y") || t.eq_ignore_ascii_case("yes")) {
            if cli.json {
                println!("{}", json!({ "reverted": false, "reason": "cancelled" }));
            } else {
                println!("{}", t!("revert.cancelled"));
            }
            return Ok(());
        }
    }

    store.revert(ws, file, v.id)?;
    // Sau revert: version mới nhất kind=PreRevert (nếu disk trước đó khác target) là snapshot
    // an toàn; restore nằm trên nó. Disk đã khớp sẵn → engine không ghi gì.
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
    let store = LocalHistory::open(&root)?; // rw — add_label tạo version
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
        // GUI tự canonicalize + resolve workspace (contract `lh-gui --file <path>`)
        cmd.args(["--file", &f.display().to_string()]);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // detached: không console, process group riêng — GUI sống độc lập với lh
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

/// Resolve file → (workspace id, ws root): match canonical root là prefix của file, chọn ws
/// có root DÀI nhất (workspace lồng nhau → cụ thể nhất thắng). Ngoài mọi ws → lỗi rõ.
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

/// Đưa đường dẫn về tuyệt đối đã chuẩn hóa: tồn tại → canonicalize (strip `\\?\`);
/// chưa tồn tại (vd file đã bị xóa) → canonicalize ancestor gần nhất còn tồn tại rồi ghép
/// phần đuôi. Khớp quy ước của engine (`normalize_canonical`/`rel_path_against`) để so
/// prefix chính xác với root đã lưu trong db.
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

/// Canonicalize + strip prefix `\\?\` của Windows (giữ UNC `\\?\UNC\...` → `\\...`).
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

/// `path` nằm TRONG `root` (so component, không phân biệt hoa thường trên Windows)?
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

/// `--at <SEQ|latest>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum At {
    Latest,
    Seq(i64),
}

/// Parse `--at`: `latest` (không phân biệt hoa thường) hoặc số seq.
pub fn parse_at(s: &str) -> Result<At> {
    if s.eq_ignore_ascii_case("latest") {
        return Ok(At::Latest);
    }
    Ok(At::Seq(parse_seq(s)?))
}

/// Parse seq: số nguyên ≥ 0.
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

/// Tìm version theo seq trong danh sách (mới nhất trước). Không thấy → lỗi rõ kèm phạm vi.
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

/// `--at` → version cụ thể (`latest` = bản mới nhất trong history).
fn pick_version(versions: &[Version], at: &str) -> Result<Version> {
    if versions.is_empty() {
        return Err(CliError::Msg(t!("err.no_versions").to_string()));
    }
    match parse_at(at)? {
        At::Latest => Ok(versions[0].clone()),
        At::Seq(n) => find_by_seq(versions, n),
    }
}

/// Như `pick_version` nhưng CHỈ nhận seq (dùng cho hai bên của `lh diff`).
fn pick_by_seq(versions: &[Version], s: &str) -> Result<Version> {
    if versions.is_empty() {
        return Err(CliError::Msg(t!("err.no_versions").to_string()));
    }
    let seq = parse_seq(s)?;
    find_by_seq(versions, seq)
}

// ---------- helpers: thời gian / dung lượng / bảng / pid ----------

/// unix micros → `YYYY-MM-DD HH:MM:SS` (UTC — crate time/chrono không có trong deps cli,
/// tự viết). Thuật toán civil-from-days (Howard Hinnant) — đúng cho ngày trước 1970,
/// năm nhuận và quy tắc century (2100 không nhuận).
pub fn format_time(micros: i64) -> String {
    let secs = micros.div_euclid(1_000_000);
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400); // giây trong ngày [0, 86399]
    let (y, m, d) = civil_from_days(days);
    let (hh, mm, ss) = (sod / 3600, (sod % 3600) / 60, sod % 60);
    format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}:{ss:02}")
}

/// Số ngày kể từ 1970-01-01 → (năm, tháng, ngày). Tham chiếu: Howard Hinnant,
/// "chrono-Compatible Low-Level Date Algorithms" (được điều chỉnh div_euclid cho âm).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468; // dịch về mốc 0000-03-01
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

/// Định dạng dung lượng người đọc được: B / KB / MB / GB (1 KB = 1024 B).
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

/// Padding cột canh trái — đếm theo CHAR (label tiếng Việt có dấu, không đếm bytes).
fn pad(s: &str, width: usize) -> String {
    let n = s.chars().count();
    if n >= width {
        s.to_string()
    } else {
        format!("{s}{}", " ".repeat(width - n))
    }
}

/// Bảng cột đơn giản: header + đường kẻ + body, canh trái, gap 2 khoảng trắng. Không dep.
/// Header là chuỗi đã i18n — width canh lại theo header (header dài hơn cell → dãn cột ra).
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

/// pid còn sống? Windows: `tasklist /FI "PID eq <pid>" /NH /FO CSV` — parse cột pid;
/// dòng "INFO: ..." (không thấy process) chỉ có 1 cột → không match. OS khác: /proc.
/// Lỗi chạy lệnh → false (thận trọng: báo "không phản hồi").
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

/// unix micros hiện tại (cho gc dry-run) — cùng quy ước với engine/daemon.
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
        // 1 micros trước epoch
        assert_eq!(format_time(-1), "1969-12-31 23:59:59");
    }

    #[test]
    fn format_time_known_value() {
        // 1700000000s = 2023-11-14 22:13:20 UTC (giá trị tham chiếu public)
        assert_eq!(format_time(1_700_000_000_000_000), "2023-11-14 22:13:20");
    }

    #[test]
    fn format_time_leap_day() {
        // 2024 nhuận: 29/02 tồn tại
        assert_eq!(format_time(1_709_164_800_000_000), "2024-02-29 00:00:00");
    }

    #[test]
    fn format_time_century_non_leap() {
        // 2100 KHÔNG nhuận (div 100, không div 400): 28/02 → 01/03, không có 29/02
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
        // bằng nhau → không tính là "bên trong"
        assert!(!under_root(Path::new(r"D:\Work\Proj"), Path::new(r"D:\Work\Proj")));
        // Windows: hoa thường không phân biệt
        if cfg!(windows) {
            assert!(under_root(p, Path::new(r"D:\WORK\proj")));
        }
    }

    #[test]
    fn resolve_ws_longest_prefix_and_errors_outside() {
        let store_dir = tempfile::tempdir().unwrap();
        let store = LocalHistory::open(store_dir.path()).unwrap();
        let ws_a = tempfile::tempdir().unwrap();
        let ws_b = tempfile::tempdir_in(ws_a.path()).unwrap(); // ws lồng trong ws_a
        let id_a = store.register_workspace(ws_a.path()).unwrap();
        let id_b = store.register_workspace(ws_b.path()).unwrap();

        // file tồn tại trong ws con → chọn root dài nhất (cụ thể nhất)
        let f_b = ws_b.path().join("x.txt");
        std::fs::write(&f_b, b"hi").unwrap();
        let (got, root_b) = resolve_ws_for_file(&store, &f_b).unwrap();
        assert_eq!(got, id_b);
        // assert trên path đã normalize (tempdir raw có thể khác canonical qua junction)
        assert!(under_root(&normalized_abs(&f_b).unwrap(), &root_b));

        // file trong ws cha (không rơi vào ws con)
        let f_a = ws_a.path().join("y.txt");
        std::fs::write(&f_a, b"hi").unwrap();
        let (got2, root_a) = resolve_ws_for_file(&store, &f_a).unwrap();
        assert_eq!(got2, id_a);
        assert!(under_root(&normalized_abs(&f_a).unwrap(), &root_a));

        // file KHÔNG tồn tại vẫn resolve được (ancestor gần nhất còn tồn tại)
        let (got3, _) = resolve_ws_for_file(&store, &ws_a.path().join("gone").join("z.txt")).unwrap();
        assert_eq!(got3, id_a);

        // đường tương đối → resolve theo cwd (chạy trong ws_a)
        let old_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(ws_a.path()).unwrap();
        let got4 = resolve_ws_for_file(&store, Path::new("y.txt")).unwrap();
        std::env::set_current_dir(old_cwd).unwrap();
        assert_eq!(got4.0, id_a);

        // ngoài mọi ws → lỗi rõ (assert token "workspace" ổn định ở cả 2 locale)
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
        let versions = vec![mk(3), mk(2), mk(1)]; // mới nhất trước
        assert_eq!(find_by_seq(&versions, 2).unwrap().seq, 2);
        let err = find_by_seq(&versions, 9).unwrap_err().to_string();
        assert!(err.contains("seq 1..3"), "{err}");
    }

    // ----- i18n -----

    /// set_locale là trạng thái toàn cục của process — khóa để các test đổi locale không
    /// đua với test khác đang đọc message.
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
        // prompt xác nhận giữ dấu cách cuối — sau "[y/N] " người dùng gõ ngay trên cùng dòng
        assert!(t!("revert.confirm", locale = "vi", seq = 3).ends_with("[y/N] "));
        assert!(t!("revert.confirm", locale = "en", seq = 3).ends_with("[y/N] "));
    }

    #[test]
    fn i18n_invalid_locale_falls_back_to_en() {
        let _guard = LOCALE_TEST_LOCK.lock().unwrap();
        // locale không tồn tại trong locales/*.yml → rơi về fallback "en"
        rust_i18n::set_locale("fr");
        assert_eq!(t!("err.no_versions"), "file has no versions yet");
        // đường đi thật của run(): set_locale("vi") rồi tra cứu KHÔNG ghi rõ locale
        rust_i18n::set_locale("vi");
        assert_eq!(t!("err.no_versions"), "file chưa có version nào");
        rust_i18n::set_locale("en"); // trả về mặc định cho test khác
    }

    #[test]
    fn parse_lang_accepts_en_vi_rejects_other() {
        assert_eq!(parse_lang("en").unwrap(), "en");
        assert_eq!(parse_lang("VI").unwrap(), "vi");
        assert_eq!(parse_lang(" vi ").unwrap(), "vi");
        // lang_choice: flag hợp lệ thắng; sai giá trị thì thử ở parse_lang phía sau
        assert_eq!(lang_choice(Some("en")), "en");
        assert_eq!(lang_choice(Some("vi")), "vi");
        let err = parse_lang("fr").unwrap_err().to_string();
        // token "--lang" xuất hiện ở cả 2 locale — assert không phụ thuộc locale hiện tại
        assert!(err.contains("--lang"), "{err}");
    }

    #[test]
    fn i18n_yml_key_parity_and_cover_t_calls() {
        let en = yml_keys(include_str!("../locales/en.yml"));
        let vi = yml_keys(include_str!("../locales/vi.yml"));

        // không có key trùng trong cùng 1 file (YAML trùng key bị nuốt im lặng)
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

        // mọi key được gọi dịch trong lib.rs phải được định nghĩa trong yml (không quên dịch)
        for key in t_keys_in_source(include_str!("lib.rs")) {
            assert!(
                en.contains(&key),
                "key `{key}` được gọi trong lib.rs nhưng không có trong en.yml"
            );
        }
    }

    /// Parser tối giản đúng định dạng locales/*.yml của crate này: mỗi file là 1 cây YAML
    /// (locale = tên file), map lồng nhiều cấp (namespace → nhóm → key), value là chuỗi
    /// double-quoted nằm trọn 1 dòng. Trả về key đầy đủ "a.b.c" theo thứ tự file.
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

    /// Quét source lấy mọi key gọi qua macro dịch (macro 2 ký hiệu "t" + "!(")
    /// — bỏ qua dòng comment, chỉ nhận key namespaced (chữ/số/gạch dưới, có dấu chấm).
    /// Lưu ý: nhiều macro chuẩn (`assert!`, `print!`, `concat!`...) cũng chứa chuỗi
    /// "t" + "!(" — prefix được dựng lúc chạy và key bị lọc chặt để không bắt nhầm.
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
