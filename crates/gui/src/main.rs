//! lh-gui — trình xem lịch sử file local-history (egui/eframe), dark theme.
//!
//! Chạy: `lh-gui [--file <path>]` — `--file` nhảy thẳng tới timeline của file
//! (contract cho editor mở GUI).
//!
//! i18n: rust-i18n, locales = `locales/{en,vi}.yml` (locale = tên file), fallback "en".
//! Ngôn ngữ chọn qua trường `"lang"` trong gui-state.json ("auto"|"en"|"vi").

mod app;
mod diff_view;
mod icons;
mod state;

rust_i18n::i18n!("locales", fallback = "en");

use std::path::{Path, PathBuf};

use eframe::egui;

/// Args dòng lệnh — chỉ hỗ trợ `--file <path>`; tham số lạ bị bỏ qua.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Args {
    pub file: Option<PathBuf>,
}

pub fn parse_args<I: Iterator<Item = String>>(mut args: I) -> Args {
    let mut out = Args::default();
    while let Some(a) = args.next() {
        if a == "--file" {
            out.file = args.next().map(PathBuf::from);
        }
    }
    out
}

/// Canonicalize nếu được (không được thì giữ nguyên); bỏ prefix `\\?\` của Windows
/// (giữ UNC `\\?\UNC\...` → `\\...`) để hiển thị/match thân thiện.
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

