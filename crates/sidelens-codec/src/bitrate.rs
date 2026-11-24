//! 码率统计与帧率估计
//!
//! # 为什么要在码流层做
//!
//! 拥塞控制需要知道"实际发送速率"，而发送速率取决于编码器的输出大小。
//! 各平台编码器都会给出码流字节数，但**统计口径**容易做错：
//!
//! - 只统计最近 1 秒 → 抖动大，误判拥塞
//! - 用算术平均 → 被单个大 I 帧拉偏
//! - 忘记算上协议头 → 低估带宽占用
//!
//! 本模块用**滑动窗口 + 指数平滑**，并区分"含头/不含头"两种口径。

#![forbid(unsafe_code)]

use std::collections::VecDeque;
use std::time::Duration;

/// 码率统计配置
#[derive(Debug, Clone, Copy)]
pub struct BitrateConfig {
    /// 滑动窗口时长（毫秒）
    pub window_ms: u64,
    /// 指数平滑系数（0-1，越大越跟手）
    pub smoothing: f64,
}

impl Default for BitrateConfig {
    fn default() -> Self {
        Self {
            window_ms: 1000,
            smoothing: 0.2,
        }
    }
}

/// 一个已发送帧的记录
#[derive(Debug, Clone, Copy)]
struct FrameRecord {
    /// 帧大小（字节，不含协议头）
    bytes: usize,
    /// 发送时刻（微秒）
    at_us: u64,
    /// 是否关键帧
    keyframe: bool,
}

/// 码率统计器
///
/// # 用法
///
/// ```
/// # use sidelens_codec::bitrate::BitrateTracker;
/// let mut t = BitrateTracker::default();
/// t.record(8000, 1_000_000, true);
/// t.record(3000, 1_016_667, false);
/// let bps = t.bitrate_bps();
/// assert!(bps > 0.0);
/// ```
#[derive(Debug)]
pub struct BitrateTracker {
    config: BitrateConfig,
    frames: VecDeque<FrameRecord>,
    /// 指数平滑后的码率（bps）
    smoothed_bps: f64,
    /// 协议头开销（每帧字节数，用于换算含头口径）
    header_overhead: usize,
    /// 累计统计
    total_frames: u64,
    total_bytes: u64,
    total_keyframes: u64,
}

impl Default for BitrateTracker {
    fn default() -> Self {
        Self::new(BitrateConfig::default(), 32)
    }
}

impl BitrateTracker {
    /// `header_overhead` 是每帧的协议头字节数（我们的帧协议是 32 字节）
    pub fn new(config: BitrateConfig, header_overhead: usize) -> Self {
        Self {
            config,
            frames: VecDeque::new(),
            smoothed_bps: 0.0,
            header_overhead,
            total_frames: 0,
            total_bytes: 0,
            total_keyframes: 0,
        }
    }

    /// 记录一帧
    pub fn record(&mut self, bytes: usize, at_us: u64, keyframe: bool) {
        self.frames.push_back(FrameRecord {
            bytes,
            at_us,
            keyframe,
        });
        self.total_frames += 1;
        self.total_bytes += bytes as u64;
        if keyframe {
            self.total_keyframes += 1;
        }
        self.evict(at_us);
        self.recompute();
    }

    /// 丢弃窗口外的记录
    fn evict(&mut self, now_us: u64) {
        let window_us = self.config.window_ms * 1000;
        while let Some(front) = self.frames.front() {
            if now_us.saturating_sub(front.at_us) > window_us {
                self.frames.pop_front();
            } else {
                break;
            }
        }
    }

    fn recompute(&mut self) {
        if self.frames.len() < 2 {
            return;
        }
        let Some(first) = self.frames.front() else {
            return;
        };
        let Some(last) = self.frames.back() else {
            return;
        };

        let span_us = last.at_us.saturating_sub(first.at_us);
        if span_us == 0 {
            return;
        }
        let bytes: usize = self.frames.iter().map(|f| f.bytes).sum();
        // 窗口内的瞬时码率
        let instant_bps = bytes as f64 * 8.0 * 1_000_000.0 / span_us as f64;

        // 指数平滑：避免单个 I 帧把读数拉爆
        let s = self.config.smoothing.clamp(0.01, 1.0);
        self.smoothed_bps = if self.smoothed_bps == 0.0 {
            instant_bps
        } else {
            self.smoothed_bps * (1.0 - s) + instant_bps * s
        };
    }

    /// 当前码率（bps，不含协议头）
    pub fn bitrate_bps(&self) -> f64 {
        self.smoothed_bps
    }

