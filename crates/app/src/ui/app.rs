//! 主界面：顶栏 + 工具栏（筛选/批量操作/快速添加）+ 任务列表 + 状态栏 + 弹窗层。
//!
//! 数据流是"拉取式"的：UI 每 500ms 从 `Manager` 取一次快照。
//! `Manager` 内部全部由 `parking_lot` 锁保护，跨线程读取安全，
//! 因此 UI 线程（GPUI）与 tokio 下载线程无需共享 executor。

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use acgdev_core::task::{done_segments, fmt_segments, pending_segments};
use acgdev_core::{Gid, Manager, Snapshot, TaskOptions, TaskStatus};
use gpui::prelude::*;
use gpui::{Context, Div, FocusHandle, Focusable, KeyDownEvent, SharedString, Window, div, px};

use crate::ui::dialog::{
    AddTaskDialog, ConfirmAction, ConfirmState, Field, ParsedSettings, SettingsField,
    SettingsState, Toast, ToastKind,
};
use crate::ui::theme as t;
use crate::ui::widgets as w;

/// 列表筛选。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Filter {
    All,
    Active,
    Waiting,
    Done,
}

impl Filter {
    const ALL: [Filter; 4] = [Filter::All, Filter::Active, Filter::Waiting, Filter::Done];

    fn label(self) -> &'static str {
        match self {
            Filter::All => "全部",
            Filter::Active => "下载中",
            Filter::Waiting => "等待中",
            Filter::Done => "已结束",
        }
    }

    fn matches(self, s: TaskStatus) -> bool {
        match self {
            Filter::All => true,
            Filter::Active => matches!(s, TaskStatus::Active),
            Filter::Waiting => matches!(s, TaskStatus::Waiting),
            Filter::Done => matches!(
                s,
                TaskStatus::Complete | TaskStatus::Error | TaskStatus::Paused
            ),
        }
    }
}

pub struct AcgDevApp {
    manager: Arc<Manager>,
    /// 快速添加栏的键盘焦点。
    focus: FocusHandle,
    /// 弹窗的键盘焦点（新建任务对话框）。
    dlg_focus: FocusHandle,
    url_input: String,
    tasks: Vec<Snapshot>,
    speed: u64,
    num_active: usize,
    num_waiting: usize,
    num_done: usize,
    rpc_hint: SharedString,

    // ── 弹窗 / 提示 ──
    toasts: Vec<Toast>,
    toast_seq: u64,
    dialog: Option<AddTaskDialog>,
    /// 高级设置对话框（编辑中）。
    settings: Option<SettingsState>,
    /// 已生效的高级设置，作为新建任务的默认值。
    defaults: ParsedSettings,
    confirm: Option<ConfirmState>,
    /// 展开详情的任务。
    expanded: HashSet<Gid>,
    filter: Filter,
}

impl Focusable for AcgDevApp {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl AcgDevApp {
    pub fn new(manager: Arc<Manager>, rpc_hint: String, cx: &mut Context<Self>) -> Self {
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
            dlg_focus: cx.focus_handle(),
            url_input: String::new(),
            tasks: Vec::new(),
            speed: 0,
            num_active: 0,
            num_waiting: 0,
            num_done: 0,
            rpc_hint: rpc_hint.into(),
            toasts: Vec::new(),
            toast_seq: 0,
            dialog: None,
            settings: None,
            defaults: ParsedSettings::default(),
            confirm: None,
            expanded: HashSet::new(),
            filter: Filter::All,
        };
        app.refresh();
        app
    }

    // ── 数据 ──────────────────────────────────────────────────────────────

    fn refresh(&mut self) {
        let all = self.manager.all();
        self.tasks = all.iter().map(|t| t.snapshot()).collect();
        let g = self.manager.global_stat();
        self.speed = g.download_speed;
        self.num_active = g.num_active;
        self.num_waiting = g.num_waiting;
        self.num_done = g.num_stopped;
        // Toast 借 500ms 的刷新节拍过期回收，无需额外定时器。
        self.toasts.retain(|t| !t.expired());
    }

    fn default_dir(&self) -> std::path::PathBuf {
        self.manager.config().default_dir.clone()
    }

    /// 以「高级设置」为基准构造任务选项；未设置的项沿用 core 的内置默认。
    fn default_options(&self) -> TaskOptions {
        let mut o = TaskOptions::default();
        o.split = 8;
        let d = &self.defaults;
        if let Some(dir) = &d.dir {
            o.dir = dir.clone();
        }
        if let Some(n) = d.split {
            o.split = n;
        }
        if let Some(n) = d.min_split_size {
            o.min_split_size = n;
        }
        if let Some(ua) = &d.user_agent {
            o.user_agent = Some(ua.clone());
        }
        if let Some(r) = &d.referer {
            o.referer = Some(r.clone());
        }
        if !d.headers.is_empty() {
            o.headers = d.headers.clone();
        }
        o
    }

