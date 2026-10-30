//! 暗色主题色板（参考 aria2 WebUI 的夜间配色）。

use gpui::Rgba;

/// 十六进制 → Rgba。
///
/// `gpui::rgb()` 不是 const fn，而色板需要在常量位置使用，
/// 因此这里手写一个 const 版本（分量为 0.0–1.0 的 f32）。
const fn hex(v: u32) -> Rgba {
    Rgba {
        r: ((v >> 16) & 0xff) as f32 / 255.0,
        g: ((v >> 8) & 0xff) as f32 / 255.0,
        b: (v & 0xff) as f32 / 255.0,
        a: 1.0,
    }
}

pub const BG: Rgba = hex(0x14161a);
pub const PANEL: Rgba = hex(0x1c1f26);
pub const PANEL_ALT: Rgba = hex(0x23272f);
pub const BORDER: Rgba = hex(0x2e333d);
pub const TEXT: Rgba = hex(0xe6e9ef);
pub const TEXT_DIM: Rgba = hex(0x8b93a3);
pub const ACCENT: Rgba = hex(0x4c9aff);
pub const ACCENT_SOFT: Rgba = hex(0x2a4a72);
pub const OK: Rgba = hex(0x3fb950);
pub const WARN: Rgba = hex(0xd29922);
pub const ERR: Rgba = hex(0xf85149);
