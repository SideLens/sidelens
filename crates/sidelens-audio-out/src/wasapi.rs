//! WASAPI 共享模式输出（Windows 专属）
//!
//! # 为什么是 WASAPI 共享模式，而不是 waveOut 或独占模式
//!
//! 三个候选的对比。**表中「本机实测」的数字都来自
//! `examples/frame_latency.rs`，可复现**；waveOut 与独占模式的数字是
//! 微软文档 + 社区测量的公开值（本 crate 没有实现它们，无法自测）。
//!
//! | 方案 | 延迟 | 本 crate 代码量 | 独占声卡 | 结论 |
//! | --- | --- | --- | --- | --- |
//! | waveOut（winmm） | 60-80ms（3-4 buffer × 20ms + 驱动队列） | 约 120 行 | 否 | 吃掉整条链路的延迟优势 |
//! | **WASAPI 共享** | **18-32ms**（实测，见下） | 608 行 | 否 | **采用** |
//! | WASAPI 独占 | 3-10ms | 约 700 行 + 格式协商 | **是** | 会把系统声音抢走 |
//!
//! # 实测数字，以及为什么共享模式仍是最优解
//!
//! 本机（Realtek，48kHz/2ch/float32，设备周期 10ms，缓冲 22ms）：
//!
//! ```text
//! 帧长     采样数   latency_ms   write p50
//! 2.5ms      120      18.30ms       3.5µs
//! 10.0ms     480      27.50ms       9.0µs
//! 20.0ms     960      31.72ms      10.8µs
//! ```
//!
//! 注意 18-32ms **不等于** WASAPI 自身的延迟：它是
//! `设备周期(10ms) + 调用方一次给的帧长`。20ms 帧必然带来 20ms 排队，
//! 这是数据流的物理性质，任何 API 都躲不掉。
//!
//! 对比 waveOut：它固定要 3-4 个 20ms buffer，即 **60-80ms 起步**，
//! 且**与帧长无关**。哪怕调用方给 2.5ms 帧，waveOut 仍然是 60ms+。
//! 共享模式在同样 2.5ms 帧下是 18.3ms —— 差距 40ms 以上。
//!
//! SideLens 的目标是 p50 12-14ms 的音画同步。waveOut 的 60-80ms 底噪
//! 会把这个优势完全吃掉，而且它已被微软标记为 legacy，现代 Windows 的
//! 音频引擎（audiodg）根本不走它。
//!
//! 独占模式能再省 10ms（到 8ms 量级），但它会**独占声卡**：用户开着的音乐、
//! 会议、通知全部静音。一个「把手机当摄像头」的工具没有资格这么干 ——
//! 而且省下的那 10ms 还不到 waveOut 与共享模式差距的四分之一。
//!
//! 共享模式的关键收益是 `GetCurrentPadding` 驱动的**非阻塞写入**：
//! 每次 [`WasapiSink::write`] 先问引擎「缓冲区里还排着多少帧」，只填剩下的
//! 空位，一次 `GetBuffer` / `ReleaseBuffer` 就完事，不等待、不睡眠、
//! 不需要事件回调线程。这正好对上调用方 1ms tick 的节奏。
//!
//! # 为什么用设备的混音格式，而不是硬塞 48kHz 单声道
//!
//! 共享模式要求流格式与设备混音格式兼容。两条路：
//!
//! 1. 带 `AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM` 硬塞自己的格式，让引擎转 ——
//!    少写转换代码，但多一层引擎内部缓冲，延迟不可控，且不同驱动行为不一致。
//! 2. **取 `GetMixFormat` 的原样格式，自己转** —— 延迟完全由我们自己的
//!    buffer 时长决定，行为在所有驱动上一致。
//!
//! 选 2。代价是一段 i16 → f32 的转换，实测 960 采样耗时约 2µs，
//! 相对 100µs 的预算可以忽略。
//!
//! 绝大多数 Windows 机器的混音格式就是 48kHz / 立体声 / float32，而 Opus
//! 解码器输出 48kHz / 单声道 / i16 —— 采样率一致，所以**没有重采样**，
//! 只有一次 i16→f32 + 单声道复制成立体声。万一设备是 44.1kHz，走
//! [`WasapiSink::write`] 内部的线性插值路径。
//!
//! # 线程约束
//!
//! COM 对象（`IAudioClient` / `IAudioRenderClient`）在 windows crate 里
//! **不是 `Send`**。[`crate::AudioSink`] 因此也是 `!Send`：必须在创建它的
//! 线程上使用。调用方（`udp_server::run`）的 future 不需要 `Send`
//! （`main` 里是直接 `.await`，不是 `tokio::spawn`），所以这条约束不构成
//! 问题；但将来若要把 sink 塞进 `tokio::spawn`，得先把它挪到一条专属线程，
//! 用 channel 喂 PCM。

