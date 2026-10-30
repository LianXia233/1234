//! AcgDev — Rust + GPUI 重构版。
//!
//! 进程结构：
//! ```text
//!  main thread            tokio runtime thread
//!  ┌──────────────┐        ┌───────────────────────────────┐
//!  │ GPUI 窗口     │◀─快照──│ Manager (任务表 / 调度 / 统计) │
//!  │ 500ms 轮询    │        │   ├─ Task workers (pwrite)    │
//!  └──────────────┘        │   └─ aria2 RPC :6800          │
//!                          └───────────────────────────────┘
//! ```
//!
//! UI 与下载引擎之间**只共享 `Arc<Manager>`**，不做跨 executor 的 future 传递，
//! 因此 GPUI 的 smol executor 与 tokio 互不干扰。

// 发布版按 GUI 程序启动：双击不会弹出一个黑色控制台窗口。
// 需要用命令行（--help / --headless / 日志）时，由 ensure_console 临时接管。
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod ui;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use acgdev_core::manager::{Manager, ManagerConfig};
use acgdev_core::rpc::{Dispatcher, RpcConfig, serve};
use gpui::prelude::*;
use gpui::{App, Application, Bounds, WindowBounds, WindowOptions, px, size};
use tokio_util::sync::CancellationToken;

struct Args {
    dir: PathBuf,
    rpc_bind: Option<SocketAddr>,
    secret: Option<String>,
    concurrent: usize,
    headless: bool,
}

/// GUI 子系统启动时进程没有控制台，`println!` 会静默丢字节。
///
/// 但从命令行（或带重定向）启动时，标准句柄是**继承**来的、本身有效，
/// 这时绝不能覆盖：一覆盖 `acgdev --help > help.txt` 就写不进文件了。
/// 只有句柄为空（双击启动）才自己开一个控制台。
///
/// 直接声明 kernel32 导入，避免为一个函数拖进整个 windows-sys 依赖。
#[cfg(all(windows, not(debug_assertions)))]
fn ensure_console() {
    use std::ffi::c_void;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetStdHandle(which: u32) -> *mut c_void;
        fn AllocConsole() -> i32;
        fn SetStdHandle(which: u32, h_handle: *mut c_void) -> i32;
        fn CreateFileW(
            name: *const u16,
            access: u32,
            share: u32,
            sa: *const c_void,
            disposition: u32,
            flags: u32,
            template: *mut c_void,
        ) -> *mut c_void;
    }

    const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;
    const STD_ERROR_HANDLE: u32 = -12i32 as u32;
    const GENERIC_READ: u32 = 0x8000_0000;
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const FILE_SHARE_READ: u32 = 0x1;
    const FILE_SHARE_WRITE: u32 = 0x2;
    const OPEN_EXISTING: u32 = 3;
    let invalid = -1isize as *mut c_void;

    // SAFETY: 只调用无需前置状态的 Win32 控制台 API。
    unsafe {
        let inherited = GetStdHandle(STD_OUTPUT_HANDLE);
        if !inherited.is_null() && inherited != invalid {
            return; // 命令行/重定向启动，句柄已可用
        }
        if AllocConsole() == 0 {
            return;
        }
        // AllocConsole 后把标准句柄显式接到 CONOUT$。
        let name: Vec<u16> = "CONOUT$\0".encode_utf16().collect();
        let h = CreateFileW(
            name.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        );
        if h != invalid {
            SetStdHandle(STD_OUTPUT_HANDLE, h);
            SetStdHandle(STD_ERROR_HANDLE, h);
        }
    }
}

#[cfg(not(all(windows, not(debug_assertions))))]
fn ensure_console() {}

