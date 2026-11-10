//! GUI state (gui-state.json) plus pure helpers: line splitting, time formatting,
//! day grouping, human-readable sizes, kind colors, local time zone.
//!
//! The pure helpers live here so they can be unit-tested (the `app` UI is not
//! automatically tested).

use std::path::{Path, PathBuf};

use eframe::egui;
use history::EventKind;
use rust_i18n::t;
use serde::{Deserialize, Serialize};
use time::{Date, Duration, OffsetDateTime, UtcOffset};

// ---------------------------------------------------------------------------
// Persisted state + language
// ---------------------------------------------------------------------------

/// GUI state persisted to `<store_root>/gui-state.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuiState {
    /// Store root used in the previous session; absent means the engine's
    /// `default_root()`.
    pub last_store_root: Option<String>,
    /// Workspace viewed last (canonical root path).
    pub last_ws_root: Option<String>,
    /// UI language: "auto" | "en" | "vi"; `None` means an old state file
    /// (treated as "auto").
    pub lang: Option<String>,
}

/// The user's language preference, stored as the `"lang"` field of gui-state.json.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LangPref {
    /// Resolve from the system locale.
    Auto,
    /// English.
    En,
    /// Vietnamese.
    Vi,
}

impl LangPref {
    /// Parses the stored value; missing or unknown values map to [`LangPref::Auto`].
    pub fn parse(s: Option<&str>) -> Self {
        match s {
            Some("en") => LangPref::En,
            Some("vi") => LangPref::Vi,
            _ => LangPref::Auto,
        }
    }

    /// Value stored in gui-state.json.
    pub fn as_str(self) -> &'static str {
        match self {
            LangPref::Auto => "auto",
            LangPref::En => "en",
            LangPref::Vi => "vi",
        }
    }
}

/// Resolves the preference to an actual locale ("en"|"vi"). `Auto` picks "vi"
/// when the system locale starts with "vi", else "en" (like the CLI's
/// `auto_lang`, without env/flag overrides).
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

/// Path of the state file inside a store root.
pub fn state_path(store_root: &Path) -> PathBuf {
    store_root.join("gui-state.json")
}

/// Loads the state; any read or parse error yields the default (never panics).
pub fn load_state(path: &Path) -> GuiState {
    match std::fs::read_to_string(path) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => GuiState::default(),
    }
}

