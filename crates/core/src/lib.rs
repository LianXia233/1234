//! AcgDev 下载引擎核心。
//!
//! 模块划分：
//! - [`range`]  待下载区间集合（0-based 半开区间，可合并）
//! - [`util`]   单位换算、文件名清洗、GID 编解码
//! - [`error`]  显式化错误（拒绝静默失败）
//! - `probe`    远端探测（长度 / Range 支持 / ETag）
//! - `resume`   断点元数据（原子写 + 校验）
//! - `writer`   预分配 + pwrite + 短写检测
//! - `task`     单任务状态机与执行
//! - `manager`  任务表 + 事件广播
//! - `rpc`      aria2 兼容的 JSON-RPC 2.0 服务端

pub mod error;
pub mod manager;
pub mod probe;
pub mod range;
pub mod resume;
pub mod rpc;
pub mod task;
pub mod util;
pub mod writer;

pub use error::{Error, Result};
pub use manager::{Manager, ManagerConfig};
pub use range::{PendingRanges, Segment};
pub use rpc::{Dispatcher, RpcConfig};
pub use task::{Gid, Snapshot, Task, TaskOptions, TaskStatus};