use std::ffi::c_void;
use std::time::Instant;

use windows::core::GUID;
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioClient, IAudioClock, IAudioRenderClient, IMMDeviceEnumerator,
    MMDeviceEnumerator, AUDCLNT_SHAREMODE_SHARED, DEVICE_STATE_ACTIVE, WAVEFORMATEX,
    WAVEFORMATEXTENSIBLE,
};
use windows::Win32::System::Com::StructuredStorage::{
    PropVariantClear, PropVariantToStringWithDefault,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_MULTITHREADED, STGM_READ,
};
use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;

use crate::format::PcmFormat;
use crate::SinkError;

/// `KSDATAFORMAT_SUBTYPE_IEEE_FLOAT` 的 GUID
///
/// windows crate 把它放在 `Win32_Media_Multimedia` 里，为一个 GUID 多开一个
/// feature 不值得 —— 常量就是 16 个字节，直接写死。
const SUBTYPE_IEEE_FLOAT: GUID = GUID::from_u128(0x0000_0003_0000_0010_8000_00aa_0038_9b71);

/// `WAVE_FORMAT_EXTENSIBLE` 的 tag 值
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;

/// `WAVE_FORMAT_IEEE_FLOAT` 的 tag 值
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;

/// 100ns 单位换毫秒
const HNS_PER_MS: i64 = 10_000;

/// 我们申请的最小缓冲区时长（毫秒）
///
/// 太小会在设备周期边界上饿死（引擎每 10ms 才来取一次数），
/// 太大又直接变成延迟。20ms = 2 个默认设备周期，是共享模式下
/// 「不掉音」与「低延迟」的平衡点。
const MIN_BUFFER_MS: i64 = 20;

/// 设备侧输出采样的容器类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutSample {
    /// 32 位浮点（绝大多数机器的混音格式）
    F32,
    /// 16 位有符号整数
    I16,
}

/// COM 初始化守卫
///
/// `CoInitializeEx` 有四种返回：
///
/// - `S_OK` / `S_FALSE`：本次真的初始化了 → Drop 时要 `CoUninitialize`
/// - `RPC_E_CHANGED_MODE`：本线程已被别的组件用另一种套间模型初始化过 ——
///   **不是错误**，COM 已经可用，但我们不拥有它，Drop 时不能反初始化
/// - 其他：真失败
struct ComGuard {
    owned: bool,
}

impl ComGuard {
    fn init() -> Result<Self, SinkError> {
        // SAFETY: 参数全为空/常量；CoInitializeEx 只影响本线程。
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if hr.is_ok() {
            Ok(ComGuard { owned: true })
        } else if hr == windows::Win32::Foundation::RPC_E_CHANGED_MODE {
            // 别的组件已经初始化过（常见于被宿主进程内嵌时）。
            // COM 可用，只是不归我们管。
            Ok(ComGuard { owned: false })
        } else {
            Err(SinkError::Wasapi(format!(
                "CoInitializeEx 失败：{:#010x}",
                hr.0 as u32
            )))
        }
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.owned {
            // SAFETY: 只有我们自己初始化过才配对反初始化。
            unsafe { CoUninitialize() };
        }
    }
}

