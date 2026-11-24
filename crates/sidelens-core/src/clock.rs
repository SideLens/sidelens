//! 时钟与音画同步
//!
//! 音画同步的正确做法：**以音频为主时钟**，视频按 PTS 对齐。
//! 原因：音频一断就听得出来（爆音、卡顿），视频掉一帧人眼几乎无感。

use std::collections::VecDeque;

/// RTT / 单向延迟估计
///
/// 用滑动窗口 + 中位数抗离群值。为什么不用平均值：
/// 一次 GC 停顿或系统调度抖动就能把平均值拉高几十毫秒，
/// 而中位数只在真正持续劣化时才变化。
#[derive(Debug)]
pub struct RttEstimator {
    samples: VecDeque<f64>,
    window: usize,
}

impl Default for RttEstimator {
    fn default() -> Self {
        Self::new(32)
    }
}

impl RttEstimator {
    pub fn new(window: usize) -> Self {
        Self {
            samples: VecDeque::with_capacity(window.max(1)),
            window: window.max(1),
        }
    }

    /// 加入一个样本。非有限值或负数被忽略。
    pub fn add(&mut self, rtt_ms: f64) {
        if !rtt_ms.is_finite() || rtt_ms < 0.0 {
            return;
        }
        if self.samples.len() == self.window {
            self.samples.pop_front();
        }
        self.samples.push_back(rtt_ms);
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// 最小样本：最接近「真实单向延迟」的估计
    pub fn min(&self) -> f64 {
        if self.samples.is_empty() {
            return 0.0;
        }
        self.samples.iter().copied().fold(f64::INFINITY, f64::min)
    }

    /// 中位数
    pub fn median(&self) -> f64 {
        if self.samples.is_empty() {
            return 0.0;
        }
        let mut v: Vec<f64> = self.samples.iter().copied().collect();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let mid = v.len() / 2;
        if v.len() % 2 == 1 {
            v[mid]
        } else {
            (v[mid - 1] + v[mid]) / 2.0
        }
    }

    /// 当前使用的估计值
    pub fn smoothed(&self) -> f64 {
        self.median()
    }

    pub fn clear(&mut self) {
        self.samples.clear();
    }
}

/// 音画同步裁决
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncVerdict {
    /// 正常播放
    Play,
    /// 视频落后太多，丢弃
    Drop,
    /// 视频超前，等一会儿
    Wait,
}

/// 音画同步器
///
/// 音频立即播放并推进主时钟；视频按「音频已播到哪儿」对齐。
#[derive(Debug)]
pub struct AvSync {
    max_drift_ms: f64,
    drop_threshold_ms: f64,
    audio_clock_us: Option<u64>,
    /// 丢弃的迟到视频帧
    pub dropped_late: u64,
    /// 过早的视频帧（当前策略是等待而非丢弃，保留计数做诊断）
    pub dropped_early: u64,
    /// 正常对齐的次数
    pub synced: u64,
    /// 诊断：见过的最大正向 drift（微秒）
    ///
    /// # 为什么需要这个
    ///
    /// 30fps 视频 + 20ms 音频实测出现 `video_dropped_late=26`，
    /// 而 60fps + 20ms 音频是 0。需要知道 drift 到底偏到多少才能定位。
    /// 只记极值，不影响热路径（两次比较）。
    pub max_drift_us: i64,
    /// 诊断：见过的最小（最负）drift（微秒）
    pub min_drift_us: i64,
    /// 诊断：judge 调用次数（视频帧被裁决的次数，含重传帧）
    pub judge_calls: u64,
    /// 诊断：on_audio 调用次数（音频帧出队次数）
    pub on_audio_calls: u64,