    fn push_toast(&mut self, msg: impl Into<String>, kind: ToastKind) {
        self.toast_seq += 1;
        self.toasts.push(Toast::new(self.toast_seq, msg, kind));
        // 最多同时显示 4 条，超出丢最早的
        while self.toasts.len() > 4 {
            self.toasts.remove(0);
        }
    }

    fn toggle_expand(&mut self, gid: Gid) {
        if !self.expanded.remove(&gid) {
            self.expanded.insert(gid);
        }
    }

    // ── 快速添加栏 ────────────────────────────────────────────────────────

    fn add_from_input(&mut self, cx: &mut Context<Self>) {
        let raw = self.url_input.trim().to_string();
        if raw.is_empty() {
            return;
        }
        let uris: Vec<String> = raw
            .split_whitespace()
            .filter(|s| {
                s.starts_with("http://") || s.starts_with("https://") || s.starts_with("ftp://")
            })
            .map(|s| s.to_string())
            .collect();

        if uris.is_empty() {
            self.push_toast("请输入以 http:// 或 https:// 开头的地址", ToastKind::Warn);
            cx.notify();
            return;
        }

        let opts = self.default_options();
        let n = uris.len();
        match self.manager.add_uri(uris, opts) {
            Ok(_) => {
                self.url_input.clear();
                self.push_toast(
                    if n > 1 {
                        format!("已添加 {n} 个任务")
                    } else {
                        "已添加任务".to_string()
                    },
                    ToastKind::Ok,
                );
                self.refresh();
            }
            Err(e) => self.push_toast(format!("添加失败：{e}"), ToastKind::Error),
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

    // ── 新建任务对话框 ────────────────────────────────────────────────────

    /// 打开新建任务对话框；需要 `window` 才能把键盘焦点移到弹窗上。
    fn open_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dir = self.default_dir();
        self.dialog = Some(AddTaskDialog::new(dir));
        self.confirm = None;
        window.focus(&self.dlg_focus);
        cx.notify();
    }

    fn close_dialog(&mut self, cx: &mut Context<Self>) {
        self.dialog = None;
        cx.notify();
    }

    fn submit_dialog(&mut self, cx: &mut Context<Self>) {
        let fallback = self.default_dir();
        // 先把需要的值取出来，避免后续对 self.dialog 的可变借用与这里的只读借用冲突
        let (parsed, paused) = match self.dialog.as_ref() {
            Some(d) => (d.parse(fallback), d.start_paused),
            None => return,
        };

        match parsed {
            Ok(p) => {
                let n = p.uris.len();
                let mut opts = self.default_options();
                opts.split = p.split;
                opts.dir = p.dir;
                opts.out = p.out;
                opts.pause = paused;
                match self.manager.add_uri(p.uris, opts) {
                    Ok(_) => {
                        self.dialog = None;
                        self.push_toast(
                            if n > 1 {
                                format!("已添加 {n} 个任务")
                            } else {
                                "已添加任务".to_string()
                            },
                            ToastKind::Ok,
                        );
                        self.refresh();
                    }
                    Err(e) => {
                        if let Some(d) = self.dialog.as_mut() {
                            d.error = Some(format!("添加失败：{e}"));
                        }
                    }
                }
            }
            Err(msg) => {
                if let Some(d) = self.dialog.as_mut() {
                    d.error = Some(msg);
                }
            }
        }
        cx.notify();
    }

    fn on_dialog_key(&mut self, ev: &KeyDownEvent, _w: &mut Window, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        let cmd = ks.modifiers.platform || ks.modifiers.control;

        if cmd && ks.key == "v" {
            if let Some(item) = cx.read_from_clipboard() {
                if let Some(text) = item.text() {
                    if let Some(d) = self.dialog.as_mut() {
                        let f = d.field;
                        d.field_mut(f).push_str(text.trim());
                    }
                }
            }
            cx.notify();
            return;
        }
        match ks.key.as_str() {
            "escape" => self.close_dialog(cx),
            "enter" => self.submit_dialog(cx),
            "tab" => {
                if let Some(d) = self.dialog.as_mut() {
                    let f = d.field;
                    d.field = f.next();
                }
                cx.notify();
            }
            "backspace" => {
                if let Some(d) = self.dialog.as_mut() {
                    let f = d.field;
                    d.field_mut(f).pop();
                }
                cx.notify();
            }
            _ => {
                if !cmd {
                    if let Some(ch) = ks.key_char.as_ref() {
                        if ch.chars().all(|c| !c.is_control()) {
                            if let Some(d) = self.dialog.as_mut() {
                                let f = d.field;
                                d.field_mut(f).push_str(ch);
                            }
                            cx.notify();
                        }
                    }
                }
            }
        }
    }

    /// 对话框里的一行字段（标签 + 输入框外观）。
    fn dialog_field(&self, f: Field, value: &str, cx: &mut Context<Self>) -> Div {
        let active = self.dialog.as_ref().map(|d| d.field == f).unwrap_or(false);
        let label_color = if active { t::ACCENT } else { t::TEXT_FAINT };
        let shown = if value.is_empty() { f.placeholder() } else { value };
        let value_color = if value.is_empty() { t::TEXT_FAINT } else { t::TEXT };

        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_xs()
                            .text_color(label_color)
                            .child(f.label().to_string()),
                    )
                    .when(active, |el| {
                        el.child(
                            div()
                                .text_xs()
                                .text_color(t::ACCENT)
                                .child("正在输入"),
                        )
                    }),
            )
            .child(
                w::input_box(active, 34.0)
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(move |this, _e: &gpui::MouseDownEvent, _w, cx| {
                            if let Some(d) = this.dialog.as_mut() {
                                d.field = f;
                            }
                            cx.notify();
                        }),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(value_color)
                            .child(shown.to_string()),
                    ),
            )
    }

    fn render_add_dialog(&mut self, cx: &mut Context<Self>) -> gpui::Stateful<Div> {
        let (urls, filename, dir, split, paused, error) = match self.dialog.as_ref() {
            Some(d) => (
                d.urls.clone(),
                d.filename.clone(),
                d.dir.clone(),
                d.split.clone(),
                d.start_paused,
                d.error.clone(),
            ),
            None => return div().id("noop-dialog"),
        };

        let mut card = div()
            .id("add-dialog")
            .w(px(600.0))
            .flex()
            .flex_col()
            .gap_3()
            .p_4()
            .rounded_lg()
            .bg(t::DIALOG_BG)
            .border_1()
            .border_color(t::BORDER)
            .track_focus(&self.dlg_focus)
            .on_key_down(cx.listener(Self::on_dialog_key))
            // ── 标题 ──
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_lg()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child("新建下载任务"),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(t::TEXT_FAINT)
                            .child("Esc 取消 · Enter 添加 · Tab 切换字段"),
                    ),
            )
            // ── 字段 ──
            .child(self.dialog_field(Field::Urls, &urls, cx))
            .child(self.dialog_field(Field::Filename, &filename, cx))
            .child(self.dialog_field(Field::Dir, &dir, cx))
            .child(self.dialog_field(Field::Split, &split, cx))
            // ── 添加后暂停 ──
            .child(
                div()
                    .id("toggle-paused")
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _e: &gpui::ClickEvent, _w, cx| {
                        if let Some(d) = this.dialog.as_mut() {
                            d.start_paused = !d.start_paused;
                        }
                        cx.notify();
                    }))
                    .child(
                        div()
                            .w(px(16.0))
                            .h(px(16.0))
                            .rounded_md()
                            .border_1()
                            .border_color(if paused { t::ACCENT } else { t::BORDER })
                            .bg(if paused { t::ACCENT } else { t::BG })
                            .flex()
                            .items_center()
                            .justify_center()
                            .when(paused, |el| {
                                el.child(
                                    div()
                                        .text_xs()
                                        .text_color(gpui::white())
                                        .child("✓"),
                                )
                            }),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(t::TEXT_DIM)
                            .child("添加后先暂停，稍后手动开始"),
                    ),
            );

        if let Some(err) = error {
            card = card.child(
                div()
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .bg(t::ERR_SOFT)
                    .text_xs()
                    .text_color(t::ERR)
                    .child(err),
            );
        }

        card.child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_end()
                .gap_2()
                .child(
                    w::btn_ghost()
                        .id("dlg-cancel")
                        .child("取消")
                        .on_click(cx.listener(|this, _e: &gpui::ClickEvent, _w, cx| {
                            this.close_dialog(cx)
                        })),
                )
                .child(
                    w::btn_primary()
                        .id("dlg-submit")
                        .child("添加任务")
                        .on_click(cx.listener(|this, _e: &gpui::ClickEvent, _w, cx| {
                            this.submit_dialog(cx)
                        })),
                ),
        )
    }

    // ── 高级设置 ──────────────────────────────────────────────────────────

    fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.settings = Some(SettingsState::from_current(&self.defaults));
        self.confirm = None;
        window.focus(&self.dlg_focus);
        cx.notify();
    }

    fn save_settings(&mut self, cx: &mut Context<Self>) {
        let parsed = match self.settings.as_ref() {
            Some(s) => s.parse(),
            None => return,
        };
        match parsed {
            Ok(p) => {
                self.defaults = p;
                self.settings = None;
                self.push_toast("高级设置已保存，将用于之后新建的任务", ToastKind::Ok);
            }
            Err(msg) => {
                if let Some(s) = self.settings.as_mut() {
                    s.error = Some(msg);
                }
            }
        }
        cx.notify();
    }

    fn on_settings_key(&mut self, ev: &KeyDownEvent, _w: &mut Window, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        let cmd = ks.modifiers.platform || ks.modifiers.control;

        if cmd && ks.key == "v" {
            if let Some(item) = cx.read_from_clipboard() {
                if let Some(text) = item.text() {
                    if let Some(s) = self.settings.as_mut() {
                        let f = s.field;
                        s.field_mut(f).push_str(text.trim());
                    }
                }
            }
            cx.notify();
            return;
        }
        match ks.key.as_str() {
            "escape" => {
                self.settings = None;
                cx.notify();
            }
            "enter" => self.save_settings(cx),
            "tab" => {
                if let Some(s) = self.settings.as_mut() {
                    let f = s.field;
                    s.field = f.next();
                }
                cx.notify();
            }
            "backspace" => {
                if let Some(s) = self.settings.as_mut() {
                    let f = s.field;
                    s.field_mut(f).pop();
                }
                cx.notify();
            }
            _ => {
                if !cmd {
                    if let Some(ch) = ks.key_char.as_ref() {
                        if ch.chars().all(|c| !c.is_control()) {
                            if let Some(s) = self.settings.as_mut() {
                                let f = s.field;
                                s.field_mut(f).push_str(ch);
                            }
                            cx.notify();
                        }
                    }
                }
            }
        }
    }

    fn settings_field(&self, f: SettingsField, value: &str, cx: &mut Context<Self>) -> Div {
        let active = self.settings.as_ref().map(|s| s.field == f).unwrap_or(false);
        let shown = if value.is_empty() { f.placeholder() } else { value };
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .text_xs()
                    .text_color(if active { t::ACCENT } else { t::TEXT_FAINT })
                    .child(f.label().to_string()),
            )
            .child(
                w::input_box(active, 34.0)
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(move |this, _e: &gpui::MouseDownEvent, _w, cx| {
                            if let Some(s) = this.settings.as_mut() {
                                s.field = f;
                            }
                            cx.notify();
                        }),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(if value.is_empty() { t::TEXT_FAINT } else { t::TEXT })
                            .child(shown.to_string()),
                    ),
            )
    }

    fn render_settings(&mut self, cx: &mut Context<Self>) -> gpui::Stateful<Div> {
        let (dir, split, min_split, ua, referer, headers, error) = match self.settings.as_ref() {
            Some(s) => (
                s.dir.clone(),
                s.split.clone(),
                s.min_split.clone(),
                s.user_agent.clone(),
                s.referer.clone(),
                s.headers.clone(),
                s.error.clone(),
            ),
            None => return div().id("noop-settings"),
        };

        let mut card = div()
            .id("settings-dialog")
            .w(px(600.0))
            .flex()
            .flex_col()
            .gap_3()
            .p_4()
            .rounded_lg()
            .bg(t::DIALOG_BG)
            .border_1()
            .border_color(t::BORDER)
            .track_focus(&self.dlg_focus)
            .on_key_down(cx.listener(Self::on_settings_key))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_lg()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child("高级设置"),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(t::TEXT_FAINT)
                            .child("Esc 取消 · Enter 保存 · Tab 切换字段"),
                    ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(t::TEXT_FAINT)
                    .child("留空的项表示不覆盖内置默认值；设置将应用于之后新建的任务。"),
            )
            .child(self.settings_field(SettingsField::Dir, &dir, cx))
            .child(self.settings_field(SettingsField::Split, &split, cx))
            .child(self.settings_field(SettingsField::MinSplit, &min_split, cx))
            .child(self.settings_field(SettingsField::UserAgent, &ua, cx))
            .child(self.settings_field(SettingsField::Referer, &referer, cx))
            .child(self.settings_field(SettingsField::Headers, &headers, cx));

        if let Some(err) = error {
            card = card.child(
                div()
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .bg(t::ERR_SOFT)
                    .text_xs()
                    .text_color(t::ERR)
                    .child(err),
            );
        }

        card.child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_end()
                .gap_2()
                .child(
                    w::btn_ghost()
                        .id("set-cancel")
                        .child("取消")
                        .on_click(cx.listener(|this, _e: &gpui::ClickEvent, _w, cx| {
                            this.settings = None;
                            cx.notify();
                        })),
                )
                .child(
                    w::btn_ghost()
                        .id("set-reset")
                        .child("恢复默认")
                        .on_click(cx.listener(|this, _e: &gpui::ClickEvent, _w, cx| {
                            this.defaults = ParsedSettings::default();
                            this.settings = None;
                            this.push_toast("已恢复为内置默认设置", ToastKind::Info);
                            cx.notify();
                        })),
                )
                .child(
                    w::btn_primary()
                        .id("set-save")
                        .child("保存设置")
                        .on_click(cx.listener(|this, _e: &gpui::ClickEvent, _w, cx| {
                            this.save_settings(cx)
                        })),
                ),
        )
    }

    // ── 二次确认 ──────────────────────────────────────────────────────────

    fn ask_confirm(&mut self, state: ConfirmState, cx: &mut Context<Self>) {
        self.confirm = Some(state);
        cx.notify();
    }

    fn do_confirm(&mut self, cx: &mut Context<Self>) {
        let Some(c) = self.confirm.take() else { return };
        match c.action {
            ConfirmAction::Remove { gid, force } => match self.manager.remove(gid, force) {
                Ok(_) => {
                    self.expanded.remove(&gid);
                    self.push_toast("已移除任务", ToastKind::Ok);
                }
                Err(e) => self.push_toast(format!("移除失败：{e}"), ToastKind::Error),
            },
            ConfirmAction::PurgeCompleted => {
                self.manager.purge_download_result();
                self.push_toast("已清理已结束的任务", ToastKind::Ok);
            }
        }
        self.refresh();
        cx.notify();
    }

    fn render_confirm(&mut self, cx: &mut Context<Self>) -> gpui::Stateful<Div> {
        let (title, message, danger, confirm_text) = match self.confirm.as_ref() {
            Some(c) => (
                c.title.clone(),
                c.message.clone(),
                c.danger,
                c.confirm_text.clone(),
            ),
            None => return div().id("noop-confirm"),
        };

        div()
            .id("confirm-dialog")
            .w(px(420.0))
            .flex()
            .flex_col()
            .gap_3()
            .p_4()
            .rounded_lg()
            .bg(t::DIALOG_BG)
            .border_1()
            .border_color(t::BORDER)
            .track_focus(&self.dlg_focus)
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _w, cx| {
                match ev.keystroke.key.as_str() {
                    "escape" => {
                        this.confirm = None;
                        cx.notify();
                    }
                    "enter" => this.do_confirm(cx),
                    _ => {}
                }
            }))
            .child(
                div()
                    .text_lg()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child(title),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(t::TEXT_DIM)
                    .child(message),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_end()
                    .gap_2()
                    .child(
                        w::btn_ghost()
                            .id("confirm-cancel")
                            .child("取消")
                            .on_click(cx.listener(|this, _e: &gpui::ClickEvent, _w, cx| {
                                this.confirm = None;
                                cx.notify();
                            })),
                    )
                    .child({
                        let btn = if danger { w::btn_danger() } else { w::btn_primary() };
                        btn.id("confirm-ok")
                            .child(confirm_text)
                            .on_click(cx.listener(|this, _e: &gpui::ClickEvent, _w, cx| {
                                this.do_confirm(cx)
                            }))
                    }),
            )
    }

    // ── 弹窗遮罩 ──────────────────────────────────────────────────────────

    /// 居中模态：半透明遮罩 + 居中卡片。点遮罩或按 Esc 关闭。
    fn modal(&mut self, inner: impl IntoElement, cx: &mut Context<Self>) -> Div {
        div()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(t::OVERLAY)
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _e: &gpui::MouseDownEvent, _w, cx| {
                    this.dialog = None;
                    this.settings = None;
                    this.confirm = None;
                    cx.notify();
                }),
            )
            .child(
                div()
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(|_this, _e: &gpui::MouseDownEvent, _w, cx| {
                            // 阻止冒泡：点卡片本身不应关闭弹窗
                            cx.stop_propagation();
                        }),
                    )
                    .child(inner),
            )
    }

    // ── 任务行 ────────────────────────────────────────────────────────────

    fn task_row(&self, idx: usize, s: &Snapshot, cx: &mut Context<Self>) -> impl IntoElement {
        let gid = s.gid;
        let status = s.status;
        let color = status_color(status);
        let pct = match s.total {
            Some(total) if total > 0 => (s.completed as f32 / total as f32).clamp(0.0, 1.0),
            _ => 0.0,
        };
        let expanded = self.expanded.contains(&gid);
        let manager = self.manager.clone();
        let manager2 = self.manager.clone();

        let (can_pause, can_resume) = match status {
            TaskStatus::Active | TaskStatus::Waiting => (true, false),
            TaskStatus::Paused => (false, true),
            _ => (false, false),
        };
        let can_remove = !matches!(status, TaskStatus::Removed);

        // 进度细分：把分片位图聚合成最多 72 个可视桶
        let buckets = w::piece_buckets(&s.bitfield, s.num_pieces, 72);
        let eta = w::eta_text(s.total, s.completed, s.speed);
        let remaining = match s.total {
            Some(total) => total.saturating_sub(s.completed),
            None => 0,
        };

        let mut row = div()
            .id(("row", idx))
            .flex()
            .flex_col()
            .gap_2()
            .px_4()
            .py_3()
            .border_b_1()
            .border_color(t::BORDER_SOFT)
            .bg(if expanded { t::CARD_ACTIVE } else { t::CARD })
            .hover(|st| st.bg(if expanded { t::CARD_ACTIVE } else { t::CARD_HOVER }))
            // ── 第一行：状态点 + 文件名 + 状态徽标 + 速度 ──
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(div().w(px(8.0)).h(px(8.0)).rounded_full().bg(color))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_sm()
                            .text_color(t::TEXT)
                            .child(s.filename.clone()),
                    )
                    .child(w::badge(status_label(status), color))
                    .child(
                        div()
                            .w(px(88.0))
                            .text_right()
                            .text_xs()
                            .text_color(if s.speed > 0 { t::ACCENT } else { t::TEXT_FAINT })
                            .child(if s.speed > 0 {
                                format!("{}/s", human(s.speed))
                            } else {
                                "—".to_string()
                            }),
                    ),
            )
            // ── 第二行：分片进度图（进度细分）或退化为整体进度条 ──
            .child(if buckets.is_empty() {
                w::progress_bar(pct, color, 6.0).into_any_element()
            } else {
                w::piece_map(&buckets, color, 8.0).into_any_element()
            })
            // ── 第三行：百分比 · 体量 · 剩余 · ETA · 连接数 · 操作 ──
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .text_xs()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(color)
                            .child(format!("{:.1}%", pct * 100.0)),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(t::TEXT_DIM)
                            .child(format!(
                                "{} / {}",
                                human(s.completed),
                                s.total.map(human).unwrap_or_else(|| "未知".into())
                            )),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(t::TEXT_FAINT)
                            .child(format!("剩余 {}", human(remaining))),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(t::TEXT_FAINT)
                            .child(format!("ETA {}", eta)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .text_xs()
                            .text_color(t::TEXT_FAINT)
                            .child(format!("{} 连接", s.connections)),
                    )
                    .child({
                        // 详情开关
                        w::btn_mini(t::TEXT_FAINT, t::TEXT_DIM)
                            .id(("detail", gid as usize))
                            .child(if expanded { "收起" } else { "详情" })
                            .on_click(cx.listener(move |this, _e: &gpui::ClickEvent, _w, cx| {
                                this.toggle_expand(gid);
                                cx.notify();
                            }))
                    })
                    .when(can_pause, |el| {
                        el.child(
                            w::btn_mini(t::TEXT_FAINT, t::TEXT_DIM)
                                .id(("pause", gid as usize))
                                .child("暂停")
                                .on_click(cx.listener(move |this, _e: &gpui::ClickEvent, _w, cx| {
                                    let _ = manager.pause(gid);
                                    this.refresh();
                                    cx.notify();
                                })),
                        )
                    })
                    .when(can_resume, |el| {
                        el.child(
                            w::btn_mini(t::ACCENT, t::ACCENT)
                                .id(("resume", gid as usize))
                                .child("继续")
                                .on_click(cx.listener(move |this, _e: &gpui::ClickEvent, _w, cx| {
                                    let _ = manager2.unpause(gid);
                                    manager2.kick();
                                    this.refresh();
                                    cx.notify();
                                })),
                        )
                    })
                    .when(can_remove, |el| {
                        el.child(
                            w::btn_mini(t::ERR, t::ERR)
                                .id(("remove", gid as usize))
                                .child("移除")
                                .on_click(cx.listener(move |this, _e: &gpui::ClickEvent, _w, cx| {
                                    this.ask_confirm(
                                        ConfirmState {
                                            title: "移除任务".into(),
                                            message: format!("确定要移除「{}」吗？", name_of(this, gid)),
                                            action: ConfirmAction::Remove { gid, force: true },
                                            danger: true,
                                            confirm_text: "移除".into(),
                                        },
                                        cx,
                                    )
                                })),
                        )
                    }),
            )
            .when_some(s.error.clone(), |el, err| {
                el.child(
                    div()
                        .text_xs()
                        .text_color(t::ERR)
                        .child(format!("错误：{err}")),
                )
            });

        if expanded {
            row = row.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .bg(t::BG)
                    .border_1()
                    .border_color(t::BORDER_SOFT)
                    .child(w::kv("GID", acgdev_core::util::gid_to_hex(gid)))
                    .child(w::kv("地址", s.uris.join(" , ")))
                    .child(w::kv(
                        "保存位置",
                        s.dir.join(&s.filename).to_string_lossy().to_string(),
                    ))
                    .child(w::kv(
                        "分片",
                        format!(
                            "{} 片 × {}（已完成 {} 片）",
                            s.num_pieces,
                            human(s.piece_len),
                            count_done_pieces(&s.bitfield, s.num_pieces)
                        ),
                    ))
                    // 「具体分片」：把位图还原成连续区间，直接看出哪几段已下完、哪几段还空着
                    .child(w::kv(
                        "已完成分片",
                        fmt_segments(&done_segments(&s.bitfield, s.num_pieces, s.piece_len), 4),
                    ))
                    .child(w::kv(
                        "待下载分片",
                        fmt_segments(&pending_segments(&s.bitfield, s.num_pieces, s.piece_len), 4),
                    ))
                    .child(w::kv("连接数", s.connections.to_string()))
                    .child(w::kv("已下载", format!("{} 字节", s.completed))),
            );
        }

        row
    }
}

