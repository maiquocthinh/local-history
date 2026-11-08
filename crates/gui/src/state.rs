//! Trạng thái GUI (gui-state.json) + các hàm tiện ích THUẦN: chia dòng, format thời gian,
//! nhóm theo ngày, human size, màu kind, múi giờ local.
//!
//! Phần thuần tách riêng để unit test được (UI trong `app` không test tự động).

use std::path::{Path, PathBuf};

use eframe::egui;
use history::EventKind;
use rust_i18n::t;
use serde::{Deserialize, Serialize};
use time::{Date, Duration, OffsetDateTime, UtcOffset};

// ---------------------------------------------------------------------------
// Trạng thái lưu bền + ngôn ngữ
// ---------------------------------------------------------------------------

/// Trạng thái GUI ghi ra `<store_root>/gui-state.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuiState {
    /// Store root dùng lần trước (tùy chọn — mặc định là `default_root()` của engine).
    pub last_store_root: Option<String>,
    /// Workspace đang xem lần trước (canonical root).
    pub last_ws_root: Option<String>,
    /// Ngôn ngữ UI: "auto" | "en" | "vi" (None = file cũ → auto).
    pub lang: Option<String>,
}

/// Sở thích ngôn ngữ người dùng — lưu trong gui-state.json (trường `"lang"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LangPref {
    Auto,
    En,
    Vi,
}

impl LangPref {
    /// Parse giá trị trong state (None / giá trị lạ → Auto).
    pub fn parse(s: Option<&str>) -> Self {
        match s {
            Some("en") => LangPref::En,
            Some("vi") => LangPref::Vi,
            _ => LangPref::Auto,
        }
    }

    /// Chuỗi lưu vào gui-state.json.
    pub fn as_str(self) -> &'static str {
        match self {
            LangPref::Auto => "auto",
            LangPref::En => "en",
            LangPref::Vi => "vi",
        }
    }
}

/// Resolve sở thích → locale thực tế ("en"|"vi"). Auto: sys locale bắt đầu "vi" → vi,
/// còn lại en (giống `auto_lang` của CLI nhưng không có override env/flag).
pub fn resolve_lang(pref: LangPref, sys_locale: Option<&str>) -> &'static str {
    match pref {
        LangPref::En => "en",
        LangPref::Vi => "vi",
        LangPref::Auto => match sys_locale {
            Some(l) if l.to_ascii_lowercase().starts_with("vi") => "vi",
            _ => "en",
        },
    }
}

/// Đường dẫn file trạng thái của một store root.
pub fn state_path(store_root: &Path) -> PathBuf {
    store_root.join("gui-state.json")
}

/// Đọc trạng thái; lỗi đọc/parse → trả về mặc định (không panic).
pub fn load_state(path: &Path) -> GuiState {
    match std::fs::read_to_string(path) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => GuiState::default(),
    }
}