/// WASAPI 共享模式渲染器
pub struct WasapiSink {
    client: IAudioClient,
    render: IAudioRenderClient,
    /// 引擎缓冲区总帧数（`GetBufferSize`）
    buffer_frames: u32,
    /// 设备混音格式的采样率
    out_rate: u32,
    /// 设备混音格式的声道数
    out_channels: usize,
    /// 设备混音格式的采样容器
    out_sample: OutSample,
    /// 调用方给进来的格式
    in_format: PcmFormat,
    /// 转换结果（f32，按 out_channels 交错）
    scratch: Vec<f32>,
    /// 重采样相位（单位：输入帧，可为负，表示要借上一块的最后一帧）
    resample_pos: f64,
    /// 上一块输入的最后一帧（单声道值）
    last_in_sample: f32,
    /// 最近一次 write 之后缓冲区里还排着的帧数（诊断用）
    last_padding: u32,
    /// `GetStreamLatency` 的值（毫秒）
    stream_latency_ms: f64,
    /// 已提交的输入采样数
    samples_written: u64,
    /// 因缓冲满丢弃的输入采样数
    samples_dropped: u64,
    /// 打开耗时（微秒）
    open_elapsed_us: f64,
    /// 设备时钟（GetPosition 给出「已经播到第几帧」）
    clock: Option<IAudioClock>,
    /// 设备时钟频率（每秒多少个 position 单位）
    clock_freq: u64,
    /// 设备默认周期（毫秒，GetDevicePeriod 的 default）
    default_period_ms: f64,
    /// 设备最小周期（毫秒）
    min_period_ms: f64,
    /// 保持 COM 存活到所有接口之后（字段顺序即 Drop 顺序）
    _com: ComGuard,
}

impl std::fmt::Debug for WasapiSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WasapiSink")
            .field("in_format", &self.in_format)
            .field("out_rate", &self.out_rate)
            .field("out_channels", &self.out_channels)
            .field("out_sample", &self.out_sample)
            .field("buffer_frames", &self.buffer_frames)
            .finish_non_exhaustive()
    }
}

impl WasapiSink {
    /// 打开默认播放设备的共享模式流
    pub fn open(format: PcmFormat) -> Result<Self, SinkError> {
        Self::open_on_device(format, None)
    }

