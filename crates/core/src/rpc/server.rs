//! aria2 兼容的 HTTP/WebSocket 服务端。
//!
//! 端点与 aria2 保持一致：
//! - `POST /jsonrpc`  —— 标准 JSON-RPC
//! - `GET  /jsonrpc?method=..&params=..&id=..` —— aria2 支持的 query 形式
//! - `GET  /jsonrpc` + `Upgrade: websocket` —— 事件推送（AriaNg 实时刷新依赖它）
//!
//! 默认端口 6800，与 aria2 相同，因此现有客户端改个地址即可接管。

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::Value;
use tokio::sync::broadcast::error::RecvError;
use tokio_util::sync::CancellationToken;
use tower_http::cors::{Any, CorsLayer};

use crate::error::Result;
use crate::rpc::aria2::Dispatcher;
use crate::rpc::protocol::{Request, Response, err, notification, code};
use crate::task::Event;

#[derive(Debug, Clone)]
pub struct RpcConfig {
    pub bind: SocketAddr,
    pub secret: Option<String>,
    pub enable_websocket: bool,
    pub allow_cors: bool,
}

impl Default for RpcConfig {
    fn default() -> Self {
        Self {
            bind: SocketAddr::from(([0, 0, 0, 0], 6800)),
            secret: None,
            enable_websocket: true,
            allow_cors: true,
        }
    }
}

#[derive(Clone)]
struct AppState {
    dispatcher: Arc<Dispatcher>,
    events: tokio::sync::broadcast::Sender<Event>,
    enable_ws: bool,
}

/// 启动 RPC 服务（阻塞直到 `shutdown` 触发）。
pub async fn serve(
    dispatcher: Arc<Dispatcher>,
    events: tokio::sync::broadcast::Sender<Event>,
    cfg: RpcConfig,
    shutdown: CancellationToken,
) -> Result<()> {
    let state = AppState {
        dispatcher,
        events,
        enable_ws: cfg.enable_websocket,
    };

    let mut router = Router::new()
        .route("/jsonrpc", post(handle_post).get(handle_get))
        .route("/rpc", post(handle_post))
        .with_state(state);

    if cfg.allow_cors {
        router = router.layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods(Any)
                .allow_headers(Any),
        );
    }

    let listener = tokio::net::TcpListener::bind(cfg.bind)
        .await
        .map_err(crate::error::Error::Io)?;
    tracing::info!("aria2 兼容 RPC 已监听 http://{}", cfg.bind);

    axum::serve(listener, router)
        .with_graceful_shutdown(async move { shutdown.cancelled().await })
        .await
        .map_err(crate::error::Error::Io)
}

async fn handle_post(State(st): State<AppState>, body: String) -> impl IntoResponse {
    match serde_json::from_str::<Request>(&body) {
        Ok(req) => {
            let resp = st.dispatcher.handle(req);
            (StatusCode::OK, Json(resp))
        }
        Err(e) => {
            let r = err(Value::Null, code::INVALID_PARAMS, format!("JSON 解析失败: {e}"));
            (StatusCode::OK, Json(r))
        }
    }
}

async fn handle_get(
    State(st): State<AppState>,
    ws: std::result::Result<WebSocketUpgrade, axum::extract::ws::rejection::WebSocketUpgradeRejection>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> axum::response::Response {
    if let Ok(ws) = ws {
        if st.enable_ws {
            return ws.on_upgrade(move |socket| ws_loop(socket, st));
        }
        return (StatusCode::NOT_IMPLEMENTED, "WebSocket 已禁用").into_response();
    }

    let Some(method) = q.get("method").cloned() else {
        return (
            StatusCode::BAD_REQUEST,
            Json(err(Value::Null, code::INVALID_PARAMS, "缺少 method 参数")),
        )
            .into_response();
    };

    let params: Option<Value> = q
        .get("params")
        .and_then(|s| serde_json::from_str::<Value>(s).ok());
    let id: Value = q
        .get("id")
        .map(|s| serde_json::from_str::<Value>(s).unwrap_or_else(|_| Value::String(s.clone())))
        .unwrap_or(Value::Null);

    let req = Request {
        jsonrpc: Some("2.0".into()),
        id: Some(id),
        method,
        params,
    };
    Json(st.dispatcher.handle(req)).into_response()
}

async fn ws_loop(mut socket: WebSocket, st: AppState) {
    let mut rx = st.events.subscribe();

    loop {
        tokio::select! {
            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(Message::Text(t))) => {
                        let resp = match serde_json::from_str::<Request>(&t) {
                            Ok(req) => st.dispatcher.handle(req),
                            Err(e) => err(Value::Null, code::INVALID_PARAMS, format!("JSON 解析失败: {e}")),
                        };
                        if let Ok(s) = serde_json::to_string(&resp) {
                            if socket.send(Message::Text(s.into())).await.is_err() {
                                return;
                            }
                        }
                    }
                    Some(Ok(Message::Ping(p))) => {
                        if socket.send(Message::Pong(p)).await.is_err() {
                            return;
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => return,
                    Some(Err(_)) => return,
                    _ => {}
                }
            }
            ev = rx.recv() => {
                let frame: Option<Response> = match ev {
                    Ok(e) => {
                        let (method, gid) = match &e {
                            Event::Start { gid } => ("aria2.onDownloadStart", gid),
                            Event::Pause { gid } => ("aria2.onDownloadPause", gid),
                            Event::Stop { gid } => ("aria2.onDownloadStop", gid),
                            Event::Complete { gid } => ("aria2.onDownloadComplete", gid),
                            Event::Error { gid } => ("aria2.onDownloadError", gid),
                        };
                        let body = notification(method, gid);
                        let s = serde_json::to_string(&body).unwrap_or_default();
                        if socket.send(Message::Text(s.into())).await.is_err() {
                            return;
                        }
                        None
                    }
                    Err(RecvError::Lagged(n)) => {
                        tracing::warn!("WS 客户端处理滞后，丢弃 {n} 条通知");
                        None
                    }
                    Err(RecvError::Closed) => return,
                };
                if frame.is_some() {
                    return;
                }
            }
        }
    }
}