/// Ghi trạng thái; lỗi → String để đưa vào status bar.
pub fn save_state(path: &Path, state: &GuiState) -> Result<(), String> {
    let json = serde_json::to_string_pretty(state).map_err(|e| e.to_string())?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(path, json).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// Thời gian / size
// ---------------------------------------------------------------------------

/// Giờ:phút của một OffsetDateTime, "HH:MM".
pub fn hhmm(dt: OffsetDateTime) -> String {
    format!("{:02}:{:02}", dt.hour(), dt.minute())
}

/// Ngày dạng "dd/MM/yyyy".
pub fn date_str(d: Date) -> String {
    format!("{:02}/{:02}/{:04}", d.day(), u8::from(d.month()), d.year())
}

/// unix micros → OffsetDateTime theo `offset` (bỏ phần micro giây).
/// Dùng `to_offset` (giữ NGUYÊN instant, chỉ đổi múi giờ hiển thị) — KHÔNG dùng
/// `replace_offset` (API đó giữ giờ hiển thị nhưng ĐỔI instant ⇒ lệch absolute time).
pub fn unix_micros_to_local(micros: i64, offset: UtcOffset) -> OffsetDateTime {
    let secs = micros.div_euclid(1_000_000);
    OffsetDateTime::from_unix_timestamp(secs)
        .unwrap_or(OffsetDateTime::UNIX_EPOCH)
        .to_offset(offset)
}

/// Thời gian "human" so với `now_micros`: "vừa xong" → phút → giờ → ngày → ngày tuyệt đối.
/// Chuỗi qua i18n (`time.*`) — locale do rust_i18n đặt toàn cục (App::new / set_lang).
/// n == 1 dùng key `_one` (singular: "1 hour ago" thay vì "1 hours ago").
pub fn format_relative_time(micros: i64, now_micros: i64, offset: UtcOffset) -> String {
    let delta = ((now_micros - micros) / 1_000_000).max(0);
    const MIN: i64 = 60;
    const HOUR: i64 = 60 * MIN;
    const DAY: i64 = 24 * HOUR;
    if delta < MIN {
        t!("time.now").to_string()
    } else if delta < HOUR {
        let n = delta / MIN;
        if n == 1 {
            t!("time.minutes_ago_one", n = n).to_string()
        } else {
            t!("time.minutes_ago", n = n).to_string()
        }
    } else if delta < DAY {
        let n = delta / HOUR;
        if n == 1 {
            t!("time.hours_ago_one", n = n).to_string()
        } else {
            t!("time.hours_ago", n = n).to_string()
        }
    } else if delta < 7 * DAY {
        let n = delta / DAY;
        if n == 1 {
            t!("time.days_ago_one", n = n).to_string()
        } else {
            t!("time.days_ago", n = n).to_string()
        }
    } else {
        date_str(unix_micros_to_local(micros, offset).date())
    }
}

/// Header nhóm timeline theo ngày: "Hôm nay" / "Hôm qua" / "dd/MM/yyyy" (qua i18n `day.*`).
pub fn day_header(date: Date, today: Date) -> String {
    if date == today {
        t!("day.today").to_string()
    } else if today.checked_sub(Duration::days(1)).is_some_and(|y| y == date) {
        t!("day.yesterday").to_string()
    } else {
        date_str(date)
    }
}

/// Một nhóm version cùng ngày trong timeline.
pub struct DayGroup<T> {
    pub header: String,
    pub items: Vec<T>,
}

/// Gom các item (đã sắp theo thứ tự hiển thị) thành nhóm liên tiếp cùng `Date`,
/// header theo `today`. Thứ tự item giữ nguyên trong nhóm.
pub fn group_by_day<T>(items: Vec<(Date, T)>, today: Date) -> Vec<DayGroup<T>> {
    let mut out: Vec<(Date, DayGroup<T>)> = Vec::new();
    for (date, item) in items {
        match out.last_mut() {
            Some((d, _)) if *d == date => {
                out.last_mut().unwrap().1.items.push(item);
            }
            _ => out.push((
                date,
                DayGroup { header: day_header(date, today), items: vec![item] },
            )),
        }
    }
    out.into_iter().map(|(_, g)| g).collect()
}

/// Kích thước human: "7 B", "1.5 KB", "10.0 MB".
pub fn human_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    let b = bytes as f64;
    if b < KB {
        format!("{bytes} B")
    } else if b < MB {
        format!("{:.1} KB", b / KB)
    } else {
        format!("{:.1} MB", b / MB)
    }
}

// ---------------------------------------------------------------------------
// Kind — badge + màu
// ---------------------------------------------------------------------------

/// Màu badge theo kind: external=cam, save=xanh lá, delete=đỏ, restore=xanh dương,
/// pre_revert=vàng, label=tím, rename=xám.
pub fn kind_color(kind: EventKind) -> egui::Color32 {
    match kind {
        EventKind::Save => egui::Color32::from_rgb(80, 200, 120),
        EventKind::External => egui::Color32::from_rgb(235, 150, 60),
        EventKind::Delete => egui::Color32::from_rgb(230, 90, 80),
        EventKind::Restore => egui::Color32::from_rgb(80, 160, 235),
        EventKind::PreRevert => egui::Color32::from_rgb(235, 210, 80),
        EventKind::Label => egui::Color32::from_rgb(190, 120, 235),
        EventKind::Rename => egui::Color32::from_rgb(160, 160, 160),
    }
}

