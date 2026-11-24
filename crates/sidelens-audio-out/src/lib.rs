//! SideLens 音频输出层：把 Opus 解出来的 PCM 送进声卡
//!
//! # 为什么单独一个 crate
//!
//! 与 `sidelens-render` 同构：解码出来的 PCM 要「发出去」，
//! 但**发到哪里**是平台相关的，而「格式对不对」是纯逻辑。
//!
//! | 层 | 模块 | 编译条件 | 内容 |
//! | --- | --- | --- | --- |
//! | 跨平台 | [`mod@format`] | 无条件 | `PcmFormat` / `AudioFormatError` / 校验 |
//! | Windows | [`wasapi`] | `cfg(windows)` | WASAPI 共享模式 + 无设备 headless |
//!
//! 非 Windows 上本 crate 依然能编译、能跑格式测试，只是没有真声卡输出。
//!
//! # 为什么需要这个 crate
//!
//! 在此之前 `apps/sidelensd/src/udp_server.rs` 用 `OpusDecoder` 解出 PCM，
//! 然后**只把它加进计数器**：PCM 从未到过声卡。这和视频侧「解码但不显示」
//! 是同一类缺口 —— 链路的每一环都通了，唯独最后一米没接。
//!
//! # 快速上手
//!
//! ```no_run
//! use sidelens_audio_out::{AudioSink, PcmFormat};
//!
//! fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     // 失败就降级为「只解码不播放」，不要中断接收
//!     let mut sink = AudioSink::new(PcmFormat::default())
//!         .unwrap_or_else(|_| AudioSink::headless(PcmFormat::default()));
//!
//!     let pcm = vec![0i16; 960]; // 20ms @ 48kHz 单声道
//!     let written = sink.write(&pcm)?;
//!     assert_eq!(written, 960);
//!     Ok(())
//! }
//! ```
//!
//! # write 的语义
//!
//! [`AudioSink::write`] **绝不阻塞**：调用方在 1ms tick 里跑，
//! 一次 `write` 卡住就是整条链路卡住。
//!
//! | 返回 | 含义 | 调用方该做什么 |
//! | --- | --- | --- |
//! | `Ok(n)`，n == pcm.len() | 全写进去了 | 继续 |
//! | `Ok(n)`，0 < n < pcm.len() | 只写进去一部分 | 丢掉剩下的一截（或下一 tick 重试） |
//! | `Ok(0)` | 缓冲满，一个都没进 | 丢掉这一帧 |
//!
//! 丢一帧 20ms 的音频比阻塞 20ms 好得多 —— 前者是一次轻微的咔哒，
//! 后者会让视频一起停摆。
//!
//! # headless 模式
//!
//! [`AudioSink::headless`] 不碰任何平台 API，`write` 只累计计数。
//! CI、无声卡容器、以及**用户拒绝播放**的场景都走这条路径。
//! 它和真实 sink 共用同一个类型，调用方不用分叉。

#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_debug_implementations)]

pub mod format;

#[cfg(windows)]
pub mod wasapi;

pub use format::{
    AudioFormatError, PcmFormat, MAX_CHANNELS, MAX_SAMPLE_RATE, MIN_CHANNELS, MIN_SAMPLE_RATE,
};

#[cfg(windows)]
pub use wasapi::{default_device_id, WasapiSink};

/// 音频输出层的错误
#[derive(Debug)]
pub enum SinkError {
    /// 格式不合法（采样率 / 声道数越界）
    Format(AudioFormatError),
    /// 系统里没有可用的播放设备
    NoDevice(String),
    /// 设备不接受我们的格式
    UnsupportedFormat(String),
    /// WASAPI / COM 调用失败
    Wasapi(String),
}

impl std::fmt::Display for SinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SinkError::Format(e) => write!(f, "PCM 格式不合法：{e}"),
            SinkError::NoDevice(m) => write!(f, "没有音频输出设备：{m}"),
            SinkError::UnsupportedFormat(m) => write!(f, "设备不支持该格式：{m}"),
            SinkError::Wasapi(m) => write!(f, "WASAPI 调用失败：{m}"),
        }
    }
}

