//! 断点续传元数据。
//!
//! 与原版 `.ranges` 的差异：
//! - 原版每次 `write_data` 后**全量重写**文本文件（高频小写，写放大来源之一）；
//!   这里改为 JSON + 节流（默认 2s / 64MB）+ 原子替换。
//! - 记录 ETag / Last-Modified / 总长度，续传前强制校验（F-03）。
//! - 解析失败不会 panic，而是降级为 `Corrupt` 错误并保留 `.bak`（F-11）。

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::probe::Fingerprint;
use crate::range::Segment;

pub const META_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResumeMeta {
    pub version: u32,
    /// 跟随重定向后的最终 URL。
    pub url: String,
    pub total: Option<u64>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    /// 已完成区间（0-based 半开）。
    pub done: Vec<Segment>,
    pub saved_at: u64,
}

impl ResumeMeta {
    pub fn new(url: impl Into<String>, fp: &Fingerprint, done: Vec<Segment>) -> Self {
        Self {
            version: META_VERSION,
            url: url.into(),
            total: fp.total,
            etag: fp.etag.clone(),
            last_modified: fp.last_modified.clone(),
            done,
            saved_at: now_secs(),
        }
    }

    pub fn fingerprint(&self) -> Fingerprint {
        Fingerprint {
            total: self.total,
            etag: self.etag.clone(),
            last_modified: self.last_modified.clone(),
        }
    }

    pub fn completed_bytes(&self) -> u64 {
        self.done.iter().map(|(a, b)| b.saturating_sub(*a)).sum()
    }

    /// 读取元数据。文件不存在返回 `Ok(None)`；内容损坏返回 `Corrupt`。
    pub fn load(path: &Path) -> Result<Option<Self>> {
        let data = match std::fs::read(path) {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(Error::Io(e)),
        };
        match serde_json::from_slice::<ResumeMeta>(&data) {
            Ok(m) if m.version == META_VERSION => Ok(Some(m)),
            Ok(m) => Err(Error::Corrupt(format!(
                "元数据版本不兼容：{}（期望 {}）",
                m.version, META_VERSION
            ))),
            Err(e) => {
                // 保留现场，便于人工排查；不影响重新下载
                let bak = path.with_extension("json.bak");
                let _ = std::fs::copy(path, &bak);
                Err(Error::Corrupt(format!(
                    "{e}（原始文件已备份到 {}）",
                    bak.display()
                )))
            }
        }
    }

    /// 原子写入：写临时文件 → fsync → rename。
    ///
    /// rename 在同一文件系统内是原子的，崩溃时要么是旧版本、要么是新版本，
    /// 不会出现「写了一半的 JSON」。
    pub fn save_atomic(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = tmp_path(path);
        {
            use std::io::Write;
            let mut f = std::fs::File::create(&tmp)?;
            let buf = serde_json::to_vec(self)?;
            f.write_all(&buf)?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

fn tmp_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(".tmp");
    PathBuf::from(s)
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 节流器：限制元数据落盘频率，避免像原版那样每个数据块都全量重写。
#[derive(Debug)]
pub struct SaveThrottle {
    min_interval: std::time::Duration,
    min_bytes: u64,
    last_time: std::time::Instant,
    last_bytes: u64,
}

impl SaveThrottle {
    pub fn new(min_interval: std::time::Duration, min_bytes: u64) -> Self {
        Self {
            min_interval,
            min_bytes,
            // 让第一次调用必然通过
            last_time: std::time::Instant::now() - min_interval,
            last_bytes: 0,
        }
    }

    /// 达到时间或字节阈值则返回 true（并重置计时）。
    pub fn should_save(&mut self, completed: u64, force: bool) -> bool {
        let by_time = self.last_time.elapsed() >= self.min_interval;
        let by_bytes = completed.saturating_sub(self.last_bytes) >= self.min_bytes;
        if force || by_time || by_bytes {
            self.last_time = std::time::Instant::now();
            self.last_bytes = completed;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fp(total: u64) -> Fingerprint {
        Fingerprint {
            total: Some(total),
            etag: Some("\"v1\"".into()),
            last_modified: None,
        }
    }

    #[test]
    fn roundtrip_and_atomic_save() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.json");

        let meta = ResumeMeta::new("https://e/x", &fp(1000), vec![(0, 100), (200, 300)]);
        assert_eq!(meta.completed_bytes(), 200);
        meta.save_atomic(&path).unwrap();

        let loaded = ResumeMeta::load(&path).unwrap().unwrap();
        assert_eq!(loaded.done, vec![(0, 100), (200, 300)]);
        assert_eq!(loaded.total, Some(1000));
        assert_eq!(loaded.etag.as_deref(), Some("\"v1\""));
        // 临时文件不应残留
        assert!(!tmp_path(&path).exists());
    }

    #[test]
    fn load_missing_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(ResumeMeta::load(&dir.path().join("none.json"))
            .unwrap()
            .is_none());
    }

    #[test]
    fn corrupt_file_is_reported_not_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.json");
        std::fs::write(&path, b"{ this is not json").unwrap();

        let err = ResumeMeta::load(&path).unwrap_err();
        assert!(matches!(err, Error::Corrupt(_)));
        // 备份文件存在
        assert!(path.with_extension("json.bak").exists());
    }

    #[test]
    fn version_mismatch_is_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v.json");
        std::fs::write(
            &path,
            br#"{"version":99,"url":"u","total":1,"etag":null,"last_modified":null,"done":[],"saved_at":0}"#,
        )
        .unwrap();
        assert!(matches!(
            ResumeMeta::load(&path).unwrap_err(),
            Error::Corrupt(_)
        ));
    }

    #[test]
    fn throttle_limits_calls() {
        let mut t = SaveThrottle::new(std::time::Duration::from_secs(3600), 1 << 20);
        assert!(t.should_save(0, false), "首次应通过");
        assert!(!t.should_save(100, false), "时间/字节都未到");
        assert!(t.should_save(0, true), "force 必须通过");
        assert!(t.should_save(2 << 20, false), "超过字节阈值应通过");
    }
}
