//! 弹窗相关的**纯状态与校验逻辑**（不依赖 gpui）。
//!
//! 渲染放在 `app.rs`：那里才有 `Context<Self>`，方便直接绑定 `on_click` / `on_key_down`。
//! 这样拆分的好处是本文件的解析与校验规则可以脱离 UI 单独测试。

use std::path::PathBuf;
use std::time::{Duration, Instant};

use acgdev_core::Gid;

// ── 新建任务对话框 ────────────────────────────────────────────────────────

/// 对话框里可聚焦的字段。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Field {
    Urls,
    Filename,
    Dir,
    Split,
}

impl Field {
    /// Tab 循环顺序。
    pub fn next(self) -> Field {
        match self {
            Field::Urls => Field::Filename,
            Field::Filename => Field::Dir,
            Field::Dir => Field::Split,
            Field::Split => Field::Urls,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Field::Urls => "下载地址",
            Field::Filename => "保存文件名",
            Field::Dir => "保存到",
            Field::Split => "连接数",
        }
    }

    pub fn placeholder(self) -> &'static str {
        match self {
            Field::Urls => "https://example.com/file.zip（多个用空格或换行分隔，可直接粘贴）",
            Field::Filename => "留空则自动从地址推断",
            Field::Dir => "留空则使用默认下载目录",
            Field::Split => "8",
        }
    }
}

/// 校验通过后的结构化结果。
pub struct ParsedTask {
    pub uris: Vec<String>,
    pub split: usize,
    pub dir: PathBuf,
    pub out: Option<String>,
}

/// 新建任务对话框状态。
pub struct AddTaskDialog {
    pub urls: String,
    pub filename: String,
    pub dir: String,
    pub split: String,
    /// 添加后是否先暂停（不立即开始）。
    pub start_paused: bool,
    /// 当前接收键盘输入的字段。
    pub field: Field,
    /// 校验错误，展示在弹窗底部。
    pub error: Option<String>,
}

impl AddTaskDialog {
    pub fn new(default_dir: PathBuf) -> Self {
        Self {
            urls: String::new(),
            filename: String::new(),
            dir: default_dir.to_string_lossy().to_string(),
            split: String::from("8"),
            start_paused: false,
            field: Field::Urls,
            error: None,
        }
    }

    pub fn field_mut(&mut self, f: Field) -> &mut String {
        match f {
            Field::Urls => &mut self.urls,
            Field::Filename => &mut self.filename,
            Field::Dir => &mut self.dir,
            Field::Split => &mut self.split,
        }
    }

    /// 解析并校验表单。
    ///
    /// 只接受 http/https/ftp；静默丢弃其它片段（例如粘贴时混进的说明文字），
    /// 全部无效时才报错，避免"整段粘贴却一个都不加"的困惑。
    pub fn parse(&self, fallback_dir: PathBuf) -> Result<ParsedTask, String> {
        let uris: Vec<String> = self
            .urls
            .split_whitespace()
            .filter(|s| {
                s.starts_with("http://") || s.starts_with("https://") || s.starts_with("ftp://")
            })
            .map(|s| s.to_string())
            .collect();

        if uris.is_empty() {
            return Err("请填写至少一个 http:// 或 https:// 开头的下载地址".into());
        }

        let split = match self.split.trim().parse::<usize>() {
            Ok(0) => return Err("连接数至少为 1".into()),
            Ok(n) => n.min(32),
            Err(_) => return Err("连接数必须是数字".into()),
        };

        let dir = if self.dir.trim().is_empty() {
            fallback_dir
        } else {
            PathBuf::from(self.dir.trim())
        };

        let out = match self.filename.trim() {
            "" => None,
            name => Some(name.to_string()),
        };

        Ok(ParsedTask {
            uris,
            split,
            dir,
            out,
        })
    }
}

// ── 二次确认弹窗 ──────────────────────────────────────────────────────────

/// 需要二次确认的破坏性/批量操作。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ConfirmAction {
    /// 删除单个任务（force=true 时不等任务优雅停止）。
    Remove { gid: Gid, force: bool },
    /// 清理全部已结束（完成/失败/已移除）的任务。
    PurgeCompleted,
}

pub struct ConfirmState {
    pub title: String,
    pub message: String,
    pub action: ConfirmAction,
    /// 危险操作用红色主按钮。
    pub danger: bool,
    pub confirm_text: String,
}

// ── Toast ────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ToastKind {
    Info,
    Ok,
    Warn,
    Error,
}

pub struct Toast {
    pub id: u64,
    pub msg: String,
    pub kind: ToastKind,
    pub deadline: Instant,
}

impl Toast {
    pub fn new(id: u64, msg: impl Into<String>, kind: ToastKind) -> Self {
        Self {
            id,
            msg: msg.into(),
            kind,
            deadline: Instant::now() + Duration::from_millis(3200),
        }
    }

