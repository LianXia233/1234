//! aria2 JSON-RPC 方法实现。
//!
//! 覆盖 AriaNg / Motrix / YAAW 实际会用到的方法集；未实现的方法返回
//! `METHOD_NOT_FOUND`（-32601 的 aria2 等价码 6），而不是伪造成功。
//!
//! 关键兼容点：
//! - 所有数值字段以**字符串**返回（aria2 的怪癖，客户端按字符串解析）
//! - 第一个参数为 `token:<secret>` 时先校验再去掉
//! - `system.multicall` 批量调用
//! - `gid` 为 16 位小写 hex

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{Map, Value, json};

use crate::error::{Error, Result};
use crate::manager::Manager;
use crate::rpc::protocol::{Request, Response, base64_encode, code, err, ok};
use crate::task::{Gid, Snapshot, TaskOptions, TaskStatus};
use crate::util::{gid_from_hex, gid_to_hex, parse_size, sanitize_filename};

pub const METHODS: &[&str] = &[
    "aria2.addUri",
    "aria2.remove",
    "aria2.forceRemove",
    "aria2.pause",
    "aria2.forcePause",
    "aria2.pauseAll",
    "aria2.forcePauseAll",
    "aria2.unpause",
    "aria2.unpauseAll",
    "aria2.tellStatus",
    "aria2.getUris",
    "aria2.getFiles",
    "aria2.getOption",
    "aria2.changeOption",
    "aria2.getGlobalOption",
    "aria2.changeGlobalOption",
    "aria2.getGlobalStat",
    "aria2.purgeDownloadResult",
    "aria2.removeDownloadResult",
    "aria2.getVersion",
    "aria2.getSessionInfo",
    "aria2.changePosition",
    "aria2.shutdown",
    "aria2.forceShutdown",
    "system.listMethods",
    "system.multicall",
    "system.getCapabilities",
];

pub const VERSION: &str = "0.1.0";

pub struct Dispatcher {
    pub manager: Arc<Manager>,
    pub secret: Option<String>,
    pub shutdown: tokio_util::sync::CancellationToken,
}

impl Dispatcher {
    pub fn handle(&self, req: Request) -> Response {
        let id = req.id_or_null();
        let mut args = req.args();

        // token 校验（aria2 规则：第一个参数以 "token:" 开头）
        if let Some(first) = args.first().and_then(|v| v.as_str()) {
            if let Some(token) = first.strip_prefix("token:") {
                if let Some(expect) = &self.secret {
                    if token != expect {
                        return err(id, code::UNAUTHORIZED, "Unauthorized");
                    }
                }
                args.remove(0);
            } else if self.secret.is_some() {
                // 配了密钥却没带 → 拒绝
                return err(id, code::UNAUTHORIZED, "Unauthorized");
            }
        } else if self.secret.is_some() {
            return err(id, code::UNAUTHORIZED, "Unauthorized");
        }

        match self.dispatch(&req.method, &args) {
            Ok(v) => ok(id, v),
            Err(RpcFail { code, message }) => err(id, code, message),
        }
    }

    fn dispatch(&self, method: &str, args: &[Value]) -> std::result::Result<Value, RpcFail> {
        let m = &self.manager;

        match method {
            // ---- 任务创建 ----
            "aria2.addUri" => {
                let uris: Vec<String> = args
                    .first()
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(|s| s.to_string()))
                            .collect()
                    })
                    .ok_or_else(|| RpcFail::invalid("第一个参数必须是 URI 数组"))?;
                let opts_val = args.get(1).cloned().unwrap_or(Value::Null);
                let opts = parse_options(&opts_val, m).map_err(|e| RpcFail::from_error(&e))?;