impl std::error::Error for SinkError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SinkError::Format(e) => Some(e),
            _ => None,
        }
    }
}

impl From<AudioFormatError> for SinkError {
    fn from(e: AudioFormatError) -> Self {
        SinkError::Format(e)
    }
}

/// 输出后端
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SinkMode {
    /// 真实声卡（WASAPI 共享模式）
    Device,
    /// 不碰平台 API，只累计计数
    Headless,
}

impl SinkMode {
    /// 人类可读的名字
    pub fn as_str(self) -> &'static str {
        match self {
            SinkMode::Device => "wasapi-shared",
            SinkMode::Headless => "headless",
        }
    }
}

/// 内部实现
#[derive(Debug)]
enum Backend {
    #[cfg(windows)]
    Wasapi(WasapiSink),
    Headless {
        /// 累计的输入采样数
        written: u64,
        /// 累计丢弃的输入采样数
        dropped: u64,
        /// 模拟的排队帧数（模拟一个 20ms 的缓冲，让 latency_ms 有非零值）
        queued_frames: u32,
        /// 最近一次 write 的时间戳，用来让 queued_frames 随时间衰减
        last_write: Option<std::time::Instant>,
    },
}

/// 音频输出 sink
///
/// 一个类型包住两种模式（真声卡 / headless），调用方不用为「有没有设备」分叉。
///
/// # 不是 Send
///
/// WASAPI 的 COM 对象不是 `Send`，所以这个类型也不能跨线程移动。
/// 详见 [`wasapi`] 模块文档。
#[derive(Debug)]
pub struct AudioSink {
    mode: SinkMode,
    format: PcmFormat,
    backend: Backend,
    /// 累计 write 调用次数
    write_calls: u64,
    /// 累计因缓冲满返回 Ok(0) 的次数
    write_stalls: u64,
    /// 累计 write 耗时（纳秒）
    write_ns: u128,
    /// 最近一次 write 耗时（纳秒）
    last_write_ns: u128,
}

impl AudioSink {
    /// 创建真实声卡输出
    ///
    /// 失败时返回 `Err`，调用方应降级为 [`AudioSink::headless`]。
    /// **不要**把这里的错误当成致命错误 —— 没有声卡的机器照样该能看画面。
    #[cfg(windows)]
    pub fn new(format: PcmFormat) -> Result<Self, SinkError> {
        let sink = WasapiSink::open(format)?;
        Ok(Self {
            mode: SinkMode::Device,
            format,
            backend: Backend::Wasapi(sink),
            write_calls: 0,
            write_stalls: 0,
            write_ns: 0,
            last_write_ns: 0,
        })
    }

    /// 创建真实声卡输出（非 Windows：恒失败）
    ///
    /// 非 Windows 上没有 WASAPI。保留这个签名是为了让调用方代码不分叉，
    /// 只是它一定返回 `Err(NoDevice)`，调用方自然降级到 headless。
    #[cfg(not(windows))]
    pub fn new(format: PcmFormat) -> Result<Self, SinkError> {
        format.validate()?;
        Err(SinkError::NoDevice(
            "本平台没有 WASAPI 实现（只有 Windows 有）".into(),
        ))
    }

    /// 创建一个 headless sink：不碰任何平台 API，`write` 只累计计数
    ///
    /// 用于 CI / 无音频设备环境 / 用户明确关掉了播放。
    pub fn headless(format: PcmFormat) -> Self {
        Self {
            mode: SinkMode::Headless,
            format,
            backend: Backend::Headless {
                written: 0,
                dropped: 0,
                queued_frames: 0,
                last_write: None,
            },
            write_calls: 0,
            write_stalls: 0,
            write_ns: 0,
            last_write_ns: 0,
        }
    }

    /// 当前模式
    pub fn mode(&self) -> SinkMode {
        self.mode
    }

    /// 调用方请求的格式
    pub fn format(&self) -> PcmFormat {
        self.format
    }