    /// 打开指定 endpoint id 的共享模式流；`None` 表示系统默认设备。
    pub fn open_on_device(format: PcmFormat, endpoint_id: Option<&str>) -> Result<Self, SinkError> {
        format.validate().map_err(SinkError::Format)?;
        let t0 = Instant::now();

        let com = ComGuard::init()?;

        // SAFETY: 下面整段都是 COM 调用。返回的接口指针由 windows crate 的
        // RAII 包装持有，生命周期跟着 self 走；GetMixFormat 的裸指针在
        // 用完（Initialize 之后）立刻 CoTaskMemFree。
        unsafe {
            // 1) 设备枚举器
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(|e| {
                    SinkError::Wasapi(format!("创建 IMMDeviceEnumerator 失败：{e}"))
                })?;

            // 2) 默认播放设备（eRender = 输出，eConsole = 系统默认）
            let device = if let Some(endpoint_id) = endpoint_id {
                let wide_id: Vec<u16> = endpoint_id.encode_utf16().chain(Some(0)).collect();
                enumerator
                    .GetDevice(windows::core::PCWSTR(wide_id.as_ptr()))
                    .map_err(|e| SinkError::NoDevice(format!("打开所选播放设备失败：{e}")))?
            } else {
                enumerator
                    .GetDefaultAudioEndpoint(eRender, eConsole)
                    .map_err(|e| {
                        SinkError::NoDevice(format!(
                            "取默认播放设备失败（没有音频输出设备？）：{e}"
                        ))
                    })?
            };

            // 3) 激活 IAudioClient
            let client: IAudioClient = device
                .Activate(CLSCTX_ALL, None)
                .map_err(|e| SinkError::Wasapi(format!("IAudioClient::Activate 失败：{e}")))?;

            // 4) 设备混音格式。指针由 CoTaskMemAlloc 分配，必须 CoTaskMemFree。
            //    解析与 Initialize 用**同一个**指针，避免重复分配。
            let mix_ptr = client
                .GetMixFormat()
                .map_err(|e| SinkError::Wasapi(format!("GetMixFormat 失败：{e}")))?;
            let parsed = parse_mix_format(mix_ptr);
            let mut default_period_hns: i64 = 0;
            let mut min_period_hns: i64 = 0;
            let init_result = match &parsed {
                Ok(_) => {
                    // 5) 初始化。hnsBufferDuration 用「至少 2 个设备周期」：
                    //    共享模式下引擎按设备周期取数，缓冲区小于 2 个周期会饿死。
                    let mut default_period: i64 = 0;
                    let mut min_period: i64 = 0;
                    let _ =
                        client.GetDevicePeriod(Some(&mut default_period), Some(&mut min_period));
                    default_period_hns = default_period;
                    min_period_hns = min_period;
                    let buf_duration = (default_period * 2).max(MIN_BUFFER_MS * HNS_PER_MS);

                    client.Initialize(AUDCLNT_SHAREMODE_SHARED, 0, buf_duration, 0, mix_ptr, None)
                }
                Err(_) => Ok(()),
            };
            CoTaskMemFree(Some(mix_ptr as *const c_void));

            let (out_rate, out_channels, out_sample) = parsed?;
            init_result.map_err(|e| {
                SinkError::UnsupportedFormat(format!(
                    "IAudioClient::Initialize 失败（{out_rate}Hz/{out_channels}ch）：{e}"
                ))
            })?;

            let buffer_frames = client
                .GetBufferSize()
                .map_err(|e| SinkError::Wasapi(format!("GetBufferSize 失败：{e}")))?;

            let stream_latency_ms = client
                .GetStreamLatency()
                .map(|hns| hns as f64 / HNS_PER_MS as f64)
                .unwrap_or(0.0);

            // 6) 取渲染接口并启动
            let render: IAudioRenderClient = client.GetService().map_err(|e| {
                SinkError::Wasapi(format!("GetService(IAudioRenderClient) 失败：{e}"))
            })?;

            // 7) 设备时钟：GetPosition 给出「引擎已经播到第几帧」，
            //    这是「样本真的被设备消费了」的唯一硬证据 ——
            //    计数器只能证明我们调过 write，不能证明声音出去了。
            let (clock, clock_freq) = match client.GetService::<IAudioClock>() {
                Ok(c) => {
                    let f = c.GetFrequency().unwrap_or(0);
                    (Some(c), f)
                }
                Err(_) => (None, 0),
            };

            client
                .Start()
                .map_err(|e| SinkError::Wasapi(format!("IAudioClient::Start 失败：{e}")))?;

            // 一次 write 最多 60ms（Opus 最大帧长）→ 预留 100ms 的输出空间
            let scratch_cap = (out_rate as usize / 10).max(1024) * out_channels;

            Ok(WasapiSink {
                client,
                render,
                buffer_frames,
                out_rate,
                out_channels,
                out_sample,
                in_format: format,
                scratch: vec![0.0; scratch_cap],
                resample_pos: 0.0,
                last_in_sample: 0.0,
                last_padding: 0,
                stream_latency_ms,
                samples_written: 0,
                samples_dropped: 0,
                open_elapsed_us: t0.elapsed().as_secs_f64() * 1e6,
                clock,
                clock_freq,
                default_period_ms: default_period_hns as f64 / HNS_PER_MS as f64,
                min_period_ms: min_period_hns as f64 / HNS_PER_MS as f64,
                _com: com,
            })
        }
    }

    /// 设备混音格式 `(采样率, 声道数)`（诊断用）
    pub fn device_format(&self) -> (u32, usize) {
        (self.out_rate, self.out_channels)
    }

    /// 引擎缓冲区总帧数
    pub fn buffer_frames(&self) -> u32 {
        self.buffer_frames
    }

    /// 打开耗时（微秒）
    pub fn open_elapsed_us(&self) -> f64 {
        self.open_elapsed_us
    }