fn parse_args() -> Args {
    let mut a = Args {
        dir: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        rpc_bind: Some(SocketAddr::from(([0, 0, 0, 0], 6800))),
        secret: None,
        concurrent: 5,
        headless: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        match k.as_str() {
            "--dir" | "-d" => {
                if let Some(v) = it.next() {
                    a.dir = PathBuf::from(v);
                }
            }
            "--rpc-port" => {
                if let Some(v) = it.next() {
                    match v.parse::<u16>() {
                        Ok(0) => a.rpc_bind = None,
                        Ok(p) => a.rpc_bind = Some(SocketAddr::from(([0, 0, 0, 0], p))),
                        Err(_) => eprintln!("--rpc-port 取值非法: {v}"),
                    }
                }
            }
            "--rpc-secret" => a.secret = it.next(),
            "--concurrent" | "-j" => {
                if let Some(v) = it.next() {
                    if let Ok(n) = v.parse() {
                        a.concurrent = n;
                    }
                }
            }
            "--headless" => a.headless = true,
            "--help" | "-h" => {
                ensure_console();
                println!(
                    "AcgDev {ver}
用法: acgdev [选项]

  -d, --dir <PATH>        默认下载目录（默认当前目录）
      --rpc-port <PORT>   aria2 兼容 RPC 端口（默认 6800，0 关闭）
      --rpc-secret <TOK>  RPC 密钥（对应 token:<TOK>）
  -j, --concurrent <N>    同时下载任务数（默认 5）
      --headless          不打开窗口，仅运行 RPC 服务
  -h, --help              显示帮助",
                    ver = env!("CARGO_PKG_VERSION")
                );
                std::process::exit(0);
            }
            other => {
                ensure_console();
                eprintln!("忽略未知参数: {other}");
            }
        }
    }
    a
}

fn main() {
    // 日志与帮助文本都走标准输出/错误，GUI 子系统下这些句柄是空的，
    // 必须在 tracing 初始化之前把控制台接回来。
    if std::env::args().any(|a| a == "--headless" || a == "-h" || a == "--help") {
        ensure_console();
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "acgdev=info,acgdev_core=info".into()),
        )
        .with_target(false)
        .init();

    let args = parse_args();

    // tokio 运行时放到独立线程，生命周期与进程相同
    let (init_tx, init_rx) = std::sync::mpsc::channel::<(Arc<Manager>, String)>();

    let dir = args.dir.clone();
    let rpc_bind = args.rpc_bind;
    let secret = args.secret.clone();
    let concurrent = args.concurrent;

    std::thread::Builder::new()
        .name("acgdev-engine".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(4)
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    eprintln!("无法创建 tokio 运行时: {e}");
                    std::process::exit(1);
                }
            };

            rt.block_on(async move {
                let client = match acgdev_core::probe::build_client() {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!("HTTP 客户端初始化失败: {e}");
                        std::process::exit(1);
                    }
                };

                let mgr = Manager::new(
                    ManagerConfig {
                        max_concurrent: concurrent.max(1),
                        default_dir: dir.clone(),
                    },
                    client,
                )
                .await;

                let mut hint = String::from("RPC 已关闭");
                if let Some(bind) = rpc_bind {
                    let shutdown = CancellationToken::new();
                    let dispatcher = Arc::new(Dispatcher {
                        manager: mgr.clone(),
                        secret: secret.clone(),
                        shutdown: shutdown.clone(),
                    });
                    let cfg = RpcConfig {
                        bind,
                        secret: secret.clone(),
                        ..Default::default()
                    };
                    let events = mgr.events();
                    let s2 = shutdown.clone();
                    tokio::spawn(async move {
                        if let Err(e) = serve(dispatcher, events, cfg, s2).await {
                            tracing::error!("RPC 服务退出: {e}");
                        }
                    });
                    hint = format!(
                        "RPC {}{}",
                        bind,
                        if secret.is_some() { " · 已加密" } else { "" }
                    );
                }

                let _ = init_tx.send((mgr.clone(), hint));

                // 保持运行时存活
                tokio::signal::ctrl_c().await.ok();
                mgr.shutdown();
                tokio::time::sleep(Duration::from_millis(300)).await;
            });
        })
        .expect("无法创建引擎线程");

    let (manager, rpc_hint) = init_rx.recv().expect("引擎初始化失败");

    if args.headless {
        tracing::info!("headless 模式：{rpc_hint}，Ctrl-C 退出");
        loop {
            std::thread::park();
        }
    }

    Application::new().run(move |cx: &mut App| {
        // 字体必须先注册，否则首帧会用系统字体排版、之后不会自动重排。
        ui::fonts::install(cx);

        let bounds = Bounds::centered(None, size(px(1180.0), px(760.0)), cx);
        let mgr = manager.clone();
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some("AcgDev — HTTP Downloader".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            move |_, cx| {
                let mgr = mgr.clone();
                let hint = rpc_hint.clone();
                cx.new(move |cx| ui::AcgDevApp::new(mgr, hint, cx))
            },
        )
        .expect("无法创建窗口");
        cx.activate(true);
    });
}
