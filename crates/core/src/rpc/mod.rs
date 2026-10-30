//! aria2 兼容的 JSON-RPC 2.0 服务端。
//!
//! 目标：让 AriaNg / Motrix / YAAW 等现成 aria2 前端**零改动**接管本程序。
//!
//! ```text
//!   AriaNg  ──POST /jsonrpc──▶  Dispatcher ──▶ Manager ──▶ Task
//!      ▲                          │
//!      └────WS 事件通知────────────┘
//! ```

pub mod aria2;
pub mod protocol;
pub mod server;

pub use aria2::{Dispatcher, METHODS, VERSION};
pub use protocol::{Request, Response};
pub use server::{RpcConfig, serve};