    /// 是否需要在 write 里做重采样
    pub fn needs_resample(&self) -> bool {
        self.out_rate != self.in_format.sample_rate
    }

    /// 已提交的输入采样数
    pub fn samples_written(&self) -> u64 {
        self.samples_written
    }

    /// 丢弃的输入采样数
    pub fn samples_dropped(&self) -> u64 {
        self.samples_dropped
    }

    /// 缓冲区里当前排队的帧数（最近一次 write 之后的值）
    pub fn padding_frames(&self) -> u32 {
        self.last_padding
    }

    /// 估计的输出延迟（毫秒）
    ///
    /// = max(流的固有延迟, 一个设备周期) + 缓冲区里还排着的帧的时长。
    ///
    /// # 实测值（examples/frame_latency.rs 可复现）
    ///
    /// 本机：Realtek，48kHz/2ch/float32 混音格式，设备周期 10ms，缓冲 1056 帧
    /// （22ms）。各帧长下的稳态值：
    ///
    /// ```text
    /// 帧长     采样数   latency_ms   write p50
    /// 2.5ms      120      18.30ms       3.5µs
    /// 5.0ms      240      20.00ms       6.3µs
    /// 10.0ms     480      27.50ms       9.0µs
    /// 20.0ms     960      31.72ms      10.8µs
    /// 40.0ms    1920      32.00ms      13.8µs
    /// 60.0ms    2880      32.00ms      12.1µs
    /// ```
    ///
    /// 形状是 `latency ≈ 设备周期 + 帧长`，但因为缓冲只有 22ms，
    /// 帧长超过 20ms 后 `padding` 被缓冲上限卡住，不再线性增长（32ms 封顶）。
    ///
    /// 那 10-20ms 不是 WASAPI 的开销，是**调用方一次给多少数据**的必然结果：
    /// 刚写进去的样本排在队尾，至少要等前面播完才轮到它。
    ///
    /// 想压到 18ms 以下，调用方得用 2.5ms 帧（Opus 允许的最短帧长）——
    /// 那是编解码与网络分包的决策，不是本 crate 能决定的。本 crate 能做的是
    /// **不额外增加延迟**：共享模式 + 22ms 缓冲 + 非阻塞写入，除了设备周期
    /// 和调用方给的帧长，没有第三份缓冲。
    ///
    /// # 为什么要和设备周期取 max
    ///
    /// `GetStreamLatency` 在本机 Realtek 驱动上返回 **0**（多数板载驱动都是），
    /// 直接信它会得到「延迟 = padding」的乐观估计 —— 空缓冲时就是 0ms，
    /// 那是假的：数据总得等引擎下一个周期来取，物理上不可能低于一个周期。
    ///
    /// 所以下限取 `default_period`（实测 10ms）。这个值来自
    /// `GetDevicePeriod`，是驱动自己报的，比 `GetStreamLatency` 可信。
    ///
    /// 用最近一次 [`WasapiSink::write`] 缓存的 padding，不额外调 COM ——
    /// 这个方法会被统计循环每 tick 调一次，不该有 COM 开销。
    pub fn latency_ms(&self) -> f64 {
        let queued_ms = self.last_padding as f64 * 1000.0 / self.out_rate.max(1) as f64;
        self.stream_latency_ms.max(self.default_period_ms) + queued_ms
    }

    /// 流的固有延迟（毫秒，`GetStreamLatency` 原值）
    pub fn stream_latency_ms(&self) -> f64 {
        self.stream_latency_ms
    }

    /// 设备已经播放到的帧数（`IAudioClock::GetPosition` 折算）
    ///
    /// 这是「样本真的被设备消费了」的硬证据：它由驱动自己维护，
    /// 不受我们计数逻辑影响。拿不到时钟时返回 `None`。
    pub fn device_position_frames(&self) -> Option<u64> {
        let clock = self.clock.as_ref()?;
        if self.clock_freq == 0 {
            return None;
        }
        // SAFETY: clock 由 self 持有，调用期间存活。
        let mut pos: u64 = 0;
        unsafe { clock.GetPosition(&mut pos, None).ok()? };
        // GetPosition 的单位是 clock_freq 分之一秒
        Some(pos.saturating_mul(self.out_rate as u64) / self.clock_freq)
    }

