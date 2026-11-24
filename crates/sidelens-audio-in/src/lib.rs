//! SideLens 音频采集层：麦克风 / 系统回环 PCM 采集
//!
//! # 为什么单独一个 crate
//!
//! 与 [`sidelens-audio-out`] 同构：PCM 要「从设备拿出来」，
//! 但**从哪个设备拿**是平台相关的，而「格式对不对」是纯逻辑。
//!
//! | 层 | 模块 | 编译条件 | 内容 |
//! | --- | --- | --- | --- |
//! | 跨平台 | [`mod@format`] | 无条件 | `PcmFormat` / `DeviceFormat` / 转换与降混 |
//! | Windows | [`wasapi`] | `cfg(windows)` | WASAPI 共享模式采集 + loopback |
//!
//! 非 Windows 上本 crate 依然能编译、能跑全部格式与转换测试，
//! 只是没有真设备采集（[`AudioCapture::new`] 恒返回 `Err`，调用方降级 headless）。
//!
//! # 为什么需要这个 crate
//!
//! 在此之前 `apps/sidelenssend/src/main.rs` 的 `synth_audio_i16()` 合成一段
//! 440Hz 正弦当音频发出去 —— 那是**假音频**：链路的每一环都通了，
//! 唯独「声音从哪来」是编的。接收端能听到一声稳定的嘟 —— 或者什么都听不到，
//! 你分不清是编解码坏了、网络坏了、还是扬声器坏了。
//!
//! 本 crate 让真实采集成为可能，并且把「共享电脑音频」（loopback）
//! 这条路走通：对方电脑上放的音乐、视频、会议声音，可以原样送到另一台机器。
//!
//! # 三种模式
//!
//! | 模式 | 数据源 | 用途 |
//! | --- | --- | --- |
//! | [`CaptureMode::Microphone`] | 默认采集端点（麦克风 / 声卡输入） | 语音通话 |
//! | [`CaptureMode::Loopback`] | 默认**渲染**端点的镜像 | 共享系统声音 |
//! | [`CaptureMode::Headless`] | 无，只产静音 | CI / 无声卡 / 用户关掉了采集 |
//!
//! # 快速上手
//!
//! ```no_run
//! use sidelens_audio_in::{AudioCapture, PcmFormat};
//!
//! fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     // 失败就降级为「只发静音」，不要中断整条发送链路
//!     let mut cap = AudioCapture::new(PcmFormat::default())
//!         .unwrap_or_else(|_| AudioCapture::headless(PcmFormat::default()));
//!
//!     let mut pcm = Vec::with_capacity(4096);
//!     // 非阻塞：没数据就是 0，绝不能在这里等
//!     let n = cap.read(&mut pcm)?;
//!     println!("本次拿到 {n} 采样，设备格式 {:?}", cap.device_format());
//!     Ok(())
//! }
//! ```
//!
//! # read 的语义
//!
//! [`AudioCapture::read`] **绝不阻塞**：调用方在 1ms tick 里跑，
//! 一次 `read` 卡住就是整条链路卡住。
//!
//! | 返回 | 含义 | 调用方该做什么 |
//! | --- | --- | --- |
//! | `Ok(n)`，n > 0 | 拿到了 n 个采样（已转成 i16 单/多声道） | 送进 Opus 编码器 |
//! | `Ok(0)`，麦克风 | 这一 tick 没有新数据 | 跳过，下一 tick 再来 |
//! | `Ok(0)`，**loopback** | **系统当前没有声音在播放** | 跳过 —— 这是正常状态，不是错误 |
//! | `Err(..)` | 设备真的出错了 | 降级 headless 或重开设备 |
//!
//! loopback 的 `Ok(0)` 是最容易误判的一条：WASAPI 的 loopback
//! 在没有声音播放时**根本不推进**，它不是「送静音」而是「没有数据」。
//! 把它当错误会导致「静音时疯狂重连设备」。
//!
//! # 关于格式转换
//!
//! 设备混音格式（[`DeviceFormat`]）通常是 48kHz / 2ch / f32，
//! 而 Opus 要 48kHz / 1ch / i16。转换发生在 [`mod@format`] 里，
//! 跨平台可测：
//!
//! - f32 → i16 用**饱和**转换，越界样本压平成满幅而不是回绕成爆音
//! - 多声道 → 单声道用**平均**而不是取第一个声道（否则硬声像内容会消失）
//! - 采样率不一致直接报 [`CaptureError::RateMismatch`]，不做隐式重采样
//!
//! # 线程约束
//!
//! WASAPI 的 COM 对象不是 `Send`，所以 [`AudioCapture`] 也 `!Send`：
//! 必须在创建它的线程上使用。详见 [`wasapi`] 模块文档。

