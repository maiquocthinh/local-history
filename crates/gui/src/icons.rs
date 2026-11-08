//! icons — Material Icons (classic) nhúng trong binary qua `include_bytes!` (không đọc
//! runtime). egui/ab_glyph KHÔNG render ligature của icon font ⇒ mọi icon dùng
//! codepoint tường minh (`\u{XXXX}`); tên ↔ codepoint lấy từ file `.codepoints` đi kèm
//! font và được test kiểm chéo từng hằng số (xem tests cuối file).

use eframe::egui;

/// MaterialIcons-Regular.ttf nhúng lúc compile.
pub const FONT_BYTES: &[u8] = include_bytes!("../assets/MaterialIcons-Regular.ttf");

/// Bản đồ tên → codepoint đi kèm font — nguồn sự thật cho test kiểm chéo.
/// (chỉ test đọc trực tiếp — runtime chỉ cần FONT_BYTES)
#[cfg_attr(not(test), allow(dead_code))]
pub const CODEPOINTS: &str = include_str!("../assets/MaterialIcons-Regular.codepoints");

// Codepoint xác minh thủ công trong MaterialIcons-Regular.codepoints (11 icon cần dùng):
pub const UNDO: &str = "\u{e166}"; // undo — nút Revert
pub const LABEL: &str = "\u{e892}"; // label — nút Gắn nhãn
pub const ADD: &str = "\u{e145}"; // add — thêm workspace
pub const SEARCH: &str = "\u{e8b6}"; // search — ô tìm file
pub const FOLDER_OPEN: &str = "\u{e2c8}"; // folder_open — chọn store
pub const FILTER_LIST: &str = "\u{e152}"; // filter_list — hàng filter timeline
pub const HISTORY: &str = "\u{e889}"; // history — heading panel
pub const CHECK: &str = "\u{e5ca}"; // check — toast thành công
pub const WARNING: &str = "\u{e002}"; // warning — toast lỗi
pub const CLOSE: &str = "\u{e5cd}"; // close — nút Huỷ
pub const SWAP_HORIZ: &str = "\u{e8d4}"; // swap_horiz — giữa 2 picker version (AnyTwo)

/// (tên trong .codepoints, hằng số) — test dùng để kiểm chéo + chống trùng codepoint.
#[cfg_attr(not(test), allow(dead_code))]
pub const ALL: [(&str, &str); 11] = [
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
];

/// Đăng ký Material Icons vào family Proportional với priority `Lowest` (fallback CUỐI —
/// chỉ dùng khi glyph không có trong các font sẵn có, tức đúng các codepoint icon).
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

/// Parse file `.codepoints` (dòng "tên hex") → map tên → giá trị hex (chuỗi thường).
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
        // TTF hợp lệ bắt đầu bằng signature sfnt (00 01 00 00 / 'OTTO' / 'true' / 'ttcf')
        assert!(FONT_BYTES.len() > 100_000, "font nhúng quá nhỏ — download hỏng?");
        let sig = &FONT_BYTES[..4];
        let ok = sig == [0x00, 0x01, 0x00, 0x00] || &FONT_BYTES[..4] == b"OTTO" || &FONT_BYTES[..4] == b"true" || &FONT_BYTES[..4] == b"ttcf";
        assert!(ok, "FONT_BYTES không phải file TTF/OTF hợp lệ");
        assert!(CODEPOINTS.lines().count() > 1000, ".codepoints quá ít dòng");
    }

    #[test]
    fn icons_match_codepoints_file() {
        let map = codepoints_map();
        assert!(map.len() > 1000, "parser .codepoints hỏng");
        for (name, glyph) in ALL {
            let hex = map
                .get(name)
                .unwrap_or_else(|| panic!("icon `{name}` không có trong .codepoints"));
            let expected = format!("{:04x}", glyph.chars().next().unwrap() as u32);
            assert_eq!(hex, &expected, "codepoint của `{name}` lệch file .codepoints");
        }
    }

    #[test]
    fn icons_unique_codepoints_and_single_char() {
        let mut seen = std::collections::BTreeSet::new();
        for (name, glyph) in ALL {
            let mut chars = glyph.chars();
            let c = chars.next().unwrap();
            assert!(chars.next().is_none(), "`{name}` phải là đúng 1 codepoint");
            let cp = c as u32;
            assert!(
                ('\u{e000}'..='\u{f8ff}').contains(&c) || ('\u{f0000}'..='\u{ffffd}').contains(&c),
                "`{name}` = U+{cp:04X} nằm ngoài vùng private use area của icon font"
            );
            assert!(seen.insert(c), "codepoint U+{cp:04X} bị trùng giữa các icon");
        }
    }
}
