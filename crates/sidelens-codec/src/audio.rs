//! 音频帧封装与时间戳管理
//!
//! # 为什么音频要单独一层
//!
//! 这里**没有 Opus 编解码器**。编码器是平台专属的（Android 用 MediaCodec，
//! iOS 用 AudioToolbox，Windows 用 Media Foundation），而且都要外部库。
//! 但**时间戳的产生与消费是跨平台通用的纯逻辑**，把它抽出来能做到：
//!
//! 1. 完全可测（不需要编码器、不需要声卡、不需要时钟）
//! 2. 各平台复用（三端产出的音频帧都走同一套 PTS 规则）
//! 3. 音画同步的裁决逻辑可以离线验证
//!
//! # 音频是主时钟
//!
//! SideLens 目前只有视频链路。加上音频之后，同步的正确做法是
//! **以音频为主时钟、视频按 PTS 对齐**，而不是反过来：
//!
//! - 人耳对音频断档、爆音极其敏感（几毫秒的裂缝就听得出来）
//! - 人眼对掉一帧几乎无感（视频本来就是 16.7ms 一格）
//!
//! 所以视频可以为了追上音频而丢帧、等待，音频不可以。
//! `sidelens-core` 的 `AvSync` 已经实现了这套裁决，但它需要音频侧提供两样东西：
//!
//! 1. 每帧的 **PTS**（微秒）
//! 2. 每帧的 **时长**（微秒）—— 这个才是关键，
//!    `AvSync` 用 `pts + duration` 推进主时钟
//!
//! 本模块的 `OpusConfig::frame_duration_us` 与 `AudioPacer` 就负责稳定地
//! 产出这两个值。
//!
//! # 核心原则：按采样计数，不按墙上时钟
//!
//! 音频时间戳**绝不能用「当前墙上时间」**。必须用
//! 「起始 PTS + 帧序号 × 帧时长」累加。
//!
//! 原因：
//!
//! - 采集回调的实际间隔会抖动（±2ms 很常见，系统调度、GC、重采样都会引入）
//! - 如果用采集时刻当 PTS，抖动会**累积**进音画同步，
//!   表现就是视频播放速度追着音频抖动，肉眼可见地一顿一顿
//! - 按帧序号算，PTS 是严格等距的，接收端能稳定推进主时钟
//!
//! 这就是所谓的「按采样计数而不是按墙上时钟」原则。
//! 抖动只影响**帧什么时候被推入缓冲**，不影响**帧自己声称自己是什么时候的**。
//!
//! # Opus 帧长是硬约束
//!
//! Opus 只允许 `2.5 / 5 / 10 / 20 / 40 / 60` 毫秒这六档帧长。
//! 传入 15ms 这类「看起来合理」的值，编码器不会报错，而是**静默地**
//! 编码成 10ms 或 20ms —— 于是 `frame_duration_us()` 算出来的时长
//! 和实际码流里的时长对不上，主时钟每帧偏 5ms，一秒就偏 250ms。
//!
//! 这种错误极难在联调时定位（音频听起来完全正常，只有音画不同步），
//! 所以 `OpusConfig::validate` 必须**主动拒绝**非标准帧长，而不是信任调用方。

#![forbid(unsafe_code)]

/// Opus 支持的采样率（Hz）
///
/// 固定 48kHz 是最省心的选择：Opus 内部永远以 48kHz 工作，
/// 其他采样率进入编码器时会被重采样，多一次转换就多一份延迟与失真。
pub const OPUS_SAMPLE_RATES: [u32; 5] = [8_000, 12_000, 16_000, 24_000, 48_000];

/// Opus 允许的帧长（毫秒）
///
/// 这是 Opus 规范（RFC 6716）里 SILK/CELT 混合模式唯一合法的六档。
pub const OPUS_FRAME_MS: [f64; 6] = [2.5, 5.0, 10.0, 20.0, 40.0, 60.0];

/// 时间戳跳变判定阈值（微秒）
///
/// 相邻帧的 PTS 差距超过 500ms，基本不可能是网络抖动造成的，
/// 更可能是采集重启、切换设备、或者时间基被重置。
pub const DISCONTINUITY_THRESHOLD_US: u64 = 500_000;

/// 浮点比较的容差（帧长以毫秒表示，用 f64 是 Opus 的 2.5ms 逼的）
const FRAME_MS_EPSILON: f64 = 1e-9;

/// 抖动缓冲容量的硬上限（帧）
///
/// 正常目标深度是 20-40ms，折算成 2.5ms 帧也就 17 帧。
/// 上限存在的意义是防止调用方传一个离谱的 `target_ms` 导致巨量内存。
const MAX_CAPACITY_FRAMES: usize = 512;

// ---------------------------------------------------------------------------
// 自适应目标深度的常量
//
// 这套常量是视频侧 `sidelens_core::jitter` 那套「三道闸门」的音频版本。
// 数字全部重新定过，原因见 `AudioJitterBuffer::adapt` 的文档。
// ---------------------------------------------------------------------------

/// 自适应目标深度的下限（毫秒）
///
/// 音频**不能**像视频那样降到 10ms。20ms 是默认帧长，也是「至少能装下
/// 一帧」的物理底线：目标深度低于一帧时 `capacity_frames()` 只剩
/// `ceil(x/20)+1 = 2`，任何一个乱序帧都会直接顶到溢出。
pub const AUDIO_MIN_TARGET_MS: f64 = 20.0;

/// 自适应目标深度的上限（毫秒）
///
/// 视频侧的上限是 200ms（要等关键帧，200ms 不算多）。音频到这个量级
/// 已经不是「缓冲」而是「对讲机」了 —— 人耳对 200ms 以上的音频延迟
/// 会明显感到回声感。再往上加只会让体验更差，不如让上层去做丢包隐藏。
pub const AUDIO_MAX_TARGET_MS: f64 = 200.0;

