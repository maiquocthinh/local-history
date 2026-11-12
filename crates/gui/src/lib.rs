//! `gui` — local-history desktop viewer library (egui/eframe, dark theme).
//!
//! Consumers:
//! - the thin `lh-gui` binary in this crate (`src/main.rs`), and
//! - the multi-call `lh` binary (crate `cli`), which embeds this library and
//!   runs the GUI in-process via [`run`].
//!
//! Usage: `run(parse_args(...).file.as_deref().map(normalize_user_path))` — a
//! `Some` file opens straight into that file's timeline (the contract for
//! editors launching the GUI).
//!
//! i18n: rust-i18n, locales in `locales/{en,vi}.yml` (locale = file name),
//! fallback "en". The language is chosen via the `"lang"` field of
//! gui-state.json ("auto"|"en"|"vi").

pub mod app;
pub mod diff_view;
pub mod icons;
pub mod state;

rust_i18n::i18n!("locales", fallback = "en");

use std::path::{Path, PathBuf};

use eframe::egui;

/// Command-line arguments. Only `--file <path>` is recognized; anything else
/// is ignored.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Args {
    /// Raw `--file` value, if present; normalized before use.
    pub file: Option<PathBuf>,
}

/// Parses raw argument strings (typically `env::args().skip(1)`) into [`Args`].
pub fn parse_args<I: Iterator<Item = String>>(mut args: I) -> Args {
    let mut out = Args::default();
    while let Some(a) = args.next() {
        if a == "--file" {
            out.file = args.next().map(PathBuf::from);
        }
    }
    out
}

/// Canonicalizes the path when possible (falling back to the input as-is), then
/// strips the Windows `\\?\` prefix (UNC `\\?\UNC\...` becomes `\\...`) so paths
/// display and match in a friendly form.
pub fn normalize_user_path(p: &Path) -> PathBuf {
    let canonical = p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    let s = canonical.as_os_str().to_string_lossy().to_string();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{rest}"))
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        canonical
    }
}

/// Window/taskbar icon: `assets/icon-128.png` decoded at startup. The exe
/// resource icon is separate — see build.rs.
fn window_icon() -> Option<egui::IconData> {
    let img = image::load_from_memory(include_bytes!("../assets/icon-128.png")).ok()?;
    let rgba = img.to_rgba8().into_raw();
    Some(egui::IconData { width: img.width(), height: img.height(), rgba })
}

