//! 统一错误类型。
//!
//! 设计原则（对应原版缺陷）：
//! - F-01 写入失败静默丢弃 → 短写必须成为显式错误，不能"当成功"
//! - F-03 续传无校验 → 远端变更必须拒绝续传
//! - F-11 配置解析崩溃 → 解析错误必须可捕获，不得 panic

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("HTTP 错误: {0}")]
    Http(#[from] reqwest::Error),

    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON 错误: {0}")]
    Json(#[from] serde_json::Error),

    #[error("URL 无效: {0}")]
    InvalidUrl(String),

    #[error("服务器不支持 Range，无法多线程/续传")]
    NoRangeSupport,

    /// F-03：续传前必须校验 ETag / Last-Modified / 总长度。
    #[error("远端文件已变更（{field}：期望 {expected}，实际 {actual}），拒绝续传")]
    RemoteChanged {
        field: &'static str,
        expected: String,
        actual: String,
    },

    #[error("响应长度不符：期望 {expected} 字节，实际 {actual} 字节")]
    LengthMismatch { expected: u64, actual: u64 },

    /// F-01：pwrite 返回的写入字节数小于请求，属于**数据损坏**，
    /// 绝不能像原版那样静默忽略。
    #[error("短写：offset={offset} 请求 {want} 字节，实际写入 {got} 字节")]
    ShortWrite {
        offset: u64,
        want: usize,
        got: usize,
    },

    #[error("下载已被取消")]
    Cancelled,

    #[error("任务不存在: {0}")]
    TaskNotFound(String),

    #[error("参数错误: {0}")]
    BadRequest(String),

    #[error("状态文件损坏: {0}")]
    Corrupt(String),

    #[error("其它: {0}")]
    Other(String),
}

impl Error {
    /// 是否属于可重试的瞬时错误（网络抖动、超时、5xx）。
    pub fn is_retryable(&self) -> bool {
        match self {
            Error::Http(e) => e.is_timeout() || e.is_connect() || e.is_request(),
            Error::Io(e) => matches!(
                e.kind(),
                std::io::ErrorKind::Interrupted
                    | std::io::ErrorKind::WouldBlock
                    | std::io::ErrorKind::TimedOut
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
            ),
            _ => false,
        }
    }

    /// 是否属于致命错误（重试无意义）。
    pub fn is_fatal(&self) -> bool {
        matches!(
            self,
            Error::NoRangeSupport
                | Error::RemoteChanged { .. }
                | Error::LengthMismatch { .. }
                | Error::Corrupt(_)
                | Error::InvalidUrl(_)
                | Error::BadRequest(_)
        )
    }
}

pub type Result<T> = std::result::Result<T, Error>;