/// 抖动 → 目标深度的安全系数
///
/// 视频侧是 2.5，音频取 2.0。方向反直觉但理由很直接：
///
/// - 视频估小了只是掉一帧画面，人眼几乎无感
/// - 音频估小了是一段可听见的咔哒声，比多一点延迟难听得多
///
/// 所以宁可多留。2.0 而不是 2.5 是因为音频的抖动 EWMA 本身已经偏保守
/// （见 `JITTER_EWMA_ALPHA`），再乘 2.5 会让目标深度长期贴着上限，
/// 白白把延迟堆上去。
pub const AUDIO_JITTER_SAFETY_FACTOR: f64 = 2.0;

/// 单次下调的最大幅度（毫秒）
///
/// 视频侧是 2ms，音频放宽到 10ms。原因：音频的目标深度本来就窄
/// （20-60ms），按 2ms 一步要从 40 收敛到 20 得走 10 次、跨 5 秒驻留期，
/// 链路早就变了。10ms 一步在 20-60ms 区间里最多两跳到位，
/// 而且每一步仍然是「一个完整帧长」的量级，不会砍出空洞。
pub const AUDIO_ADAPT_MAX_STEP_DOWN_MS: f64 = 10.0;

/// 小于这个幅度就不调整（避免为浮点噪声反复写同一个值）
pub const AUDIO_ADAPT_MIN_DELTA_MS: f64 = 0.1;

/// 调整后的驻留期（微秒）
///
/// 视频侧 200ms，音频拉长到 500ms。音频比视频更怕震荡：视频的
/// `target_ms` 抖一下只是多等或少等，音频的 `target_ms` 抖一下
/// 会直接改变迟到窗口，把本来能补播的帧判成迟到 —— 那是听得见的爆音。
/// 500ms = 25 帧 @20ms，足够让 EWMA 在新目标上稳定下来再评估下一次。
pub const AUDIO_ADAPT_HOLD_US: u64 = 500_000;

/// 单个抖动样本的合法性上限（微秒）
///
/// 1 秒以上的偏差不是抖动，是时基重置、暂停或采集重启。
/// 丢掉这个样本，否则一次卡顿会把 EWMA 拉高并保持很久。
const JITTER_SAMPLE_MAX_US: f64 = 1_000_000.0;

/// 抖动 EWMA 的平滑系数（新样本权重）
///
/// 与视频侧一致（0.9 / 0.1）。约 10 个样本跟上一次阶跃，
/// 20ms 帧下就是 200ms —— 比 500ms 驻留期短，驻留期结束时 EWMA
/// 已经反映完新链路了。
const JITTER_EWMA_ALPHA: f64 = 0.1;

/// `adapt` 至少需要多少个 `|D|` 样本才敢动
///
/// 一个样本分不清「偶发尖峰」和「稳定抖动」。
const JITTER_MIN_SAMPLES: u64 = 2;

/// 音频抖动缓冲的自适应统计
///
/// 字段命名对齐 `sidelens_core::jitter::JitterStats` 的
/// `adaptations / adapt_ups / adapt_downs / avg_jitter_ms`，
/// 这样两边的诊断日志可以直接并排看。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct AudioJitterStats {
    /// `adapt` 实际改过多少次 `target_ms`
    pub adaptations: u64,
    /// 其中向上调（加缓冲）的次数
    pub adapt_ups: u64,
    /// 其中向下调（减缓冲）的次数
    pub adapt_downs: u64,
    /// 当前抖动 EWMA（毫秒）。也就是 `adapt` 的唯一输入
    pub avg_jitter_ms: f64,
}

/// Opus 编码参数
///
/// # 为什么帧长用 `f64` 而不是整数
///
/// 因为 Opus 的最小合法帧长是 **2.5ms**。用 `u32` 毫秒表示会丢掉这一档，
/// 而 2.5ms 恰恰是低延迟场景（游戏、实时通话）最常用的值。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OpusConfig {
    /// 采样率（Hz）。Opus 支持 8000/12000/16000/24000/48000
    pub sample_rate: u32,
    /// 声道数（1=单声道，2=立体声）
    pub channels: u8,
    /// 每帧时长（毫秒）。Opus 标准值：2.5/5/10/20/40/60
    pub frame_ms: f64,
}

impl Default for OpusConfig {
    /// WebRTC 的默认组合：48kHz / 单声道 / 20ms
    ///
    /// 20ms 是延迟与开销的折中点：帧太短（2.5ms）协议头占比过高、
    /// 包太密；帧太长（60ms）单帧丢失就是 60ms 的空洞，丢包隐藏救不回来。
    fn default() -> Self {
        Self {
            sample_rate: 48_000,
            channels: 1,
            frame_ms: 20.0,
        }
    }
}

