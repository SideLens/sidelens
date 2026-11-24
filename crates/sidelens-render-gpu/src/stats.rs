//! 呈现耗时的统计（**纯跨平台**，纯算术）
//!
//! # 为什么要自己算分位数
//!
//! 「平均耗时」在渲染这条链路上是个骗人的指标：一帧里只要有一次 GPU 队列
//! 排满或者驱动重新编译着色器，平均值就被稀释掉了，但用户感受到的卡顿
//! 恰恰来自那些**长尾**。所以本模块同时保留平均、p50、p95、最大值 ——
//! examples/bench_present.rs 报的就是 p50/p95。
//!
//! # 为什么用环形缓冲而不是全存
//!
//! 60fps 跑一分钟就是 3600 个样本，全存没必要；分位数只关心最近一段时间的
//! 分布。环形缓冲固定 [CAPACITY] 个样本，内存恒定、无分配抖动 ——
//! 而**测量本身不能影响测量对象**，所以采集路径上不能有会引起重新分配的 push。
//!
//! 排序只在读取分位数时做（[FrameStats::p50_us]），一次 512 个 f64 的
//! 排序在微秒级，调用方本来就是每几百帧才问一次。

/// 环形缓冲容量
///
/// 512 帧 ≈ 8.5 秒 @60fps，足够描述「最近这段时间的抖动」；
/// 同时 512 个 f64 = 4 KB，能待在 L1 里，采集时不会把缓存冲掉。
pub const CAPACITY: usize = 512;

/// 一组耗时样本的统计
///
/// 所有耗时单位都是**微秒**（f64）—— 纳秒在报告里读起来太长，
/// 毫秒又看不出 GPU 路径和 GDI 的差距（一个是 0.2ms 一个是 3ms，
/// 微秒刚好）。
#[derive(Debug, Clone)]
pub struct FrameStats {
    /// 累计样本数（不受环形缓冲容量限制）
    count: u64,
    /// 累计耗时（微秒），用于算平均
    total_us: f64,
    /// 最近一个样本
    last_us: f64,
    /// 最近 [CAPACITY] 个样本
    ring: [f64; CAPACITY],
    /// 下一个写入位置
    pos: usize,
    /// 环形缓冲里已经填了多少个
    filled: usize,
}

impl Default for FrameStats {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameStats {
    /// 全零
    pub const fn new() -> Self {
        FrameStats {
            count: 0,
            total_us: 0.0,
            last_us: 0.0,
            ring: [0.0; CAPACITY],
            pos: 0,
            filled: 0,
        }
    }

    /// 记一个样本（微秒）
    pub fn record(&mut self, us: f64) {
        // 负值 / NaN 只可能来自计时器的时钟回拨，丢掉比污染统计好
        if !us.is_finite() || us < 0.0 {
            return;
        }
        self.count += 1;
        self.total_us += us;
        self.last_us = us;
        self.ring[self.pos] = us;
        self.pos = (self.pos + 1) % CAPACITY;
        if self.filled < CAPACITY {
            self.filled += 1;
        }
    }

    /// 样本总数
    pub const fn count(&self) -> u64 {
        self.count
    }

    /// 平均耗时（微秒）；没有样本时 0
    pub fn avg_us(&self) -> f64 {
        if self.count == 0 {
            0.0
        } else {
            self.total_us / self.count as f64
        }
    }

    /// 最近一个样本（微秒）
    pub const fn last_us(&self) -> f64 {
        self.last_us
    }

    /// 把环形缓冲里的样本排序后返回（升序）
    fn sorted(&self) -> Vec<f64> {
        let mut v = self.ring[..self.filled].to_vec();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        v
    }

    /// 第 p 百分位（p 取 0..=100）
    ///
    /// 用「最近秩」法：取排序后第 ceil(p/100 * n) - 1 个样本。
    /// 样本少的时候（n=1）任何分位数都等于那唯一一个样本，符合直觉。
    pub fn percentile_us(&self, p: f64) -> f64 {
        if self.filled == 0 {
            return 0.0;
        }
        let sorted = self.sorted();
        let n = sorted.len();
        let rank = ((p / 100.0) * n as f64).ceil() as usize;
        let idx = rank.saturating_sub(1).min(n - 1);
        sorted[idx]
    }

    /// p50（中位数）
    pub fn p50_us(&self) -> f64 {
        self.percentile_us(50.0)
    }

    /// p95 —— 卡顿的体感主要来自这一档
    pub fn p95_us(&self) -> f64 {
        self.percentile_us(95.0)
    }

    /// 最大值（最近 [CAPACITY] 个样本里）
    pub fn max_us(&self) -> f64 {
        self.percentile_us(100.0)
    }

    /// 清空（尺寸变化 / 重建管线后重新开始统计）
    pub fn reset(&mut self) {
        *self = FrameStats::new();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_stats_are_zero() {
        let s = FrameStats::new();
        assert_eq!(s.count(), 0);
        assert_eq!(s.avg_us(), 0.0);
        assert_eq!(s.p50_us(), 0.0);
        assert_eq!(s.p95_us(), 0.0);
        assert_eq!(s.max_us(), 0.0);
        assert_eq!(s.last_us(), 0.0);
    }

    #[test]
    fn average_is_arithmetic_mean() {
        let mut s = FrameStats::new();
        for v in [100.0, 200.0, 300.0] {
            s.record(v);
        }
        assert_eq!(s.count(), 3);
        assert_eq!(s.avg_us(), 200.0);
        assert_eq!(s.last_us(), 300.0);
    }

    #[test]
    fn percentile_picks_nearest_rank() {
        let mut s = FrameStats::new();
        for i in 1..=100 {
            s.record(i as f64);
        }
        assert_eq!(s.p50_us(), 50.0);
        assert_eq!(s.p95_us(), 95.0);
        assert_eq!(s.max_us(), 100.0);
    }

    #[test]
    fn percentile_of_single_sample_is_that_sample() {
        let mut s = FrameStats::new();
        s.record(42.0);
        assert_eq!(s.p50_us(), 42.0);
        assert_eq!(s.p95_us(), 42.0);
        assert_eq!(s.max_us(), 42.0);
    }

    #[test]
    fn ring_wraps_and_percentiles_use_recent_window() {
        let mut s = FrameStats::new();
        // 先灌满 CAPACITY 个 1000，再灌满 CAPACITY 个 1 —— 窗口里应该只剩 1
        for _ in 0..CAPACITY {
            s.record(1000.0);
        }
        for _ in 0..CAPACITY {
            s.record(1.0);
        }
        assert_eq!(s.count(), (CAPACITY * 2) as u64);
        assert_eq!(s.p50_us(), 1.0);
        assert_eq!(s.max_us(), 1.0);
        // 但平均值是全程的
        assert!((s.avg_us() - 500.5).abs() < 1e-9);
    }

    #[test]
    fn rejects_non_finite_and_negative_samples() {
        let mut s = FrameStats::new();
        s.record(f64::NAN);
        s.record(f64::INFINITY);
        s.record(-1.0);
        assert_eq!(s.count(), 0);
        assert_eq!(s.last_us(), 0.0);
    }

    #[test]
    fn reset_clears_everything() {
        let mut s = FrameStats::new();
        s.record(10.0);
        s.reset();
        assert_eq!(s.count(), 0);
        assert_eq!(s.p95_us(), 0.0);
    }
}
