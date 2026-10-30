//! 端到端测试：真实 HTTP 服务 + aria2 RPC 全链路。
//!
//! 覆盖四个最容易出错的场景：
//! 1. 多连接 Range 下载后文件逐字节一致
//! 2. 服务器不支持 Range 时单流仍能完成
//! 3. ETag 变化时拒绝续传（F-03）
//! 4. RPC token 鉴权

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use acgdev_core::manager::{Manager, ManagerConfig};
use acgdev_core::rpc::Dispatcher;
use acgdev_core::rpc::protocol::Request;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::Response;
use axum::routing::get;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

/// 2 MiB 的可复现数据。
fn make_data(n: usize) -> Vec<u8> {
    (0..n).map(|i| ((i.wrapping_mul(31).wrapping_add(7)) & 0xff) as u8).collect()
}

struct ServerState {
    data: Vec<u8>,
    etag: String,
    support_range: bool,
    hits: AtomicUsize,
    fail_after: Option<usize>,
}

async fn file_handler(State(st): State<Arc<ServerState>>, headers: HeaderMap) -> Response {
    let n = st.hits.fetch_add(1, Ordering::SeqCst);
    if let Some(f) = st.fail_after {
        if n >= f {
            return Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::empty())
                .unwrap();
        }
    }

    let total = st.data.len();
    let range_hdr = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let etag = st.etag.clone();
    let builder = |code: StatusCode| {
        let mut b = Response::builder()
            .status(code)
            .header(header::ETAG, &etag)
            .header(header::ACCEPT_RANGES, if st.support_range { "bytes" } else { "none" });
        if !st.support_range {
            b = b.header(header::CONTENT_LENGTH, total.to_string());
        }
        b
    };

    if st.support_range {
        if let Some(r) = range_hdr {
            // bytes=a-b
            let spec = r.trim_start_matches("bytes=");
            let (a, b) = spec.split_once('-').unwrap_or(("0", ""));
            let start: usize = a.parse().unwrap_or(0);
            let end: usize = if b.is_empty() {
                total - 1
            } else {
                b.parse::<usize>().unwrap_or(total - 1).min(total - 1)
            };
            if start >= total {
                return Response::builder()
                    .status(StatusCode::RANGE_NOT_SATISFIABLE)
                    .header(header::CONTENT_RANGE, format!("bytes */{total}"))
                    .body(Body::empty())
                    .unwrap();
            }
            let chunk = st.data[start..=end].to_vec();
            return builder(StatusCode::PARTIAL_CONTENT)
                .header(
                    header::CONTENT_RANGE,
                    format!("bytes {start}-{end}/{total}"),
                )
                .header(header::CONTENT_LENGTH, chunk.len().to_string())
                .body(Body::from(chunk))
                .unwrap();
        }
    }

    builder(StatusCode::OK)
        .body(Body::from(st.data.clone()))
        .unwrap()
}