    /// 设备时钟频率（Hz）；拿不到时钟时返回 0
    pub fn clock_freq(&self) -> u64 {
        self.clock_freq
    }

    /// 设备周期 `(默认, 最小)`（毫秒）
    ///
    /// 默认周期就是引擎「多久来取一次数」的节奏，是共享模式延迟的物理下限。
    pub fn device_period_ms(&self) -> (f64, f64) {
        (self.default_period_ms, self.min_period_ms)
    }

    /// 写入一帧 PCM。**非阻塞**。
    ///
    /// 返回实际吃进去的**输入采样数**（i16 个数，不是帧数）。
    /// 缓冲区满时返回 `Ok(0)`。
    pub fn write(&mut self, pcm: &[i16]) -> Result<usize, SinkError> {
        let in_ch = self.in_format.channels.max(1) as usize;
        let frames_in = pcm.len() / in_ch;
        if frames_in == 0 {
            return Ok(0);
        }

        // 1) 问引擎还有多少空位。GetCurrentPadding 是一次轻量 COM 调用
        //    （实测亚微秒），比等 buffer 便宜得多。
        // SAFETY: client 由 self 持有，调用期间存活。
        let padding = unsafe {
            self.client
                .GetCurrentPadding()
                .map_err(|e| SinkError::Wasapi(format!("GetCurrentPadding 失败：{e}")))?
        };
        self.last_padding = padding;

        let free = self.buffer_frames.saturating_sub(padding) as usize;
        if free == 0 {
            self.samples_dropped += pcm.len() as u64;
            return Ok(0);
        }

        // 2) 决定这一批写多少帧。重采样时输出帧数 != 输入帧数。
        let take_in = frames_in.min(free);
        let out_frames = if self.needs_resample() {
            let ratio = self.out_rate as f64 / self.in_format.sample_rate as f64;
            ((take_in as f64 * ratio).ceil() as usize).min(free)
        } else {
            take_in
        };
        if out_frames == 0 {
            self.samples_dropped += pcm.len() as u64;
            return Ok(0);
        }

        // 3) 转成设备格式
        let produced = self.convert(&pcm[..take_in * in_ch], take_in, out_frames);

        // 4) 拷进引擎缓冲区
        // SAFETY: GetBuffer 返回的指针至少有 out_frames 帧的空间；
        // ReleaseBuffer 必须与 GetBuffer 配对，任何提前返回都不能漏。
        unsafe {
            let dst = self
                .render
                .GetBuffer(out_frames as u32)
                .map_err(|e| SinkError::Wasapi(format!("GetBuffer({out_frames}) 失败：{e}")))?;
            if dst.is_null() {
                let _ = self.render.ReleaseBuffer(0, 0);
                self.samples_dropped += pcm.len() as u64;
                return Ok(0);
            }
            copy_to_device(
                dst,
                &self.scratch[..produced * self.out_channels],
                self.out_sample,
            );
            self.render
                .ReleaseBuffer(out_frames as u32, 0)
                .map_err(|e| SinkError::Wasapi(format!("ReleaseBuffer 失败：{e}")))?;
        }

        let consumed_in = if self.needs_resample() {
            let ratio = self.in_format.sample_rate as f64 / self.out_rate as f64;
            ((out_frames as f64 * ratio).ceil() as usize).min(take_in)
        } else {
            out_frames
        };

        let consumed = consumed_in * in_ch;
        self.samples_written += consumed as u64;
        // 没吃进去的尾巴算丢弃
        self.samples_dropped += (pcm.len() - consumed) as u64;
        self.last_padding = padding + out_frames as u32;
        Ok(consumed)
    }