/// Saves the state; errors are returned as a message string for the status bar.
pub fn save_state(path: &Path, state: &GuiState) -> Result<(), String> {
    let json = serde_json::to_string_pretty(state).map_err(|e| e.to_string())?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(path, json).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// Time / size
// ---------------------------------------------------------------------------

/// Formats the wall-clock time as "HH:MM".
pub fn hhmm(dt: OffsetDateTime) -> String {
    format!("{:02}:{:02}", dt.hour(), dt.minute())
}

/// Formats a date as "dd/MM/yyyy".
pub fn date_str(d: Date) -> String {
    format!("{:02}/{:02}/{:04}", d.day(), u8::from(d.month()), d.year())
}

/// Converts unix micros to an `OffsetDateTime` in `offset` (sub-microsecond
/// precision dropped). Uses `to_offset`, which keeps the instant and only shifts
/// the displayed wall clock — `replace_offset` would keep the wall clock but
/// change the instant (wrong absolute time).
pub fn unix_micros_to_local(micros: i64, offset: UtcOffset) -> OffsetDateTime {
    let secs = micros.div_euclid(1_000_000);
    OffsetDateTime::from_unix_timestamp(secs)
        .unwrap_or(OffsetDateTime::UNIX_EPOCH)
        .to_offset(offset)
}

/// Human-readable age relative to `now_micros`: "just now" → minutes → hours →
/// days → absolute date. Strings go through i18n (`time.*`) using the
/// process-global locale set by `App::new`/`set_lang`. A count of 1 uses the
/// `_one` key ("1 hour ago", not "1 hours ago").
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

/// Timeline group header for a date: "Today" / "Yesterday" / "dd/MM/yyyy" (i18n `day.*`).
pub fn day_header(date: Date, today: Date) -> String {
    if date == today {
        t!("day.today").to_string()
    } else if today.checked_sub(Duration::days(1)).is_some_and(|y| y == date) {
        t!("day.yesterday").to_string()
    } else {
        date_str(date)
    }
}

/// A timeline group of entries sharing one date.
pub struct DayGroup<T> {
    /// Heading shown above the group (from `day_header`).
    pub header: String,
    /// Entries in display order.
    pub items: Vec<T>,
}

/// Groups items (already in display order) into consecutive runs sharing one
/// `Date`, with headers derived from `today`. Item order is preserved within
/// each group.
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

/// Human-readable size: "7 B", "1.5 KB", "10.0 MB".
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
// Kind — badge colors
// ---------------------------------------------------------------------------

/// Badge color per event kind: external=orange, save=green, delete=red,
/// restore=blue, pre_revert=yellow, label=purple, rename=gray.
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

/// Every event kind, in the filter's display order.
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
// Local time zone — the time crate's "local-offset" feature is not enabled (and
// Cargo.toml must not be changed). Windows: read the bias via kernel32
// GetTimeZoneInformation; other platforms fall back to UTC.
// ---------------------------------------------------------------------------

/// Current local UTC offset; falls back to UTC when the system one can't be read.
pub fn local_utc_offset() -> UtcOffset {
    system_utc_offset().unwrap_or(UtcOffset::UTC)
}

/// Current instant expressed in the local offset (`to_offset` — instant preserved).
pub fn local_now() -> OffsetDateTime {
    OffsetDateTime::now_utc().to_offset(local_utc_offset())
}

#[cfg(windows)]
const TZ_STANDARD: u32 = 1;
#[cfg(windows)]
const TZ_DAYLIGHT: u32 = 2;

/// UTC→local offset in minutes, from the TIME_ZONE_INFORMATION biases.
/// Win32 convention: UTC = local + bias, so local = UTC − bias (negative for
/// UTC+ zones, e.g. Vietnam UTC+7). `result` is GetTimeZoneInformation's
/// return value (0/1/2).
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
    /// Win32 SYSTEMTIME — layout only, contents are never read.
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

    /// Win32 TIME_ZONE_INFORMATION — layout must match the original exactly.
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
    // SAFETY: `tzi` is a valid struct whose layout matches Win32 TIME_ZONE_INFORMATION.
    let result = unsafe { GetTimeZoneInformation(&mut tzi) };
    if result > TZ_DAYLIGHT {
        return None;
    }
    let minutes = tz_offset_minutes(result, tzi.bias, tzi.standard_bias, tzi.daylight_bias);
    // Real-world time zones stay within ±14h; anything outside is garbage data.
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
// Line splitting (gui's own copy — `patch::split_lines` is pub(crate) in history)
// ---------------------------------------------------------------------------

/// Splits text into lines: cuts after every `\n`, keeping the terminator in
/// the line (matching `model::Hunk` semantics); a final line without `\n`
/// still counts as a line.
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
// Tests — pure helpers
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
        // CRLF terminators are kept intact
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

    /// `set_locale` is process-global — serializes tests that depend on
    /// locale-sensitive strings.
    static LOCALE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn relative_time_buckets() {
        let _guard = LOCALE_TEST_LOCK.lock().unwrap();
        rust_i18n::set_locale("vi");
        let now: i64 = 1_800_000_000_000_000; // some arbitrary instant
        let utc = UtcOffset::UTC;
        assert_eq!(format_relative_time(now - 30_000_000, now, utc), "vừa xong");
        assert_eq!(format_relative_time(now - 5 * 60_000_000, now, utc), "5 phút trước");
        assert_eq!(format_relative_time(now - 3 * 3_600_000_000, now, utc), "3 giờ trước");
        assert_eq!(format_relative_time(now - 2 * 86_400_000_000, now, utc), "2 ngày trước");
        // beyond 7 days → absolute dd/MM/yyyy date (per the offset passed in)
        let old = now - 10 * 86_400_000_000;
        let expected = date_str(unix_micros_to_local(old, utc).date());
        assert_eq!(format_relative_time(old, now, utc), expected);
        // future timestamp (skewed clock) → never negative
        assert_eq!(format_relative_time(now + 60_000_000, now, utc), "vừa xong");
        // same points under the "en" locale
        rust_i18n::set_locale("en");
        assert_eq!(format_relative_time(now - 30_000_000, now, utc), "just now");
        assert_eq!(format_relative_time(now - 5 * 60_000_000, now, utc), "5 minutes ago");
        assert_eq!(format_relative_time(now - 3 * 3_600_000_000, now, utc), "3 hours ago");
        assert_eq!(format_relative_time(now - 2 * 86_400_000_000, now, utc), "2 days ago");
        rust_i18n::set_locale("en"); // default for other tests
    }

    // ----- day grouping -----

    #[test]
    fn day_headers() {
        let _guard = LOCALE_TEST_LOCK.lock().unwrap();
        rust_i18n::set_locale("vi");
        let today = d(2026, 11, 8);
        assert_eq!(day_header(today, today), "Hôm nay");
        assert_eq!(day_header(d(2026, 11, 7), today), "Hôm qua");
        assert_eq!(day_header(d(2026, 11, 1), today), "01/11/2026");
        // different year
        assert_eq!(day_header(d(2025, 12, 31), today), "31/12/2025");
        // "en" locale
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

    // ----- language (LangPref / resolve_lang) -----

    #[test]
    fn lang_pref_parse_and_str_roundtrip() {
        // None / unknown value → Auto
        assert_eq!(LangPref::parse(None), LangPref::Auto);
        assert_eq!(LangPref::parse(Some("auto")), LangPref::Auto);
        assert_eq!(LangPref::parse(Some("rubbish")), LangPref::Auto);
        assert_eq!(LangPref::parse(Some("en")), LangPref::En);
        assert_eq!(LangPref::parse(Some("vi")), LangPref::Vi);
        // parse → as_str round-trips the value stored in gui-state.json
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
        // an explicit override wins regardless of the system locale
        assert_eq!(resolve_lang(LangPref::En, Some("vi-VN")), "en");
        assert_eq!(resolve_lang(LangPref::Vi, Some("en-US")), "vi");
        // auto: a system locale starting with "vi" → vi
        assert_eq!(resolve_lang(LangPref::Auto, Some("vi")), "vi");
        assert_eq!(resolve_lang(LangPref::Auto, Some("vi-VN")), "vi");
        assert_eq!(resolve_lang(LangPref::Auto, Some("VI-vn")), "vi");
        // auto: anything else → en
        assert_eq!(resolve_lang(LangPref::Auto, Some("en-US")), "en");
        assert_eq!(resolve_lang(LangPref::Auto, Some("fr-FR")), "en");
        assert_eq!(resolve_lang(LangPref::Auto, Some("ja-JP")), "en");
        // auto: unreadable system locale → en
        assert_eq!(resolve_lang(LangPref::Auto, None), "en");
    }

    #[test]
    fn gui_state_lang_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = state_path(dir.path());
        let mut st = GuiState::default();
        assert_eq!(st.lang, None, "old state file without lang → None");
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
        assert_eq!(load_state(&path), GuiState::default()); // no file yet → default
        let st = GuiState {
            last_store_root: Some(r"C:\store".into()),
            last_ws_root: Some(r"D:\proj".into()),
            lang: Some("vi".into()),
        };
        save_state(&path, &st).unwrap();
        assert_eq!(load_state(&path), st);
        // corrupt file → default, no panic
        std::fs::write(&path, "{not json").unwrap();
        assert_eq!(load_state(&path), GuiState::default());
    }

    // ----- time zone -----

    #[cfg(windows)]
    #[test]
    fn tz_offset_math() {
        // Vietnam UTC+7: bias = -420, no DST → +420 minutes
        assert_eq!(tz_offset_minutes(TZ_STANDARD, -420, 0, -60), 420);
        // European DST: bias = -60, daylight_bias = -60 → +120
        assert_eq!(tz_offset_minutes(TZ_DAYLIGHT, -60, 0, -60), 120);
        // fractional UTC+X: bias = -330 → +330 (5h30)
        assert_eq!(tz_offset_minutes(TZ_STANDARD, -330, 0, 0), 330);
        // UTC-8: bias = +480 → -480
        assert_eq!(tz_offset_minutes(TZ_STANDARD, 480, 0, 0), -480);
        // unknown result (0) → only the raw bias applies
        assert_eq!(tz_offset_minutes(0, -420, 0, -60), 420);
        // the real function runs and yields a plausible offset (±14h)
        let off = system_utc_offset();
        if let Some(off) = off {
            let s = off.as_hms();
            assert!((s.0 as i32).abs() <= 14);
        }
    }

    #[test]
    fn local_now_is_close_to_utc_now() {
        // local_now() must be the CURRENT instant (checked against the system
        // clock, 60s tolerance) — catches accidental replace_offset use (keeps
        // the wall clock but shifts the absolute time).
        let sys_nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as i128;
        let local = local_now();
        let delta = (local.unix_timestamp_nanos() - sys_nanos).abs();
        assert!(
            delta < 60 * 1_000_000_000i128,
            "local_now deviates from the system clock by more than 60s: {delta} ns"
        );
    }

    /// `unix_micros_to_local` must keep the instant (checked via unix_timestamp)
    /// and shift the DISPLAYED wall clock by the offset — regression test for
    /// the replace_offset mistake (a 7h zone shift).
    #[test]
    fn unix_micros_to_local_shifts_wall_keeps_instant() {
        let offset = UtcOffset::from_hms(7, 0, 0).unwrap();
        // known point: 1_794_147_692 = 2026-11-08 14:21:32 UTC
        let micros = 1_794_147_692_403_787;
        let dt = unix_micros_to_local(micros, offset);
        // instant is unchanged
        assert_eq!(dt.unix_timestamp(), 1_794_147_692);
        // displayed time = UTC + 7h → 21:21
        assert_eq!(dt.hour(), 21, "displayed hour must be local wall time (UTC+7)");
        assert_eq!(dt.minute(), 21);
        assert_eq!(date_str(dt.date()), "08/11/2026");
        // UTC offset: wall clock = UTC
        let dt_utc = unix_micros_to_local(micros, UtcOffset::UTC);
        assert_eq!(dt_utc.hour(), 14);
        assert_eq!(dt_utc.minute(), 21);
        // hhmm differs across the two zones for the same instant
        assert_ne!(hhmm(dt), hhmm(dt_utc));
    }
}