/// Danh sách mọi kind theo thứ tự hiển thị trong filter.
pub const ALL_KINDS: [EventKind; 7] = [
    EventKind::Save,
    EventKind::External,
    EventKind::Delete,
    EventKind::Restore,
    EventKind::PreRevert,
    EventKind::Rename,
    EventKind::Label,
];

// ---------------------------------------------------------------------------
// Múi giờ local — không dùng feature "local-offset" của time (workspace không bật
// và không được sửa Cargo.toml). Windows: đọc bias qua kernel32 GetTimeZoneInformation;
// nền tảng khác → fallback UTC.
// ---------------------------------------------------------------------------

/// Múi giờ local hiện tại (UTC fallback khi đọc thất bại).
pub fn local_utc_offset() -> UtcOffset {
    system_utc_offset().unwrap_or(UtcOffset::UTC)
}

/// Thời điểm hiện tại theo múi giờ local (`to_offset` — instant giữ nguyên).
pub fn local_now() -> OffsetDateTime {
    OffsetDateTime::now_utc().to_offset(local_utc_offset())
}

#[cfg(windows)]
const TZ_STANDARD: u32 = 1;
#[cfg(windows)]
const TZ_DAYLIGHT: u32 = 2;

/// Offset UTC→local (phút) từ các bias của TIME_ZONE_INFORMATION.
/// Quy ước Win32: UTC = local + bias ⇒ local = UTC − bias (âm vì Việt Nam là UTC+7).
/// `result` = giá trị trả về của GetTimeZoneInformation (0/1/2).
#[cfg(windows)]
fn tz_offset_minutes(result: u32, bias: i32, standard_bias: i32, daylight_bias: i32) -> i32 {
    let extra = match result {
        TZ_DAYLIGHT => daylight_bias,
        TZ_STANDARD => standard_bias,
        _ => 0,
    };
    -(bias + extra)
}

#[cfg(windows)]
fn system_utc_offset() -> Option<UtcOffset> {
    /// SYSTEMTIME của Win32 — chỉ cần layout, không đọc nội dung.
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Systemtime {
        year: u16,
        month: u16,
        day_of_week: u16,
        day: u16,
        hour: u16,
        minute: u16,
        second: u16,
        millis: u16,
    }

    /// TIME_ZONE_INFORMATION của Win32 — layout phải khớp nguyên bản.
    #[repr(C)]
    struct TimeZoneInformation {
        bias: i32,
        standard_name: [u16; 32],
        standard_date: Systemtime,
        standard_bias: i32,
        daylight_name: [u16; 32],
        daylight_date: Systemtime,
        daylight_bias: i32,
    }

    unsafe extern "system" {
        fn GetTimeZoneInformation(tzi: *mut TimeZoneInformation) -> u32;
    }

    let zero = Systemtime {
        year: 0,
        month: 0,
        day_of_week: 0,
        day: 0,
        hour: 0,
        minute: 0,
        second: 0,
        millis: 0,
    };
    let mut tzi = TimeZoneInformation {
        bias: 0,
        standard_name: [0; 32],
        standard_date: zero,
        standard_bias: 0,
        daylight_name: [0; 32],
        daylight_date: zero,
        daylight_bias: 0,
    };
    // SAFETY: `tzi` là struct hợp lệ, layout khớp TIME_ZONE_INFORMATION của Win32.
    let result = unsafe { GetTimeZoneInformation(&mut tzi) };
    if result > TZ_DAYLIGHT {
        return None;
    }
    let minutes = tz_offset_minutes(result, tzi.bias, tzi.standard_bias, tzi.daylight_bias);
    // Múi giờ thật trên thế giới nằm trong ±14h — lệch khỏi vùng này nghĩa là dữ liệu rác.
    if !(-14 * 60..=14 * 60).contains(&minutes) {
        return None;
    }
    let hours = (minutes / 60) as i8;
    let rest = (minutes % 60) as i8;
    UtcOffset::from_hms(hours, rest, 0).ok()
}