    /// 真实设备使用的格式（headless 下等于请求的格式）
    #[cfg(windows)]
    pub fn device_format(&self) -> PcmFormat {
        match &self.backend {
            Backend::Wasapi(s) => {
                let (rate, ch) = s.device_format();
                PcmFormat::new(rate, ch.min(u16::MAX as usize) as u16)
            }
            Backend::Headless { .. } => self.format,
        }
    }

    /// 写入一帧 PCM（i16，单声道或交错）
    ///
    /// **非阻塞**：不能在音频线程里等 buffer。
    /// 若缓冲满，返回 `Ok(0)` 表示「这次没写进去」，调用方自己决定丢还是等。
    ///
    /// 返回的是**输入采样数**（i16 个数），不是帧数。
    pub fn write(&mut self, pcm: &[i16]) -> Result<usize, SinkError> {
        let t0 = std::time::Instant::now();
        let result = self.write_inner(pcm);
        let ns = t0.elapsed().as_nanos();
        self.last_write_ns = ns;
        self.write_ns += ns;
        self.write_calls += 1;
        if let Ok(0) = result {
            self.write_stalls += 1;
        }
        result
    }

    fn write_inner(&mut self, pcm: &[i16]) -> Result<usize, SinkError> {
        match &mut self.backend {
            #[cfg(windows)]
            Backend::Wasapi(s) => s.write(pcm),
            Backend::Headless {
                written,
                dropped,
                queued_frames,
                last_write,
            } => {
                // 模拟真实 sink 的行为：缓冲区（20ms）满就返回 Ok(0)。
                // 这样 headless 下的丢弃计数不是恒 0，测试能覆盖到。
                let capacity = self.format.frames_for_ms(20.0).max(1) as u32;
                let frame_samples = self.format.frame_samples().max(1);
                let frames = (pcm.len() / frame_samples) as u32;

                // 先按「距上次 write 的真实时间」把队列排空一点。
                //
                // 这里**不能**用 PcmFormat::frames_for_ms：那个函数为了
                // 避免除零会把 0ms 抬成 1 帧，于是每次 write 都凭空排空
                // 1 帧，队列永远到不了容量上限，Ok(0) 也就永远测不出来。
                // 所以直接按 floor 算，不足 1 帧就是 0 帧。
                let now = std::time::Instant::now();
                if let Some(prev) = *last_write {
                    let elapsed_ms = now.duration_since(prev).as_secs_f64() * 1000.0;
                    let drained = (elapsed_ms * self.format.sample_rate as f64 / 1000.0)
                        .floor()
                        .max(0.0) as u32;
                    *queued_frames = queued_frames.saturating_sub(drained);
                }

                let free = capacity.saturating_sub(*queued_frames);
                if free == 0 {
                    *dropped += pcm.len() as u64;
                    *last_write = Some(now);
                    return Ok(0);
                }
                let take_frames = frames.min(free);
                let consumed = (take_frames as usize) * frame_samples;
                *written += consumed as u64;
                *dropped += (pcm.len() - consumed) as u64;
                *queued_frames = (*queued_frames + take_frames).min(capacity);
                *last_write = Some(now);
                Ok(consumed)
            }
        }
    }

    /// 已写入的总采样数（诊断用）
    pub fn samples_written(&self) -> u64 {
        match &self.backend {
            #[cfg(windows)]
            Backend::Wasapi(s) => s.samples_written(),
            Backend::Headless { written, .. } => *written,
        }
    }

    /// 因缓冲满而丢弃的采样数
    pub fn samples_dropped(&self) -> u64 {
        match &self.backend {
            #[cfg(windows)]
            Backend::Wasapi(s) => s.samples_dropped(),
            Backend::Headless { dropped, .. } => *dropped,
        }
    }