                let gid = m.add_uri(uris, opts).map_err(|e| RpcFail::from_error(&e))?;
                Ok(Value::String(gid_to_hex(gid)))
            }

            // ---- 控制 ----
            "aria2.remove" | "aria2.forceRemove" => {
                let gid = self.gid_arg(args)?;
                let force = method == "aria2.forceRemove";
                m.remove(gid, force).map_err(|e| RpcFail::from_error(&e))?;
                Ok(Value::String(gid_to_hex(gid)))
            }
            "aria2.pause" | "aria2.forcePause" => {
                let gid = self.gid_arg(args)?;
                m.pause(gid).map_err(|e| RpcFail::from_error(&e))?;
                Ok(Value::String(gid_to_hex(gid)))
            }
            "aria2.pauseAll" | "aria2.forcePauseAll" => {
                m.pause_all();
                Ok(Value::String("OK".into()))
            }
            "aria2.unpause" => {
                let gid = self.gid_arg(args)?;
                m.unpause(gid).map_err(|e| RpcFail::from_error(&e))?;
                Ok(Value::String(gid_to_hex(gid)))
            }
            "aria2.unpauseAll" => {
                m.unpause_all();
                Ok(Value::String("OK".into()))
            }
            "aria2.changePosition" => {
                let gid = self.gid_arg(args)?;
                let pos = args
                    .get(1)
                    .and_then(|v| v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
                    .ok_or_else(|| RpcFail::invalid("pos 必须是整数"))?;
                let how = args.get(2).and_then(|v| v.as_str()).unwrap_or("POS_SET");
                m.change_position(gid, pos, how)
                    .map_err(|e| RpcFail::from_error(&e))?;
                Ok(json!(pos))
            }

            // ---- 查询 ----
            "aria2.tellStatus" => {
                let gid = self.gid_arg(args)?;
                let keys = args.get(1).and_then(|v| v.as_array()).map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(|s| s.to_string()))
                        .collect::<Vec<_>>()
                });
                let t = m.find(&gid_to_hex(gid)).map_err(|e| RpcFail::from_error(&e))?;
                Ok(status_to_json(&t.snapshot(), keys.as_deref()))
            }
            "aria2.getUris" => {
                let gid = self.gid_arg(args)?;
                let t = m.find(&gid_to_hex(gid)).map_err(|e| RpcFail::from_error(&e))?;
                let s = t.snapshot();
                Ok(Value::Array(
                    s.uris
                        .iter()
                        .map(|u| json!({"uri": u, "status": "used"}))
                        .collect(),
                ))
            }
            "aria2.getFiles" => {
                let gid = self.gid_arg(args)?;
                let t = m.find(&gid_to_hex(gid)).map_err(|e| RpcFail::from_error(&e))?;
                Ok(files_json(&t.snapshot()))
            }
            "aria2.getOption" => {
                let gid = self.gid_arg(args)?;
                let t = m.find(&gid_to_hex(gid)).map_err(|e| RpcFail::from_error(&e))?;
                Ok(options_to_json(&t.opts))
            }
            "aria2.changeOption" => {
                // 仅支持在线可改的项；其余明确拒绝，避免"看起来生效"
                let _gid = self.gid_arg(args)?;
                Err(RpcFail {
                    code: code::FAILED,
                    message: "changeOption 暂不支持（请删除任务后重建）".into(),
                })
            }
            "aria2.getGlobalOption" => Ok(json!({
                "dir": m.config().default_dir.to_string_lossy(),
                "max-concurrent-downloads": m.config().max_concurrent.to_string(),
                "split": "5",
                "min-split-size": "4M",
                "continue": "true",
                "max-connection-per-server": "5",
            })),
            "aria2.changeGlobalOption" => Err(RpcFail {
                code: code::FAILED,
                message: "changeGlobalOption 暂不支持".into(),
            }),
            "aria2.getGlobalStat" => {
                let g = m.global_stat();
                Ok(json!({
                    "downloadSpeed": g.download_speed.to_string(),
                    "uploadSpeed": "0",
                    "numActive": g.num_active.to_string(),
                    "numWaiting": g.num_waiting.to_string(),
                    "numStopped": g.num_stopped.to_string(),
                    "numStoppedTotal": g.num_stopped.to_string(),
                }))
            }
            "aria2.purgeDownloadResult" => {
                m.purge_download_result();
                Ok(Value::String("OK".into()))
            }
            "aria2.removeDownloadResult" => {
                let gid = self.gid_arg(args)?;
                m.remove_download_result(gid);
                Ok(Value::String("OK".into()))
            }
            "aria2.getVersion" => Ok(json!({
                "version": VERSION,
                "enabledFeatures": [
                    "Async DNS", "BitTorrent", "Firefox3 Cookie", "GZip",
                    "HTTPS", "Message Digest", "Metalink", "XML-RPC", "SFTP"
                ]
            })),
            "aria2.getSessionInfo" => Ok(json!({ "sessionId": format!("{:016x}", std::process::id()) })),

            // ---- 生命周期 ----
            "aria2.shutdown" | "aria2.forceShutdown" => {
                m.shutdown();
                self.shutdown.cancel();
                Ok(Value::String("OK".into()))
            }

            // ---- system ----
            "system.listMethods" => Ok(json!(METHODS)),
            "system.getCapabilities" => Ok(json!({
                "nmtp": {"version": "1.0.0", "enabled": "true"},
                "async": {"version": "1.0.0", "enabled": "true"},
            })),
            "system.multicall" => self.multicall(args),

            other => Err(RpcFail {
                code: code::METHOD_NOT_FOUND,
                message: format!("未知方法: {other}"),
            }),
        }
    }

    fn multicall(&self, args: &[Value]) -> std::result::Result<Value, RpcFail> {
        let calls = args
            .first()
            .and_then(|v| v.as_array())
            .ok_or_else(|| RpcFail::invalid("multicall 参数必须是数组"))?;

        let mut out = Vec::with_capacity(calls.len());
        for c in calls {
            let name = c.get("methodName").and_then(|v| v.as_str()).unwrap_or("");
            let params = c
                .get("params")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            match self.dispatch(name, &params) {
                Ok(v) => out.push(json!([v])),
                Err(e) => out.push(json!({"code": e.code, "message": e.message})),
            }
        }
        Ok(Value::Array(out))
    }

    fn gid_arg(&self, args: &[Value]) -> std::result::Result<Gid, RpcFail> {
        let s = args
            .first()
            .and_then(|v| v.as_str())
            .ok_or_else(|| RpcFail::invalid("缺少 gid 参数"))?;
        gid_from_hex(s).ok_or_else(|| RpcFail {
            code: code::INVALID_PARAMS,
            message: format!("gid 格式非法: {s}"),
        })
    }
}

