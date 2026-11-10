//! System tray mode for `lh-daemon --tray` (Windows-first, see docs/architecture.md).
//!
//! Layout: a hidden winit event loop on the main thread (no window is ever created) keeps the
//! process alive and pumps the messages a `tray-icon` notification icon + context menu need.
//! The watcher pipeline (`run_with_root_paused`) runs on a background thread owned by main.rs;
//! the tray only shares the store root plus two flags: `stop` (Quit) and `pause` (Pause/Resume
//! watch — the pipeline then drops fs events, see lib.rs).
//!
//! Menu entries: Open GUI · Pause watch / Resume watch (this pair toggles hide-show) ·
//! Clean up history (gc) · Quit. A left double-click on the icon also opens the GUI.
//!
//! Implementation notes:
//! - the tray icon is built once the event loop is running (`StartCause::Init` in
//!   `new_events`) — the upstream winit example does the same (tray-icon#90);
//! - muda's `MenuItem` has no `set_visible`, so hide/show is done by removing the item from
//!   the menu and re-inserting it at its slot (`set_paused`);
//! - tray gc opens its own connection to the same store: the engine runs WAL + busy_timeout
//!   (crates/history/src/store.rs), the same setup `lh gc` already relies on while the daemon
//!   holds its connection.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use history::{LocalHistory, RetentionPolicy};
use rust_i18n::t;
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem};
use tray_icon::{BadIcon, Icon, MouseButton, TrayIcon, TrayIconBuilder, TrayIconEvent};
use winit::application::ApplicationHandler;
use winit::event::StartCause;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};

use crate::{tray_icon_data, DaemonError, Result};

/// Menu item ids — matched against `MenuEvent.id` in `about_to_wait`.
const ID_OPEN_GUI: &str = "tray.open_gui";
const ID_PAUSE_WATCH: &str = "tray.pause_watch";
const ID_RESUME_WATCH: &str = "tray.resume_watch";
const ID_GC_NOW: &str = "tray.gc_now";
const ID_QUIT: &str = "tray.quit";

/// Shared state the tray loop needs from its caller (main.rs).
pub struct TrayConfig {
    /// Store root — used by the tray gc (menu "Clean up history") via a second connection.
    pub store_root: PathBuf,
    /// Set true by the Quit item → the watcher pipeline exits and cleans `daemon.json`.
    pub stop: Arc<AtomicBool>,
    /// true → the pipeline drops fs events (no recording); toggled by the pause/resume pair.
    pub pause: Arc<AtomicBool>,
}

/// Entry point of tray mode: run the hidden winit event loop (tray icon + menu) until the
/// user quits or the watcher thread finishes on its own.
///
/// `watcher` is the join handle of the pipeline thread spawned by main.rs. After the loop
/// ends this thread is joined — its clean shutdown is what removes `daemon.json` — and its
/// result is returned (a watcher error propagates to main for the exit code).
pub fn run_tray(cfg: TrayConfig, watcher: JoinHandle<Result<()>>) -> Result<()> {
    // Menu labels follow the CLI's locale rule (see `auto_lang`); set once before the loop.
    rust_i18n::set_locale(&auto_lang());
    let event_loop = EventLoop::builder()
        .build()
        .map_err(|e| DaemonError::Tray(format!("event loop: {e}")))?;
    let mut app = TrayApp { cfg, watcher, tray: None, menu_items: None, tray_error: None };
    event_loop
        .run_app(&mut app)
        .map_err(|e| DaemonError::Tray(format!("event loop: {e}")))?;
    // A tray build failure stops the loop on purpose (see `new_events`) — it must
    // not be reported as a clean exit just because the watcher finished normally.
    if let Some(e) = app.tray_error.take() {
        let _ = app.watcher.join();
        return Err(DaemonError::Tray(e));
    }
    app.watcher
        .join()
        .map_err(|_| DaemonError::Tray("watcher thread panicked".into()))?
}

/// Handles kept after the menu is handed over to the tray icon: the menu itself (muda types
/// are refcounted — the clone here talks to the same native menu) plus the pause/resume pair
/// for the hide-show toggle. The other items are matched by id in `about_to_wait`.
struct MenuItems {
    menu: Menu,
    pause_watch: MenuItem,
    resume_watch: MenuItem,
}

/// State for the `ApplicationHandler` impl below.
struct TrayApp {
    cfg: TrayConfig,
    watcher: JoinHandle<Result<()>>,
    tray: Option<TrayIcon>,
    menu_items: Option<MenuItems>,
    /// Set when the tray itself failed to build (icon/menu) — surfaced by
    /// `run_tray` after the loop ends, so a dead tray is not reported as a
    /// clean exit.
    tray_error: Option<String>,
}