impl OpusConfig {
    /// 校验参数组合是否合法
    ///
    /// # 为什么必须拒绝非标准帧长
    ///
    /// Opus 只允许 2.5/5/10/20/40/60ms 六档。传 15ms 进去，编码器
    /// **不会报错**，而是静默地按 10ms 或 20ms 编码。后果是：
    ///
    /// - `frame_duration_us()` 返回 15000
    /// - 实际码流里每帧只有 10000 或 20000 微秒
    /// - 主时钟每帧偏 5ms，一秒钟偏 250ms，十秒钟音画彻底错开
    ///
    /// 而且音频本身听起来完全正常 —— 只有音画不同步这一个症状，
    /// 排查时极易误判成视频侧的问题。所以宁可在这里硬失败。
    pub fn validate(&self) -> Result<(), String> {
        if !OPUS_SAMPLE_RATES.contains(&self.sample_rate) {
            return Err(format!(
                "不支持的采样率 {}Hz：Opus 只接受 8000/12000/16000/24000/48000",
                self.sample_rate
            ));
        }

        if self.channels != 1 && self.channels != 2 {
            return Err(format!(
                "不支持的声道数 {}：只接受 1（单声道）或 2（立体声）",
                self.channels
            ));
        }

        if !self.frame_ms.is_finite() {
            return Err(format!("帧长必须是有限数，收到 {}", self.frame_ms));
        }

        // 逐档比对而不是用区间判断：Opus 的帧长是**离散的六档**，
        // 区间内的其他值都会被编码器静默改写
        let matched = OPUS_FRAME_MS
            .iter()
            .any(|v| (self.frame_ms - v).abs() < FRAME_MS_EPSILON);
        if !matched {
            return Err(format!(
                "不支持的帧长 {}ms：Opus 只接受 2.5/5/10/20/40/60，其他值会被编码器静默改写导致时间戳错位",
                self.frame_ms
            ));
        }

        // 理论上标准组合都能算出非零采样，这里兜住除零与浮点退化
        if self.frame_samples() == 0 {
            return Err(format!(
                "{}Hz 采样率下 {}ms 帧长算不出采样数",
                self.sample_rate, self.frame_ms
            ));
        }

        Ok(())
    }

    /// 每帧的采样数
    ///
    /// 例：48000Hz × 20ms = 960 个采样。
    ///
    /// 这个值同时是 Opus 编码器的 `frame_size` 参数，
    /// 也是解码端校验「收到的是不是完整一帧」的依据。
    pub fn frame_samples(&self) -> u32 {
        (self.sample_rate as f64 * self.frame_ms / 1000.0).round() as u32
    }

    /// 每帧的时长（微秒）
    ///
    /// 由采样数反推，而不是直接 `frame_ms * 1000` —— 这是「按采样计数」
    /// 原则的体现：时长永远等于「采样数 ÷ 采样率」，两条路径不会算出
    /// 互相矛盾的值。对所有合法组合，两种算法结果一致
    /// （48000Hz 的 20ms 帧是 960 采样，恰好 20000 微秒）。
    ///
    /// # 这个值为什么最关键
    ///
    /// `AvSync` 用 `pts + duration` 推进主时钟。duration 差 1 微秒，
    /// 一秒钟（50 帧）就差 50 微秒，十分钟后就是 30ms 的音画偏移。
    pub fn frame_duration_us(&self) -> u64 {
        if self.sample_rate == 0 {
            return 0;
        }
        let samples = self.frame_samples() as u64;
        samples * 1_000_000 / self.sample_rate as u64
    }

    /// 每秒帧数（诊断用）
    pub fn frames_per_second(&self) -> f64 {
        if self.frame_ms <= 0.0 {
            return 0.0;
        }
        1000.0 / self.frame_ms
    }
}

/// 一帧编码后的音频
///
/// 字段刻意保持「哑数据」形态：不含编解码器、不含解码后的 PCM，
/// 只有时间信息与字节。这样它可以无成本地跨线程、跨进程、跨网络传递。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioFrame {
    /// 呈现时间戳（微秒）
    pub pts_us: u64,
    /// 帧时长（微秒）
    pub duration_us: u64,
    /// 帧序号（单调递增）
    pub seq: u32,
    /// 是否静音帧（DTX 产生的舒适噪声帧）
    pub silent: bool,
    /// 这一帧**到达本地**的时刻（微秒）
    ///
    /// # 为什么必须有它
    ///
    /// 自适应目标深度（[`AudioJitterBuffer::adapt`]）要按实测抖动调整，
    /// 而抖动的定义需要「发送时刻」与「到达时刻」两个时间轴：
    ///
    /// ```text
    /// D(i-1,i) = (R_i - R_{i-1}) - (S_i - S_{i-1})
    /// ```
    ///
    /// 但 `AudioFrame` 是发送端产出的，它只知道 PTS，不知道这一帧在路上
    /// 走了多久。接收端在把帧交给 [`AudioJitterBuffer::push`] 之前填这个
    /// 字段即可。
    ///
    /// # 为什么不能只用 PTS 间隔当代理
    ///
    /// `|(pts_i - pts_{i-1}) - frame_duration_us|` 看着是等价物，但
    /// `AudioPacer` 产出的 PTS 严格等距（那是「按采样计数」原则的核心
    /// 承诺），这个式子**恒等于 0**。它对网络抖动完全失明 —— 拿它喂
    /// `adapt` 只会把目标深度一路压到下限，然后在链路真变差时开始丢帧。
    ///
    /// # 默认值 0 的含义
    ///
    /// 0 表示「调用方没填」。此时抖动样本没有意义，`adapt` 会因为
    /// 样本不足直接返回 `None`（见 [`AudioJitterBuffer::adapt`]），
    /// 行为与没有这个功能时完全一致 —— 老调用点不受影响。
    pub arrived_at_us: u64,
    /// 编码后的字节
    pub payload: Vec<u8>,
}

impl AudioFrame {
    /// 构造一帧
    pub fn new(pts_us: u64, duration_us: u64, seq: u32, payload: Vec<u8>) -> Self {
        Self {
            pts_us,
            duration_us,
            seq,
            silent: false,
            arrived_at_us: 0,
            payload,
        }
    }

    /// 构造一帧并带上到达时刻
    ///
    /// 等价于 [`AudioFrame::new`] 之后再赋 `arrived_at_us`，只是为了
    /// 让接收端的调用点更短。
    pub fn with_arrival(
        pts_us: u64,
        duration_us: u64,
        seq: u32,
        arrived_at_us: u64,
        payload: Vec<u8>,
    ) -> Self {
        Self {
            arrived_at_us,
            ..Self::new(pts_us, duration_us, seq, payload)
        }
    }