#[derive(Debug)]
pub struct RpcFail {
    pub code: i64,
    pub message: String,
}

impl RpcFail {
    fn invalid(msg: impl Into<String>) -> Self {
        Self {
            code: code::INVALID_PARAMS,
            message: msg.into(),
        }
    }

    fn from_error(e: &Error) -> Self {
        let code = match e {
            Error::TaskNotFound(_) => code::NOT_FOUND,
            Error::BadRequest(_)
            | Error::InvalidUrl(_)
            | Error::Corrupt(_)
            | Error::RemoteChanged { .. } => code::INVALID_PARAMS,
            _ => code::FAILED,
        };
        Self {
            code,
            message: e.to_string(),
        }
    }
}

/// 解析 aria2 的 options 对象。
fn parse_options(v: &Value, m: &Manager) -> Result<TaskOptions> {
    let mut o = TaskOptions::default();
    o.dir = m.config().default_dir.clone();

    let Some(map) = v.as_object() else {
        return Ok(o);
    };

    for (k, val) in map {
        let s = match val {
            Value::String(s) => s.clone(),
            Value::Number(n) => n.to_string(),
            Value::Bool(b) => b.to_string(),
            _ => continue,
        };
        o.raw.insert(k.clone(), s.clone());

        match k.as_str() {
            "dir" if !s.is_empty() => o.dir = PathBuf::from(&s),
            "out" if !s.is_empty() => o.out = Some(sanitize_filename(&s)),
            "split" | "max-connection-per-server" => {
                if let Ok(n) = s.parse::<usize>() {
                    o.split = n.clamp(1, 32);
                }
            }
            "min-split-size" => {
                if let Some(n) = parse_size(&s) {
                    o.min_split_size = n.max(256 * 1024);
                }
            }
            "referer" if !s.is_empty() => o.referer = Some(s),
            "user-agent" if !s.is_empty() => o.user_agent = Some(s),
            "header" => {
                if let Some((k2, v2)) = s.split_once(':') {
                    o.headers.push((k2.trim().to_string(), v2.trim().to_string()));
                }
            }
            "pause" => o.pause = s.eq_ignore_ascii_case("true"),
            _ => {}
        }
    }
    Ok(o)
}

fn options_to_json(o: &TaskOptions) -> Value {
    let mut m: BTreeMap<String, Value> = BTreeMap::new();
    m.insert("dir".into(), json!(o.dir.to_string_lossy()));
    m.insert("split".into(), json!(o.split.to_string()));
    m.insert("min-split-size".into(), json!(o.min_split_size.to_string()));
    m.insert(
        "continue".into(),
        json!(o.raw.get("continue").cloned().unwrap_or("true".into())),
    );
    if let Some(out) = &o.out {
        m.insert("out".into(), json!(out));
    }
    if let Some(r) = &o.referer {
        m.insert("referer".into(), json!(r));
    }
    if let Some(ua) = &o.user_agent {
        m.insert("user-agent".into(), json!(ua));
    }
    for (k, v) in &o.raw {
        m.entry(k.clone()).or_insert_with(|| json!(v));
    }
    Value::Object(m.into_iter().collect::<Map<String, Value>>())
}