#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_debug_implementations)]

pub mod format;

#[cfg(windows)]
pub mod wasapi;

pub use format::{
    convert_frames, downmix_f32_to_mono, downmix_i16_to_mono, f32_to_i16, formats_compatible,
    i16_to_f32, plan_for, push_silence, AudioFormatError, DeviceFormat, DeviceSamples, MixPlan,
    PcmFormat, SampleKind, MAX_CHANNELS, MAX_SAMPLE_RATE, MIN_CHANNELS, MIN_SAMPLE_RATE,
};

#[cfg(windows)]
pub use wasapi::{default_capture_device_id, default_render_device_id, WasapiCapture};

use std::time::Instant;

/// 采集层的错误
#[derive(Debug)]
pub enum CaptureError {
    /// 格式不合法（采样率 / 声道数越界）
    Format(AudioFormatError),
    /// 系统里没有可用的采集设备
    NoDevice(String),
    /// 设备不接受我们的格式 / 位深
    UnsupportedFormat(String),
    /// 设备采样率与请求采样率不一致（本 crate 不做重采样）
    RateMismatch {
        /// 设备混音格式的采样率
        device_rate: u32,
        /// 调用方请求的采样率
        requested_rate: u32,
    },
    /// WASAPI / COM 调用失败
    Wasapi(String),
}

impl std::fmt::Display for CaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CaptureError::Format(e) => write!(f, "PCM 格式不合法：{e}"),
            CaptureError::NoDevice(m) => write!(f, "没有音频采集设备：{m}"),
            CaptureError::UnsupportedFormat(m) => write!(f, "设备不支持该格式：{m}"),
            CaptureError::RateMismatch {
                device_rate,
                requested_rate,
            } => write!(
                f,
                "采样率不一致：设备是 {device_rate}Hz，请求的是 {requested_rate}Hz（本 crate 不做重采样）"
            ),
            CaptureError::Wasapi(m) => write!(f, "WASAPI 调用失败：{m}"),
        }
    }
}

impl std::error::Error for CaptureError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            CaptureError::Format(e) => Some(e),
            _ => None,
        }
    }
}

impl From<AudioFormatError> for CaptureError {
    fn from(e: AudioFormatError) -> Self {
        CaptureError::Format(e)
    }
}

/// 采集模式
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureMode {
    /// 真实输入设备（麦克风 / 声卡 line-in）
    Microphone,
    /// 采集系统正在播放的声音（WASAPI loopback）
    Loopback,
    /// 只产生静音，用于 CI / 无声卡环境
    Headless,
}

impl CaptureMode {
    /// 人类可读的名字
    pub fn as_str(self) -> &'static str {
        match self {
            CaptureMode::Microphone => "wasapi-microphone",
            CaptureMode::Loopback => "wasapi-loopback",
            CaptureMode::Headless => "headless",
        }
    }
}

/// 内部实现
#[derive(Debug)]
enum Backend {
    #[cfg(windows)]
    Wasapi(WasapiCapture),
    /// 只产静音
    ///
    /// 不是「什么都不做」：它必须按**真实时间**推进采样数，
    /// 否则调用方会看到 `samples_captured()` 恒为 0，
    /// 误以为采集坏了。这里用墙上时钟折算应该产出多少采样。
    Headless {
        /// 起点（第一次 read 的时刻）
        started: Option<Instant>,
        /// 已经产出的帧数
        produced: u64,
        /// 累计丢弃（调用方读得太慢时模拟溢出）的帧数
        dropped: u64,
    },
}

/// 音频采集器
///
/// 一个类型包住两种模式（真设备 / headless），调用方不用为「有没有设备」分叉。
///
/// # 不是 Send
///
/// WASAPI 的 COM 对象不是 `Send`，所以这个类型也不能跨线程移动。
/// 详见 [`wasapi`] 模块文档。
#[derive(Debug)]
pub struct AudioCapture {
    mode: CaptureMode,
    format: PcmFormat,
    backend: Backend,
    /// 累计 read 调用次数
    read_calls: u64,
    /// 累计「这一 tick 没数据」的次数
    read_empty: u64,
    /// 累计 read 耗时（纳秒）
    read_ns: u128,
    /// 最近一次 read 耗时（纳秒）
    last_read_ns: u128,
}