    /// 本帧结束时刻（微秒）
    ///
    /// 用饱和加法：PTS 已经贴近 `u64::MAX` 时不应回绕成 0，
    /// 那会让抖动缓冲把一帧当成「远古帧」直接丢掉。
    pub fn end_pts_us(&self) -> u64 {
        self.pts_us.saturating_add(self.duration_us)
    }

    /// 帧的字节数
    pub fn len(&self) -> usize {
        self.payload.len()
    }

    /// 负载是否为空（静音帧常见）
    pub fn is_empty(&self) -> bool {
        self.payload.is_empty()
    }
}

/// 音频时间戳发生器
///
/// # 它到底解决什么问题
///
/// 采集回调被调用时，你手上只有「墙上时间」。如果直接拿它当 PTS，
/// 下面这些抖动会全部进入音画同步：
///
/// ```text
/// 理论间隔 : 20.000 20.000 20.000 20.000 20.000   （严格等距）
/// 实际回调 : 21.8   18.3   22.1   19.7   20.4     （±2ms 抖动）
/// ```
///
/// 主时钟按实际回调推进，就会一会儿快一会儿慢，视频跟着它抖动。
/// 更糟的是抖动会**累积**：连续 100 次 +2ms 就是 200ms 的漂移。
///
/// 本结构改为 `PTS = 起始 + 帧序号 × 帧时长`，输出严格等距，
/// 抖动被完全隔离在「帧什么时候被送进缓冲」这一侧。
///
/// # 用法
///
/// ```
/// # use sidelens_codec::audio::{AudioPacer, OpusConfig};
/// let mut pacer = AudioPacer::new(OpusConfig::default(), 0).expect("默认配置合法");
///
/// let (pts, dur) = pacer.next_frame();
/// assert_eq!(pts, 0);
/// assert_eq!(dur, 20_000);
///
/// let (pts, _) = pacer.next_frame();
/// assert_eq!(pts, 20_000, "第二帧严格等于一帧时长之后");
/// ```
#[derive(Debug, Clone)]
pub struct AudioPacer {
    config: OpusConfig,
    frame_duration_us: u64,
    next_pts_us: u64,
    frames_emitted: u64,
}

impl AudioPacer {
    /// 创建时间戳发生器，从 `start_pts_us` 开始
    ///
    /// 配置非法时返回错误 —— 这里不静默修正，因为修正之后的帧长
    /// 与真实码流不一致，比直接失败更难查。
    pub fn new(config: OpusConfig, start_pts_us: u64) -> Result<Self, String> {
        config.validate()?;
        Ok(Self {
            frame_duration_us: config.frame_duration_us(),
            config,
            next_pts_us: start_pts_us,
            frames_emitted: 0,
        })
    }

    /// 取下一帧的时间戳与时长，并推进内部时钟
    ///
    /// 返回 `(pts_us, duration_us)`。**不看墙上时间**，所以调用多少次、
    /// 什么时候调用，结果都一样。
    pub fn next_frame(&mut self) -> (u64, u64) {
        let pts = self.next_pts_us;
        let dur = self.frame_duration_us;
        self.next_pts_us = self.next_pts_us.saturating_add(dur);
        self.frames_emitted = self.frames_emitted.saturating_add(1);
        (pts, dur)
    }

    /// 当前应到达的 PTS（不推进）
    ///
    /// 也就是「下一帧会拿到的时间戳」。
    pub fn current_pts_us(&self) -> u64 {
        self.next_pts_us
    }

    /// 已产生的帧数
    pub fn frames_emitted(&self) -> u64 {
        self.frames_emitted
    }

    /// 重置到指定 PTS
    ///
    /// 语义是「这条时间线作废，从新起点重来」，因此**帧计数一并归零**。
    /// 需要跨重置的累计值请由上层自己维护 —— 重置后沿用旧计数会让人
    /// 误以为中途没有断过。
    pub fn reset(&mut self, start_pts_us: u64) {
        self.next_pts_us = start_pts_us;
        self.frames_emitted = 0;
    }

    /// 当前配置
    pub fn config(&self) -> OpusConfig {
        self.config
    }

    /// 每帧时长（微秒）
    pub fn frame_duration_us(&self) -> u64 {
        self.frame_duration_us
    }
}

/// 抖动缓冲的入队结果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushOutcome {
    /// 正常入队
    Ok,
    /// 帧太旧，已被播放时钟越过，只能丢弃
    Late,
    /// 缓冲已满，丢弃了最旧的一帧
    Overflow,
    /// 时间戳跳变过大，缓冲已自动清空重建
    Discontinuity,
}

impl std::fmt::Display for PushOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            PushOutcome::Ok => "OK",
            PushOutcome::Late => "LATE",
            PushOutcome::Overflow => "OVERFLOW",
            PushOutcome::Discontinuity => "DISCONTINUITY",
        };
        f.write_str(s)
    }
}

/// 按目标深度算容量（帧）
///
/// 容量 = 目标深度能装下的帧数 + 1 帧余量。多留一帧是为了吸收
/// 「刚攒够深度又来一帧」的正常情况，否则缓冲会在目标深度附近反复触发溢出。
///
/// 抽成自由函数是因为 `new` 与 `adapt` 都要用它 —— 两处各写一遍迟早会
/// 漂移，而容量算错的症状（莫名溢出 / 缓冲囤不住）极难归因到具体那一处。
fn capacity_for(config: OpusConfig, target_ms: f64) -> usize {
    let frames_for_target = (target_ms / config.frame_ms).ceil();
    ((frames_for_target as usize) + 1).clamp(2, MAX_CAPACITY_FRAMES)
}

