//! 落盘层：预分配 + 定位写 + 短写检测。
//!
//! 关键修复（原版 F-01）：
//! 原版把「已接收字节数」当作进度，`write_data` 返回非 0 时**静默跳过**，
//! 导致文件缺块却被判定为完成。这里 `write_at` 要么完整写完，要么返回
//! [`Error::ShortWrite`]，调用方必须先拿到 `Ok` 才允许提交区间。
//!
//! 预分配使用 `posix_fallocate`（Linux），失败时退回 `set_len`。
//! 预分配能显著减少 HDD 上的碎片，也让写入偏移始终落在已分配簇内。

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

pub struct ChunkWriter {
    file: File,
    path: PathBuf,
    total: Option<u64>,
    preallocated: bool,
}

impl ChunkWriter {
    /// 创建（或截断）目标文件并预分配 `total` 字节。
    pub fn create(path: impl AsRef<Path>, total: Option<u64>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .read(true)
            .truncate(true)
            .open(&path)?;

        let mut preallocated = false;
        if let Some(len) = total {
            if len > 0 && preallocate(&file, len).is_ok() {
                preallocated = true;
            }
        }
        Ok(Self {
            file,
            path,
            total,
            preallocated,
        })
    }

    /// 打开已存在的文件（续传），不截断。
    pub fn open_existing(path: impl AsRef<Path>, total: Option<u64>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = OpenOptions::new().read(true).write(true).open(&path)?;
        Ok(Self {
            file,
            path,
            total,
            preallocated: false,
        })
    }

    /// 首次下载与续传的统一入口。
    ///
    /// 多线程路径必须走这里：续传时 `.part` 已存在（用 open_existing 保留数据），
    /// 首次下载时它还不存在（用 create 建文件 + 预分配）。
    /// 早先直接调 open_existing，导致所有首次多线程下载都以 ENOENT 失败。
    pub fn create_or_open(path: impl AsRef<Path>, total: Option<u64>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if path.exists() {
            Self::open_existing(path, total)
        } else {
            Self::create(path, total)
        }
    }

    /// 定位写入。保证要么写入 `buf.len()` 字节，要么报错。
    pub fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
        if buf.is_empty() {
            return Ok(());
        }
        write_all_at(&self.file, buf, offset).map_err(|e| {
            if e.kind() == io::ErrorKind::WriteZero {
                Error::ShortWrite {
                    offset,
                    want: buf.len(),
                    got: 0,
                }
            } else {
                Error::Io(e)
            }
        })
    }

    /// 把数据刷到磁盘（数据 + 元数据）。
    pub fn sync_all(&self) -> Result<()> {
        self.file.sync_all()?;
        Ok(())
    }

    /// 只刷数据（比 sync_all 轻，但不保证文件长度等元数据落盘）。
    pub fn sync_data(&self) -> Result<()> {
        self.file.sync_data()?;
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn total(&self) -> Option<u64> {
        self.total
    }

    pub fn was_preallocated(&self) -> bool {
        self.preallocated
    }

    /// 校验最终文件长度。
    pub fn verify_len(&self, expected: u64) -> Result<()> {
        let actual = self.file.metadata()?.len();
        if actual != expected {
            return Err(Error::LengthMismatch { expected, actual });
        }
        Ok(())
    }
}

#[cfg(unix)]
fn write_all_at(file: &File, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    while !buf.is_empty() {
        match file.write_at(buf, offset) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "pwrite 返回 0 字节",
                ));
            }
            Ok(n) => {
                buf = &buf[n..];
                offset += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(windows)]
fn write_all_at(file: &File, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        match file.seek_write(buf, offset) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "WriteFile 返回 0 字节",
                ));
            }
            Ok(n) => {
                buf = &buf[n..];
                offset += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// 预分配：优先 `posix_fallocate`，退化为 `set_len`。
#[cfg(target_os = "linux")]
fn preallocate(file: &File, len: u64) -> io::Result<()> {
    use std::os::unix::io::AsRawFd;
    if len > i64::MAX as u64 {
        return file.set_len(len);
    }
    // SAFETY: fd 有效且生命周期覆盖调用；off_t 已做范围检查。
    let rc = unsafe { libc::posix_fallocate(file.as_raw_fd(), 0, len as libc::off_t) };
    if rc == 0 {
        Ok(())
    } else {
        // 某些文件系统（tmpfs/网络盘）不支持，退回稀疏扩展
        file.set_len(len)
    }
}

#[cfg(not(target_os = "linux"))]
fn preallocate(file: &File, len: u64) -> io::Result<()> {
    file.set_len(len)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn create_preallocates_and_writes_at_offset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.bin");
        let w = ChunkWriter::create(&path, Some(1024)).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 1024);

        w.write_at(100, b"hello").unwrap();
        w.write_at(0, b"HEAD").unwrap();
        w.sync_all().unwrap();

        let mut f = File::open(&path).unwrap();
        let mut buf = vec![0u8; 1024];
        f.read_exact(&mut buf).unwrap();
        assert_eq!(&buf[0..4], b"HEAD");
        assert_eq!(&buf[100..105], b"hello");
        assert_eq!(buf[5], 0, "未写入区域应为 0");
    }

    #[test]
    fn write_across_prealloc_boundary_extends() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("b.bin");
        let w = ChunkWriter::create(&path, Some(16)).unwrap();
        w.write_at(8, &[0xAB; 32]).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 40);
    }

    #[test]
    fn open_existing_preserves_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.bin");
        std::fs::write(&path, b"0123456789").unwrap();

        let w = ChunkWriter::open_existing(&path, Some(10)).unwrap();
        w.write_at(2, b"XY").unwrap();
        w.sync_data().unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"01XY456789");
    }

    #[test]
    fn verify_len_detects_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d.bin");
        let w = ChunkWriter::create(&path, Some(10)).unwrap();
        assert!(w.verify_len(10).is_ok());
        assert!(matches!(
            w.verify_len(11).unwrap_err(),
            Error::LengthMismatch { .. }
        ));
    }

    /// F-01 回归：写入必须真正落到正确偏移，而不是"计数即完成"。
    #[test]
    fn interleaved_offsets_stay_correct() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("e.bin");
        let total = 4096u64 * 4;
        let w = ChunkWriter::create(&path, Some(total)).unwrap();

        // 模拟 4 个 worker 乱序写入
        for i in [3u64, 1, 0, 2] {
            let off = i * 4096;
            w.write_at(off, &vec![i as u8; 4096]).unwrap();
        }
        w.sync_all().unwrap();

        let data = std::fs::read(&path).unwrap();
        assert_eq!(data.len(), total as usize);
        for i in 0..4u64 {
            let blk = &data[(i * 4096) as usize..((i + 1) * 4096) as usize];
            assert!(blk.iter().all(|&b| b == i as u8), "块 {i} 内容错位");
        }
    }
}
