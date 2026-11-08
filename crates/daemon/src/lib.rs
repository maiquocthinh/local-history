//! Daemon `lh-daemon` — theo dõi các thư mục workspace qua fs events (notify), snapshot tự động
//! mọi thay đổi file vào engine `history`.
//!
//! Pipeline (docs/architecture.md §4.2):
//!   fs event → filter ignore (theo component) → debounce map (path → instant)
//!   → path yên tĩnh ≥ DEBOUNCE_MS → đọc file → `record(..., External)` (dedup trong engine)
//!
//! Toàn bộ logic nằm ở lib (không chỉ main.rs) để test in-process qua `run_with_root`.

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

/// Thư mục bỏ qua — match theo path component (không phân biệt hoa thường).
pub const IGNORE_DIRS: &[&str] =
    &[".git", ".hg", ".svn", "target", "node_modules", "dist", "build", ".zcode"];

/// Khoảng yên tĩnh tối thiểu trước khi xử lý một path (debounce).
pub const DEBOUNCE_MS: u64 = 400;

/// Chu kỳ collector dậy: nhận event mới / phát các path đã yên tĩnh đủ lâu.
pub const COLLECTOR_INTERVAL_MS: u64 = 100;

/// Chu kỳ GC định kỳ.
const GC_INTERVAL_SECS: u64 = 6 * 3600;

pub type Result<T> = std::result::Result<T, DaemonError>;

/// Lỗi daemon — enum thủ công (crate này không có thiserror).
#[derive(Debug)]
pub enum DaemonError {
    /// Lỗi watcher (notify).
    Watcher(String),
    /// Không chiếm được daemon.lock — daemon khác đang chạy trên store này.
    Lock,
    Io(std::io::Error),
    History(history::Error),
    /// Tham số / thư mục không hợp lệ.
    Path(String),
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

/// Trạng thái daemon — ghi `<store_root>/daemon.json`, cho CLI/GUI đọc (`lh status`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonStatus {
    pub pid: u32,
    pub workspaces: Vec<String>,
    pub started_at_micros: i64,
}

/// Chạy daemon tới khi `stop` được set true. Blocking trên thread gọi.
///
/// `store_root` = thư mục data (test truyền tempdir; production là root mặc định của engine,
/// xem `run`). `watch_dirs` = các thư mục workspace cần theo dõi (rỗng → lỗi).
pub fn run_with_root(store_root: &Path, watch_dirs: &[PathBuf], stop: Arc<AtomicBool>) -> Result<()> {
    if watch_dirs.is_empty() {
        return Err(DaemonError::Path("không có thư mục nào để theo dõi".into()));
    }

    // 1. Chuẩn hóa watch dirs (canonicalize + strip `\\?\` — cùng quy ước với engine) rồi mở
    //    store + đăng ký workspace. Map (canonical root → ws_id) để gán event về workspace.
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

    // 2. Lock đơn nhất. Guard giữ đến hết hàm; process chết → OS tự nhả.
    //    (File lock rỗng, không ghi nội dung → truncate(false) rõ ý.)
    let lock_file = File::options()
        .create(true)
        .write(true)
        .truncate(false)
        .open(store_root.join("daemon.lock"))?;
    let mut lock = fd_lock::RwLock::new(lock_file);
    let _lock_guard = lock.try_write().map_err(|_| DaemonError::Lock)?;

    // 3. Status file. Gì SAU khi giữ lock → CLI đọc thấy daemon.json ⇒ daemon thật sự đang chạy.
    let status = DaemonStatus {
        pid: std::process::id(),
        workspaces: dirs.iter().map(|d| d.to_string_lossy().into_owned()).collect(),
        started_at_micros: now_micros(),
    };
    write_status(store_root, &status)?;
    log::info!("daemon start: pid={} store={} ws={:?}", status.pid, store_root.display(), dirs);

    // 4. Watcher — lỗi thì dọn status trước khi trả (không để daemon.json stale).
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

    // 5. GC lúc start + thread GC định kỳ (daemon lifetime — không cần dừng gọn).
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

    // 6. Vòng collector: event mới → debounce map; mỗi lần dậy phát path yên tĩnh ≥ DEBOUNCE_MS.
    //    Xử lý ngay trong thread này (giữ đơn giản); mọi lỗi record đều log + bỏ qua.
    let mut last_event: HashMap<String, Instant> = HashMap::new();
    let debounce = Duration::from_millis(DEBOUNCE_MS);
    let tick = Duration::from_millis(COLLECTOR_INTERVAL_MS);
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        match rx.recv_timeout(tick) {
            Ok(Ok(event)) => collect_event(&mut last_event, &event, &ws_entries),
            Ok(Err(e)) => log::warn!("watcher lỗi (bỏ qua): {e}"),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break, // watcher đã drop
        }
        drain_ready(&mut last_event, debounce, &engine, &ws_entries);
    }

    // 7. Dọn status (best effort). daemon.lock để lại — lock tự nhả khi process chết.
    let _ = std::fs::remove_file(store_root.join("daemon.json"));
    log::info!("daemon dừng sạch");
    Ok(())
}