/// 音频抖动缓冲
///
/// # 音频缓冲和视频缓冲不是一回事
///
/// | 维度 | 视频 | 音频 |
/// | --- | --- | --- |
/// | 能否跳帧 | 能，掉一帧几乎无感 | **不能**，跳一帧就是可听见的咔哒声 |
/// | 目标深度 | 100-300ms（要等关键帧） | 20-40ms（帧小、带宽压力低） |
/// | 迟到处理 | 直接丢 | 窗口内还能补播，超出窗口才丢 |
/// | 乱序 | 基本无解（依赖参考帧） | 可以重排，相邻帧互相独立 |
///
/// 具体展开：
///
/// - **音频不能跳过帧**：视频丢一帧只是画面跳一下；音频丢一帧是波形断裂，
///   扬声器会发出「咔哒」声。所以本缓冲宁可多等，也不主动跳帧。
/// - **目标深度应该比视频小**：一帧音频才 20ms 的数据量（Opus 单声道约
///   40-80 字节），缓冲 40ms 也只有几百字节。视频缓冲要等关键帧，动辄上百毫秒。
/// - **迟到超过阈值只能丢**：播放时钟已经越过那一段，补播意味着要
///   重放过去的声音，听感上比丢掉更糟（会听起来像倒带）。
/// - **跳变超过 500ms 返回 `PushOutcome::Discontinuity`**：这不是网络抖动
///   能造成的量级，说明采集重启或切换了设备，上层必须重置视频侧的对齐。
///
/// # 时基约定（重要）
///
/// `pop_due` 的 `now_us` 与帧的 `pts_us` **必须是同一时基**。
/// 本缓冲不关心这个时基的零点在哪（可以是系统开机时间、可以是流开始的 0），
/// 只要求两者可比。
///
/// `target_ms` 在这里的职责是：
///
/// 1. 决定容量 —— 大约能囤多少帧（见 `capacity_frames`）
/// 2. 决定迟到窗口 —— 落后播放时钟超过 `target_ms` 的帧才判定为 `Late`
///
/// 它**不**自动把播放推迟 `target_ms`：真正的延迟由上层在调用 `pop_due`
/// 时体现（把播放时钟整体推后 `target_ms` 再传进来）。缓冲层不知道帧的
/// 到达时刻，硬要自己加延迟只会让时基变得难以推理。
///
/// # 自适应目标深度
///
/// `target_ms` 在构造时固定，但可以用 [`AudioJitterBuffer::adapt`] 按实测
/// 到达抖动调整。**不调 `adapt` 就永远是构造时的值** —— 老调用点的行为
/// 完全不变，这是刻意保留的向后兼容。
#[derive(Debug)]
pub struct AudioJitterBuffer {
    config: OpusConfig,
    target_ms: f64,
    target_us: u64,
    capacity: usize,
    /// 按 PTS 升序排列；容量上限只有几百，用 Vec 比堆更划算
    queue: Vec<AudioFrame>,
    /// 播放时钟（微秒）。第一次 `pop_due` 时确立基准
    playout_clock_us: Option<u64>,
    /// 最近一次入队的 PTS，用于跳变检测
    last_pts_us: Option<u64>,
    dropped_late: u64,
    dropped_overflow: u64,
    /// 最近一次入队帧的**到达时刻**（微秒）
    ///
    /// 与 `last_pts_us` 分开存，原因和视频侧 `JitterBuffer` 里
    /// `last_send_ts_us` 必须独立于 `last_arrival_us` 是同一个：
    /// 抖动要用 `(R_i - R_{i-1}) - (S_i - S_{i-1})`，两项必须来自
    /// **不同的两帧**。拿同一帧的两个值相减，抖动会被抵消成 0。
    last_arrival_us: Option<u64>,
    /// 抖动 EWMA（微秒）
    jitter_ewma_us: f64,
    /// 已累计的抖动样本数
    jitter_samples: u64,
    /// 上次 `adapt` 改动 `target_ms` 的时刻（驻留期判定用）
    last_adapt_us: Option<u64>,
    stats: AudioJitterStats,
}

impl AudioJitterBuffer {
    /// 创建抖动缓冲
    ///
    /// `target_ms` 是目标缓冲深度，建议 20-40ms。必须是有限非负数。
    pub fn new(config: OpusConfig, target_ms: f64) -> Result<Self, String> {
        config.validate()?;

        if !target_ms.is_finite() {
            return Err(format!("目标缓冲深度必须是有限数，收到 {target_ms}"));
        }
        if target_ms < 0.0 {
            return Err(format!("目标缓冲深度不能为负，收到 {target_ms}"));
        }

        let target_us = (target_ms * 1000.0).round() as u64;

        let capacity = capacity_for(config, target_ms);

        Ok(Self {
            config,
            target_ms,
            target_us,
            capacity,
            queue: Vec::with_capacity(capacity),
            playout_clock_us: None,
            last_pts_us: None,
            dropped_late: 0,
            dropped_overflow: 0,
            last_arrival_us: None,
            jitter_ewma_us: 0.0,
            jitter_samples: 0,
            last_adapt_us: None,
            stats: AudioJitterStats::default(),
        })
    }