impl TrayApp {
    /// Build the tray icon + context menu. Must run after the event loop started.
    fn build_tray(&mut self) -> Result<(TrayIcon, MenuItems)> {
        let icon = icon_rgba().map_err(|e| DaemonError::Tray(format!("icon: {e}")))?;

        let menu = Menu::new();
        let open_gui = MenuItem::with_id(MenuId::new(ID_OPEN_GUI), &t!("tray.open_gui"), true, None);
        let pause_watch = MenuItem::with_id(MenuId::new(ID_PAUSE_WATCH), &t!("tray.pause_watch"), true, None);
        let resume_watch = MenuItem::with_id(MenuId::new(ID_RESUME_WATCH), &t!("tray.resume_watch"), true, None);
        let gc_now = MenuItem::with_id(MenuId::new(ID_GC_NOW), &t!("tray.gc_now"), true, None);
        let quit = MenuItem::with_id(MenuId::new(ID_QUIT), &t!("tray.quit"), true, None);
        menu.append_items(&[&open_gui, &pause_watch, &resume_watch, &gc_now, &quit])
            .map_err(|e| DaemonError::Tray(format!("menu: {e}")))?;

        // We start watching → "Resume watch" is hidden, "Pause watch" shown (best effort:
        // a failed toggle must not keep the tray from starting).
        let _ = menu.remove(&resume_watch);

        let items = MenuItems {
            menu: menu.clone(),
            pause_watch: pause_watch.clone(),
            resume_watch: resume_watch.clone(),
        };
        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip(&t!("tray.tooltip"))
            .with_icon(icon)
            .build()
            .map_err(|e| DaemonError::Tray(format!("build tray icon: {e}")))?;
        Ok((tray, items))
    }

    /// Toggle the pipeline pause flag and swap the pause/resume menu entries.
    fn set_paused(&mut self, paused: bool) {
        self.cfg.pause.store(paused, Ordering::Relaxed);
        let Some(items) = &self.menu_items else { return };
        let (show, hide) = if paused {
            (&items.resume_watch, &items.pause_watch)
        } else {
            (&items.pause_watch, &items.resume_watch)
        };
        // muda has no MenuItem::set_visible → remove from / insert back into the menu at
        // position 1 (right below "Open GUI"). Best effort — same as the initial state.
        let _ = items.menu.remove(hide);
        let _ = items.menu.insert(show, 1);
        log::info!(
            "tray: {}",
            if paused { "đã tạm dừng watch (bỏ qua event mới)" } else { "tiếp tục watch" }
        );
    }

    /// Menu "Clean up history": run `gc_auto` on the same store, on its own thread — opening
    /// the store and scanning can take seconds and the tray loop must not freeze.
    fn gc_now(&self) {
        let root = self.cfg.store_root.clone();
        let spawned = std::thread::Builder::new().name("lh-tray-gc".into()).spawn(move || {
            match LocalHistory::open(&root) {
                Ok(store) => match store.gc_auto(None, &RetentionPolicy::default()) {
                    Ok(report) => log::info!("gc (tray): {report:?}"),
                    Err(e) => log::warn!("gc (tray) lỗi (bỏ qua): {e}"),
                },
                Err(e) => log::warn!("gc (tray) mở store lỗi (bỏ qua): {e}"),
            }
        });
        // Detached (handle dropped); spawn failure is logged, never surfaced to the menu.
        if let Err(e) = spawned {
            log::warn!("gc (tray) không tạo được thread (bỏ qua): {e}");
        }
    }
}

impl ApplicationHandler for TrayApp {
    // No window is ever created in tray mode → both callbacks stay empty.
    fn resumed(&mut self, _event_loop: &ActiveEventLoop) {}