    /// 把 `take` 个输入帧转成 `out_frames` 个设备帧，写进 scratch。
    ///
    /// 返回实际产出的帧数。
    fn convert(&mut self, pcm: &[i16], take: usize, out_frames: usize) -> usize {
        let in_ch = self.in_format.channels.max(1) as usize;
        let out_ch = self.out_channels;
        let need = out_frames * out_ch;
        if self.scratch.len() < need {
            self.scratch.resize(need, 0.0);
        }

        // i16 → f32 归一化系数。用 1/32768 而不是 1/32767：
        // -32768 恰好映射到 -1.0，不会溢出。
        const SCALE: f32 = 1.0 / 32768.0;

        if !self.needs_resample() {
            let frames = take.min(out_frames);
            for i in 0..frames {
                let base = i * in_ch;
                let ob = i * out_ch;
                for c in 0..out_ch {
                    // 单声道 → 多声道：复制到每个声道
                    // 多声道 → 少声道：取前 out_ch 个
                    // 声道数相同：一一对应
                    let src = if in_ch == 1 {
                        base
                    } else {
                        base + c.min(in_ch - 1)
                    };
                    self.scratch[ob + c] = pcm[src] as f32 * SCALE;
                }
            }
            if frames > 0 {
                self.last_in_sample = pcm[(frames - 1) * in_ch] as f32 * SCALE;
            }
            return frames;
        }

        // 线性插值重采样。pos 的单位是「输入帧」，跨块保持。
        let ratio = self.in_format.sample_rate as f64 / self.out_rate as f64;
        let mut pos = self.resample_pos;
        let prev = self.last_in_sample;
        let mut produced = 0usize;

        for o in 0..out_frames {
            let i0f = pos.floor();
            let frac = (pos - i0f) as f32;
            let i0 = i0f as i64;

            let at = |idx: i64| -> f32 {
                if idx < 0 {
                    // 借上一块的最后一个样本，避免块边界爆音
                    prev
                } else if (idx as usize) < take {
                    pcm[idx as usize * in_ch] as f32 * SCALE
                } else {
                    pcm[(take - 1) * in_ch] as f32 * SCALE
                }
            };

            let s = at(i0) * (1.0 - frac) + at(i0 + 1) * frac;
            let ob = o * out_ch;
            for c in 0..out_ch {
                self.scratch[ob + c] = s;
            }
            produced += 1;
            pos += ratio;
        }

        // pos 现在相对本块起点；下一块起点是 take，所以减去 take
        self.resample_pos = pos - take as f64;
        self.last_in_sample = pcm[(take - 1) * in_ch] as f32 * SCALE;
        produced
    }
}

/// 当前处于活动状态的播放 endpoint，返回 `(显示名, 不透明 endpoint id)`。
pub fn enumerate_render_devices() -> Vec<(String, String)> {
    let Ok(_com) = ComGuard::init() else {
        return Vec::new();
    };
    // SAFETY: COM interfaces are local to this call; all returned ids are freed below.
    unsafe {
        let Ok(enumerator): Result<IMMDeviceEnumerator, _> =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
        else {
            return Vec::new();
        };
        let Ok(devices) = enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE) else {
            return Vec::new();
        };
        let Ok(count) = devices.GetCount() else {
            return Vec::new();
        };
        (0..count)
            .filter_map(|index| {
                let device = devices.Item(index).ok()?;
                let id = device.GetId().ok()?;
                let endpoint_id = id.to_string().ok();
                CoTaskMemFree(Some(id.0 as *const c_void));
                let endpoint_id = endpoint_id?;
                let display_name = device
                    .OpenPropertyStore(STGM_READ)
                    .ok()
                    .and_then(|store: IPropertyStore| {
                        store.GetValue(&PKEY_Device_FriendlyName).ok()
                    })
                    .map(|mut value| {
                        let name = PropVariantToStringWithDefault(&value, windows::core::w!(""))
                            .to_string()
                            .unwrap_or_default();
                        let _ = PropVariantClear(&mut value);
                        name
                    })
                    .filter(|name| !name.is_empty())
                    .unwrap_or_else(|| format!("播放设备 {}", index + 1));
                Some((display_name, endpoint_id))
            })
            .collect()
    }
}