impl AudioCapture {
    /// 打开默认采集设备（麦克风）
    ///
    /// 失败时返回 `Err`，调用方应降级为 [`AudioCapture::headless`]。
    /// **不要**把这里的错误当成致命错误 —— 没有麦克风的机器照样该能发屏幕。
    #[cfg(windows)]
    pub fn new(format: PcmFormat) -> Result<Self, CaptureError> {
        let cap = WasapiCapture::open_microphone(format)?;
        Ok(Self::from_wasapi(CaptureMode::Microphone, format, cap))
    }

    /// 打开默认采集设备（非 Windows：恒失败）
    ///
    /// 非 Windows 上没有 WASAPI。保留这个签名是为了让调用方代码不分叉，
    /// 只是它一定返回 `Err(NoDevice)`，调用方自然降级到 headless。
    #[cfg(not(windows))]
    pub fn new(format: PcmFormat) -> Result<Self, CaptureError> {
        format.validate()?;
        Err(CaptureError::NoDevice(
            "本平台没有 WASAPI 实现（只有 Windows 有）".into(),
        ))
    }

    /// 打开 loopback：采集系统正在播放的声音
    ///
    /// 这是「共享电脑音频」的关键。底层取的是**渲染**端点的镜像，
    /// 详见 [`wasapi`] 模块文档「坑 1」。
    ///
    /// # 没有声音播放时 read 返回 0
    ///
    /// 这是正常状态，不是错误 —— loopback 在静音时引擎根本不推进。
    #[cfg(windows)]
    pub fn loopback(format: PcmFormat) -> Result<Self, CaptureError> {
        let cap = WasapiCapture::open_loopback(format)?;
        Ok(Self::from_wasapi(CaptureMode::Loopback, format, cap))
    }

    /// 打开 loopback（非 Windows：恒失败）
    #[cfg(not(windows))]
    pub fn loopback(format: PcmFormat) -> Result<Self, CaptureError> {
        format.validate()?;
        Err(CaptureError::NoDevice(
            "本平台没有 WASAPI loopback 实现（只有 Windows 有）".into(),
        ))
    }

    #[cfg(windows)]
    fn from_wasapi(mode: CaptureMode, format: PcmFormat, cap: WasapiCapture) -> Self {
        Self {
            mode,
            format,
            backend: Backend::Wasapi(cap),
            read_calls: 0,
            read_empty: 0,
            read_ns: 0,
            last_read_ns: 0,
        }
    }

    /// 创建一个 headless 采集器：不碰任何平台 API，只按真实时间产静音
    ///
    /// 用于 CI / 无音频设备环境 / 用户明确关掉了采集。
    pub fn headless(format: PcmFormat) -> Self {
        Self {
            mode: CaptureMode::Headless,
            format,
            backend: Backend::Headless {
                started: None,
                produced: 0,
                dropped: 0,
            },
            read_calls: 0,
            read_empty: 0,
            read_ns: 0,
            last_read_ns: 0,
        }
    }

    /// 当前模式
    pub fn mode(&self) -> CaptureMode {
        self.mode
    }

    /// 调用方请求的格式
    pub fn format(&self) -> PcmFormat {
        self.format
    }

    /// 设备实际混音格式；headless 下返回 `None`
    ///
    /// 返回 `Option` 而不是「等于请求格式」：headless 根本没有设备，
    /// 谎报一个格式会让诊断日志显示「48000Hz/1ch/i16 设备」，
    /// 而那台机器上可能连声卡都没有。
    pub fn device_format(&self) -> Option<DeviceFormat> {
        match &self.backend {
            #[cfg(windows)]
            Backend::Wasapi(c) => Some(c.device_format()),
            Backend::Headless { .. } => None,
        }
    }

    /// 非阻塞读：把当前可用的 PCM 追加进 `out`，返回追加的采样数
    ///
    /// **绝不阻塞**。返回 `Ok(0)` 是完全正常的（见模块文档的语义表）。
    ///
    /// 返回的是**采样数**（i16 个数），不是帧数：
    /// `out` 会追加 `帧数 × format.channels` 个 i16。
    pub fn read(&mut self, out: &mut Vec<i16>) -> Result<usize, CaptureError> {
        let t0 = Instant::now();
        let result = self.read_inner(out);
        let ns = t0.elapsed().as_nanos();
        self.last_read_ns = ns;
        self.read_ns += ns;
        self.read_calls += 1;
        if let Ok(0) = result {
            self.read_empty += 1;
        }
        result
    }

