//! 主界面：任务列表 + 输入栏 + 状态栏。
//!
//! 数据流是"拉取式"的：UI 每 500ms 从 `Manager` 取一次快照。
//! `Manager` 内部全部由 `parking_lot` 锁保护，跨线程读取安全，
//! 因此 UI 线程（GPUI）与 tokio 下载线程无需共享 executor。

use std::sync::Arc;
use std::time::Duration;

use acgdev_core::{Manager, Snapshot, TaskOptions, TaskStatus};
use gpui::prelude::*;
use gpui::{
    Context, FocusHandle, Focusable, KeyDownEvent, SharedString, Window, div, px, relative,
};

use crate::ui::theme as t;

pub struct AcgDevApp {
    manager: Arc<Manager>,
    focus: FocusHandle,
    url_input: String,
    tasks: Vec<Snapshot>,
    speed: u64,
    num_active: usize,
    num_waiting: usize,
    num_done: usize,
    rpc_hint: SharedString,
    toast: Option<SharedString>,
}

impl Focusable for AcgDevApp {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl AcgDevApp {
    pub fn new(
        manager: Arc<Manager>,
        rpc_hint: String,
        cx: &mut Context<Self>,
    ) -> Self {
        // 定时刷新
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(500))
                    .await;
                let alive = this.update(cx, |this, cx| {
                    this.refresh();
                    cx.notify();
                });
                if alive.is_err() {
                    break;
                }
            }
        })
        .detach();

        let mut app = Self {
            manager,
            focus: cx.focus_handle(),
            url_input: String::new(),
            tasks: Vec::new(),
            speed: 0,
            num_active: 0,
            num_waiting: 0,
            num_done: 0,
            rpc_hint: rpc_hint.into(),
            toast: None,
        };
        app.refresh();
        app
    }

    fn refresh(&mut self) {
        let all = self.manager.all();
        self.tasks = all.iter().map(|t| t.snapshot()).collect();
        let g = self.manager.global_stat();
        self.speed = g.download_speed;
        self.num_active = g.num_active;
        self.num_waiting = g.num_waiting;
        self.num_done = g.num_stopped;
    }

    fn add_from_input(&mut self, cx: &mut Context<Self>) {
        let raw = self.url_input.trim().to_string();
        if raw.is_empty() {
            return;
        }
        let uris: Vec<String> = raw
            .split_whitespace()
            .filter(|s| s.starts_with("http://") || s.starts_with("https://") || s.starts_with("ftp://"))
            .map(|s| s.to_string())
            .collect();

        if uris.is_empty() {
            self.toast = Some("请输入以 http:// 或 https:// 开头的地址".into());
            cx.notify();
            return;
        }

        let mut opts = TaskOptions::default();
        opts.split = 8;
        match self.manager.add_uri(uris, opts) {
            Ok(_) => {
                self.url_input.clear();
                self.toast = None;
                self.refresh();
            }
            Err(e) => self.toast = Some(format!("添加失败：{e}").into()),
        }
        cx.notify();
    }

    fn on_key(&mut self, ev: &KeyDownEvent, _w: &mut Window, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        let cmd = ks.modifiers.platform || ks.modifiers.control;

        if cmd && ks.key == "v" {
            if let Some(item) = cx.read_from_clipboard() {
                if let Some(text) = item.text() {
                    self.url_input.push_str(text.trim());
                }
            }
            cx.notify();
            return;
        }
        match ks.key.as_str() {
            "backspace" => {
                self.url_input.pop();
                cx.notify();
            }
            "enter" => self.add_from_input(cx),
            "escape" => {
                self.url_input.clear();
                cx.notify();
            }
            _ => {
                if !cmd {
                    if let Some(ch) = ks.key_char.as_ref() {
                        // key_char 是字符串（可能含多个码位），不能按 char 判控制符
                        if ch.chars().all(|c| !c.is_control()) {
                            self.url_input.push_str(ch);
                            cx.notify();
                        }
                    }
                }
            }
        }
    }
}

fn status_color(s: TaskStatus) -> gpui::Rgba {
    match s {
        TaskStatus::Active => t::ACCENT,
        TaskStatus::Waiting => t::TEXT_DIM,
        TaskStatus::Paused => t::WARN,
        TaskStatus::Complete => t::OK,
        TaskStatus::Error => t::ERR,
        TaskStatus::Removed => t::TEXT_DIM,
    }
}