/// Tiện ích cho main.rs: chạy với store root mặc định của engine (env `LOCAL_HISTORY_DIR`
/// hoặc `%LOCALAPPDATA%\local-history`).
pub fn run(watch_dirs: &[PathBuf], stop: Arc<AtomicBool>) -> Result<()> {
    let root = LocalHistory::default_root()?;
    run_with_root(&root, watch_dirs, stop)
}

/// Đọc trạng thái daemon (`<store_root>/daemon.json`). File thiếu/hỏng → `None`.
pub fn read_status(store_root: &Path) -> Option<DaemonStatus> {
    let data = std::fs::read_to_string(store_root.join("daemon.json")).ok()?;
    serde_json::from_str(&data).ok()
}

/// true nếu BẤT KỲ path component nào thuộc `IGNORE_DIRS`. Dùng cho filter trước debounce.
pub fn is_ignored(path: &Path) -> bool {
    path.components().any(|c| {
        IGNORE_DIRS.iter().any(|d| c.as_os_str().eq_ignore_ascii_case(*d))
    })
}

// ---------- nội bộ ----------

/// Filter theo component TƯƠNG ĐỐI với watched root — tránh false positive khi đường tuyệt đối
/// phía trên root tình cờ chứa tên trong `IGNORE_DIRS` (vd chạy daemon trong `target/`).
fn is_ignored_under(path: &Path, root: &Path) -> bool {
    match path.strip_prefix(root) {
        Ok(rel) => is_ignored(rel),
        Err(_) => is_ignored(path),
    }
}

/// path có nằm TRONG `dir` (so component, không phân biệt hoa thường trên Windows)?
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

/// Canonicalize + strip prefix `\\?\` của Windows (giữ UNC `\\?\UNC\...` → `\\...`).
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

/// unix micros UTC hiện tại.
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

/// Watcher setup thất bại: dọn daemon.json (best effort) rồi trả lỗi.
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

/// Nhận 1 event: chỉ Create/Modify/Remove (bỏ Access/Other); path phải qua filter ignore và
/// thuộc 1 watch dir đã đăng ký. Event mới về cùng path reset đồng hồ debounce.
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

/// Phát các path đã yên tĩnh ≥ `debounce` ra xử lý — ngay trong thread collector (giữ đơn giản).
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

/// Xử lý 1 path sau debounce. KHÔNG BAO GIỜ panic — mọi lỗi log + bỏ qua (daemon phải sống
/// sót qua file lởm chởm / đang bị khóa ghi).
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
            return; // thư mục mới tạo — chỉ quan tâm file
        }
        // Đang bị khóa ghi → đọc lỗi → bỏ qua; event sau (nếu còn thay đổi) sẽ ghi lại.
        match std::fs::read(&path) {
            Ok(content) => {
                match engine.record(ws, &path, &content, EventKind::External, None) {
                    Ok(Some(_)) => log::info!("đã record (external): {path_str}"),
                    Ok(None) => {} // dedup trong engine — không tạo version thừa
                    Err(e) => log::warn!("record {path_str} lỗi (bỏ qua): {e}"),
                }
            }
            Err(e) => log::warn!("đọc {path_str} lỗi (bỏ qua): {e}"),
        }
    } else {
        // File biến mất: chỉ ghi marker delete khi file có row trong files table của ws
        // (tránh ghi rác cho thư mục tạm / file chưa từng track).
        match engine.history(ws, &path) {
            Ok(versions) if !versions.is_empty() => {
                match engine.record(ws, &path, &[], EventKind::Delete, None) {
                    Ok(_) => log::info!("đã ghi marker delete: {path_str}"),
                    Err(e) => log::warn!("record delete {path_str} lỗi (bỏ qua): {e}"),
                }
            }
            Ok(_) => {} // chưa từng có lịch sử — bỏ qua
            Err(e) => log::warn!("history {path_str} lỗi (bỏ qua): {e}"),
        }
    }
}
