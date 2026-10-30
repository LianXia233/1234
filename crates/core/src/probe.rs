//! 远端探测：总长度、Range 支持、ETag / Last-Modified、文件名。
//!
//! 策略：优先 HEAD（不消耗流量），HEAD 不可用时退回 `Range: bytes=0-0`。
//! 全程强制 `Accept-Encoding: identity` —— 下载器必须逐字节保真，
//! 一旦被压缩，`Content-Length` 与 Range 语义全部失真。

use reqwest::header::{
    ACCEPT_ENCODING, ACCEPT_RANGES, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_RANGE,
    CONTENT_TYPE, ETAG, LAST_MODIFIED, RANGE,
};
use reqwest::{Client, StatusCode};
use std::time::Duration;
use url::Url;

use crate::error::{Error, Result};
use crate::util::sanitize_filename;

pub const UA: &str = "AcgDev/0.1 (Rust+GPUI; aria2-compatible)";

#[derive(Debug, Clone, Default)]
pub struct Probe {
    pub total_len: Option<u64>,
    pub accept_ranges: bool,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub filename: Option<String>,
    pub content_type: Option<String>,
    /// 跟随重定向后的最终 URL（续传元数据应以它为准）。
    pub final_url: Option<Url>,
}

impl Probe {
    /// 用于续传一致性校验的指纹。
    pub fn fingerprint(&self) -> Fingerprint {
        Fingerprint {
            total: self.total_len,
            etag: self.etag.clone(),
            last_modified: self.last_modified.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Fingerprint {
    pub total: Option<u64>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

impl Fingerprint {
    /// F-03 修复：续传前必须校验远端未变化。
    ///
    /// 判定规则（从严）：
    /// - 长度已知且不一致 → 拒绝
    /// - ETag 双方都有且不一致 → 拒绝
    /// - 无 ETag 时退回 Last-Modified 比较
    pub fn ensure_compatible(&self, remote: &Fingerprint) -> Result<()> {
        if let (Some(a), Some(b)) = (self.total, remote.total) {
            if a != b {
                return Err(Error::RemoteChanged {
                    field: "Content-Length",
                    expected: a.to_string(),
                    actual: b.to_string(),
                });
            }
        }
        if let (Some(a), Some(b)) = (&self.etag, &remote.etag) {
            if a != b {
                return Err(Error::RemoteChanged {
                    field: "ETag",
                    expected: a.clone(),
                    actual: b.clone(),
                });
            }
            return Ok(());
        }
        if let (Some(a), Some(b)) = (&self.last_modified, &remote.last_modified) {
            if a != b {
                return Err(Error::RemoteChanged {
                    field: "Last-Modified",
                    expected: a.clone(),
                    actual: b.clone(),
                });
            }
        }
        Ok(())
    }
}

pub fn build_client() -> Result<Client> {
    Client::builder()
        .user_agent(UA)
        .connect_timeout(Duration::from_secs(15))
        // 整体请求不设超时：大文件需要长时间。改为读超时由流式循环控制。
        .pool_max_idle_per_host(32)
        .tcp_keepalive(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()
        .map_err(Error::Http)
}

/// 探测远端。
pub async fn probe(client: &Client, url: &Url, extra_headers: &[(String, String)]) -> Result<Probe> {
    // 1) HEAD
    let mut head_req = client
        .head(url.clone())
        .header(ACCEPT_ENCODING, "identity");
    for (k, v) in extra_headers {
        head_req = head_req.header(k, v);
    }

    let head = head_req.send().await;
    if let Ok(resp) = head {
        let status = resp.status();
        if status.is_success() {
            let mut p = parse_headers(&resp);
            p.final_url = Some(resp.url().clone());
            if p.total_len.is_some() || !p.accept_ranges {
                return Ok(with_fallback_name(p, url));
            }
        }
    }

    // 2) 退回 GET Range: bytes=0-0
    let mut get_req = client
        .get(url.clone())
        .header(ACCEPT_ENCODING, "identity")
        .header(RANGE, "bytes=0-0");
    for (k, v) in extra_headers {
        get_req = get_req.header(k, v);
    }

    let resp = get_req.send().await?;
    let status = resp.status();

    if status == StatusCode::RANGE_NOT_SATISFIABLE {
        // 0 字节文件
        return Ok(with_fallback_name(
            Probe {
                total_len: Some(0),
                accept_ranges: true,
                final_url: Some(resp.url().clone()),
                ..Default::default()
            },
            url,
        ));
    }
    if !status.is_success() {
        return Err(Error::Other(format!("探测失败：HTTP {}", status.as_u16())));
    }

    let mut p = parse_headers(&resp);
    p.final_url = Some(resp.url().clone());
    p.accept_ranges = status == StatusCode::PARTIAL_CONTENT;

    if p.accept_ranges {
        // 从 Content-Range 取总长：bytes 0-0/12345
        if let Some(cr) = resp.headers().get(CONTENT_RANGE) {
            p.total_len = parse_content_range_total(cr.to_str().unwrap_or(""));
        }
    } else {
        // 不支持 Range：Content-Length 即全长
        p.total_len = resp
            .headers()
            .get(CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse().ok());
    }
    Ok(with_fallback_name(p, url))
}

fn parse_headers(resp: &reqwest::Response) -> Probe {
    let h = resp.headers();
    let s = |name: reqwest::header::HeaderName| -> Option<String> {
        h.get(name)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };

    Probe {
        total_len: s(CONTENT_LENGTH).and_then(|v| v.parse().ok()),
        accept_ranges: s(ACCEPT_RANGES)
            .map(|v| v.to_ascii_lowercase().contains("bytes"))
            .unwrap_or(false),
        etag: s(ETAG),
        last_modified: s(LAST_MODIFIED),
        filename: s(CONTENT_DISPOSITION).and_then(|v| parse_content_disposition(&v)),
        content_type: s(CONTENT_TYPE),
        final_url: None,
    }
}

/// 解析 `bytes 0-0/12345` 中的总长；`*` 表示未知。
pub fn parse_content_range_total(v: &str) -> Option<u64> {
    let (_, total) = v.rsplit_once('/')?;
    let t = total.trim();
    if t == "*" {
        None
    } else {
        t.parse().ok()
    }
}

/// 从 URL 路径推导默认文件名。
///
/// aria2 的行为：未显式指定 `out` 时，取 URL 最后一个 path segment。
/// `path_segments()` 返回的仍是 **percent-encoded** 原样字节，必须先解码 ——
/// 否则中文/空格文件名会落成 `%E6%B5%8B%E8%AF%95%20...` 这种乱码文件名。
fn filename_from_url(url: &Url) -> Option<String> {
    // 以 '/' 结尾说明指向目录，最后一个 segment 只是目录名，不能当文件名
    if url.path().ends_with('/') {
        return None;
    }
    let seg = url
        .path_segments()?
        .filter(|s| !s.is_empty())
        .next_back()?;
    let decoded = percent_encoding::percent_decode_str(seg).decode_utf8_lossy();
    let trimmed = decoded.trim();
    // 空段与 "."/".." 无法作为文件名，交给上层用 "download" 兜底
    if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
        return None;
    }
    Some(sanitize_filename(trimmed))
}

/// 探测结果补齐文件名：Content-Disposition 优先，其次 URL 路径。
fn with_fallback_name(mut p: Probe, url: &Url) -> Probe {
    if p.filename.is_none() {
        let base = p.final_url.clone().unwrap_or_else(|| url.clone());
        p.filename = filename_from_url(&base);
    }
    p
}

/// 解析 `Content-Disposition`，优先 RFC 5987 的 `filename*`。
pub fn parse_content_disposition(v: &str) -> Option<String> {
    let mut plain: Option<String> = None;
    let mut extended: Option<String> = None;

    for part in v.split(';').skip(1) {
        let part = part.trim();
        let Some((key, val)) = part.split_once('=') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        let val = val.trim().trim_matches('"');

        match key.as_str() {
            "filename" => plain = Some(val.to_string()),
            "filename*" => {
                // UTF-8''name%20with%20space
                let raw = val.splitn(3, '\'').nth(2).unwrap_or(val);
                let decoded = percent_encoding::percent_decode_str(raw)
                    .decode_utf8_lossy()
                    .to_string();
                extended = Some(decoded);
            }
            _ => {}
        }
    }

    extended.or(plain).map(|n| sanitize_filename(&n))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_range_total() {
        assert_eq!(parse_content_range_total("bytes 0-0/12345"), Some(12345));
        assert_eq!(parse_content_range_total("bytes 0-0/*"), None);
        assert_eq!(parse_content_range_total("garbage"), None);
    }

    #[test]
    fn disposition_plain() {
        assert_eq!(
            parse_content_disposition("attachment; filename=\"a.zip\""),
            Some("a.zip".to_string())
        );
    }

    #[test]
    fn disposition_extended_preferred() {
        let v = "attachment; filename=\"fallback.zip\"; filename*=UTF-8''%E6%B5%8B%E8%AF%95.zip";
        assert_eq!(parse_content_disposition(v), Some("测试.zip".to_string()));
    }

    #[test]
    fn disposition_strips_path() {
        assert_eq!(
            parse_content_disposition("attachment; filename=\"../../evil.sh\""),
            Some("evil.sh".to_string())
        );
    }

    #[test]
    fn fingerprint_rejects_length_change() {
        let old = Fingerprint {
            total: Some(100),
            ..Default::default()
        };
        let new = Fingerprint {
            total: Some(200),
            ..Default::default()
        };
        assert!(old.ensure_compatible(&new).is_err());
    }

    #[test]
    fn fingerprint_rejects_etag_change() {
        let old = Fingerprint {
            total: Some(100),
            etag: Some("\"abc\"".into()),
            ..Default::default()
        };
        let new = Fingerprint {
            total: Some(100),
            etag: Some("\"def\"".into()),
            ..Default::default()
        };
        assert!(old.ensure_compatible(&new).is_err());
    }

    #[test]
    fn fingerprint_accepts_same() {
        let a = Fingerprint {
            total: Some(100),
            etag: Some("\"abc\"".into()),
            last_modified: Some("Wed, 21 Oct 2015 07:28:00 GMT".into()),
        };
        assert!(a.ensure_compatible(&a).is_ok());
    }

    #[test]
    fn fingerprint_falls_back_to_last_modified() {
        let old = Fingerprint {
            total: None,
            etag: None,
            last_modified: Some("a".into()),
        };
        let new = Fingerprint {
            total: None,
            etag: None,
            last_modified: Some("b".into()),
        };
        assert!(old.ensure_compatible(&new).is_err());
    }

    #[test]
    fn filename_from_percent_encoded_url() {
        // 中文 + 空格 + 方括号，全部 percent-encoded
        let u = Url::parse(
            "http://h/%5B%E6%B5%8B%E8%AF%95%5D%20%E4%B8%AD%E6%96%87.bin",
        )
        .unwrap();
        assert_eq!(
            super::filename_from_url(&u).as_deref(),
            Some("[测试] 中文.bin")
        );
    }

    #[test]
    fn filename_from_plain_url() {
        let u = Url::parse("http://h/a/b/movie.mkv?token=x").unwrap();
        assert_eq!(super::filename_from_url(&u).as_deref(), Some("movie.mkv"));
    }

    #[test]
    fn filename_absent_for_directory_url() {
        for s in ["http://h/", "http://h/a/b/"] {
            let u = Url::parse(s).unwrap();
            assert_eq!(super::filename_from_url(&u), None, "{s}");
        }
    }

    #[test]
    fn filename_never_escapes_directory() {
        // %2F 解码出 '/'，若不处理会变成 a/../etc/passwd 造成路径穿越。
        // sanitize 只取 basename，最终 join 必须仍落在下载目录内。
        let base = std::path::Path::new("/tmp/dl");
        for raw in ["http://h/a%2F..%2Fetc%2Fpasswd", "http://h/..%2F..%2Fetc%2Fshadow"] {
            let u = Url::parse(raw).unwrap();
            let name = super::filename_from_url(&u).unwrap();
            assert!(!name.contains('/') && !name.contains('\\'), "含分隔符: {name}");
            assert!(base.join(&name).starts_with(base), "逃出下载目录: {name}");
        }
    }
}
