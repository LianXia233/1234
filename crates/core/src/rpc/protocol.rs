//! JSON-RPC 2.0 协议类型（aria2 风格：数值一律以字符串返回）。

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// aria2 使用的错误码。
pub mod code {
    pub const UNKNOWN: i64 = 1;
    pub const TIMEOUT: i64 = 2;
    pub const NOT_FOUND: i64 = 3;
    pub const TOO_MANY: i64 = 4;
    pub const FAILED: i64 = 5;
    pub const METHOD_NOT_FOUND: i64 = 6;
    pub const INVALID_PARAMS: i64 = 7;
    pub const UNAUTHORIZED: i64 = 8;
}

#[derive(Debug, Deserialize)]
pub struct Request {
    #[serde(default)]
    pub jsonrpc: Option<String>,
    #[serde(default)]
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Option<Value>,
}

impl Request {
    /// 取出参数数组（aria2 只使用数组形式）。缺省视为空数组。
    pub fn args(&self) -> Vec<Value> {
        match &self.params {
            Some(Value::Array(a)) => a.clone(),
            Some(Value::Object(o)) => vec![Value::Object(o.clone())],
            _ => Vec::new(),
        }
    }

    pub fn id_or_null(&self) -> Value {
        self.id.clone().unwrap_or(Value::Null)
    }
}

#[derive(Debug, Serialize)]
pub struct Response {
    pub jsonrpc: &'static str,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

#[derive(Debug, Serialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

pub fn ok(id: Value, result: Value) -> Response {
    Response {
        jsonrpc: "2.0",
        id,
        result: Some(result),
        error: None,
    }
}

pub fn err(id: Value, code: i64, message: impl Into<String>) -> Response {
    Response {
        jsonrpc: "2.0",
        id,
        result: None,
        error: Some(RpcError {
            code,
            message: message.into(),
        }),
    }
}

/// aria2 的 WebSocket 通知帧。
pub fn notification(method: &str, gid: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": [{ "gid": gid }]
    })
}

/// 极简 Base64（只用于 aria2 的 `bitfield` 字段，标准字母表 + padding）。
pub fn base64_encode(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            out.push(T[(n >> 6) as usize & 63] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(T[n as usize & 63] as char);
        } else {
            out.push('=');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_reference() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64_encode(&[0b1010_0000u8]), "oA==");
    }

    #[test]
    fn request_args_handles_array_and_missing() {
        let r: Request = serde_json::from_str(r#"{"method":"m"}"#).unwrap();
        assert!(r.args().is_empty());

        let r: Request =
            serde_json::from_str(r#"{"jsonrpc":"2.0","id":"1","method":"m","params":[1,2]}"#)
                .unwrap();
        assert_eq!(r.args().len(), 2);
    }

    #[test]
    fn notification_shape() {
        let v = notification("aria2.onDownloadStart", "2089b05ecca3d829");
        assert_eq!(v["method"], "aria2.onDownloadStart");
        assert_eq!(v["params"][0]["gid"], "2089b05ecca3d829");
    }
}
