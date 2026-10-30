//! 暗色主题色板（参考 aria2 WebUI 的夜间配色）。
//!
//! 颜色分两类：
//! - 不透明色：用 `hex()`，在常量位置可用（背景、边框、文字）。
//! - 半透明色：用 `rgba()`，主要用于遮罩与"部分完成"的分片着色。

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

/// 十六进制 + alpha → Rgba。
const fn rgba(v: u32, a: f32) -> Rgba {
    Rgba {
        r: ((v >> 16) & 0xff) as f32 / 255.0,
        g: ((v >> 8) & 0xff) as f32 / 255.0,
        b: (v & 0xff) as f32 / 255.0,
        a,
    }
}

/// 运行时调整透明度（分片进度需要按完成比例插值）。
pub fn with_alpha(c: Rgba, a: f32) -> Rgba {
    Rgba { a, ..c }
}

// ── 底色层级 ────────────────────────────────────────────────────────────
pub const BG: Rgba = hex(0x14161a);
pub const PANEL: Rgba = hex(0x1c1f26);
pub const PANEL_ALT: Rgba = hex(0x23272f);
/// 任务卡片默认底色，比 BG 亮一档，与列表底色形成层次。
pub const CARD: Rgba = hex(0x1a1d24);
pub const CARD_HOVER: Rgba = hex(0x21262f);
pub const CARD_ACTIVE: Rgba = hex(0x272d38);
pub const BORDER: Rgba = hex(0x2e333d);
pub const BORDER_SOFT: Rgba = hex(0x262b34);

// ── 文字 ────────────────────────────────────────────────────────────────
pub const TEXT: Rgba = hex(0xe6e9ef);
pub const TEXT_DIM: Rgba = hex(0x8b93a3);
pub const TEXT_FAINT: Rgba = hex(0x69707e);

// ── 强调 / 语义色 ────────────────────────────────────────────────────────
pub const ACCENT: Rgba = hex(0x4c9aff);
pub const ACCENT_SOFT: Rgba = hex(0x2a4a72);
pub const OK: Rgba = hex(0x3fb950);
pub const OK_SOFT: Rgba = hex(0x1f3a26);
pub const WARN: Rgba = hex(0xd29922);
pub const WARN_SOFT: Rgba = hex(0x3d2f14);
pub const ERR: Rgba = hex(0xf85149);
pub const ERR_SOFT: Rgba = hex(0x3d1f1e);

// ── 弹窗 / 浮层 ──────────────────────────────────────────────────────────
/// 模态遮罩：压暗背景，让弹窗层级清晰。
pub const OVERLAY: Rgba = rgba(0x080a0d, 0.72);
/// 弹窗卡片底色，比 PANEL 再亮一档并带一点蓝调，视觉上"浮起来"。
pub const DIALOG_BG: Rgba = hex(0x1e222b);
/// Toast 浮层底色。
pub const TOAST_BG: Rgba = hex(0x242932);
/// 分片进度条的空槽底色。
pub const SLOT: Rgba = hex(0x2c313b);