fn status_label(s: TaskStatus) -> &'static str {
    match s {
        TaskStatus::Active => "下载中",
        TaskStatus::Waiting => "等待",
        TaskStatus::Paused => "已暂停",
        TaskStatus::Complete => "已完成",
        TaskStatus::Error => "失败",
        TaskStatus::Removed => "已移除",
    }
}

fn human(bytes: u64) -> String {
    acgdev_core::util::size_str(bytes)
}

impl AcgDevApp {
    fn task_row(&self, idx: usize, s: &Snapshot, cx: &mut Context<Self>) -> impl IntoElement {
        let pct = match s.total {
            Some(total) if total > 0 => (s.completed as f32 / total as f32).clamp(0.0, 1.0),
            _ => 0.0,
        };
        let gid = s.gid;
        let manager = self.manager.clone();
        let status = s.status;

        let (can_pause, can_resume) = match status {
            TaskStatus::Active | TaskStatus::Waiting => (true, false),
            TaskStatus::Paused => (false, true),
            _ => (false, false),
        };
        let can_remove = !matches!(status, TaskStatus::Removed);

        div()
            .id(("row", idx))
            .flex()
            .flex_col()
            .gap_1()
            .px_4()
            .py_2()
            .border_b_1()
            .border_color(t::BORDER)
            .hover(|s| s.bg(t::PANEL_ALT))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .w(px(8.0))
                            .h(px(8.0))
                            .rounded_full()
                            .bg(status_color(status)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_sm()
                            .text_color(t::TEXT)
                            .child(s.filename.clone()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(t::TEXT_DIM)
                            .child(format!("{} / {}", human(s.completed), s.total.map(human).unwrap_or_else(|| "?".into()))),
                    )
                    .child(
                        div()
                            .w(px(92.0))
                            .text_xs()
                            .text_color(t::TEXT_DIM)
                            .child(format!("{:.1}%", pct * 100.0)),
                    )
                    .child(
                        div()
                            .w(px(96.0))
                            .text_xs()
                            .text_color(if s.speed > 0 { t::ACCENT } else { t::TEXT_DIM })
                            .child(if s.speed > 0 {
                                format!("{}/s", human(s.speed))
                            } else {
                                status_label(status).to_string()
                            }),
                    )
                    .when(can_pause, |el| {
                        el.child(
                            div()
                                .id(("pause", gid as usize))
                                .px_2()
                                .py_1()
                                .rounded_md()
                                .bg(t::PANEL_ALT)
                                .text_xs()
                                .text_color(t::TEXT_DIM)
                                .cursor_pointer()
                                .hover(|s| s.bg(t::BORDER))
                                .child("暂停")
                                .on_click(cx.listener(move |this, _e, _w, cx| {
                                    let _ = manager.pause(gid);
                                    this.refresh();
                                    cx.notify();
                                })),
                        )
                    })
                    .when(can_resume, |el| {
                        let m2 = self.manager.clone();
                        el.child(
                            div()
                                .id(("resume", gid as usize))
                                .px_2()
                                .py_1()
                                .rounded_md()
                                .bg(t::ACCENT_SOFT)
                                .text_xs()
                                .text_color(t::TEXT)
                                .cursor_pointer()
                                .hover(|s| s.bg(t::ACCENT))
                                .child("继续")
                                .on_click(cx.listener(move |this, _e, _w, cx| {
                                    let _ = m2.unpause(gid);
                                    m2.kick();
                                    this.refresh();
                                    cx.notify();
                                })),
                        )
                    })
                    .when(can_remove, |el| {
                        let m3 = self.manager.clone();
                        el.child(
                            div()
                                .id(("remove", gid as usize))
                                .px_2()
                                .py_1()
                                .rounded_md()
                                .bg(t::PANEL_ALT)
                                .text_xs()
                                .text_color(t::ERR)
                                .cursor_pointer()
                                .hover(|s| s.bg(t::BORDER))
                                .child("✕")
                                .on_click(cx.listener(move |this, _e, _w, cx| {
                                    let _ = m3.remove(gid, true);
                                    this.refresh();
                                    cx.notify();
                                })),
                        )
                    }),
            )
            .child(
                div()
                    .h(px(4.0))
                    .w_full()
                    .rounded_full()
                    .bg(t::PANEL_ALT)
                    .child(
                        div()
                            .h_full()
                            .w(relative(pct))
                            .rounded_full()
                            .bg(status_color(status)),
                    ),
            )
            .when_some(s.error.clone(), |el, err| {
                el.child(
                    div()
                        .text_xs()
                        .text_color(t::ERR)
                        .child(format!("错误：{err}")),
                )
            })
    }
}