    fn window_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        _window_id: winit::window::WindowId,
        _event: winit::event::WindowEvent,
    ) {
    }

    fn new_events(&mut self, event_loop: &ActiveEventLoop, cause: StartCause) {
        event_loop.set_control_flow(ControlFlow::Wait);
        // Build the tray icon once the loop is running (`StartCause::Init`) — the upstream
        // winit example does the same to avoid tray-icon#90-style platform issues.
        if cause == StartCause::Init && self.tray.is_none() {
            match self.build_tray() {
                Ok((tray, items)) => {
                    self.tray = Some(tray);
                    self.menu_items = Some(items);
                }
                Err(e) => {
                    log::error!("tray: {e}");
                    // No tray → nothing to show; remember why, stop the watcher
                    // and end the loop.
                    self.tray_error = Some(e.to_string());
                    self.cfg.stop.store(true, Ordering::Relaxed);
                    event_loop.exit();
                }
            }
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // Drain menu clicks. muda queues them while the loop waits; every incoming Win32
        // message wakes the winit loop, so no polling interval is needed.
        while let Ok(event) = MenuEvent::receiver().try_recv() {
            match event.id.0.as_str() {
                ID_OPEN_GUI => open_gui(),
                ID_PAUSE_WATCH => self.set_paused(true),
                ID_RESUME_WATCH => self.set_paused(false),
                ID_GC_NOW => self.gc_now(),
                ID_QUIT => {
                    // Watcher stops first — its return path cleans `daemon.json`; main.rs
                    // joins the thread right after this loop ends.
                    self.cfg.stop.store(true, Ordering::Relaxed);
                    event_loop.exit();
                }
                _ => {} // an id we don't manage (e.g. removed entry still in flight)
            }
        }
        // Left double-click on the icon also opens the GUI (DoubleClick is Windows-only).
        while let Ok(event) = TrayIconEvent::receiver().try_recv() {
            if let TrayIconEvent::DoubleClick { button: MouseButton::Left, .. } = event {
                open_gui();
            }
        }
        // The watcher finished on its own (stop from outside, or an error) → end the tray
        // loop too; `run_tray` joins and returns the watcher's result.
        //
        // `ControlFlow::WaitUntil` makes the loop wake at least once a second even with no
        // OS message, so a watcher that dies on its own (store error, bad disk) is noticed
        // here instead of leaving a zombie tray icon until the next click.
        if self.watcher.is_finished() {
            event_loop.exit();
        } else {
            event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now() + Duration::from_secs(1)));
        }
    }
}

/// Spawn `lh-gui.exe` lying next to the daemon executable, detached (no console, own process
/// group — same flags as the CLI's `lh ui`). Missing GUI → log + skip, never an error.
fn open_gui() {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            log::warn!("tray: không lấy được current_exe (bỏ qua mở GUI): {e}");
            return;
        }
    };
    let Some(dir) = exe.parent() else {
        log::warn!("tray: exe không có thư mục cha (bỏ qua mở GUI)");
        return;
    };
    let gui = dir.join(format!("lh-gui{}", std::env::consts::EXE_SUFFIX));
    if !gui.exists() {
        log::warn!("tray: chưa có {} (bỏ qua mở GUI — build trước: cargo build -p gui)", gui.display());
        return;
    }
    let mut cmd = std::process::Command::new(&gui);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    match cmd.spawn() {
        Ok(child) => log::info!("tray: đã mở lh-gui (pid {})", child.id()),
        Err(e) => log::warn!("tray: spawn {} lỗi (bỏ qua): {e}", gui.display()),
    }
}

/// 32×32 RGBA tray icon built from the auto-generated data (`tray_icon_data`, same art as
/// the exe/window icon) — raw bytes, no image decoding.
fn icon_rgba() -> std::result::Result<Icon, BadIcon> {
    Icon::from_rgba(tray_icon_data::TRAY_ICON_RGBA_32.to_vec(), 32, 32)
}

/// Normalize "en"|"vi" (trimmed, case-insensitive); anything else → None.
fn normalize_lang(s: &str) -> Option<String> {
    match s.trim().to_ascii_lowercase().as_str() {
        "en" => Some("en".into()),
        "vi" => Some("vi".into()),
        _ => None,
    }
}

/// Locale for the menu labels — same rule as the CLI (`crates/cli/src/lib.rs::auto_lang`):
/// env `LH_LANG` (en|vi) wins, otherwise the OS locale starting with "vi" → vi, else en.
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

#[cfg(test)]
mod tests {
    use super::*;

    // ----- i18n -----