    /// 估计的输出延迟（毫秒）
    ///
    /// 真设备下 = max(流固有延迟, 设备周期) + 缓冲区排队时长。
    ///
    /// 实测（48kHz/2ch 设备，10ms 周期，22ms 缓冲）：
    /// 20ms 帧 → **31.7ms**，10ms 帧 → 27.5ms，2.5ms 帧 → **18.3ms**。
    /// 形状是「设备周期 + 帧长」，帧长超过缓冲容量后封顶在 32ms。
    /// 那部分延迟来自「调用方一次给多少数据」，不是 WASAPI 的开销。
    ///
    /// headless 下按「模拟队列里的帧数 / 采样率」报，量级一致，方便日志不分叉。
    pub fn latency_ms(&self) -> f64 {
        match &self.backend {
            #[cfg(windows)]
            Backend::Wasapi(s) => s.latency_ms(),
            Backend::Headless { queued_frames, .. } => {
                // headless 没有真实延迟，按「排队帧数 / 采样率」报，
                // 让调用方的日志格式不用分叉。
                *queued_frames as f64 * 1000.0 / self.format.sample_rate.max(1) as f64
            }
        }
    }

    /// 流的固有延迟（毫秒，WASAPI ~GetStreamLatency~ 原值）
    ///
    /// headless 下恒为 0。与 [~AudioSink::latency_ms~] 的差就是「缓冲区里
    /// 还排着多少毫秒的数据」。
    pub fn stream_latency_ms(&self) -> f64 {
        match &self.backend {
            #[cfg(windows)]
            Backend::Wasapi(s) => s.stream_latency_ms(),
            Backend::Headless { .. } => 0.0,
        }
    }

    /// 设备已经播放到的帧数（WASAPI `IAudioClock::GetPosition`）
    ///
    /// 这是「样本真的被设备消费了」的硬证据：它由驱动自己维护，
    /// 不受我们计数逻辑影响。headless 或拿不到时钟时返回 `None`。
    pub fn device_position_frames(&self) -> Option<u64> {
        match &self.backend {
            #[cfg(windows)]
            Backend::Wasapi(s) => s.device_position_frames(),
            Backend::Headless { .. } => None,
        }
    }

    /// 设备周期 ~(默认, 最小)~ 毫秒；headless 下是 (0, 0)
    ///
    /// 默认周期 = 引擎多久来取一次数 = 共享模式延迟的物理下限。
    pub fn device_period_ms(&self) -> (f64, f64) {
        match &self.backend {
            #[cfg(windows)]
            Backend::Wasapi(s) => s.device_period_ms(),
            Backend::Headless { .. } => (0.0, 0.0),
        }
    }

    /// 引擎缓冲区总帧数（headless 下是模拟的 20ms 容量）
    pub fn buffer_frames(&self) -> u32 {
        match &self.backend {
            #[cfg(windows)]
            Backend::Wasapi(s) => s.buffer_frames(),
            Backend::Headless { .. } => self.format.frames_for_ms(20.0) as u32,
        }
    }

    /// 缓冲区里当前排队的帧数（最近一次 write 之后）
    pub fn padding_frames(&self) -> u32 {
        match &self.backend {
            #[cfg(windows)]
            Backend::Wasapi(s) => s.padding_frames(),
            Backend::Headless { queued_frames, .. } => *queued_frames,
        }
    }

    /// 累计 write 调用次数
    pub fn write_calls(&self) -> u64 {
        self.write_calls
    }

    /// 累计因缓冲满返回 `Ok(0)` 的次数
    pub fn write_stalls(&self) -> u64 {
        self.write_stalls
    }

    /// 累计 write 耗时（纳秒）
    pub fn write_ns(&self) -> u128 {
        self.write_ns
    }

    /// 最近一次 write 耗时（纳秒）
    pub fn last_write_ns(&self) -> u128 {
        self.last_write_ns
    }

    /// 平均每次 write 耗时（纳秒）；没有调用时返回 0
    pub fn avg_write_ns(&self) -> u128 {
        if self.write_calls == 0 {
            0
        } else {
            self.write_ns / self.write_calls as u128
        }
    }

    /// 已写入的总时长（毫秒）—— 用 `samples_written` 折算
    pub fn played_ms(&self) -> f64 {
        let frames = self.samples_written() / self.format.frame_samples().max(1) as u64;
        frames as f64 * 1000.0 / self.format.sample_rate.max(1) as f64
    }
}
