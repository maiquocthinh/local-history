"""Generate local-history app icons (ICO for exe/window, PNG for tray).

Design: rounded-rect card (dark navy) with a clock face + rewind arrow (JetBrains-like
'local history' motif). Drawn with Pillow — no external assets. Run with `-I`.
"""
import struct
import zlib
from pathlib import Path

def lerp(a, b, t):
    return int(a + (b - a) * t)

def rounded_rect_mask(size, radius):
    """Anti-aliased rounded-rect alpha mask (SS 4x). Opaque inside, transparent outside."""
    S = 4
    mask = [[0] * size for _ in range(size)]
    r = radius * S  # radius lives in the same supersampled space as px/py
    for y in range(size):
        for x in range(size):
            hit = 0
            for sy in range(S):
                for sx in range(S):
                    px = x * S + sx + 0.5
                    py = y * S + sy + 0.5
                    # Clamp sample point to the inner rect; a corner sample's distance
                    # from the clamp point decides inside (<= r) vs outside.
                    cx = min(max(px, r), size * S - r)
                    cy = min(max(py, r), size * S - r)
                    dx = px - cx
                    dy = py - cy
                    if dx * dx + dy * dy <= r * r:
                        hit += 1
            mask[y][x] = int(hit * 255 / (S * S))
    return mask

def draw_icon(size):
    """Return (pixels rgba list-of-lists, mask) for an icon of `size` px."""
    mask = rounded_rect_mask(size, max(2, size // 5))
    px = [[(0, 0, 0, 0)] * size for _ in range(size)]
    bg_dark = (24, 33, 46)      # deep navy card
    bg_edge = (43, 58, 80)      # subtle gradient edge
    clock = (86, 182, 139)      # mint-green clock ring
    hand = (235, 244, 247)      # near-white hands
    arrow = (255, 156, 66)      # amber rewind arrow
    s = size / 64.0             # design at 64 then scale

    for y in range(size):
        for x in range(size):
            a = mask[y][x]
            if a == 0:
                continue
            # card gradient (vertical, subtle)
            t = y / max(1, size - 1)
            r = lerp(bg_dark[0], bg_edge[0], t)
            g = lerp(bg_dark[1], bg_edge[1], t)
            b = lerp(bg_dark[2], bg_edge[2], t)

            # --- clock ring (centered slightly up-left) ---
            cx, cy = 32 * s, 32 * s
            R_outer, R_inner = 21 * s, 16 * s
            dx, dy = x - cx, y - cy
            d2 = dx * dx + dy * dy
            ro2, ri2 = R_outer * R_outer, R_inner * R_inner
            if ri2 <= d2 <= ro2:
                r, g, b = clock
            # tick marks at 12/3/6/9
            import math
            ang = math.atan2(dy, dx)
            d = math.sqrt(d2)
            tick_r = (12.5 * s, 15.5 * s)
            if tick_r[0] <= d <= tick_r[1]:
                deg = math.degrees(ang) % 360
                for td in (270, 0, 90, 180):
                    if min(abs(deg - td), 360 - abs(deg - td)) < 9:
                        r, g, b = hand
            # hands: hour (to 10 o'clock) + minute (to 2 o'clock)
            if d <= 13 * s:
                # hour hand: angle 300 deg (10 o'clock), width ~2.4px
                def near_line(deg, width_deg):
                    dd = min(abs(math.degrees(ang) - deg) % 360, 360 - (abs(math.degrees(ang) - deg) % 360))
                    return dd < width_deg
                if d <= 9 * s and near_line(300, 8):
                    r, g, b = hand
                if d <= 12.5 * s and near_line(30, 6):
                    r, g, b = hand
                # center dot
                if d <= 2.2 * s:
                    r, g, b = hand

            # --- rewind arrow: curved arrow bottom-right, wrapping the ring ---
            # arc band
            acx, acy = 32 * s, 32 * s
            ad = math.sqrt((x - acx) ** 2 + (y - acy) ** 2)
            ab = math.degrees(math.atan2(y - acy, x - acx)) % 360
            if 24 * s <= ad <= 28 * s and 300 <= ab <= 405 % 360 or (24 * s <= ad <= 28 * s and (ab >= 300 or ab <= 45)):
                r, g, b = arrow
            # arrowhead at the end of the arc (pointing counterclockwise, at ~45deg position)
            hx, hy = 32 * s + 26 * s * 0.707, 32 * s + 26 * s * 0.707
            hdx, hdy = x - hx, y - hy
            # triangle pointing up-left
            if abs(hdx + hdy) < 4.2 * s and hdx + hdy <= 0 and -6 * s < (hdx - hdy) * 0.707 < 6 * s:
                r, g, b = arrow

            px[y][x] = (r, g, b, a)
    return px

def to_png_bytes(px):
    size = len(px)
    raw = b""
    for row in px:
        raw += b"\x00" + b"".join(struct.pack("4B", *p) for p in row)
    def chunk(tag, data):
        c = struct.pack(">I", len(data)) + tag + data
        return c + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)
    ihdr = struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0)
    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", ihdr) + chunk(b"IDAT", zlib.compress(raw, 9))
            + chunk(b"IEND", b""))

def to_ico(png_sizes):
    """ICO container embedding PNG-compressed images (Vista+ format)."""
    out = struct.pack("<HHH", 0, 1, len(png_sizes))
    blobs = []
    offset = 6 + 16 * len(png_sizes)
    for size, data in png_sizes:
        out += struct.pack("<BBBBHHII", size & 0xFF, size & 0xFF, 0, 0, 1, 32,
                           len(data), offset)
        blobs.append(data)
        offset += len(data)
    return out + b"".join(blobs)

def main():
    here = Path(r"D:\Workspace\projects\ide-extensions\local-history\crates\gui\assets")
    here.mkdir(parents=True, exist_ok=True)
    icons = {}
    for size in (16, 24, 32, 48, 64, 128, 256):
        px = draw_icon(size)
        icons[size] = to_png_bytes(px)
        if size in (32, 128):
            (here / f"icon-{size}.png").write_bytes(icons[size])
            print(f"wrote icon-{size}.png ({len(icons[size])} B)")
    ico = to_ico([(s, icons[s]) for s in (16, 24, 32, 48, 64, 128, 256)])
    (here / "app.ico").write_bytes(ico)
    print(f"wrote app.ico ({len(ico)} B)")

    # Daemon tray icon: raw RGBA bytes as a Rust const — no image dep needed
    # in the daemon (embed via include!).
    tray_path = Path(r"D:\Workspace\projects\ide-extensions\local-history\crates\daemon\src\tray_icon_data.rs")
    px32 = draw_icon(32)
    flat = [b for row in px32 for p in row for b in p]
    assert len(flat) == 32 * 32 * 4
    rows = []
    for i in range(0, len(flat), 16):
        rows.append("    " + ", ".join(f"{b}" for b in flat[i:i + 16]) + ",")
    src = (
        "//! Auto-generated by tools/gen_icon.py -- do not edit. Run the script to regenerate.\n"
        "//! 32x32 RGBA pixels of the app icon (4 bytes per pixel, row-major), for the\n"
        "//! daemon tray via `tray_icon::Icon::from_rgba`.\n\n"
        "/// 32 * 32 * 4 RGBA bytes.\n"
        "pub const TRAY_ICON_RGBA_32: [u8; 32 * 32 * 4] = [\n" + "\n".join(rows) + "\n];\n"
    )
    tray_path.write_text(src, encoding="utf-8")
    print(f"wrote {tray_path} ({len(src)} B)")

if __name__ == "__main__":
    main()