    #[test]
    fn i18n_tray_labels_vi_and_en() {
        assert_eq!(t!("tray.open_gui", locale = "vi"), "Mở GUI");
        assert_eq!(t!("tray.open_gui", locale = "en"), "Open GUI");
        assert_eq!(t!("tray.pause_watch", locale = "vi"), "Tạm dừng watch");
        assert_eq!(t!("tray.pause_watch", locale = "en"), "Pause watch");
        assert_eq!(t!("tray.resume_watch", locale = "vi"), "Tiếp tục watch");
        assert_eq!(t!("tray.resume_watch", locale = "en"), "Resume watch");
        assert_eq!(t!("tray.gc_now", locale = "vi"), "Dọn lịch sử");
        assert_eq!(t!("tray.gc_now", locale = "en"), "Clean up history");
        assert_eq!(t!("tray.quit", locale = "vi"), "Thoát");
        assert_eq!(t!("tray.quit", locale = "en"), "Quit");
        // tooltip says how to use the icon, in both locales
        assert!(t!("tray.tooltip", locale = "en").contains("double-click"), "{}", t!("tray.tooltip", locale = "en"));
        assert!(t!("tray.tooltip", locale = "vi").contains("nhấp đúp"), "{}", t!("tray.tooltip", locale = "vi"));
    }

    #[test]
    fn normalize_lang_accepts_en_vi_rejects_other() {
        assert_eq!(normalize_lang("en").as_deref(), Some("en"));
        assert_eq!(normalize_lang(" VI ").as_deref(), Some("vi"));
        assert_eq!(normalize_lang("fr"), None);
        assert_eq!(normalize_lang(""), None);
    }

    #[test]
    fn auto_lang_without_env_is_en_or_vi() {
        // The OS locale is outside test control — pin the allowed outcomes only.
        assert!(matches!(auto_lang().as_str(), "en" | "vi"));
    }

    /// set_locale is process-global state — lock so tests that depend on the default locale
    /// (here: the test below asserts "en"/"vi" only, but the lock keeps intent explicit).
    static LOCALE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn locale_lookup_without_explicit_locale_falls_back() {
        let _guard = LOCALE_TEST_LOCK.lock().unwrap();
        // An unknown locale falls back to en (fallback = "en" of the i18n! macro).
        rust_i18n::set_locale("fr");
        assert_eq!(t!("tray.quit"), "Quit");
        rust_i18n::set_locale("en"); // leave a known state for other tests
    }

    #[test]
    fn i18n_yml_key_parity_and_cover_t_calls() {
        let en = yml_keys(include_str!("../locales/en.yml"));
        let vi = yml_keys(include_str!("../locales/vi.yml"));

        // no duplicate keys within one file (YAML silently swallows duplicates)
        for (name, keys) in [("en", &en), ("vi", &vi)] {
            let mut seen = std::collections::BTreeSet::new();
            for k in keys {
                assert!(seen.insert(k), "duplicate key `{k}` in {name}.yml");
            }
        }
        assert!(!en.is_empty(), "en.yml defines no keys");
        assert_eq!(
            en.len(),
            vi.len(),
            "en key count ({}) != vi key count ({})",
            en.len(),
            vi.len()
        );
        let only_en: Vec<&String> = en.iter().filter(|k| !vi.contains(*k)).collect();
        let only_vi: Vec<&String> = vi.iter().filter(|k| !en.contains(*k)).collect();
        assert!(
            only_en.is_empty() && only_vi.is_empty(),
            "en/vi key sets differ — en only: {only_en:?} · vi only: {only_vi:?}"
        );

        // every t! key used in tray.rs must be defined in the yml files (no missed translation)
        for key in t_keys_in_source(include_str!("tray.rs")) {
            assert!(
                en.contains(&key),
                "key `{key}` used in tray.rs but missing from en.yml"
            );
        }
    }

    /// Minimal parser for the locales/*.yml format of this crate: one YAML tree per file
    /// (locale = file stem), nested maps flatten into dotted keys, values are single-line
    /// double-quoted strings. Returns full "a.b.c" keys in file order. (Ported from
    /// crates/cli/src/lib.rs — keep the two in sync if the format changes.)
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
                panic!("unexpected YAML line (missing ':'): {line}");
            };
            let k = k.trim();
            let v = v.trim();
            while stack.last().is_some_and(|(i, _)| *i >= indent) {
                stack.pop();
            }
            if v.starts_with('"') {
                assert!(
                    v.len() >= 2 && v.ends_with('"'),
                    "value of `{k}` must be a quoted single-line string: {line}"
                );
                let path: Vec<&str> = stack.iter().map(|(_, s)| *s).chain([k]).collect();
                keys.push(path.join("."));
            } else {
                stack.push((indent, k));
            }
        }
        keys
    }

    /// Scan the source for keys used through the translation macro (the two-char call "t" +
    /// "!(") — skip comment lines, accept only namespaced keys (alphanumeric/underscore,
    /// with a dot). Standard macros (`assert!`, `print!`...) also contain a "t" + "!("
    /// substring — the strict key filter keeps them out.
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
