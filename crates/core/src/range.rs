//! 待下载区间集合。
//!
//! 与原版的差异（关键修复）：
//! - 统一使用 **0-based 半开区间 `[start, end)`**。原版用 1-based 闭区间，
//!   `m_min - 1` / `+ 1` 散落在多处，是 F-02（异常分支区间错位、丢失）的根因。
//! - 归还区间（`put`）自动合并相邻/重叠段，避免碎片无限膨胀。
//! - 所有操作在单线程下是纯函数式的，便于单测；跨线程由调用方加锁。

use std::collections::BTreeMap;

/// 一段待下载数据（半开区间）。
pub type Segment = (u64, u64);

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PendingRanges {
    /// key = 起点，value = 终点（半开）。相邻区间保证已合并。
    map: BTreeMap<u64, u64>,
}

impl PendingRanges {
    pub fn new() -> Self {
        Self::default()
    }

    /// 整段待下载。
    pub fn full(len: u64) -> Self {
        let mut s = Self::default();
        if len > 0 {
            s.map.insert(0, len);
        }
        s
    }

    /// 由已完成区间列表构造待下载集合（用于断点续传恢复）。
    pub fn from_done(len: u64, done: &[Segment]) -> Self {
        let mut s = Self::full(len);
        for &(a, b) in done {
            s.subtract(a, b);
        }
        s
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// 剩余待下载字节数。
    pub fn remaining(&self) -> u64 {
        self.map.iter().map(|(s, e)| e.saturating_sub(*s)).sum()
    }

    /// 集合覆盖的字节总数（用于"已完成集合"时语义更直观）。
    pub fn covered_len(&self) -> u64 {
        self.remaining()
    }

    /// 清空集合。
    pub fn clear_all(&mut self) {
        self.map.clear();
    }

    /// 当前所有区间快照（有序）。
    pub fn segments(&self) -> Vec<Segment> {
        self.map.iter().map(|(&s, &e)| (s, e)).collect()
    }

    /// 取出第一段，最多 `max_len` 字节。
    ///
    /// 返回的区间已从集合中移除——调用方必须保证**要么写入成功并 commit，
    /// 要么通过 `put` 归还**（后者由 `RangeLease` 的 Drop 兜底）。
    pub fn take(&mut self, max_len: u64) -> Option<Segment> {
        if max_len == 0 {
            return None;
        }
        let (&start, &end) = self.map.iter().next()?;
        self.map.remove(&start);

        let take_end = end.min(start.saturating_add(max_len));
        if take_end < end {
            // 剩余部分放回
            self.map.insert(take_end, end);
        }
        Some((start, take_end))
    }

    /// 归还一段区间，自动与相邻区间合并。
    pub fn put(&mut self, start: u64, end: u64) {
        if start >= end {
            return;
        }
        let mut s = start;
        let mut e = end;

        // 向前合并：任何 end >= s 的已有区间
        if let Some((ps, pe)) = self.map.range(..=s).next_back().map(|(&a, &b)| (a, b)) {
            if pe >= s {
                s = ps;
                e = e.max(pe);
                self.map.remove(&ps);
            }
        }
        // 向后合并：任何 start <= e 的已有区间
        while let Some((ns, ne)) = self.map.range(s..).next().map(|(&a, &b)| (a, b)) {
            if ns > e {
                break;
            }
            e = e.max(ne);
            self.map.remove(&ns);
        }
        self.map.insert(s, e);
    }

    /// 从集合中扣除一段已完成区间。
    pub fn subtract(&mut self, ds: u64, de: u64) {
        if ds >= de {
            return;
        }
        let overlapped: Vec<Segment> = self
            .map
            .iter()
            .filter(|&(s, e)| *s < de && ds < *e)
            .map(|(&s, &e)| (s, e))
            .collect();

        let mut reinsert = Vec::with_capacity(overlapped.len() * 2);
        for (s, e) in overlapped {
            self.map.remove(&s);
            if s < ds {
                reinsert.push((s, ds));
            }
            if de < e {
                reinsert.push((de, e));
            }
        }
        for (s, e) in reinsert {
            self.put(s, e);
        }
    }

    /// 区间内部自检：保证有序、不重叠、不粘连。
    pub fn check_invariants(&self) -> Result<(), String> {
        let mut prev_end: Option<u64> = None;
        for (&s, &e) in &self.map {
            if s >= e {
                return Err(format!("空区间/逆序区间 [{s},{e})"));
            }
            if let Some(pe) = prev_end {
                if s <= pe {
                    return Err(format!("区间重叠或未合并: prev_end={pe}, start={s}"));
                }
            }
            prev_end = Some(e);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_then_take_sequential() {
        let mut r = PendingRanges::full(10);
        assert_eq!(r.take(4), Some((0, 4)));
        assert_eq!(r.take(4), Some((4, 8)));
        assert_eq!(r.take(4), Some((8, 10)));
        assert_eq!(r.take(4), None);
        assert!(r.is_empty());
    }

    #[test]
    fn put_merges_adjacent_and_overlapping() {
        let mut r = PendingRanges::new();
        r.put(10, 20);
        r.put(20, 30);
        assert_eq!(r.segments(), vec![(10, 30)]);

        r.put(25, 40);
        assert_eq!(r.segments(), vec![(10, 40)]);

        r.put(0, 10);
        assert_eq!(r.segments(), vec![(0, 40)]);
        r.check_invariants().unwrap();
    }

    #[test]
    fn put_bridges_gap() {
        let mut r = PendingRanges::new();
        r.put(0, 10);
        r.put(20, 30);
        assert_eq!(r.segments(), vec![(0, 10), (20, 30)]);
        r.put(10, 20);
        assert_eq!(r.segments(), vec![(0, 30)]);
        r.check_invariants().unwrap();
    }

    #[test]
    fn from_done_rebuilds_gap() {
        // 已完成 [0,4) 与 [8,10)，待下载应为 [4,8)
        let r = PendingRanges::from_done(10, &[(0, 4), (8, 10)]);
        assert_eq!(r.segments(), vec![(4, 8)]);
        assert_eq!(r.remaining(), 4);
    }

    #[test]
    fn subtract_splits_middle() {
        let mut r = PendingRanges::full(100);
        r.subtract(30, 60);
        assert_eq!(r.segments(), vec![(0, 30), (60, 100)]);
        assert_eq!(r.remaining(), 70);
    }

    #[test]
    fn subtract_handles_multiple_segments() {
        let mut r = PendingRanges::new();
        r.put(0, 20);
        r.put(40, 60);
        r.subtract(10, 50);
        assert_eq!(r.segments(), vec![(0, 10), (50, 60)]);
    }

    #[test]
    fn take_from_multiple_ranges_prefers_lowest() {
        let mut r = PendingRanges::new();
        r.put(50, 60);
        r.put(10, 20);
        assert_eq!(r.take(100), Some((10, 20)));
        assert_eq!(r.take(100), Some((50, 60)));
    }

    #[test]
    fn no_merge_on_touching_half_open() {
        // [0,10) 与 [10,20) 是相邻而非重叠，put 应当合并
        let mut r = PendingRanges::new();
        r.put(10, 20);
        r.put(0, 10);
        assert_eq!(r.segments(), vec![(0, 20)]);
    }

    #[test]
    fn stress_random_invariants() {
        // 伪随机：交替 take / put，校验不变量与总量守恒
        let mut r = PendingRanges::full(10_000);
        let mut seed = 123456789u64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let mut held: Vec<Segment> = Vec::new();

        for _ in 0..2000 {
            if next() % 2 == 0 {
                if let Some(seg) = r.take(1 + next() % 500) {
                    held.push(seg);
                }
            } else if let Some(seg) = held.pop() {
                r.put(seg.0, seg.1);
            }
            r.check_invariants().unwrap();
            let in_flight: u64 = held.iter().map(|(a, b)| b - a).sum();
            assert_eq!(r.remaining() + in_flight, 10_000);
        }
    }
}