    fn read_inner(&mut self, out: &mut Vec<i16>) -> Result<usize, CaptureError> {
        match &mut self.backend {
            #[cfg(windows)]
            Backend::Wasapi(c) => c.read(out),
            Backend::Headless {
                started,
                produced,
                dropped,
            } => {
                // 按真实时间折算应该产出多少采样，让调用方的节拍逻辑
                // 与真设备下的行为一致（否则 headless 测试永远拿不到数据）。
                let now = Instant::now();
                let start = *started.get_or_insert(now);
                let elapsed = now.duration_since(start).as_secs_f64();
                let should = (elapsed * self.format.sample_rate as f64) as u64;
                let want = should.saturating_sub(*produced);
                if want == 0 {
                    return Ok(0);
                }
                // 单次 read 最多给 100ms 的量：模拟真设备的缓冲上限，
                // 让「一次 tick 拿到一大堆」这种异常在测试里也能出现。
                let cap = self.format.frames_for_ms(100.0) as u64;
                let give = want.min(cap.max(1));
                let samples = (give as usize) * self.format.frame_samples();
                let before = out.len();
                out.resize(before + samples, 0);
                *produced += give;
                // 超出 cap 的部分是「来不及读」被引擎覆盖掉的量
                *dropped += want - give;
                Ok(samples)
            }
        }
    }

    /// 已采集的总采样数
    pub fn samples_captured(&self) -> u64 {
        match &self.backend {
            #[cfg(windows)]
            Backend::Wasapi(c) => c.samples_captured(),
            Backend::Headless { produced, .. } => {
                *produced * self.format.frame_samples().max(1) as u64
            }
        }
    }

    /// 累计丢弃（缓冲溢出）的采样数
    pub fn samples_dropped(&self) -> u64 {
        match &self.backend {
            #[cfg(windows)]
            Backend::Wasapi(c) => c.samples_dropped(),
            Backend::Headless { dropped, .. } => {
                *dropped * self.format.frame_samples().max(1) as u64
            }
        }
    }

    /// 采集侧估计延迟（毫秒）
    ///
    /// 真设备下 = `max(流固有延迟, 设备周期) + 引擎缓冲里积压的帧时长`。
    /// headless 下恒为 0（没有设备，也就没有设备延迟）。
    pub fn latency_ms(&self) -> f64 {
        match &self.backend {
            #[cfg(windows)]
            Backend::Wasapi(c) => c.latency_ms(),
            Backend::Headless { .. } => 0.0,
        }
    }

    /// 设备周期 `(默认, 最小)` 毫秒；headless 下是 `(0, 0)`
    ///
    /// 默认周期 = 引擎多久推进一次数据 = 共享模式采集延迟的物理下限。
    pub fn device_period_ms(&self) -> (f64, f64) {
        match &self.backend {
            #[cfg(windows)]
            Backend::Wasapi(c) => c.device_period_ms(),
            Backend::Headless { .. } => (0.0, 0.0),
        }
    }

    /// 引擎缓冲区总帧数；headless 下是模拟的 100ms 容量
    pub fn buffer_frames(&self) -> u32 {
        match &self.backend {
            #[cfg(windows)]
            Backend::Wasapi(c) => c.buffer_frames(),
            Backend::Headless { .. } => self.format.frames_for_ms(100.0) as u32,
        }
    }

    /// 累计 read 调用次数
    pub fn read_calls(&self) -> u64 {
        self.read_calls
    }

    /// 累计「这一 tick 没数据」的次数
    ///
    /// loopback 下这个数字大是**正常**的：系统没在放声音。
    pub fn read_empty(&self) -> u64 {
        self.read_empty
    }

    /// 累计 read 耗时（纳秒）
    pub fn read_ns(&self) -> u128 {
        self.read_ns
    }

    /// 最近一次 read 耗时（纳秒）
    pub fn last_read_ns(&self) -> u128 {
        self.last_read_ns
    }

    /// 平均每次 read 耗时（纳秒）；没有调用时返回 0
    pub fn avg_read_ns(&self) -> u128 {
        if self.read_calls == 0 {
            0
        } else {
            self.read_ns / self.read_calls as u128
        }
    }

    /// 已采集的总时长（毫秒）
    pub fn captured_ms(&self) -> f64 {
        let frames = self.samples_captured() / self.format.frame_samples().max(1) as u64;
        frames as f64 * 1000.0 / self.format.sample_rate.max(1) as f64
    }
}