    /// 推入一帧
    ///
    /// 判定顺序：跳变 → 迟到 → 入队 → 溢出。
    ///
    /// # 跳变
    ///
    /// 与最近一次入队的 PTS 相差超过 `DISCONTINUITY_THRESHOLD_US` 时，
    /// 说明时间线断了（采集重启 / 换设备 / 时间基重置）。此时缓冲会
    /// **自愈**：清空旧队列、丢弃播放时钟基准、把这一帧当作新起点入队，
    /// 然后返回 `PushOutcome::Discontinuity` 让上层去重置视频对齐。
    pub fn push(&mut self, frame: AudioFrame) -> PushOutcome {
        let arrived_at_us = frame.arrived_at_us;
        self.push_at(frame, arrived_at_us)
    }

    /// 推入一帧，并显式指定它的到达时刻（微秒）
    ///
    /// `arrived_at_us` 应当与帧自己的 `arrived_at_us` 一致；分开传是为了
    /// 让测试能构造确定的到达序列，而不必依赖真实时钟。
    ///
    /// 注意：本方法**不会**改写 `frame.arrived_at_us` —— 那个字段是发送端
    /// 填的「这一帧是什么时候到达的」，缓冲层擅自覆盖会破坏调用方自己的
    /// 时间记录（视频侧 `JitterBuffer::push_at` 会覆盖，那是因为它同一次
    /// 调用里就要用这个值算出队时刻，音频不需要）。
    pub fn push_at(&mut self, frame: AudioFrame, arrived_at_us: u64) -> PushOutcome {
        // 1) 跳变检测：先于迟到判断，因为跳变之后旧的播放时钟已经没有意义
        if let Some(last) = self.last_pts_us {
            if frame.pts_us.abs_diff(last) > DISCONTINUITY_THRESHOLD_US {
                self.queue.clear();
                self.playout_clock_us = None;
                self.last_pts_us = None;
                self.enqueue(frame);
                // 时间线断了：上一个到达时刻不再可比，样本必须作废。
                // 留着会让下一帧算出一个「跨越断点」的假抖动。
                self.last_arrival_us = None;
                return PushOutcome::Discontinuity;
            }
        }

        // 2) 迟到检测：播放时钟尚未确立时不判定 —— 那时还在预缓冲阶段，
        //    帧序本来就是乱的，过早判迟到会把乱序重排功能废掉
        if let Some(clock) = self.playout_clock_us {
            if frame.pts_us.saturating_add(self.target_us) <= clock {
                self.dropped_late += 1;
                // 迟到帧**不喂抖动样本**：它的到达时刻反映的是重传/绕路，
                // 不是链路抖动。而且它的 PTS 是旧的，和上一帧的间隔没有意义。
                return PushOutcome::Late;
            }
        }

        // 3) 抖动量测。必须在入队之前做 —— `enqueue` 会刷新 `last_pts_us`，
        //    之后就拿不到「上一帧」的 PTS 了
        self.observe_jitter(&frame, arrived_at_us);

        // 4) 有序入队
        self.enqueue(frame);

        // 4) 溢出：丢**最旧**的帧而不是最新的。
        //    丢最新的会让延迟越积越大（帧一直在队尾排队），
        //    丢最旧的则把延迟钳在容量上限，代价是牺牲一段老音频
        if self.queue.len() > self.capacity {
            self.queue.remove(0);
            self.dropped_overflow += 1;
            return PushOutcome::Overflow;
        }

        PushOutcome::Ok
    }

    /// 用 RFC 3550 的 `|D(i-1,i)|` 更新抖动 EWMA
    ///
    /// # 公式与「必须来自两帧」的坑
    ///
    /// ```text
    /// D(i-1,i) = (R_i - R_{i-1}) - (S_i - S_{i-1})
    /// 抖动样本  = |D(i-1,i)|
    /// ```
    ///
    /// `R` 是到达时刻、`S` 是发送时刻。**两项必须取自不同的两帧**：
    /// 如果拿当前帧的 `S_i` 去减上一帧的 `R_{i-1}`，`S_i` 会在减式里
    /// 自己抵消掉，剩下的是「到达间隔」—— 那等于「帧间隔 + 抖动」，
    /// 20ms 帧下恒在 20ms 附近，比真实抖动大一个数量级。
    /// 视频侧 `JitterBuffer` 就踩过这个坑（见那边的注释）。
    ///
    /// # 为什么用 `arrived_at_us` 而不是 PTS 代理
    ///
    /// 只用 PTS 间隔算（`|(pts_i - pts_{i-1}) - frame_duration_us|`）看着
    /// 也能跑，但 `AudioPacer` 产出的 PTS 是**严格等距**的，这个式子
    /// 恒等于 0 —— 它对网络抖动完全失明，`adapt` 会永远把目标压到下限。
    /// 所以必须让帧带上到达时刻。
    fn observe_jitter(&mut self, frame: &AudioFrame, arrived_at_us: u64) {
        // 0 是「调用方没填」的哨兵值（见 AudioFrame::arrived_at_us）。
        // 拿它当真实时刻会算出 |D| = 帧时长（到达间隔恒为 0、PTS 间隔却是
        // 20ms），凭空造出一个 20ms 的抖动，把目标深度顶到 40ms。
        // 直接跳过并作废上一个到达时刻，老调用点的行为就与没有本功能时一致。
        if arrived_at_us == 0 {
            self.last_arrival_us = None;
            return;
        }

        if let (Some(last_arrival), Some(last_pts)) = (self.last_arrival_us, self.last_pts_us) {
            let d_arrive = arrived_at_us as i128 - last_arrival as i128;
            let d_pts = frame.pts_us as i128 - last_pts as i128;
            let delta = (d_arrive - d_pts).unsigned_abs() as f64;

            // 1 秒以上的偏差不是抖动（时基重置 / 暂停 / 采集重启），丢弃样本
            if delta < JITTER_SAMPLE_MAX_US {
                self.jitter_ewma_us = if self.jitter_samples == 0 {
                    delta
                } else {
                    self.jitter_ewma_us * (1.0 - JITTER_EWMA_ALPHA) + delta * JITTER_EWMA_ALPHA
                };
                self.jitter_samples = self.jitter_samples.saturating_add(1);
                self.stats.avg_jitter_ms = self.jitter_ewma_us / 1000.0;
            }
        }
        self.last_arrival_us = Some(arrived_at_us);
    }

