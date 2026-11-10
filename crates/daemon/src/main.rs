//! Thin binary entry point of `lh-daemon` — parses args and calls the lib. All logic lives
//! in `lib.rs`.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

const USAGE: &str =
    "cách dùng: lh-daemon [--tray] [--dir PATH]... (--dir lặp lại được; không có --dir → theo dõi thư mục hiện tại; --tray → chạy trong system tray thay vì console treo)";

/// Minimal stderr logger (no extra dependency) so the lib's logs (gc, warnings, ...) are
/// visible. Uses a static logger — the log crate here is built without the `alloc` feature,
/// so `set_boxed_logger` is unavailable.
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
    let mut tray_mode = false;
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
            // Tray mode: same watcher pipeline, but no blocking console — icon + menu instead.
            "--tray" => tray_mode = true,
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

    if tray_mode {
        // Tray mode: watcher pipeline on a background thread; the tray (icon + menu) runs on
        // the main thread — Windows requires the icon on the thread owning the event loop.
        let stop = Arc::new(AtomicBool::new(false));
        let pause = Arc::new(AtomicBool::new(false));
        let watch_stop = stop.clone();
        let watch_pause = pause.clone();
        let dirs = watch_dirs.clone();
        let store = store_root.clone();
        let spawned = std::thread::Builder::new().name("lh-daemon-watch".into()).spawn(move || {
            daemon::run_with_root_paused(&store, &dirs, watch_stop, watch_pause)
        });
        let watcher = match spawned {
            Ok(h) => h,
            Err(e) => {
                eprintln!("lh-daemon: không tạo được thread watcher: {e}");
                std::process::exit(1);
            }
        };
        println!("chạy trong system tray — chuột phải icon để mở menu, Thoát để dừng.");
        match daemon::tray::run_tray(
            daemon::tray::TrayConfig { store_root: store_root.clone(), stop, pause },
            watcher,
        ) {
            Ok(()) => println!("đã dừng."),
            Err(e) => {
                eprintln!("lh-daemon: {e}");
                std::process::exit(1);
            }
        }
        return;
    }

    // Ctrl+C: the OS default terminate — the lock releases when the process dies; a stale
    // daemon.json is left behind and later CLI runs check the pid (not handled here).
    println!("Ctrl+C (hoặc kill tiến trình) để dừng.");

    let stop = Arc::new(AtomicBool::new(false));
    if let Err(e) = daemon::run(&watch_dirs, stop) {
        eprintln!("lh-daemon: {e}");
        std::process::exit(1);
    }
    println!("đã dừng.");
}