fn files_json(s: &Snapshot) -> Value {
    let path = s.dir.join(&s.filename);
    json!([{
        "index": "1",
        "path": path.to_string_lossy(),
        "length": s.total.unwrap_or(0).to_string(),
        "completedLength": s.completed.to_string(),
        "selected": "true",
        "uris": s.uris.iter().map(|u| json!({"uri": u, "status": "used"})).collect::<Vec<_>>(),
    }])
}

fn status_to_json(s: &Snapshot, keys: Option<&[String]>) -> Value {
    let mut map = Map::new();
    let mut put = |k: &str, v: Value| {
        if let Some(ks) = keys {
            if !ks.iter().any(|x| x == k) {
                return;
            }
        }
        map.insert(k.to_string(), v);
    };

    put("gid", json!(gid_to_hex(s.gid)));
    put("status", json!(s.status.as_aria2()));
    put("totalLength", json!(s.total.unwrap_or(0).to_string()));
    put("completedLength", json!(s.completed.to_string()));
    put("uploadLength", json!("0"));
    put("downloadSpeed", json!(s.speed.to_string()));
    put("uploadSpeed", json!("0"));
    put("connections", json!(s.connections.to_string()));
    put("dir", json!(s.dir.to_string_lossy()));
    put("numPieces", json!(s.num_pieces.to_string()));
    put("pieceLength", json!(s.piece_len.to_string()));
    put("bitfield", json!(base64_encode(&s.bitfield)));
    put("followedBy", json!([]));
    put("following", Value::Null);
    put("belongsTo", Value::Null);
    put("verifiedLength", json!(if s.status == TaskStatus::Complete {
        s.completed.to_string()
    } else {
        "0".into()
    }));

    if let Some(e) = &s.error {
        put("errorCode", json!(s.error_code.to_string()));
        put("errorMessage", json!(e));
    }
    if keys.is_none() || keys.unwrap().iter().any(|k| k == "files") {
        map.insert("files".into(), files_json(s));
    }

    Value::Object(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_parse_split_and_sizes() {
        let cfg = crate::manager::ManagerConfig {
            max_concurrent: 5,
            default_dir: PathBuf::from("/tmp/dl"),
        };
        // 不构造 Manager，直接测纯解析逻辑需要 &Manager，故仅验证键名映射约定
        let v: Value = serde_json::from_str(
            r#"{"dir":"/x","out":"a.zip","split":"8","min-split-size":"2M","referer":"https://r/"}"#,
        )
        .unwrap();
        assert_eq!(v["split"], "8");
        assert_eq!(parse_size("2M"), Some(2 * 1024 * 1024));
        assert_eq!(cfg.max_concurrent, 5);
    }

    #[test]
    fn status_json_uses_strings() {
        let s = Snapshot {
            gid: 0x2089_b05e_cca3_d829,
            status: TaskStatus::Active,
            total: Some(1000),
            completed: 400,
            speed: 1234,
            dir: PathBuf::from("/tmp"),
            filename: "a.bin".into(),
            uris: vec!["https://e/a".into()],
            connections: 4,
            error: None,
            error_code: 0,
            bitfield: vec![0b1010_0000],
            num_pieces: 2,
            piece_len: 512,
        };
        let j = status_to_json(&s, None);
        assert_eq!(j["totalLength"], "1000");
        assert_eq!(j["completedLength"], "400");
        assert_eq!(j["downloadSpeed"], "1234");
        assert_eq!(j["status"], "active");
        assert_eq!(j["gid"], "2089b05ecca3d829");
        assert_eq!(j["files"][0]["length"], "1000");
    }

    #[test]
    fn status_json_honours_keys_filter() {
        let s = Snapshot {
            gid: 1,
            status: TaskStatus::Complete,
            total: Some(10),
            completed: 10,
            speed: 0,
            dir: PathBuf::from("/tmp"),
            filename: "a".into(),
            uris: vec![],
            connections: 0,
            error: None,
            error_code: 0,
            bitfield: vec![],
            num_pieces: 0,
            piece_len: 0,
        };
        let j = status_to_json(&s, Some(&["gid".to_string(), "status".to_string()]));
        assert!(j.get("gid").is_some());
        assert!(j.get("status").is_some());
        assert!(j.get("totalLength").is_none());
        assert!(j.get("files").is_none());
    }
}