    /// 按 PTS 插入到正确位置（稳定插入：PTS 相同时后来的排在后面）
    fn enqueue(&mut self, frame: AudioFrame) {
        let pts = frame.pts_us;
        // partition_point 要求谓词在有序序列上单调，queue 始终按 PTS 升序
        let pos = self.queue.partition_point(|f| f.pts_us <= pts);
        self.queue.insert(pos, frame);
        self.last_pts_us = Some(pts);
    }

    /// 取出到期可播放的帧
    ///
    /// `now_us` 是播放时钟（与 PTS 同一时基）。返回 PTS 不晚于该时刻的
    /// 最早一帧；没有到期帧时返回 `None`。
    ///
    /// 每次只出一帧 —— 由调用方循环取到 `None` 为止，这样上层可以在
    /// 每帧之间插入 AvSync 裁决，而不会被一次性灌一堆帧。
    ///
    /// 播放时钟只前进不后退：`now_us` 比当前时钟小不会让时钟倒流
    /// （网络包乱序到达时可能出现这种调用）。
    pub fn pop_due(&mut self, now_us: u64) -> Option<AudioFrame> {
        self.playout_clock_us = Some(match self.playout_clock_us {
            Some(c) => c.max(now_us),
            None => now_us,
        });

        let clock = self.playout_clock_us.unwrap_or(now_us);
        let idx = self.queue.iter().position(|f| f.pts_us <= clock)?;
        Some(self.queue.remove(idx))
    }

    /// 当前缓冲深度（毫秒）
    ///
    /// 定义为**排队中所有帧的时长之和**，而不是「最晚 PTS − 播放时钟」。
    ///
    /// 为什么不用 PTS 跨度：丢包会在 PTS 上留下空洞，跨度会把空洞
    /// 算成缓冲，让上层误以为还有存货，于是放慢推流节奏 —— 实际上
    /// 那段时间根本没有音频数据。按时长求和得到的是「真能播出多少声音」。
    pub fn depth_ms(&self) -> f64 {
        let total_us: u64 = self.queue.iter().map(|f| f.duration_us).sum();
        total_us as f64 / 1000.0
    }

    /// 因迟到而丢弃的帧数
    pub fn dropped_late(&self) -> u64 {
        self.dropped_late
    }

    /// 因溢出而丢弃的帧数
    pub fn dropped_overflow(&self) -> u64 {
        self.dropped_overflow
    }

    /// 当前排队的帧数
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    /// 队列是否为空
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// 容量（帧）
    ///
    /// 由 `target_ms / frame_ms` 向上取整再加 1 帧余量，下限 2、上限 512。
    pub fn capacity_frames(&self) -> usize {
        self.capacity
    }

    /// 目标缓冲深度（毫秒）
    pub fn target_ms(&self) -> f64 {
        self.target_ms
    }

    /// 自适应统计（累积值）
    ///
    /// 字段命名对齐视频侧 `sidelens_core::jitter::JitterStats`，两边的
    /// 诊断日志可以直接并排看。
    pub fn stats(&self) -> AudioJitterStats {
        self.stats
    }

    /// 当前抖动估计（毫秒）
    ///
    /// 就是 `adapt` 唯一看的那个 EWMA。单独开一个方法是为了让上层
    /// 打日志时不必把整个 stats 拷出来。
    pub fn jitter_ms(&self) -> f64 {
        self.jitter_ewma_us / 1000.0
    }

    /// 已累计的抖动样本数
    pub fn jitter_samples(&self) -> u64 {
        self.jitter_samples
    }