    /// 含协议头的码率（bps）
    ///
    /// 拥塞控制与带宽估算应当用这个 —— 协议头是真实占用的带宽。
    pub fn bitrate_with_overhead_bps(&self) -> f64 {
        if self.frames.is_empty() {
            return 0.0;
        }
        let overhead_ratio = if self.frames.iter().map(|f| f.bytes).sum::<usize>() > 0 {
            let data: usize = self.frames.iter().map(|f| f.bytes).sum();
            let total = data + self.frames.len() * self.header_overhead;
            total as f64 / data as f64
        } else {
            1.0
        };
        self.smoothed_bps * overhead_ratio
    }

    /// 窗口内的帧率（fps）
    pub fn fps(&self) -> f64 {
        if self.frames.len() < 2 {
            return 0.0;
        }
        let Some(first) = self.frames.front() else {
            return 0.0;
        };
        let Some(last) = self.frames.back() else {
            return 0.0;
        };
        let span_us = last.at_us.saturating_sub(first.at_us);
        if span_us == 0 {
            return 0.0;
        }
        (self.frames.len() - 1) as f64 * 1_000_000.0 / span_us as f64
    }

    /// 窗口内的平均帧大小
    pub fn avg_frame_bytes(&self) -> f64 {
        if self.frames.is_empty() {
            return 0.0;
        }
        self.frames.iter().map(|f| f.bytes).sum::<usize>() as f64 / self.frames.len() as f64
    }

    /// 窗口内关键帧占比
    pub fn keyframe_ratio(&self) -> f64 {
        if self.frames.is_empty() {
            return 0.0;
        }
        self.frames.iter().filter(|f| f.keyframe).count() as f64 / self.frames.len() as f64
    }

    pub fn total_frames(&self) -> u64 {
        self.total_frames
    }

    pub fn total_bytes(&self) -> u64 {
        self.total_bytes
    }

    pub fn total_keyframes(&self) -> u64 {
        self.total_keyframes
    }

    pub fn window_len(&self) -> usize {
        self.frames.len()
    }

    pub fn reset(&mut self) {
        self.frames.clear();
        self.smoothed_bps = 0.0;
    }

    /// 建议的目标码率（bps）
    ///
    /// 用于码率自适应：当前码率超过 `budget_bps` 时建议降，
    /// 明显低于时建议升。返回 None 表示维持不变。
    pub fn suggest_target(&self, budget_bps: f64) -> Option<f64> {
        if self.smoothed_bps == 0.0 || budget_bps <= 0.0 {
            return None;
        }
        let ratio = self.smoothed_bps / budget_bps;
        if ratio > 1.15 {
            // 超预算 15% 以上：降到预算的 85%
            Some(budget_bps * 0.85)
        } else if ratio < 0.6 {
            // 远低于预算：试着升到预算的 75%
            Some(budget_bps * 0.75)
        } else {
            None
        }
    }
}

/// 帧间隔统计（用于检测卡顿）
#[derive(Debug, Default)]
pub struct IntervalStats {
    intervals_us: VecDeque<u64>,
    capacity: usize,
}

impl IntervalStats {
    pub fn new(capacity: usize) -> Self {
        Self {
            intervals_us: VecDeque::new(),
            capacity: capacity.max(1),
        }
    }

    pub fn record_interval(&mut self, us: u64) {
        if self.intervals_us.len() == self.capacity {
            self.intervals_us.pop_front();
        }
        self.intervals_us.push_back(us);
    }

    pub fn percentile(&self, p: f64) -> Duration {
        if self.intervals_us.is_empty() {
            return Duration::ZERO;
        }
        let mut v: Vec<u64> = self.intervals_us.iter().copied().collect();
        v.sort_unstable();
        let idx = ((v.len() as f64 - 1.0) * p.clamp(0.0, 1.0)).round() as usize;
        Duration::from_micros(v[idx.min(v.len() - 1)])
    }

    /// 帧间隔的变异系数（标准差/均值），衡量流畅度
    pub fn jitter_ratio(&self) -> f64 {
        if self.intervals_us.len() < 2 {
            return 0.0;
        }
        let n = self.intervals_us.len() as f64;
        let mean = self.intervals_us.iter().map(|v| *v as f64).sum::<f64>() / n;
        if mean == 0.0 {
            return 0.0;
        }
        let var = self
            .intervals_us
            .iter()
            .map(|v| {
                let d = *v as f64 - mean;
                d * d
            })
            .sum::<f64>()
            / n;
        var.sqrt() / mean
    }
}
