//! Material Icons (classic) embedded into the binary via `include_bytes!` (no
//! runtime file reads). egui/ab_glyph does NOT render icon-font ligatures, so
//! every icon uses an explicit codepoint (`\u{XXXX}`). Name ↔ codepoint pairs
//! come from the `.codepoints` file shipped with the font and are cross-checked
//! per constant by tests (see the tests at the end of this file).

use eframe::egui;

/// MaterialIcons-Regular.ttf, embedded at compile time.
pub const FONT_BYTES: &[u8] = include_bytes!("../assets/MaterialIcons-Regular.ttf");

/// The font's own name → codepoint map — source of truth for the cross-check
/// test (only tests read it directly; runtime only needs FONT_BYTES).
#[cfg_attr(not(test), allow(dead_code))]
pub const CODEPOINTS: &str = include_str!("../assets/MaterialIcons-Regular.codepoints");

// Codepoints verified by hand against MaterialIcons-Regular.codepoints (the 13 icons in use):
/// Undo — Revert button.
pub const UNDO: &str = "\u{e166}";
/// Label — Label button.
pub const LABEL: &str = "\u{e892}";
/// Add — add workspace.
pub const ADD: &str = "\u{e145}";
/// Search — file search box.
pub const SEARCH: &str = "\u{e8b6}";
/// Folder open — store picker.
pub const FOLDER_OPEN: &str = "\u{e2c8}";
/// Filter list — timeline filter row.
pub const FILTER_LIST: &str = "\u{e152}";
/// History — panel heading.
pub const HISTORY: &str = "\u{e889}";
/// Check — success toast.
pub const CHECK: &str = "\u{e5ca}";
/// Warning — error toast.
pub const WARNING: &str = "\u{e002}";
/// Close — Cancel button.
pub const CLOSE: &str = "\u{e5cd}";
/// Swap horizontal — between the AnyTwo version pickers.
pub const SWAP_HORIZ: &str = "\u{e8d4}";
/// Content copy — Copy diff button.
pub const COPY: &str = "\u{e14d}";
/// Control point (⊕ circle-with-plus) — timeline badge of a version that belongs to a batch.
pub const CONTROL_POINT: &str = "\u{e3ba}";

/// (name in `.codepoints`, constant) — drives the cross-check test and the
/// duplicate-codepoint check.
#[cfg_attr(not(test), allow(dead_code))]
pub const ALL: [(&str, &str); 13] = [
    ("undo", UNDO),
    ("label", LABEL),
    ("add", ADD),
    ("search", SEARCH),
    ("folder_open", FOLDER_OPEN),
    ("filter_list", FILTER_LIST),
    ("history", HISTORY),
    ("check", CHECK),
    ("warning", WARNING),
    ("close", CLOSE),
    ("swap_horiz", SWAP_HORIZ),
    ("content_copy", COPY),
    ("control_point", CONTROL_POINT),
];

/// Registers Material Icons in the Proportional family with `Lowest` priority
/// (last-resort fallback — used only for glyphs missing from the built-in
/// fonts, i.e. exactly the icon codepoints).
pub fn install(ctx: &egui::Context) {
    ctx.add_font(egui::FontInsert::new(
        "material_icons",
        egui::FontData::from_static(FONT_BYTES),
        vec![egui::InsertFontFamily {
            family: egui::FontFamily::Proportional,
            priority: egui::FontPriority::Lowest,
        }],
    ));
}

/// Parses the `.codepoints` file ("name hex" per line) into a name → hex-string map.
#[cfg(test)]
pub(crate) fn codepoints_map() -> std::collections::BTreeMap<String, String> {
    let mut map = std::collections::BTreeMap::new();
    for line in CODEPOINTS.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((name, hex)) = line.split_once(' ') else {
            continue;
        };
        map.insert(name.trim().to_string(), hex.trim().to_string());
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn font_and_codepoints_embedded() {
        // A valid TTF starts with an sfnt signature (00 01 00 00 / 'OTTO' / 'true' / 'ttcf')
        assert!(FONT_BYTES.len() > 100_000, "embedded font too small — broken download?");
        let sig = &FONT_BYTES[..4];
        let ok = sig == [0x00, 0x01, 0x00, 0x00] || &FONT_BYTES[..4] == b"OTTO" || &FONT_BYTES[..4] == b"true" || &FONT_BYTES[..4] == b"ttcf";
        assert!(ok, "FONT_BYTES is not a valid TTF/OTF file");
        assert!(CODEPOINTS.lines().count() > 1000, ".codepoints has too few lines");
    }

    #[test]
    fn icons_match_codepoints_file() {
        let map = codepoints_map();
        assert!(map.len() > 1000, ".codepoints parser is broken");
        for (name, glyph) in ALL {
            let hex = map
                .get(name)
                .unwrap_or_else(|| panic!("icon `{name}` missing from .codepoints"));
            let expected = format!("{:04x}", glyph.chars().next().unwrap() as u32);
            assert_eq!(hex, &expected, "codepoint of `{name}` diverges from .codepoints");
        }
    }

    #[test]
    fn icons_unique_codepoints_and_single_char() {
        let mut seen = std::collections::BTreeSet::new();
        for (name, glyph) in ALL {
            let mut chars = glyph.chars();
            let c = chars.next().unwrap();
            assert!(chars.next().is_none(), "`{name}` must be exactly one codepoint");
            let cp = c as u32;
            assert!(
                ('\u{e000}'..='\u{f8ff}').contains(&c) || ('\u{f0000}'..='\u{ffffd}').contains(&c),
                "`{name}` = U+{cp:04X} is outside the icon font's private use area"
            );
            assert!(seen.insert(c), "codepoint U+{cp:04X} is duplicated across icons");
        }
    }
}