fn count_done_pieces(bitfield: &[u8], num_pieces: u64) -> usize {
    (0..num_pieces as usize)
        .filter(|&i| acgdev_core::task::piece_done(bitfield, i))
        .count()
}

/// 从当前任务列表里取文件名，供确认弹窗提示用。
fn name_of(app: &AcgDevApp, gid: Gid) -> String {
    app.tasks
        .iter()
        .find(|s| s.gid == gid)
        .map(|s| s.filename.clone())
        .unwrap_or_else(|| "该任务".to_string())
}

fn status_color(s: TaskStatus) -> gpui::Rgba {
    match s {
        TaskStatus::Active => t::ACCENT,
        TaskStatus::Waiting => t::TEXT_DIM,
        TaskStatus::Paused => t::WARN,
        TaskStatus::Complete => t::OK,
        TaskStatus::Error => t::ERR,
        TaskStatus::Removed => t::TEXT_FAINT,
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

fn toast_color(kind: ToastKind) -> gpui::Rgba {
    match kind {
        ToastKind::Info => t::ACCENT,
        ToastKind::Ok => t::OK,
        ToastKind::Warn => t::WARN,
        ToastKind::Error => t::ERR,
    }
}

impl Render for AcgDevApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 先算好行数，结束对 self.tasks 的借用
        let rows: Vec<gpui::AnyElement> = {
            let filtered: Vec<&Snapshot> = self
                .tasks
                .iter()
                .filter(|s| self.filter.matches(s.status))
                .collect();
            filtered
                .iter()
                .enumerate()
                .map(|(i, s)| self.task_row(i, s, cx).into_any_element())
                .collect()
        };
        let shown = rows.len();
        let total = self.tasks.len();
        let filter = self.filter;

        let mut root = div()
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
                            .gap_2()
                            .child(
                                div()
                                    .text_lg()
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .child("AcgDev"),
                            )
                            .child(w::badge(self.rpc_hint.clone(), t::ACCENT))
                            // MiSans 许可协议第 2 条第 1 款要求：须在软件中注明使用了该字体。
                            .child(w::badge("MiSans · 小米", t::TEXT_DIM)),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_4()
                            .child(
                                div()
                                    .text_lg()
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(t::ACCENT)
                                    .child(format!("{}/s", human(self.speed))),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(t::TEXT_DIM)
                                    .child(format!(
                                        "{} 活动 · {} 等待 · {} 已结束",
                                        self.num_active, self.num_waiting, self.num_done
                                    )),
                            ),
                    ),
            )
            // ── 工具栏：筛选 + 批量操作 + 快速添加 ──
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
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
                            .justify_between()
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap_1()
                                    .children(Filter::ALL.iter().enumerate().map(|(i, f)| {
                                        let f = *f;
                                        let active = filter == f;
                                        div()
                                            .id(("tab", i))
                                            .px_3()
                                            .py(px(5.0))
                                            .rounded_md()
                                            .cursor_pointer()
                                            .bg(if active { t::ACCENT_SOFT } else { t::PANEL_ALT })
                                            .text_xs()
                                            .text_color(if active { t::ACCENT } else { t::TEXT_DIM })
                                            .child(f.label().to_string())
                                            .on_click(cx.listener(move |this, _e: &gpui::ClickEvent, _w, cxx| {
                                                this.filter = f;
                                                cxx.notify();
                                            }))
                                    })),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap_2()
                                    .child({
                                        let m = self.manager.clone();
                                        w::btn_ghost()
                                            .id("act-pause-all")
                                            .child("全部暂停")
                                            .on_click(cx.listener(move |this, _e: &gpui::ClickEvent, _w, cxx| {
                                                m.pause_all();
                                                this.refresh();
                                                this.push_toast("已暂停全部任务", ToastKind::Info);
                                                cxx.notify();
                                            }))
                                    })
                                    .child({
                                        let m = self.manager.clone();
                                        w::btn_ghost()
                                            .id("act-resume-all")
                                            .child("全部继续")
                                            .on_click(cx.listener(move |this, _e: &gpui::ClickEvent, _w, cxx| {
                                                m.unpause_all();
                                                m.kick();
                                                this.refresh();
                                                this.push_toast("已继续全部任务", ToastKind::Info);
                                                cxx.notify();
                                            }))
                                    })
                                    .child(
                                        w::btn_ghost()
                                            .id("act-settings")
                                            .child("高级设置")
                                            .on_click(cx.listener(
                                                |this, _e: &gpui::ClickEvent, w, cxx| {
                                                    this.open_settings(w, cxx)
                                                },
                                            )),
                                    )
                                    .child(
                                        w::btn_ghost()
                                            .id("act-purge")
                                            .child("清理已结束")
                                            .on_click(cx.listener(|this, _e: &gpui::ClickEvent, _w, cxx| {
                                                this.ask_confirm(
                                                    ConfirmState {
                                                        title: "清理已结束的任务".into(),
                                                        message: "将清空所有已完成 / 失败 / 已移除的记录，文件不会被删除。".into(),
                                                        action: ConfirmAction::PurgeCompleted,
                                                        danger: true,
                                                        confirm_text: "清理".into(),
                                                    },
                                                    cxx,
                                                )
                                            })),
                                    )
                                    .child(
                                        w::btn_primary()
                                            .id("act-new")
                                            .child("＋ 新建任务")
                                            .on_click(cx.listener(
                                                |this, _e: &gpui::ClickEvent, w, cxx| {
                                                    this.open_dialog(w, cxx)
                                                },
                                            )),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
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
                                                t::TEXT_FAINT
                                            } else {
                                                t::TEXT
                                            })
                                            .child(if self.url_input.is_empty() {
                                                "快速添加：粘贴或输入地址，回车即添加（多个用空格分隔）"
                                                    .to_string()
                                            } else {
                                                self.url_input.clone()
                                            }),
                                    ),
                            )
                            .child(
                                w::btn_primary()
                                    .id("quick-add")
                                    .h(px(34.0))
                                    .child("添加")
                                    .on_click(cx.listener(|this, _e: &gpui::ClickEvent, _w, cxx| {
                                        this.add_from_input(cxx)
                                    })),
                            ),
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
                                .flex_col()
                                .size_full()
                                .items_center()
                                .justify_center()
                                .gap_2()
                                .child(
                                    div()
                                        .text_sm()
                                        .text_color(t::TEXT_DIM)
                                        .child(if total == 0 {
                                            "暂无任务".to_string()
                                        } else {
                                            "当前筛选条件下没有任务".to_string()
                                        }),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(t::TEXT_FAINT)
                                        .child("点右上角「＋ 新建任务」，或把链接粘到上面的输入框"),
                                ),
                        )
                    })
                    .children(rows),
            )
            // ── 状态栏 ──
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .px_4()
                    .py_2()
                    .bg(t::PANEL)
                    .border_t_1()
                    .border_color(t::BORDER)
                    .child(
                        div()
                            .text_xs()
                            .text_color(t::TEXT_FAINT)
                            .child(format!(
                                "显示 {} / 共 {} 个任务",
                                shown, total
                            )),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(t::TEXT_FAINT)
                            .child(format!(
                                "下载目录：{}",
                                self.default_dir().to_string_lossy()
                            )),
                    ),
            );

        // ── 弹窗层 ──
        if self.settings.is_some() {
            let s = self.render_settings(cx);
            root = root.child(self.modal(s, cx));
        } else if self.dialog.is_some() {
            let d = self.render_add_dialog(cx);
            root = root.child(self.modal(d, cx));
        } else if self.confirm.is_some() {
            let c = self.render_confirm(cx);
            root = root.child(self.modal(c, cx));
        }

        // ── Toast 浮层（右上角） ──
        if !self.toasts.is_empty() {
            let mut stack = div()
                .absolute()
                .top(px(56.0))
                .right(px(16.0))
                .flex()
                .flex_col()
                .gap_2();
            for toast in &self.toasts {
                let c = toast_color(toast.kind);
                stack = stack.child(
                    div()
                        .id(("toast", toast.id as usize))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .px_3()
                        .py_2()
                        .rounded_md()
                        .bg(match toast.kind {
                            ToastKind::Ok => t::OK_SOFT,
                            ToastKind::Warn => t::WARN_SOFT,
                            ToastKind::Error => t::ERR_SOFT,
                            ToastKind::Info => t::TOAST_BG,
                        })
                        .border_1()
                        .border_color(t::with_alpha(c, 0.55))
                        .child(div().w(px(6.0)).h(px(6.0)).rounded_full().bg(c))
                        .child(
                            div()
                                .text_xs()
                                .text_color(t::TEXT)
                                .child(toast.msg.clone()),
                        ),
                );
            }
            root = root.child(stack);
        }

        root
    }
}
