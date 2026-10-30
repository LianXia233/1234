//! 可复用的纯视觉组件与格式化辅助。
//!
//! 这里只放"不带交互"的构件（背景、文字、进度条、分片图、徽标、按钮外观），
//! 交互回调（on_click 等）留在 app.rs 里就近绑定，避免把 `Context<Self>` 透传进来。
//!
//! 位图 → 分片的解码（`piece_done` / `done_segments` / `pending_segments` /
//! `fmt_segments`）在 `acgdev_core::task` 里，与编码器 `build_bitfield` 放在一起，
//! 避免"编码在 core、解码在 UI"导致两边位序约定漂移。

use gpui::prelude::*;
use gpui::{Div, Rgba, SharedString, div, px, relative};

use crate::ui::theme as t;

// ── 进度 / 分片 ──────────────────────────────────────────────────────────

/// 把 `num_pieces` 个分片聚合成不超过 `max_buckets` 个桶，每桶返回完成比例 0.0–1.0。
///
/// 大文件可能有上万个分片，逐片渲染一个 `div` 会拖垮布局；按桶聚合后
/// 视觉上仍是连续的"分片进度条"，但节点数有上限。
pub fn piece_buckets(bitfield: &[u8], num_pieces: u64, max_buckets: usize) -> Vec<f32> {
    let n = num_pieces as usize;
    if n == 0 || bitfield.is_empty() {
        return Vec::new();
    }
    let buckets = n.min(max_buckets.max(1));
    let per = n as f32 / buckets as f32;
    let mut out = Vec::with_capacity(buckets);
    for b in 0..buckets {
        let start = ((b as f32) * per) as usize;
        let mut end = (((b + 1) as f32) * per) as usize;
        if end <= start {
            end = start + 1;
        }
        let end = end.min(n);
        let mut done = 0usize;
        for i in start..end {
            if acgdev_core::task::piece_done(bitfield, i) {
                done += 1;
            }
        }
        out.push(done as f32 / (end - start) as f32);
    }
    out
}

/// 整体进度条（底槽 + 填充）。
pub fn progress_bar(pct: f32, color: Rgba, h: f32) -> Div {
    let pct = pct.clamp(0.0, 1.0);
    div()
        .h(px(h))
        .w_full()
        .rounded_full()
        .bg(t::SLOT)
        .overflow_hidden()
        .child(div().h_full().w(relative(pct)).rounded_full().bg(color))
}

/// 分片进度图：每个桶一个细长竖条，完成度越高颜色越实。
///
/// 这就是"进度细分"——能直接看出哪些区段已下载、哪些还是空洞，
/// 多连接下载时呈现为不连续的填充块。
pub fn piece_map(buckets: &[f32], color: Rgba, h: f32) -> Div {
    let mut row = div().flex().flex_row().w_full().h(px(h)).overflow_hidden();
    for &f in buckets {
        let c = if f <= 0.0 {
            t::SLOT
        } else if f >= 1.0 {
            color
        } else {
            // 部分完成的桶用半透明表示，与"实心已完成"区分开
            t::with_alpha(color, 0.30 + 0.70 * f)
        };
        row = row.child(div().flex_1().h_full().bg(c));
    }
    row
}

/// 剩余时间预估。已完成或无速度时显示占位符。
pub fn eta_text(total: Option<u64>, completed: u64, speed: u64) -> String {
    match total {
        Some(total) if total > completed && speed > 0 => {
            acgdev_core::task::eta_str(total - completed, speed)
        }
        _ => "—".to_string(),
    }
}

// ── 文字 / 徽标 ──────────────────────────────────────────────────────────

/// 状态徽标（圆点 + 文字）。
pub fn badge(text: impl Into<SharedString>, color: Rgba) -> Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_1()
        .px_2()
        .py(px(2.0))
        .rounded_md()
        .bg(t::with_alpha(color, 0.16))
        .child(div().w(px(6.0)).h(px(6.0)).rounded_full().bg(color))
        .child(
            div()
                .text_xs()
                .text_color(color)
                .child(SharedString::from(text.into())),
        )
}

/// 详情面板里的"键 — 值"行。
pub fn kv(label: &str, value: impl Into<SharedString>) -> Div {
    div()
        .flex()
        .flex_row()
        .items_start()
        .text_xs()
        .child(
            div()
                .w(px(76.0))
                .flex_shrink_0()
                .text_color(t::TEXT_FAINT)
                .child(label.to_string()),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_color(t::TEXT_DIM)
                .child(SharedString::from(value.into())),
        )
}

// ── 按钮外观 ────────────────────────────────────────────────────────────
// 返回 Div，调用方继续 .child("文案").on_click(...) 即可。

/// 主按钮（实心蓝）。
pub fn btn_primary() -> Div {
    div()
        .px_3()
        .h(px(30.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded_md()
        .bg(t::ACCENT)
        .text_sm()
        .text_color(gpui::white())
        .cursor_pointer()
        .hover(|s| s.bg(t::ACCENT_SOFT))
}

/// 次级按钮（描边/浅底）。
pub fn btn_ghost() -> Div {
    div()
        .px_3()
        .h(px(30.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded_md()
        .bg(t::PANEL_ALT)
        .text_sm()
        .text_color(t::TEXT)
        .cursor_pointer()
        .hover(|s| s.bg(t::CARD_ACTIVE))
}

/// 危险按钮（删除等破坏性操作）。
pub fn btn_danger() -> Div {
    div()
        .px_3()
        .h(px(30.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded_md()
        .bg(t::ERR_SOFT)
        .text_sm()
        .text_color(t::ERR)
        .cursor_pointer()
        .hover(|s| s.bg(t::ERR_SOFT))
}

/// 行内小按钮（暂停/继续/移除）。
pub fn btn_mini(color: Rgba, text_color: Rgba) -> Div {
    div()
        .px_2()
        .py(px(3.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded_md()
        .bg(t::with_alpha(color, 0.14))
        .text_xs()
        .text_color(text_color)
        .cursor_pointer()
        .hover(|s| s.bg(t::with_alpha(color, 0.30)))
}

/// 输入框外观；`focused` 决定边框高亮。
pub fn input_box(focused: bool, h: f32) -> Div {
    div()
        .w_full()
        .h(px(h))
        .px_3()
        .flex()
        .items_center()
        .rounded_md()
        .bg(t::BG)
        .border_1()
        .border_color(if focused { t::ACCENT } else { t::BORDER })
        .cursor_text()
}