    pub fn expired(&self) -> bool {
        Instant::now() >= self.deadline
    }
}

// ── 高级设置 ──────────────────────────────────────────────────────────────

/// 高级设置里可聚焦的字段。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SettingsField {
    Dir,
    Split,
    MinSplit,
    UserAgent,
    Referer,
    Headers,
}

impl SettingsField {
    pub fn next(self) -> SettingsField {
        match self {
            SettingsField::Dir => SettingsField::Split,
            SettingsField::Split => SettingsField::MinSplit,
            SettingsField::MinSplit => SettingsField::UserAgent,
            SettingsField::UserAgent => SettingsField::Referer,
            SettingsField::Referer => SettingsField::Headers,
            SettingsField::Headers => SettingsField::Dir,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            SettingsField::Dir => "默认下载目录",
            SettingsField::Split => "默认连接数",
            SettingsField::MinSplit => "最小分片大小",
            SettingsField::UserAgent => "User-Agent",
            SettingsField::Referer => "Referer",
            SettingsField::Headers => "自定义请求头",
        }
    }

    pub fn placeholder(self) -> &'static str {
        match self {
            SettingsField::Dir => "留空则使用启动时的目录",
            SettingsField::Split => "留空则用 8",
            SettingsField::MinSplit => "如 4M / 512K，留空则用 4M",
            SettingsField::UserAgent => "留空则用内置 UA",
            SettingsField::Referer => "留空则不发送 Referer",
            SettingsField::Headers => "每行一条：Name: Value",
        }
    }
}

/// 高级设置对话框状态（编辑中的文本）。
pub struct SettingsState {
    pub dir: String,
    pub split: String,
    pub min_split: String,
    pub user_agent: String,
    pub referer: String,
    pub headers: String,
    pub field: SettingsField,
    pub error: Option<String>,
}

/// 解析后的设置；`None` 表示该项不覆盖（沿用内置默认）。
#[derive(Clone, Default)]
pub struct ParsedSettings {
    pub dir: Option<std::path::PathBuf>,
    pub split: Option<usize>,
    pub min_split_size: Option<u64>,
    pub user_agent: Option<String>,
    pub referer: Option<String>,
    pub headers: Vec<(String, String)>,
}

impl SettingsState {
    /// 以当前生效设置为初值打开对话框。
    pub fn from_current(cur: &ParsedSettings) -> Self {
        Self {
            dir: cur
                .dir
                .as_ref()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default(),
            split: cur.split.map(|n| n.to_string()).unwrap_or_default(),
            min_split: cur
                .min_split_size
                .map(|n| acgdev_core::util::size_str(n))
                .unwrap_or_default(),
            user_agent: cur.user_agent.clone().unwrap_or_default(),
            referer: cur.referer.clone().unwrap_or_default(),
            headers: cur
                .headers
                .iter()
                .map(|(k, v)| format!("{k}: {v}"))
                .collect::<Vec<_>>()
                .join("\n"),
            field: SettingsField::Dir,
            error: None,
        }
    }

    pub fn field_mut(&mut self, f: SettingsField) -> &mut String {
        match f {
            SettingsField::Dir => &mut self.dir,
            SettingsField::Split => &mut self.split,
            SettingsField::MinSplit => &mut self.min_split,
            SettingsField::UserAgent => &mut self.user_agent,
            SettingsField::Referer => &mut self.referer,
            SettingsField::Headers => &mut self.headers,
        }
    }

    /// 解析设置。空值表示不覆盖；只有格式错误才算失败。
    pub fn parse(&self) -> Result<ParsedSettings, String> {
        let dir = match self.dir.trim() {
            "" => None,
            p => Some(PathBuf::from(p)),
        };

        let split = match self.split.trim() {
            "" => None,
            s => match s.parse::<usize>() {
                Ok(0) => return Err("连接数至少为 1".into()),
                Ok(n) => Some(n.min(32)),
                Err(_) => return Err("连接数必须是数字".into()),
            },
        };

        let min_split_size = match self.min_split.trim() {
            "" => None,
            s => match acgdev_core::util::parse_size(s) {
                Some(0) => return Err("最小分片大小必须大于 0".into()),
                Some(n) => Some(n),
                None => return Err("最小分片大小格式不对，例如 4M、512K".into()),
            },
        };

        let opt = |s: &str| match s.trim() {
            "" => None,
            v => Some(v.to_string()),
        };
        let user_agent = opt(&self.user_agent);
        let referer = opt(&self.referer);

        let mut headers = Vec::new();
        for line in self.headers.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            match line.split_once(':') {
                Some((k, v)) if !k.trim().is_empty() => {
                    headers.push((k.trim().to_string(), v.trim().to_string()))
                }
                _ => return Err(format!("请求头格式应为 “Name: Value”，收到：{line}")),
            }
        }

        Ok(ParsedSettings {
            dir,
            split,
            min_split_size,
            user_agent,
            referer,
            headers,
        })
    }
}