impl Render for AcgDevApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rows: Vec<_> = self
            .tasks
            .iter()
            .enumerate()
            .map(|(i, s)| self.task_row(i, s, cx).into_any_element())
            .collect();

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(t::BG)
            .text_color(t::TEXT)
            .font_family(super::fonts::FAMILY)
            // ── 顶栏 ──
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .px_4()
                    .py_3()
                    .bg(t::PANEL)
                    .border_b_1()
                    .border_color(t::BORDER)
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_3()
                            .child(div().text_lg().font_weight(gpui::FontWeight::SEMIBOLD).child("AcgDev"))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(t::TEXT_DIM)
                                    .child("Rust + GPUI"),
                            )
                            .child(
                                div()
                                    .px_2()
                                    .py_1()
                                    .rounded_md()
                                    .bg(t::PANEL_ALT)
                                    .text_xs()
                                    .text_color(t::ACCENT)
                                    .child(self.rpc_hint.clone()),
                            )
                            // MiSans 许可协议第 2 条第 1 款要求：须在软件中注明使用了该字体。
                            .child(
                                div()
                                    .px_2()
                                    .py_1()
                                    .rounded_md()
                                    .bg(t::PANEL_ALT)
                                    .text_xs()
                                    .text_color(t::TEXT_DIM)
                                    .child("MiSans · 小米"),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_4()
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(t::ACCENT)
                                    .child(format!("{}/s", human(self.speed))),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(t::TEXT_DIM)
                                    .child(format!(
                                        "{} 活动 · {} 等待 · {} 完成",
                                        self.num_active, self.num_waiting, self.num_done
                                    )),
                            ),
                    ),
            )
            // ── 输入栏 ──
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .px_4()
                    .py_3()
                    .bg(t::PANEL)
                    .border_b_1()
                    .border_color(t::BORDER)
                    .child(
                        div()
                            .flex_1()
                            .h(px(34.0))
                            .px_3()
                            .flex()
                            .items_center()
                            .rounded_md()
                            .bg(t::BG)
                            .border_1()
                            .border_color(if self.focus.is_focused(window) {
                                t::ACCENT
                            } else {
                                t::BORDER
                            })
                            .track_focus(&self.focus)
                            .on_key_down(cx.listener(Self::on_key))
                            .cursor_text()
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(if self.url_input.is_empty() {
                                        t::TEXT_DIM
                                    } else {
                                        t::TEXT
                                    })
                                    .child(if self.url_input.is_empty() {
                                        "粘贴或输入下载地址（支持多个，空格分隔）".to_string()
                                    } else {
                                        self.url_input.clone()
                                    }),
                            ),
                    )
                    .child(
                        div()
                            .id("add-task")
                            .flex_shrink_0()
                            .px_4()
                            .h(px(34.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_md()
                            .bg(t::ACCENT)
                            .text_sm()
                            .text_color(gpui::white())
                            .cursor_pointer()
                            .hover(|s| s.bg(t::ACCENT_SOFT))
                            .child("添加任务")
                            .on_click(cx.listener(|this, _e: &gpui::ClickEvent, _w, cx| {
                                this.add_from_input(cx);
                            })),
                    ),
            )
            // ── 列表 ──
            .child(
                div()
                    .id("task-list")
                    .flex_1()
                    .overflow_y_scroll()
                    .when(rows.is_empty(), |el| {
                        el.child(
                            div()
                                .flex()
                                .size_full()
                                .items_center()
                                .justify_center()
                                .text_sm()
                                .text_color(t::TEXT_DIM)
                                .child("暂无任务 — 把链接粘到上面，或通过 aria2 RPC 推送"),
                        )
                    })
                    .children(rows),
            )
            // ── 提示条 ──
            .when_some(self.toast.clone(), |el, msg| {
                el.child(
                    div()
                        .px_4()
                        .py_2()
                        .bg(t::PANEL_ALT)
                        .text_xs()
                        .text_color(t::WARN)
                        .child(msg),
                )
            })
    }
}