fn main() -> eframe::Result<()> {
    let args = parse_args(
        std::env::args_os()
            .skip(1)
            .map(|a| a.to_string_lossy().into_owned()),
    );
    let initial_file = args.file.as_deref().map(normalize_user_path);
    let default_root = history::LocalHistory::default_root()
        .unwrap_or_else(|_| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1280.0, 820.0]),
        // Ép renderer Glow (OpenGL): wgpu/DX12 bị ACCESS_VIOLATION lúc khởi tạo trên
        // máy GPU driver hạn chế / VM (crash 0xC0000005, stdout/stderr rỗng).
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };
    eframe::run_native(
        "local-history",
        options,
        Box::new(move |cc| {
            // Dark theme mặc định
            cc.egui_ctx.set_theme(egui::Theme::Dark);
            // Material Icons làm font fallback (priority Lowest — chỉ dùng cho glyph icon)
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
        // tham số lạ bỏ qua
        let a = parse_args(["--other", "1", "--file"].into_iter().map(String::from));
        assert_eq!(a, Args::default(), "--file không có giá trị → None");
    }

    #[test]
    fn normalize_keeps_missing_path_and_strips_prefix() {
        // đường dẫn không tồn tại → giữ nguyên
        let p = normalize_user_path(Path::new(r"D:\không\tồn\tại.rs"));
        assert_eq!(p, PathBuf::from(r"D:\không\tồn\tại.rs"));
        // đường dẫn thật của crate → canonicalize + bỏ prefix \\?\
        let p = normalize_user_path(Path::new(env!("CARGO_MANIFEST_DIR")));
        let s = p.to_string_lossy();
        assert!(!s.starts_with(r"\\?\"), "phải bỏ prefix \\\\?\\: {s}");
        assert!(p.ends_with("gui"));
    }

    /// set_locale là trạng thái toàn cục của process — khóa để các test đổi locale không
    /// đua với test khác đang đọc message.
    static LOCALE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn i18n_lookup_vi_and_en() {
        // tra cứu có ghi rõ locale — không đụng trạng thái toàn cục
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
        // locale không tồn tại trong locales/*.yml → rơi về fallback "en"
        rust_i18n::set_locale("fr");
        assert_eq!(t!("panel.search_hint"), "Search files…");
        // đường đi thật của App::new/set_lang: set_locale("vi") rồi tra cứu không ghi rõ
        rust_i18n::set_locale("vi");
        assert_eq!(t!("panel.search_hint"), "Tìm file…");
        rust_i18n::set_locale("en"); // trả về mặc định cho test khác
    }

    #[test]
    fn i18n_yml_key_parity_and_cover_t_calls() {
        let en = yml_map(include_str!("../locales/en.yml"));
        let vi = yml_map(include_str!("../locales/vi.yml"));

        // không có key trùng trong cùng 1 file (YAML trùng key bị nuốt im lặng)
        for (name, map) in [("en", &en), ("vi", &vi)] {
            let mut seen = std::collections::BTreeSet::new();
            for (k, _) in map {
                assert!(seen.insert(k.clone()), "key `{k}` bị lặp trong {name}.yml");
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
        let only_en: Vec<&String> = en.iter().map(|(k, _)| k).filter(|k| !vi.iter().any(|(k2, _)| k2 == *k)).collect();
        let only_vi: Vec<&String> = vi.iter().map(|(k, _)| k).filter(|k| !en.iter().any(|(k2, _)| k2 == *k)).collect();
        assert!(
            only_en.is_empty() && only_vi.is_empty(),
            "2 file yml lệch tập key — chỉ có ở en: {only_en:?} · chỉ có ở vi: {only_vi:?}"
        );

        // mọi key được gọi dịch trong source của gui phải được định nghĩa trong yml,
        // và TÊN tham số truyền vào t!() phải khớp đúng placeholder %{name} trong yml
        // (lệch tên ⇒ placeholder không được thay, UI hiện nguyên văn "%{...}").
        let sources = [
            include_str!("main.rs"),
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
                            panic!("key `{key}` được gọi trong source nhưng không có trong {locale}.yml")
                        })
                };
                let passed: std::collections::BTreeSet<String> =
                    args.iter().cloned().collect();
                let want_en = placeholders_of(&value(&en, "en"), &key, "en");
                let want_vi = placeholders_of(&value(&vi, "vi"), &key, "vi");
                assert_eq!(
                    &passed, &want_en,
                    "key `{key}`: tham số t!() {passed:?} lệch placeholder en.yml {want_en:?}"
                );
                assert_eq!(
                    &passed, &want_vi,
                    "key `{key}`: tham số t!() {passed:?} lệch placeholder vi.yml {want_vi:?}"
                );
            }
        }
    }

    /// Parser tối giản đúng định dạng locales/*.yml của crate này: mỗi file là 1 cây YAML
    /// (locale = tên file), map lồng nhiều cấp (namespace → nhóm → key), value là chuỗi
    /// double-quoted nằm trọn 1 dòng. Trả về (key đầy đủ "a.b.c", value) theo thứ tự file.
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
                out.push((path.join("."), v[1..v.len() - 1].to_string()));
            } else {
                stack.push((indent, k));
            }
        }
        out
    }

    /// Quét source lấy mọi lệnh gọi macro dịch dạng `t!("key", name = expr, ...)`
    /// — trả về (key, danh sách tên tham số theo thứ tự xuất hiện).
    /// Bỏ qua dòng comment; chỉ nhận key namespaced (chữ/số/gạch dưới, có dấu chấm).
    /// Nhiều macro chuẩn (`assert!`, `format!`...) cũng chứa chuỗi "t!(" — chỉ tiến
    /// hành parse khi ngay sau `t!(` là một chuỗi quoted khớp hình dạng key.
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
                continue; // assert!(/format!/... — không phải lệnh t! với key literal
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
            // quét tới dấu ')' đóng ở depth 0 — bỏ qua nội dung chuỗi quoted
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
                        ')' if depth == 0 => break, // đóng macro call
                        ')' | ']' => depth -= 1,
                        ',' if depth == 0 => {
                            // segment giữa các dấu phẩy top-level
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

    /// Tên tham số ở đầu một segment `name = expr` (bỏ qua `locale` — tham số của macro,
    /// không phải placeholder). Trả None cho segment không có dạng `name =`.
    fn top_level_arg_name(seg: &str) -> Option<String> {
        let eq = seg.find('=')?;
        let name = seg[..eq].trim();
        // `==` so sánh — không phải gán tham số
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

    /// Tập placeholder `%{name}` trong một giá trị yml.
    fn placeholders_of(value: &str, key: &str, locale: &str)
    -> std::collections::BTreeSet<String> {
        let mut out = std::collections::BTreeSet::new();
        let mut i = 0;
        while let Some(p) = value[i..].find("%{") {
            let start = i + p + 2;
            let Some(end) = value[start..].find('}') else {
                panic!("placeholder không đóng trong `{key}` ({locale}.yml)");
            };
            let name = &value[start..start + end];
            assert!(
                !name.is_empty()
                    && name
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_'),
                "placeholder kỳ lạ trong `{key}` ({locale}.yml): %{name}"
            );
            out.insert(name.to_string());
            i = start + end;
        }
        out
    }
}
