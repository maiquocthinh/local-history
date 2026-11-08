//! Entry mỏng của `lh-daemon` — parse args rồi gọi lib. Toàn bộ logic ở `lib.rs`.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

const USAGE: &str =
    "cách dùng: lh-daemon [--dir PATH]... (--dir lặp lại được; không có --dir → theo dõi thư mục hiện tại)";

/// Logger tối giản ra stderr (không thêm dep) — để thấy log của lib (gc, cảnh báo...).
/// Dùng static logger (log crate không bật feature `alloc` nên không có set_boxed_logger).
struct StderrLogger;

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

fn main() {
    let _ = log::set_logger(&STDERR_LOGGER);
    log::set_max_level(log::LevelFilter::Info);

    let mut watch_dirs: Vec<PathBuf> = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--dir" => match args.next() {
                Some(p) => watch_dirs.push(PathBuf::from(p)),
                None => {
                    eprintln!("lh-daemon: --dir cần một PATH\n{USAGE}");
                    std::process::exit(2);
                }
            },
            "--help" | "-h" => {
                println!("{USAGE}");
                return;
            }
            other => {
                eprintln!("lh-daemon: không hiểu argument `{other}`\n{USAGE}");
                std::process::exit(2);
            }
        }
    }
    if watch_dirs.is_empty() {
        match std::env::current_dir() {
            Ok(d) => watch_dirs.push(d),
            Err(e) => {
                eprintln!("lh-daemon: không lấy được thư mục hiện tại: {e}");
                std::process::exit(2);
            }
        }
    }

    let store_root = match history::LocalHistory::default_root() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("lh-daemon: {e}");
            std::process::exit(1);
        }
    };

    println!("lh-daemon {} (store root: {})", env!("CARGO_PKG_VERSION"), store_root.display());
    for d in &watch_dirs {
        println!("  watch: {}", d.display());
    }
    // Ctrl+C: terminate mặc định của OS — lock tự nhả khi process chết; daemon.json stale
    // còn lại, CLI sau check pid (không xử lý ở đây).
    println!("Ctrl+C (hoặc kill tiến trình) để dừng.");

    let stop = Arc::new(AtomicBool::new(false));
    if let Err(e) = daemon::run(&watch_dirs, stop) {
        eprintln!("lh-daemon: {e}");
        std::process::exit(1);
    }
    println!("đã dừng.");
}
