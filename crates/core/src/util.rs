//! 通用工具：单位换算、文件名字清洗、大小格式化。
//!
//! 单位表修复了原版 F-05（TB 乘数被写成 256，偏差 2^32 倍）。

/// 单位乘数，索引对应 `B / KB / MB / GB / TB`。
///
/// 原版数组为 `{1, 1024, 1048576, 1073741824, 256}` —— 最后一项本应是
/// 1099511627776，写成了 256，导致「1 TB」被解释成 256 字节。
pub const UNIT_MUL: [u64; 5] = [
    1,
    1024,
    1024 * 1024,
    1024 * 1024 * 1024,
    1024u64 * 1024 * 1024 * 1024,
];

pub const UNIT_NAMES: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];

/// 由「数值 + 单位索引」计算出字节数，带溢出保护。
pub fn scale(value: u64, unit_index: usize) -> Option<u64> {
    let mul = *UNIT_MUL.get(unit_index)?;
    value.checked_mul(mul)
}

/// 解析 `"10M"` / `"1G"` / `"512K"` / `"1048576"` 这类 aria2 风格尺寸。
pub fn parse_size(input: &str) -> Option<u64> {
    let s = input.trim();
    if s.is_empty() {
        return None;
    }
    let (num_part, suffix) = s.split_at(
        s.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(s.len()),
    );
    let value: f64 = num_part.parse().ok()?;
    if !value.is_finite() || value < 0.0 {
        return None;
    }
    let mul: f64 = match suffix.trim().to_ascii_uppercase().as_str() {
        "" | "B" => 1.0,
        "K" | "KB" | "KIB" => 1024.0,
        "M" | "MB" | "MIB" => 1024.0 * 1024.0,
        "G" | "GB" | "GIB" => 1024.0 * 1024.0 * 1024.0,
        "T" | "TB" | "TIB" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    let bytes = value * mul;
    if bytes > u64::MAX as f64 {
        return None;
    }
    Some(bytes as u64)
}

/// 人类可读的大小，例如 `1.42 GB`。
pub fn size_str(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    format!("{:.2} {}", v, UNITS[i])
}

/// 人类可读的速度，例如 `12.3 MB/s`。
pub fn speed_str(bytes_per_sec: f64) -> String {
    if !bytes_per_sec.is_finite() || bytes_per_sec <= 0.0 {
        return "0 B/s".to_string();
    }
    format!("{}/s", size_str(bytes_per_sec as u64))
}

/// Windows / Linux 通用的非法文件名字符（原版用 `Path.GetInvalidFileNameChars`，
/// 在 Linux 上只过滤 `/` 和 `\0`，跨平台行为不一致）。
const INVALID_CHARS: &[char] = &['<', '>', ':', '"', '/', '\\', '|', '?', '*'];

/// Windows 保留设备名（大小写不敏感，含扩展名也算）。
const RESERVED_NAMES: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// 清洗文件名：去除路径分隔符、非法字符、控制字符、Windows 保留名，
/// 并防止 `.` / `..` 造成的目录穿越。
pub fn sanitize_filename(raw: &str) -> String {
    let base = raw
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(raw)
        .trim()
        .trim_matches('.');

    let mut out: String = base
        .chars()
        .map(|c| {
            if c.is_control() || INVALID_CHARS.contains(&c) {
                '_'
            } else {
                c
            }
        })
        .collect();

    if out.is_empty() {
        out.push_str("download");
    }

    // Windows 保留名：加下划线前缀
    let stem = out.split('.').next().unwrap_or("").to_ascii_uppercase();
    if RESERVED_NAMES.contains(&stem.as_str()) {
        out.insert(0, '_');
    }

    // 极端长名截断（保留扩展名）
    const MAX: usize = 180;
    if out.chars().count() > MAX {
        let ext = std::path::Path::new(&out)
            .extension()
            .map(|e| e.to_string_lossy().to_string())
            .unwrap_or_default();
        let keep = MAX.saturating_sub(ext.len() + 1);
        let truncated: String = out.chars().take(keep).collect();
        out = if ext.is_empty() {
            truncated
        } else {
            format!("{truncated}.{ext}")
        };
    }
    out
}

/// 十六进制 GID（aria2 使用 16 位小写 hex）。
pub fn gid_to_hex(gid: u64) -> String {
    format!("{gid:016x}")
}

pub fn gid_from_hex(s: &str) -> Option<u64> {
    let t = s.trim();
    if t.len() != 16 {
        return None;
    }
    u64::from_str_radix(t, 16).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// F-05 回归：TB 乘数必须是 2^40，而不是 256。
    #[test]
    fn unit_table_is_correct() {
        assert_eq!(UNIT_MUL[0], 1);
        assert_eq!(UNIT_MUL[1], 1 << 10);
        assert_eq!(UNIT_MUL[2], 1 << 20);
        assert_eq!(UNIT_MUL[3], 1 << 30);
        assert_eq!(UNIT_MUL[4], 1u64 << 40);
        assert_eq!(UNIT_MUL[4], 1_099_511_627_776);
        // 原版的值会在这里失败
        assert_ne!(UNIT_MUL[4], 256);
    }

    #[test]
    fn scale_detects_overflow() {
        assert_eq!(scale(1, 4), Some(1u64 << 40));
        assert_eq!(scale(u64::MAX, 1), None);
        assert_eq!(scale(1, 9), None);
    }

    #[test]
    fn parse_size_variants() {
        assert_eq!(parse_size("1048576"), Some(1048576));
        assert_eq!(parse_size("1M"), Some(1 << 20));
        assert_eq!(parse_size("1m"), Some(1 << 20));
        assert_eq!(parse_size("10 MB"), Some(10 << 20));
        assert_eq!(parse_size("1.5G"), Some(1_610_612_736));
        assert_eq!(parse_size("1T"), Some(1u64 << 40));
        assert_eq!(parse_size(""), None);
        assert_eq!(parse_size("abc"), None);
        assert_eq!(parse_size("-5M"), None);
    }

    #[test]
    fn size_str_formats() {
        assert_eq!(size_str(512), "512 B");
        assert_eq!(size_str(1536), "1.50 KB");
        assert_eq!(size_str(1 << 20), "1.00 MB");
    }

    #[test]
    fn sanitize_blocks_traversal_and_reserved() {
        assert_eq!(sanitize_filename("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_filename("C:\\Windows\\x.dll"), "x.dll");
        assert_eq!(sanitize_filename("a/b/c.zip"), "c.zip");
        assert_eq!(sanitize_filename("CON"), "_CON");
        assert_eq!(sanitize_filename("nul.txt"), "_nul.txt");
        assert!(!sanitize_filename("a:b?c*d|e\"f<g>h").contains(':'));
        assert_eq!(sanitize_filename(""), "download");
        assert_eq!(sanitize_filename("..."), "download");
    }

    #[test]
    fn gid_roundtrip() {
        let g = 0x2089_b05e_cca3_d829u64;
        assert_eq!(gid_to_hex(g), "2089b05ecca3d829");
        assert_eq!(gid_from_hex("2089b05ecca3d829"), Some(g));
        assert_eq!(gid_from_hex("zzzz"), None);
    }
}