    /// 音频时钟的「真实时间 → 音频时间轴」偏移校正（微秒）
    ///
    /// # 为什么需要它（第三十八轮修的根因）
    ///
    /// 原实现 `on_audio` 直接把 `pts_us + duration_us` 设为 `audio_clock`。
    /// 这在**逐帧均匀出队**时没问题，但音频抖动缓冲的 `pop_due` 是
    /// `while` 循环 —— 某个 tick 被阻塞后会一次补出多帧，于是：
    ///
    /// ```text
    /// seq=7  Δvideo=16666  Δclock=20000   drift=-4
    /// seq=8  Δvideo=16666  Δclock=60000   drift=-43338   <- 一次出队 3 帧
    /// ```
    ///
    /// 阶梯式的 `clock` 让 drift 单次抖动可达 60ms，叠加初始偏移后
    /// 稳定超过 `drop_threshold_ms=120`，实测 `video_dropped_late` 从 0
    /// 一路涨到 311（20 秒）。
    ///
    /// 修法：`clock` 不再取「已出队帧的 PTS」，而是
    /// `真实时间 + offset_us`。`offset_us` 由每帧音频的
    /// `(pts + duration) - 真实时间` 平滑逼近，变化率受限。
    ///
    /// 这样批量出队不影响 `clock` 连续性，音频丢帧时 `clock` 也照常前进。
    clock_offset_us: i64,
    /// 上一次 `on_audio` 的真实时刻（微秒，发送端时基）
    last_real_us: Option<u64>,
    /// 单次 `clock_offset_us` 的最大调整量（微秒）
    ///
    /// 不限制的话，一帧异常样本会把 clock 拽走，而 clock 抖动本身
    /// 会制造 Drop —— 正反馈。
    max_offset_step_us: i64,
}

/// `clock_offset_us` 每次最多调整多少（微秒）
///
/// 1ms 的依据：音频帧间隔是 20ms，若每帧最多校正 1ms，
/// 20 帧（400ms）能校正 20ms。真实的音画偏移是**缓慢变化**的，
/// 这个速度足够跟上，又不会被单帧抖动带偏。
pub const CLOCK_OFFSET_MAX_STEP_US: i64 = 1000;

impl Default for AvSync {
    fn default() -> Self {
        Self::new(40.0, 120.0)
    }
}

impl AvSync {
    /// `max_drift_ms`：视频允许超前多少毫秒（超过就等）
    /// `drop_threshold_ms`：视频落后多少毫秒就丢弃
    pub fn new(max_drift_ms: f64, drop_threshold_ms: f64) -> Self {
        Self {
            max_drift_ms,
            drop_threshold_ms,
            audio_clock_us: None,
            dropped_late: 0,
            dropped_early: 0,
            synced: 0,
            max_drift_us: i64::MIN,
            min_drift_us: i64::MAX,
            judge_calls: 0,
            on_audio_calls: 0,
            clock_offset_us: 0,
            last_real_us: None,
            max_offset_step_us: CLOCK_OFFSET_MAX_STEP_US,
        }
    }

    /// 音频帧出队时调用，推进主时钟
    ///
    /// `pts_us` / `duration_us` 是**发送端时间轴**上的值；
    /// `real_now_us` 是**同一时间轴**上的「当前时刻」
    /// （调用方用 `now - clock_offset` 换算好再传）。
    ///
    /// # 为什么需要 `real_now_us`
    ///
    /// 见 `clock_offset_us` 字段的文档：`clock` 必须按真实时间连续推进，
    /// 不能取「已出队帧的 PTS」—— 批量出队会让它阶梯式跳跃。
    ///
    /// # 兼容旧调用
    ///
    /// `on_audio` 保留原签名（`real_now_us` 传 `None`），此时退化成
    /// 旧行为。生产路径应改用 [`Self::on_audio_at`]。
    pub fn on_audio(&mut self, pts_us: u64, duration_us: u64) {
        self.on_audio_at(pts_us, duration_us, None);
    }

    /// 带真实时刻的 `on_audio`
    ///
    /// `real_now_us` 是「发送端时间轴上的当前时刻」。传 `None` 退化成
    /// 旧的阶梯行为（仅供未改造的调用方与旧测试使用）。
    pub fn on_audio_at(&mut self, pts_us: u64, duration_us: u64, real_now_us: Option<u64>) {
        self.on_audio_calls += 1;

        let target = pts_us.saturating_add(duration_us);

        let Some(real_now) = real_now_us else {
            // 旧行为：直接取 PTS。批量出队时会阶梯跳。
            self.audio_clock_us = Some(target);
            return;
        };

        // 目标偏移：让 clock 在 real_now 时刻恰好等于 target
        let want_offset = target as i64 - real_now as i64;

        // 限速逼近：单帧最多挪 max_offset_step_us
        let delta = (want_offset - self.clock_offset_us)
            .clamp(-self.max_offset_step_us, self.max_offset_step_us);
        self.clock_offset_us += delta;

        // clock 按真实时间线性推进
        let clock = (real_now as i64 + self.clock_offset_us).max(0) as u64;
        self.audio_clock_us = Some(clock);
        self.last_real_us = Some(real_now);
    }

    /// 当前时钟偏移校正量（微秒），诊断用
    pub fn clock_offset_us(&self) -> i64 {
        self.clock_offset_us
    }

    /// 设置单次偏移校正上限（测试用）
    pub fn with_offset_step(mut self, step_us: i64) -> Self {
        self.max_offset_step_us = step_us.max(1);
        self
    }

    /// 当前音频播放位置（微秒）
    pub fn audio_clock_us(&self) -> Option<u64> {
        self.audio_clock_us
    }

    /// 裁决一帧视频是否应当播放
    pub fn judge(&mut self, video_pts_us: u64) -> SyncVerdict {
        self.judge_calls += 1;
        let Some(clock) = self.audio_clock_us else {
            // 纯视频模式：不做对齐
            return SyncVerdict::Play;
        };

        let drift_us = (video_pts_us as i128 - clock as i128) as i64;
        self.max_drift_us = self.max_drift_us.max(drift_us);
        self.min_drift_us = self.min_drift_us.min(drift_us);

        let drift_ms = drift_us as f64 / 1000.0;

        if drift_ms > self.max_drift_ms {
            return SyncVerdict::Wait;
        }
        if drift_ms < -self.drop_threshold_ms {
            self.dropped_late += 1;
            return SyncVerdict::Drop;
        }
        self.synced += 1;
        SyncVerdict::Play
    }

    pub fn reset(&mut self) {
        self.audio_clock_us = None;
        self.clock_offset_us = 0;
        self.last_real_us = None;
    }
}