async fn start_server(st: Arc<ServerState>) -> SocketAddr {
    let app = axum::Router::new()
        .route("/file", get(file_handler))
        .with_state(st);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

async fn new_manager(dir: &Path) -> Arc<Manager> {
    let client = acgdev_core::probe::build_client().unwrap();
    Manager::new(
        ManagerConfig {
            max_concurrent: 4,
            default_dir: dir.to_path_buf(),
        },
        client,
    )
    .await
}

fn req(method: &str, params: Value) -> Request {
    serde_json::from_value(json!({
        "jsonrpc": "2.0", "id": "1", "method": method, "params": params
    }))
    .unwrap()
}

fn call(d: &Dispatcher, method: &str, params: Value) -> Value {
    let resp = d.handle(req(method, params));
    if let Some(e) = resp.error {
        return json!({"__error": e.code, "message": e.message});
    }
    resp.result.unwrap_or(Value::Null)
}

async fn wait_done(d: &Dispatcher, gid: &str, timeout: Duration) -> Value {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let st = call(d, "aria2.tellStatus", json!([gid]));
        let status = st["status"].as_str().unwrap_or("").to_string();
        if status == "complete" || status == "error" || status == "removed" {
            return st;
        }
        if tokio::time::Instant::now() > deadline {
            panic!("超时未完成，最后状态: {st}");
        }
        tokio::time::sleep(Duration::from_millis(80)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn range_download_is_byte_exact() {
    let dir = tempfile::tempdir().unwrap();
    let data = make_data(2 * 1024 * 1024);
    let st = Arc::new(ServerState {
        data: data.clone(),
        etag: "\"v1\"".into(),
        support_range: true,
        hits: AtomicUsize::new(0),
        fail_after: None,
    });
    let addr = start_server(st.clone()).await;

    let mgr = new_manager(dir.path()).await;
    let d = Dispatcher {
        manager: mgr.clone(),
        secret: None,
        shutdown: CancellationToken::new(),
    };

    let gid = call(
        &d,
        "aria2.addUri",
        json!([
            [format!("http://{addr}/file")],
            {"dir": dir.path().to_string_lossy(), "out": "a.bin", "split": "4"}
        ]),
    );
    let gid = gid.as_str().expect("addUri 应返回 gid").to_string();

    let final_state = wait_done(&d, &gid, Duration::from_secs(30)).await;
    assert_eq!(final_state["status"], "complete", "{final_state}");
    assert_eq!(final_state["totalLength"], data.len().to_string());

    let out = std::fs::read(dir.path().join("a.bin")).unwrap();
    assert_eq!(out.len(), data.len(), "长度不符");
    assert!(out == data, "内容不一致（前 64 字节: {:?}）", &out[..64.min(out.len())]);
    assert!(
        st.hits.load(Ordering::SeqCst) >= 2,
        "应当发生多次 Range 请求，实际 {}",
        st.hits.load(Ordering::SeqCst)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_range_support_falls_back_to_single_stream() {
    let dir = tempfile::tempdir().unwrap();
    let data = make_data(300 * 1024);
    let st = Arc::new(ServerState {
        data: data.clone(),
        etag: "\"v1\"".into(),
        support_range: false,
        hits: AtomicUsize::new(0),
        fail_after: None,
    });
    let addr = start_server(st).await;

    let mgr = new_manager(dir.path()).await;
    let d = Dispatcher {
        manager: mgr.clone(),
        secret: None,
        shutdown: CancellationToken::new(),
    };

    let gid = call(
        &d,
        "aria2.addUri",
        json!([
            [format!("http://{addr}/file")],
            {"dir": dir.path().to_string_lossy(), "out": "b.bin", "split": "8"}
        ]),
    )
    .as_str()
    .unwrap()
    .to_string();

    let stt = wait_done(&d, &gid, Duration::from_secs(30)).await;
    assert_eq!(stt["status"], "complete", "{stt}");

    let out = std::fs::read(dir.path().join("b.bin")).unwrap();
    assert!(out == data, "单流下载内容不一致");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn etag_change_rejects_resume() {
    let dir = tempfile::tempdir().unwrap();
    let data = make_data(64 * 1024);
    let st = Arc::new(ServerState {
        data: data.clone(),
        etag: "\"v2\"".into(), // 远端已变
        support_range: true,
        hits: AtomicUsize::new(0),
        fail_after: None,
    });
    let addr = start_server(st).await;

    // 手工构造一份"旧版本"续传元数据：ETag 为 v1，声明前 32KiB 已完成
    let target = dir.path().join("c.bin");
    let part = dir.path().join("c.bin.acgdev.part");
    let meta = dir.path().join("c.bin.acgdev.json");
    std::fs::write(&part, &data[..32 * 1024]).unwrap();
    std::fs::write(
        &meta,
        json!({
            "version": 1,
            "url": format!("http://{addr}/file"),
            "total": data.len() as u64,
            "etag": "\"v1\"",
            "last_modified": null,
            "done": [[0, 32768]],
            "saved_at": 0u64
        })
        .to_string(),
    )
    .unwrap();

    let mgr = new_manager(dir.path()).await;
    let d = Dispatcher {
        manager: mgr.clone(),
        secret: None,
        shutdown: CancellationToken::new(),
    };

    let gid = call(
        &d,
        "aria2.addUri",
        json!([[format!("http://{addr}/file")], {"dir": dir.path().to_string_lossy(), "out": "c.bin"}]),
    )
    .as_str()
    .unwrap()
    .to_string();

    let stt = wait_done(&d, &gid, Duration::from_secs(20)).await;
    // 关键：必须报错，而不是"接着写出一份坏文件"
    assert_eq!(stt["status"], "error", "应拒绝对已变更的远端续传: {stt}");
    let msg = stt["errorMessage"].as_str().unwrap_or("");
    assert!(msg.contains("ETag") || msg.contains("变更"), "错误信息应指明原因，实际: {msg}");
    assert!(!target.exists(), "不应产出最终文件");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rpc_secret_is_enforced() {
    let dir = tempfile::tempdir().unwrap();
    let mgr = new_manager(dir.path()).await;
    let d = Dispatcher {
        manager: mgr,
        secret: Some("s3cret".into()),
        shutdown: CancellationToken::new(),
    };

    // 不带 token
    let bad = call(&d, "aria2.getVersion", json!([]));
    assert_eq!(bad["__error"], 8, "应返回鉴权失败: {bad}");

    // 错误 token
    let bad2 = call(&d, "aria2.getVersion", json!(["token:wrong"]));
    assert_eq!(bad2["__error"], 8);

    // 正确 token
    let good = call(&d, "aria2.getVersion", json!(["token:s3cret"]));
    assert!(good["version"].is_string(), "{good}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multicall_and_global_stat() {
    let dir = tempfile::tempdir().unwrap();
    let mgr = new_manager(dir.path()).await;
    let d = Dispatcher {
        manager: mgr,
        secret: None,
        shutdown: CancellationToken::new(),
    };

    let r = call(
        &d,
        "system.multicall",
        json!([[
            {"methodName": "aria2.getVersion", "params": []},
            {"methodName": "aria2.getGlobalStat", "params": []}
        ]]),
    );
    let arr = r.as_array().expect("multicall 返回数组");
    assert_eq!(arr.len(), 2);
    assert!(arr[0][0]["version"].is_string());
    assert!(arr[1][0]["numActive"].is_string());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_method_returns_6() {
    let dir = tempfile::tempdir().unwrap();
    let mgr = new_manager(dir.path()).await;
    let d = Dispatcher {
        manager: mgr,
        secret: None,
        shutdown: CancellationToken::new(),
    };
    let r = call(&d, "aria2.notARealMethod", json!([]));
    assert_eq!(r["__error"], 6);
}

/// 校验：中途失败时不应产出最终文件，也不应把进度算成完成（F-01 回归）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failure_does_not_produce_final_file() {
    let dir = tempfile::tempdir().unwrap();
    let data = make_data(1024 * 1024);
    let st = Arc::new(ServerState {
        data: data.clone(),
        etag: "\"v1\"".into(),
        support_range: true,
        hits: AtomicUsize::new(0),
        fail_after: Some(0), // 首个请求即 500
    });
    let addr = start_server(st).await;

    let mgr = new_manager(dir.path()).await;
    let d = Dispatcher {
        manager: mgr,
        secret: None,
        shutdown: CancellationToken::new(),
    };

    let gid = call(
        &d,
        "aria2.addUri",
        json!([[format!("http://{addr}/file")], {"dir": dir.path().to_string_lossy(), "out": "d.bin"}]),
    )
    .as_str()
    .unwrap()
    .to_string();

    let stt = wait_done(&d, &gid, Duration::from_secs(20)).await;
    assert_eq!(stt["status"], "error", "{stt}");
    assert!(!dir.path().join("d.bin").exists(), "失败不应产出最终文件");
}

/// 断点续传正向路径：先下一部分再中断，重启后应只补剩余部分。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resume_from_partial_completes_correctly() {
    let dir = tempfile::tempdir().unwrap();
    let data = make_data(1024 * 1024);
    let st = Arc::new(ServerState {
        data: data.clone(),
        etag: "\"v1\"".into(),
        support_range: true,
        hits: AtomicUsize::new(0),
        fail_after: None,
    });
    let addr = start_server(st).await;

    // 预置：前 512 KiB 已下载，元数据一致
    let part = dir.path().join("e.bin.acgdev.part");
    let meta = dir.path().join("e.bin.acgdev.json");
    std::fs::write(&part, &data[..512 * 1024]).unwrap();
    std::fs::write(
        &meta,
        json!({
            "version": 1,
            "url": format!("http://{addr}/file"),
            "total": data.len() as u64,
            "etag": "\"v1\"",
            "last_modified": null,
            "done": [[0, 524288]],
            "saved_at": 0u64
        })
        .to_string(),
    )
    .unwrap();

    let mgr = new_manager(dir.path()).await;
    let d = Dispatcher {
        manager: mgr,
        secret: None,
        shutdown: CancellationToken::new(),
    };

    let gid = call(
        &d,
        "aria2.addUri",
        json!([[format!("http://{addr}/file")], {"dir": dir.path().to_string_lossy(), "out": "e.bin", "split": "4"}]),
    )
    .as_str()
    .unwrap()
    .to_string();

    let stt = wait_done(&d, &gid, Duration::from_secs(30)).await;
    assert_eq!(stt["status"], "complete", "{stt}");

    let out = std::fs::read(dir.path().join("e.bin")).unwrap();
    assert!(out == data, "续传后内容不一致");
}