/// Runs the GUI until the window is closed. `initial_file` (already normalized
/// via [`normalize_user_path`]) opens straight into that file's timeline;
/// `None` restores the last state from gui-state.json.
pub fn run(initial_file: Option<PathBuf>) -> eframe::Result<()> {
    let default_root = history::LocalHistory::default_root()
        .unwrap_or_else(|_| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

    let mut viewport = egui::ViewportBuilder::default().with_inner_size([1280.0, 820.0]);
    if let Some(icon) = window_icon() {
        viewport = viewport.with_icon(icon);
    }
    let options = eframe::NativeOptions {
        viewport,
        // Force the Glow (OpenGL) renderer: wgpu/DX12 hits an ACCESS_VIOLATION
        // during init on machines with limited GPU drivers or in VMs
        // (crash 0xC0000005 with empty stdout/stderr).
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };
    eframe::run_native(
        "local-history",
        options,
        Box::new(move |cc| {
            cc.egui_ctx.set_theme(egui::Theme::Dark);
            icons::install(&cc.egui_ctx);
            Ok(Box::new(app::App::new(default_root, initial_file)))
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_i18n::t;

    #[test]
    fn parse_args_file() {
        let a = parse_args(["--file", r"D:\x\y.rs"].into_iter().map(String::from));
        assert_eq!(a.file, Some(PathBuf::from(r"D:\x\y.rs")));
        let a = parse_args(std::iter::empty());
        assert_eq!(a, Args::default());
        let a = parse_args(["--other", "1", "--file"].into_iter().map(String::from));
        assert_eq!(a, Args::default(), "--file without a value → None");
    }

    #[test]
    fn normalize_keeps_missing_path_and_strips_prefix() {
        // nonexistent path → returned as-is
        let p = normalize_user_path(Path::new(r"D:\does\not\exist.rs"));
        assert_eq!(p, PathBuf::from(r"D:\does\not\exist.rs"));
        // a real crate path → canonicalized + `\\?\` prefix stripped
        let p = normalize_user_path(Path::new(env!("CARGO_MANIFEST_DIR")));
        let s = p.to_string_lossy();
        assert!(!s.starts_with(r"\\?\"), "must strip the \\\\?\\ prefix: {s}");
        assert!(p.ends_with("gui"));
    }

    /// `set_locale` is process-global state — this lock keeps locale-switching
    /// tests from racing tests that read messages.
    static LOCALE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn i18n_lookup_vi_and_en() {
        // explicit-locale lookups — process-global state untouched
        assert_eq!(t!("status.ready", locale = "vi"), "Sẵn sàng.");
        assert_eq!(t!("status.ready", locale = "en"), "Ready.");
        assert_eq!(
            t!("diff.mode_disk", locale = "vi"),
            "với nội dung disk hiện tại"
        );
        assert_eq!(
            t!("diff.mode_disk", locale = "en"),
            "with current disk content"
        );
        assert_eq!(t!("day.today", locale = "vi"), "Hôm nay");
        assert_eq!(t!("day.today", locale = "en"), "Today");
        assert_eq!(
            t!("msg.reverted_pre", locale = "vi", file = "a.rs", seq = 3, pre = 4),
            "đã revert 'a.rs' về seq 3 (pre_revert: seq 4)"
        );
        assert_eq!(
            t!("msg.reverted_pre", locale = "en", file = "a.rs", seq = 3, pre = 4),
            "reverted 'a.rs' to seq 3 (pre_revert: seq 4)"
        );
    }

    #[test]
    fn i18n_invalid_locale_falls_back_to_en() {
        let _guard = LOCALE_TEST_LOCK.lock().unwrap();
        // a locale absent from locales/*.yml falls back to "en"
        rust_i18n::set_locale("fr");
        assert_eq!(t!("panel.search_hint"), "Search files…");
        // the real App::new/set_lang path: set_locale("vi") then an unannotated lookup
        rust_i18n::set_locale("vi");
        assert_eq!(t!("panel.search_hint"), "Tìm file…");
        rust_i18n::set_locale("en"); // restore the default for other tests
    }

    #[test]
    fn i18n_yml_key_parity_and_cover_t_calls() {
        let en = yml_map(include_str!("../locales/en.yml"));
        let vi = yml_map(include_str!("../locales/vi.yml"));

        // no duplicate keys within one file (YAML silently swallows duplicates)
        for (name, map) in [("en", &en), ("vi", &vi)] {
            let mut seen = std::collections::BTreeSet::new();
            for (k, _) in map {
                assert!(seen.insert(k.clone()), "key `{k}` is duplicated in {name}.yml");
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
        let only_en: Vec<&String> = en.iter().map(|(k, _)| k).filter(|k| !vi.iter().any(|(k2, _)| k2 == *k)).collect();
        let only_vi: Vec<&String> = vi.iter().map(|(k, _)| k).filter(|k| !en.iter().any(|(k2, _)| k2 == *k)).collect();
        assert!(
            only_en.is_empty() && only_vi.is_empty(),
            "yml key sets diverge — only in en: {only_en:?} · only in vi: {only_vi:?}"
        );

        // every key translated in gui source must be defined in the yml, and the
        // t!() ARGUMENT NAMES must match the %{name} placeholders exactly
        // (a name mismatch leaves the placeholder unsubstituted, rendering "%{...}").
        let sources = [
            include_str!("main.rs"),
            include_str!("lib.rs"),
            include_str!("app.rs"),
            include_str!("state.rs"),
            include_str!("diff_view.rs"),
            include_str!("icons.rs"),
        ];
        for src in sources {
            for (key, args) in t_calls_in_source(src) {
                let value = |map: &[(String, String)], locale: &str| -> String {
                    map.iter()
                        .find(|(k, _)| k == &key)
                        .map(|(_, v)| v.clone())
                        .unwrap_or_else(|| {
                            panic!("key `{key}` used in source but missing from {locale}.yml")
                        })
                };
                let passed: std::collections::BTreeSet<String> =
                    args.iter().cloned().collect();
                let want_en = placeholders_of(&value(&en, "en"), &key, "en");
                let want_vi = placeholders_of(&value(&vi, "vi"), &key, "vi");
                assert_eq!(
                    &passed, &want_en,
                    "key `{key}`: t!() args {passed:?} mismatch en.yml placeholders {want_en:?}"
                );
                assert_eq!(
                    &passed, &want_vi,
                    "key `{key}`: t!() args {passed:?} mismatch vi.yml placeholders {want_vi:?}"
                );
            }
        }
    }

    /// Minimal parser matching this crate's locales/*.yml format: each file is one
    /// YAML tree (locale = file name), nested maps (namespace → group → key), values
    /// are double-quoted strings on a single line. Returns (full "a.b.c" key, value)
    /// in file order.
    fn yml_map(yml: &str) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = Vec::new();
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
                    "value of `{k}` must be a single-line quoted string: {line}"
                );
                let path: Vec<&str> = stack.iter().map(|(_, s)| *s).chain([k]).collect();
                out.push((path.join("."), v[1..v.len() - 1].to_string()));
            } else {
                stack.push((indent, k));
            }
        }
        out
    }

    /// Scans source for every translation call shaped like `t!("key", name = expr, ...)`,
    /// returning (key, argument names in order of appearance). Comment lines are
    /// skipped; only namespaced keys (alphanumerics, underscores, dashes, with a dot)
    /// count. Other macros (`assert!`, `format!`, ...) also contain the text "t!(" —
    /// parsing proceeds only when a quoted key-shaped string follows `t!(`.
    fn t_calls_in_source(src: &str) -> Vec<(String, Vec<String>)> {
        const PREFIX: &str = "t!(";
        let mut out = Vec::new();
        for (idx, _) in src.match_indices(PREFIX) {
            let line_start = src[..idx].rfind('\n').map_or(0, |p| p + 1);
            if src[line_start..idx].trim_start().starts_with("//") {
                continue;
            }
            let bytes = src.as_bytes();
            let mut i = idx + PREFIX.len();
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            if i >= bytes.len() || bytes[i] != b'"' {
                continue; // assert!/format!/... — not a t! call with a literal key
            }
            let key_start = i + 1;
            let Some(key_end) = src[key_start..].find('"') else {
                continue;
            };
            let key_end = key_start + key_end;
            let key = &src[key_start..key_end];
            if key.is_empty()
                || !key.contains('.')
                || !key
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
            {
                continue;
            }
            // scan to the closing ')' at depth 0, skipping quoted string contents
            let mut args = Vec::new();
            let mut depth = 0usize;
            let mut seg_start = key_end + 1;
            let mut in_str = false;
            let mut j = key_end + 1;
            while j < bytes.len() {
                let c = bytes[j] as char;
                if in_str {
                    if c == '\\' {
                        j += 2;
                        continue;
                    }
                    if c == '"' {
                        in_str = false;
                    }
                } else {
                    match c {
                        '"' => in_str = true,
                        '(' | '[' => depth += 1,
                        ')' if depth == 0 => break,
                        ')' | ']' => depth -= 1,
                        ',' if depth == 0 => {
                            // segment between top-level commas
                            let seg = src[seg_start..j].trim();
                            if !seg.is_empty()
                                && let Some(name) = top_level_arg_name(seg)
                            {
                                args.push(name);
                            }
                            seg_start = j + 1;
                        }
                        _ => {}
                    }
                }
                j += 1;
            }
            let seg = src[seg_start..j.min(src.len())].trim();
            if !seg.is_empty() && let Some(name) = top_level_arg_name(seg) {
                args.push(name);
            }
            out.push((key.to_string(), args));
        }
        out
    }

    /// Argument name at the start of a `name = expr` segment (skips `locale` — a macro
    /// parameter, not a placeholder). Returns None for segments without the `name =` shape.
    fn top_level_arg_name(seg: &str) -> Option<String> {
        let eq = seg.find('=')?;
        let name = seg[..eq].trim();
        // `==` is a comparison, not an argument assignment
        if seg[eq..].starts_with("==") || name.is_empty() {
            return None;
        }
        if !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return None;
        }
        if name == "locale" {
            return None;
        }
        Some(name.to_string())
    }

    /// Set of `%{name}` placeholders inside one yml value.
    fn placeholders_of(value: &str, key: &str, locale: &str)
    -> std::collections::BTreeSet<String> {
        let mut out = std::collections::BTreeSet::new();
        let mut i = 0;
        while let Some(p) = value[i..].find("%{") {
            let start = i + p + 2;
            let Some(end) = value[start..].find('}') else {
                panic!("unclosed placeholder in `{key}` ({locale}.yml)");
            };
            let name = &value[start..start + end];
            assert!(
                !name.is_empty()
                    && name
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_'),
                "malformed placeholder in `{key}` ({locale}.yml)"
            );
            out.insert(name.to_string());
            i = start + end;
        }
        out
    }
}