    /// 按实测到达抖动自适应调整目标缓冲深度
    ///
    /// 返回 `Some((旧值, 新值))` 表示发生了调整，`None` 表示没动
    /// （样本不足、驻留期内、或算出的新值与当前值相同）。
    ///
    /// # 为什么需要它
    ///
    /// `target_ms` 决定两件事：能囤多少帧（`capacity_frames`）和迟到窗口
    /// （落后播放时钟超过 `target_ms` 的帧判 `Late`）。它是个常量时，
    /// 链路上的两个方向只有一个能是对的：
    ///
    /// - 链路好时，40ms 里 40ms 全是**纯等待**，直接进端到端延迟
    /// - 链路差时，40ms 又装不下抖动，帧被顶出迟到窗口
    ///
    /// 视频侧 `sidelens_core::jitter::JitterBuffer::adapt` 早就做了这件事，
    /// 音频侧一直没有 —— 这是补上那一半。
    ///
    /// # 为什么是「抖动 × 系数」而不是固定步进
    ///
    /// 固定步进收敛太慢：从 40 降到 20 按 2ms 一步要 10 次、跨 5 秒驻留期，
    /// 链路状态早就变了。按抖动算能直接跳到「刚好够用」的位置。
    ///
    /// # 四道闸门（缺一个就会震荡）
    ///
    /// | 闸门 | 值 | 作用 |
    /// | --- | --- | --- |
    /// | 下限 `AUDIO_MIN_TARGET_MS` | 20ms | 音频不能像视频那样降到 10ms，20ms 帧长决定了最小可用深度 |
    /// | 单次降幅 `AUDIO_ADAPT_MAX_STEP_DOWN_MS` | 10ms | 防止一次砍到底 |
    /// | 驻留期 `AUDIO_ADAPT_HOLD_US` | 500ms | 防止跟随抖动 EWMA 震荡 |
    /// | 安全系数 `AUDIO_JITTER_SAFETY_FACTOR` | 2.0 | 抖动 × 系数 = 目标 |
    ///
    /// # 为什么「降」比「升」保守得多
    ///
    /// 降错了的代价是**丢帧**（一段可听见的咔哒声），升错了的代价只是
    /// 多一点延迟（20ms 量级，人耳无感）。所以降有步长上限和驻留期，
    /// 升则立即全额生效 —— 抖动一变大就马上加缓冲，晚一步就已经在丢帧了。
    ///
    /// # 改完之后队列里的帧怎么办
    ///
    /// **一帧都不丢**。已入队的帧本来就是有效的，目标深度只影响
    /// 「之后还能囤多少」和「迟到窗口多宽」。调小容量时队列可能暂时
    /// 比容量长，下一次 `push` 触发溢出判定时才会裁到新容量 —— 那时
    /// 被丢的是最旧的一帧，语义与正常溢出完全一致。
    pub fn adapt(&mut self, now_us: u64) -> Option<(f64, f64)> {
        // 样本闸门：少于 JITTER_MIN_SAMPLES 个 |D| 样本时，EWMA 不可信。
        // 视频侧用 `received < 2` 表达同一件事，但音频的样本是按
        // 「相邻两帧」算的，帧数够了不等于样本够了（迟到帧、跳变帧、
        // 没填 arrived_at_us 的帧都不产生样本）。
        //
        // # 为什么这里**不**像视频侧那样再要求 `jitter > 0`
        //
        // 视频侧那条 `jitter_ewma <= 0.0` 的早退是为了避免「零抖动时
        // 反复写同一个值」。但音频要的恰恰是**零抖动时降到下限**：
        // 链路完全干净时 40ms 的缓冲就是 40ms 的纯延迟，没有任何收益。
        // 要求抖动为正会让干净链路永远停在构造时的深度，自适应等于白做。
        if self.jitter_samples < JITTER_MIN_SAMPLES {
            return None;
        }

        // 驻留期闸门
        if let Some(last) = self.last_adapt_us {
            if now_us.saturating_sub(last) < AUDIO_ADAPT_HOLD_US {
                return None;
            }
        }

        let jitter_ms = self.jitter_ewma_us / 1000.0;
        // 抖动 × 系数，并钳到 [下限, 上限]
        let want = (jitter_ms * AUDIO_JITTER_SAFETY_FACTOR)
            .clamp(AUDIO_MIN_TARGET_MS, AUDIO_MAX_TARGET_MS);

        let cur = self.target_ms;
        let diff = want - cur;

        let next = if diff > 0.0 {
            // 升：立即全额生效。抖动变大时每一帧都在丢，等不起。
            want
        } else if diff < 0.0 {
            // 降：单次最多 AUDIO_ADAPT_MAX_STEP_DOWN_MS
            let step = (-diff).min(AUDIO_ADAPT_MAX_STEP_DOWN_MS);
            // 降幅小于阈值就别动了，否则会为了浮点噪声反复写同一个值
            if step < AUDIO_ADAPT_MIN_DELTA_MS {
                return None;
            }
            cur - step
        } else {
            return None;
        };

        // 浮点比较留 1e-9 容差：want 与 cur 在数学上相等时不该触发调整
        if (next - cur).abs() < 1e-9 {
            return None;
        }

        self.set_target_ms(next);
        self.last_adapt_us = Some(now_us);
        self.stats.adaptations = self.stats.adaptations.saturating_add(1);
        if next > cur {
            self.stats.adapt_ups = self.stats.adapt_ups.saturating_add(1);
        } else {
            self.stats.adapt_downs = self.stats.adapt_downs.saturating_add(1);
        }
        Some((cur, next))
    }

    /// 写入新的目标深度并同步 `target_us` / `capacity`
    ///
    /// 三者的同步是必须的：`capacity_frames` 读 `capacity`，迟到判定读
    /// `target_us`。只改 `target_ms` 会让「报告的目标深度」和「实际的
    /// 迟到窗口」对不上，而这种不一致在日志里完全看不出来。
    fn set_target_ms(&mut self, target_ms: f64) {
        self.target_ms = target_ms;
        self.target_us = (target_ms * 1000.0).round() as u64;
        self.capacity = capacity_for(self.config, target_ms);
        // Vec 的容量只是提示，缩小时不动已分配的堆内存 —— 已入队的帧
        // 必须原样留着，它们仍然有效
        if self.queue.capacity() < self.capacity {
            self.queue.reserve(self.capacity - self.queue.capacity());
        }
    }

    /// 当前配置
    pub fn config(&self) -> OpusConfig {
        self.config
    }

    /// 清空队列与播放时钟基准
    ///
    /// **不重置丢帧计数** —— 计数器是诊断指标，跨清空累计才有意义，
    /// 清零会让「到底丢了多严重」这个信息丢失。
    ///
    /// 但**会重置抖动估计**：清空意味着时间线作废，跨断点算出来的
    /// `|D|` 是假样本（那一段是暂停或重启，不是网络抖动）。
    /// `adapt` 的累计计数同样保留，理由与丢帧计数一致。
    ///
    /// 注意 `target_ms` **不会**被重置回构造时的值 —— 它是自适应算法
    /// 的收敛结果，清空一次队列不代表链路变好了。
    pub fn clear(&mut self) {
        self.queue.clear();
        self.playout_clock_us = None;
        self.last_pts_us = None;
        self.last_arrival_us = None;
        self.last_adapt_us = None;
        self.jitter_ewma_us = 0.0;
        self.jitter_samples = 0;
        self.stats.avg_jitter_ms = 0.0;
    }
}