#[cfg(not(windows))]
fn system_utc_offset() -> Option<UtcOffset> {
    None
}

// ---------------------------------------------------------------------------
// Chia dòng (bản của gui — `patch::split_lines` là pub(crate) của history)
// ---------------------------------------------------------------------------

/// Chia text thành các dòng: cắt sau mỗi `\n`, terminator giữ nguyên trong dòng
/// (giống ngữ nghĩa `model::Hunk`); dòng cuối không có `\n` vẫn là 1 dòng.
pub fn split_lines(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0usize;
    for (i, b) in s.bytes().enumerate() {
        if b == b'\n' {
            out.push(&s[start..=i]);
            start = i + 1;
        }
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

// ---------------------------------------------------------------------------
// Tests — phần thuần
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use time::Month;

    fn d(y: i32, m: u8, day: u8) -> Date {
        Date::from_calendar_date(y, Month::try_from(m).unwrap(), day).unwrap()
    }

    // ----- split_lines -----

    #[test]
    fn split_lines_basics() {
        assert!(split_lines("").is_empty());
        assert_eq!(split_lines("abc"), vec!["abc"]);
        assert_eq!(split_lines("abc\n"), vec!["abc\n"]);
        assert_eq!(split_lines("a\nb\nc"), vec!["a\n", "b\n", "c"]);
        // CRLF giữ nguyên terminator
        assert_eq!(split_lines("a\r\nb\r\n"), vec!["a\r\n", "b\r\n"]);
        assert_eq!(split_lines("\n\n"), vec!["\n", "\n"]);
    }

    // ----- human_size -----

    #[test]
    fn human_sizes() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1023), "1023 B");
        assert_eq!(human_size(1024), "1.0 KB");
        assert_eq!(human_size(1536), "1.5 KB");
        assert_eq!(human_size(1024 * 1024), "1.0 MB");
        assert_eq!(human_size(10 * 1024 * 1024), "10.0 MB");
    }

    // ----- format_relative_time -----

    /// set_locale là trạng thái toàn cục — khóa các test phụ thuộc chuỗi theo locale.
    static LOCALE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn relative_time_buckets() {
        let _guard = LOCALE_TEST_LOCK.lock().unwrap();
        rust_i18n::set_locale("vi");
        let now: i64 = 1_800_000_000_000_000; // một mốc nào đó
        let utc = UtcOffset::UTC;
        assert_eq!(format_relative_time(now - 30_000_000, now, utc), "vừa xong");
        assert_eq!(format_relative_time(now - 5 * 60_000_000, now, utc), "5 phút trước");
        assert_eq!(format_relative_time(now - 3 * 3_600_000_000, now, utc), "3 giờ trước");
        assert_eq!(format_relative_time(now - 2 * 86_400_000_000, now, utc), "2 ngày trước");
        // quá 7 ngày → ngày tuyệt đối dd/MM/yyyy (theo offset truyền vào)
        let old = now - 10 * 86_400_000_000;
        let expected = date_str(unix_micros_to_local(old, utc).date());
        assert_eq!(format_relative_time(old, now, utc), expected);
        // timestamp tương lai (clock lệch) → không âm
        assert_eq!(format_relative_time(now + 60_000_000, now, utc), "vừa xong");
        // cùng mốc theo locale en
        rust_i18n::set_locale("en");
        assert_eq!(format_relative_time(now - 30_000_000, now, utc), "just now");
        assert_eq!(format_relative_time(now - 5 * 60_000_000, now, utc), "5 minutes ago");
        assert_eq!(format_relative_time(now - 3 * 3_600_000_000, now, utc), "3 hours ago");
        assert_eq!(format_relative_time(now - 2 * 86_400_000_000, now, utc), "2 days ago");
        rust_i18n::set_locale("en"); // mặc định cho test khác
    }

    // ----- nhóm theo ngày -----

    #[test]
    fn day_headers() {
        let _guard = LOCALE_TEST_LOCK.lock().unwrap();
        rust_i18n::set_locale("vi");
        let today = d(2026, 11, 8);
        assert_eq!(day_header(today, today), "Hôm nay");
        assert_eq!(day_header(d(2026, 11, 7), today), "Hôm qua");
        assert_eq!(day_header(d(2026, 11, 1), today), "01/11/2026");
        // khác năm
        assert_eq!(day_header(d(2025, 12, 31), today), "31/12/2025");
        // locale en
        rust_i18n::set_locale("en");
        assert_eq!(day_header(today, today), "Today");
        assert_eq!(day_header(d(2026, 11, 7), today), "Yesterday");
        rust_i18n::set_locale("en");
    }

    #[test]
    fn group_by_day_consecutive() {
        let _guard = LOCALE_TEST_LOCK.lock().unwrap();
        rust_i18n::set_locale("vi");
        let today = d(2026, 11, 8);
        let items = vec![
            (today, "a"),
            (today, "b"),
            (d(2026, 11, 7), "c"),
            (d(2026, 11, 7), "d"),
            (d(2026, 11, 1), "e"),
        ];
        let groups = group_by_day(items, today);
        assert_eq!(groups.len(), 3);
        assert_eq!(groups[0].header, "Hôm nay");
        assert_eq!(groups[0].items, vec!["a", "b"]);
        assert_eq!(groups[1].header, "Hôm qua");
        assert_eq!(groups[1].items, vec!["c", "d"]);
        assert_eq!(groups[2].header, "01/11/2026");
        assert_eq!(groups[2].items, vec!["e"]);
        rust_i18n::set_locale("en");
    }

    // ----- ngôn ngữ (LangPref / resolve_lang) -----

    #[test]
    fn lang_pref_parse_and_str_roundtrip() {
        // None / giá trị lạ → Auto
        assert_eq!(LangPref::parse(None), LangPref::Auto);
        assert_eq!(LangPref::parse(Some("auto")), LangPref::Auto);
        assert_eq!(LangPref::parse(Some("rubbish")), LangPref::Auto);
        assert_eq!(LangPref::parse(Some("en")), LangPref::En);
        assert_eq!(LangPref::parse(Some("vi")), LangPref::Vi);
        // parse → as_str khớp giá trị ghi trong gui-state.json
        for (s, pref) in [
            ("auto", LangPref::Auto),
            ("en", LangPref::En),
            ("vi", LangPref::Vi),
        ] {
            assert_eq!(pref.as_str(), s);
            assert_eq!(LangPref::parse(Some(pref.as_str())), pref);
        }
    }

    #[test]
    fn resolve_lang_auto_by_sys_locale_and_override() {
        // override thắng tuyệt đối, bất kể sys locale
        assert_eq!(resolve_lang(LangPref::En, Some("vi-VN")), "en");
        assert_eq!(resolve_lang(LangPref::Vi, Some("en-US")), "vi");
        // auto: sys locale bắt đầu "vi" → vi
        assert_eq!(resolve_lang(LangPref::Auto, Some("vi")), "vi");
        assert_eq!(resolve_lang(LangPref::Auto, Some("vi-VN")), "vi");
        assert_eq!(resolve_lang(LangPref::Auto, Some("VI-vn")), "vi");
        // auto: còn lại → en
        assert_eq!(resolve_lang(LangPref::Auto, Some("en-US")), "en");
        assert_eq!(resolve_lang(LangPref::Auto, Some("fr-FR")), "en");
        assert_eq!(resolve_lang(LangPref::Auto, Some("ja-JP")), "en");
        // auto: không đọc được sys locale → en
        assert_eq!(resolve_lang(LangPref::Auto, None), "en");
    }

    #[test]
    fn gui_state_lang_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = state_path(dir.path());
        let mut st = GuiState::default();
        assert_eq!(st.lang, None, "state cũ không có lang → None");
        st.lang = Some("vi".into());
        save_state(&path, &st).unwrap();
        let loaded = load_state(&path);
        assert_eq!(loaded.lang.as_deref(), Some("vi"));
        assert_eq!(LangPref::parse(loaded.lang.as_deref()), LangPref::Vi);
        st.lang = Some("en".into());
        save_state(&path, &st).unwrap();
        assert_eq!(load_state(&path).lang.as_deref(), Some("en"));
    }

    // ----- gui-state.json -----

    #[test]
    fn state_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = state_path(dir.path());
        assert_eq!(load_state(&path), GuiState::default()); // chưa có file → default
        let st = GuiState {
            last_store_root: Some(r"C:\store".into()),
            last_ws_root: Some(r"D:\proj".into()),
            lang: Some("vi".into()),
        };
        save_state(&path, &st).unwrap();
        assert_eq!(load_state(&path), st);
        // file hỏng → default, không panic
        std::fs::write(&path, "{không phải json").unwrap();
        assert_eq!(load_state(&path), GuiState::default());
    }

    // ----- múi giờ -----

    #[cfg(windows)]
    #[test]
    fn tz_offset_math() {
        // Việt Nam UTC+7: bias = -420, không DST → +420 phút
        assert_eq!(tz_offset_minutes(TZ_STANDARD, -420, 0, -60), 420);
        // châu Âu DST: bias = -60, daylight_bias = -60 → +120
        assert_eq!(tz_offset_minutes(TZ_DAYLIGHT, -60, 0, -60), 120);
        // UTC+X quá nửa giờ: bias = -330 → +330 (5h30)
        assert_eq!(tz_offset_minutes(TZ_STANDARD, -330, 0, 0), 330);
        // UTC-8: bias = +480 → -480
        assert_eq!(tz_offset_minutes(TZ_STANDARD, 480, 0, 0), -480);
        // result unknown (0) → chỉ dùng bias thô
        assert_eq!(tz_offset_minutes(0, -420, 0, -60), 420);
        // hàm thật chạy được và cho offset hợp lý (±14h)
        let off = system_utc_offset();
        if let Some(off) = off {
            let s = off.as_hms();
            assert!((s.0 as i32).abs() <= 14);
        }
    }

    #[test]
    fn local_now_is_close_to_utc_now() {
        // local_now() phải là THỜI ĐIỂM hiện tại (đối chiếu đồng hồ hệ thống, dung sai 60s)
        // — bắt lỗi dùng nhầm replace_offset (giữ giờ hiển thị, lệch cả absolute time).
        let sys_nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as i128;
        let local = local_now();
        let delta = (local.unix_timestamp_nanos() - sys_nanos).abs();
        assert!(
            delta < 60 * 1_000_000_000i128,
            "local_now lệch đồng hồ hệ thống quá 60s: {delta} ns"
        );
    }

    /// unix_micros_to_local phải giữ nguyên instant (đối chiếu unix_timestamp) và dịch
    /// GIỜ HIỂN THỊ theo offset — hồi quy cho lỗi replace_offset (lệch múi giờ 7h).
    #[test]
    fn unix_micros_to_local_shifts_wall_keeps_instant() {
        let offset = UtcOffset::from_hms(7, 0, 0).unwrap();
        // mốc đã biết: 1_794_147_692 = 2026-11-08 14:21:32 UTC
        let micros = 1_794_147_692_403_787;
        let dt = unix_micros_to_local(micros, offset);
        // instant KHÔNG đổi
        assert_eq!(dt.unix_timestamp(), 1_794_147_692);
        // giờ hiển thị = UTC + 7h → 21:21
        assert_eq!(dt.hour(), 21, "giờ hiển thị phải là giờ local (UTC+7)");
        assert_eq!(dt.minute(), 21);
        assert_eq!(date_str(dt.date()), "08/11/2026");
        // offset UTC: wall = UTC
        let dt_utc = unix_micros_to_local(micros, UtcOffset::UTC);
        assert_eq!(dt_utc.hour(), 14);
        assert_eq!(dt_utc.minute(), 21);
        // hhmm qua 2 múi giờ khác nhau cho cùng 1 instant
        assert_ne!(hhmm(dt), hhmm(dt_utc));
    }
}