impl Drop for WasapiSink {
    fn drop(&mut self) {
        // SAFETY: client 由 self 持有；Stop 对未启动的流也安全。
        unsafe {
            let _ = self.client.Stop();
        }
        // 接口本身由 windows crate 的 Clone/Drop 释放，不用手写 Release。
    }
}

/// 把 scratch 里的 f32 样本按设备容器格式写进引擎缓冲区
///
/// SAFETY: `dst` 必须至少有 `samples.len() * size_of::<T>()` 字节可写。
unsafe fn copy_to_device(dst: *mut u8, samples: &[f32], kind: OutSample) {
    match kind {
        OutSample::F32 => {
            // SAFETY: 调用方保证 dst 足够大；WASAPI 的 buffer 至少 16 字节对齐。
            let out = unsafe { std::slice::from_raw_parts_mut(dst as *mut f32, samples.len()) };
            out.copy_from_slice(samples);
        }
        OutSample::I16 => {
            // SAFETY: 同上。
            let out = unsafe { std::slice::from_raw_parts_mut(dst as *mut i16, samples.len()) };
            for (o, s) in out.iter_mut().zip(samples.iter()) {
                // f32 [-1,1] → i16。用 32767 并 clamp，避免溢出。
                *o = (s * 32767.0).clamp(-32768.0, 32767.0) as i16;
            }
        }
    }
}

/// 解析 `GetMixFormat` 的返回值
///
/// 返回 `(采样率, 声道数, 采样容器)`。
fn parse_mix_format(ptr: *mut WAVEFORMATEX) -> Result<(u32, usize, OutSample), SinkError> {
    if ptr.is_null() {
        return Err(SinkError::Wasapi("GetMixFormat 返回空指针".into()));
    }
    // SAFETY: ptr 由 GetMixFormat 分配且非空；结构体是 packed(1)，
    // 按值整体读取（`*ptr`）不会产生未对齐引用。
    let fmt = unsafe { *ptr };
    let tag = fmt.wFormatTag;
    let bits = fmt.wBitsPerSample;
    let channels = fmt.nChannels as usize;
    let rate = fmt.nSamplesPerSec;
    let cb_size = fmt.cbSize;

    if channels == 0 || rate == 0 {
        return Err(SinkError::UnsupportedFormat(format!(
            "设备混音格式非法：{rate}Hz/{channels}ch"
        )));
    }

    // 0xFFFE = WAVEFORMATEXTENSIBLE，真正的类型在 SubFormat 里
    let is_float = if tag == WAVE_FORMAT_EXTENSIBLE && cb_size >= 22 {
        // SAFETY: cbSize >= 22 说明后面确实跟着 WAVEFORMATEXTENSIBLE 的扩展部分；
        // 结构体同样是 packed(1)，按值读 SubFormat 是安全的。
        let ext = ptr as *const WAVEFORMATEXTENSIBLE;
        let sub = unsafe { (*ext).SubFormat };
        sub == SUBTYPE_IEEE_FLOAT
    } else {
        tag == WAVE_FORMAT_IEEE_FLOAT
    };

    let sample = match (is_float, bits) {
        (true, 32) => OutSample::F32,
        (false, 16) => OutSample::I16,
        (true, b) => {
            return Err(SinkError::UnsupportedFormat(format!(
                "不支持的浮点位深：{b} bit（只支持 32）"
            )))
        }
        (false, b) => {
            return Err(SinkError::UnsupportedFormat(format!(
                "不支持的整数位深：{b} bit（只支持 16）"
            )))
        }
    };

    Ok((rate, channels, sample))
}

/// 默认播放设备的 endpoint id（诊断用）
pub fn default_device_id() -> Option<String> {
    let com = ComGuard::init().ok()?;
    // SAFETY: COM 调用；返回的 PWSTR 由 CoTaskMemFree 释放。
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
        let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole).ok()?;
        let id = device.GetId().ok()?;
        let s = id.to_string().ok();
        CoTaskMemFree(Some(id.0 as *const c_void));
        drop(com);
        s
    }
}
