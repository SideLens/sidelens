//! WASAPI 共享模式采集（Windows 专属）
//!
//! # 为什么是共享模式 + 轮询，而不是事件回调
//!
//! 采集有两种驱动方式：
//!
//! | 方式 | 延迟 | 线程模型 | 结论 |
//! | --- | --- | --- | --- |
//! | 事件回调（`AUDCLNT_STREAMFLAGS_EVENTCALLBACK`） | 低 | 需要一条专属线程 + 一个 Win32 事件句柄 | 不用 |
//! | **轮询（`GetNextPacketSize`）** | 低 | 无额外线程，调用方在 1ms tick 里顺手调 | **采用** |
//!
//! SideLens 的发送端已经有一条 1ms 的 tick（视频编码节拍），
//! 采集只要挂在这条 tick 上就够了。再开一条线程意味着多一个
//! 跨线程缓冲，而那正是延迟的来源。
//!
//! # 三个必须踩过的坑
//!
//! ## 坑 1：loopback 只能用**渲染**端点
//!
//! `AUDCLNT_STREAMFLAGS_LOOPBACK` 的语义是「把正在播放的声音镜像出来」，
//! 所以它必须配 `GetDefaultAudioEndpoint(eRender, eConsole)`。
//! 对着 `eCapture` 端点传这个标志，`Initialize` 直接返回
//! `E_INVALIDARG` —— 而且错误信息完全看不出原因。
//!
//! ## 坑 2：loopback 在没有声音播放时**不产生数据**
//!
//! 它不是「送静音」，是**引擎根本不推进**。此时 `GetNextPacketSize`
//! 恒为 0，[`WasapiCapture::read`] 返回 `Ok(0)`。
//! 调用方必须把「0」当成正常状态，不能当成设备坏了 ——
//! 反过来，麦克风路径下 0 通常意味着真的没数据。
//!
//! ## 坑 3：`AUDCLNT_BUFFERFLAGS_SILENT` 时 `pData` 是 NULL
//!
//! 引擎认为这一段是静音时，会把数据指针置空而不是填零。
//! 直接解引用就是空指针解引用（进程直接死）。
//! 必须按「填 frames × channels 个 0」处理。
//!
//! # 线程约束
//!
//! COM 对象（`IAudioClient` / `IAudioCaptureClient`）不是 `Send`，
//! 所以 [`crate::AudioCapture`] 也 `!Send`：必须在创建它的线程上用。
//! 发送端的 future 不要求 `Send`（`main` 里直接 `.await`），
//! 这条约束目前不构成问题；将来要塞进 `tokio::spawn` 得先挪到专属线程。
//!
//! # 本机实测
//!
//! 混音格式 `48000Hz/2ch/32bit float`，设备周期默认 `10ms` / 最小 `2.5ms`，
//! 引擎缓冲 `1056` 帧（`22ms`）。详见 `examples/probe_capture.rs` 的输出。

use std::ffi::c_void;
use std::time::Instant;

use windows::core::GUID;
use windows::Win32::Media::Audio::{
    eCapture, eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator,
    MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY, AUDCLNT_BUFFERFLAGS_SILENT,
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK, WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_MULTITHREADED,
};

use crate::format::{
    convert_frames, formats_compatible, push_silence, DeviceFormat, DeviceSamples, PcmFormat,
    SampleKind,
};
use crate::{CaptureError, CaptureMode};

/// `KSDATAFORMAT_SUBTYPE_IEEE_FLOAT` 的 GUID
///
/// windows crate 把它放在 `Win32_Media_Multimedia` 里，为一个 16 字节常量
/// 多开一个 feature 不值得 —— 直接写死。
const SUBTYPE_IEEE_FLOAT: GUID = GUID::from_u128(0x0000_0003_0000_0010_8000_00aa_0038_9b71);

/// `WAVE_FORMAT_EXTENSIBLE` 的 tag 值
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;

/// `WAVE_FORMAT_IEEE_FLOAT` 的 tag 值
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;

/// 100ns 单位换毫秒
const HNS_PER_MS: i64 = 10_000;

/// 一次 `read` 最多消费多少个 packet
///
/// 正常情况一次 tick 里只有 1-3 个 packet。设上限是为了防驱动 bug：
/// 某些虚拟声卡（VB-Cable 之类）会一直返回非零 packet size，
/// 没有上限的话 `read` 会变成死循环，把整条 1ms tick 卡死。
const MAX_PACKETS_PER_READ: usize = 256;

/// COM 初始化守卫
///
/// `CoInitializeEx` 的四种返回要区别对待：
///
/// - `S_OK` / `S_FALSE`：本次真的初始化了 → Drop 时配对 `CoUninitialize`
/// - `RPC_E_CHANGED_MODE`：本线程已被别的组件用另一种套间模型初始化过 ——
///   **不是错误**，COM 已经可用，只是不归我们管，Drop 时不能反初始化
/// - 其它：真失败
struct ComGuard {
    owned: bool,
}

impl ComGuard {
    fn init() -> Result<Self, CaptureError> {
        // SAFETY: 参数全为空/常量；CoInitializeEx 只影响本线程。
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if hr.is_ok() {
            Ok(ComGuard { owned: true })
        } else if hr == windows::Win32::Foundation::RPC_E_CHANGED_MODE {
            Ok(ComGuard { owned: false })
        } else {
            Err(CaptureError::Wasapi(format!(
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

/// WASAPI 共享模式采集器
pub struct WasapiCapture {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    /// 设备混音格式
    device: DeviceFormat,
    /// 设备采样的容器类型
    kind: SampleKind,
    /// 调用方请求的格式
    in_format: PcmFormat,
    /// 引擎缓冲区总帧数
    buffer_frames: u32,
    /// 是否是 loopback（决定 `read` 返回 0 的语义）
    is_loopback: bool,
    /// `GetStreamLatency` 的值（毫秒）
    stream_latency_ms: f64,
    /// 设备默认周期（毫秒）
    default_period_ms: f64,
    /// 设备最小周期（毫秒）
    min_period_ms: f64,
    /// 上一次 `read` 开始时引擎里排着的帧数（算延迟用）
    last_pending_frames: u32,
    /// 已产出的采样数（i16 个数，已按请求声道数折算）
    samples_captured: u64,
    /// 因引擎缓冲被覆盖而丢掉的采样数（由 device position 跳变推算）
    samples_dropped: u64,
    /// 上一次 `GetBuffer` 报的 device position + 帧数，用来发现跳变
    expected_pos: Option<u64>,
    /// 打开耗时（微秒）
    open_elapsed_us: f64,
    /// 保持 COM 存活到所有接口之后（字段顺序即 Drop 顺序）
    _com: ComGuard,
}

impl std::fmt::Debug for WasapiCapture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WasapiCapture")
            .field("in_format", &self.in_format)
            .field("device", &self.device)
            .field("kind", &self.kind)
            .field("buffer_frames", &self.buffer_frames)
            .field("is_loopback", &self.is_loopback)
            .finish_non_exhaustive()
    }
}

impl WasapiCapture {
    /// 打开默认采集设备（麦克风）
    pub fn open_microphone(format: PcmFormat) -> Result<Self, CaptureError> {
        Self::open(format, CaptureMode::Microphone)
    }

    /// 打开默认**渲染**设备的 loopback（采集系统正在播放的声音）
    ///
    /// 注意这里取的是 `eRender` 端点 —— 见模块文档「坑 1」。
    pub fn open_loopback(format: PcmFormat) -> Result<Self, CaptureError> {
        Self::open(format, CaptureMode::Loopback)
    }

    fn open(format: PcmFormat, mode: CaptureMode) -> Result<Self, CaptureError> {
        format.validate()?;
        let t0 = Instant::now();

        let com = ComGuard::init()?;
        let is_loopback = mode == CaptureMode::Loopback;

        // SAFETY: 整段是 COM 调用。接口指针由 windows crate 的 RAII 包装持有，
        // 生命周期跟着 self 走；GetMixFormat 的裸指针在 Initialize 之后立刻
        // CoTaskMemFree。
        unsafe {
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(|e| {
                    CaptureError::Wasapi(format!("创建 IMMDeviceEnumerator 失败：{e}"))
                })?;

            // loopback 要渲染端点，麦克风要采集端点。搞反的话
            // Initialize 会以 E_INVALIDARG 失败，且错误信息毫无线索。
            let (flow, flow_name) = if is_loopback {
                (eRender, "渲染（播放）")
            } else {
                (eCapture, "采集（麦克风）")
            };

            let device = enumerator
                .GetDefaultAudioEndpoint(flow, eConsole)
                .map_err(|e| CaptureError::NoDevice(format!("取默认{flow_name}设备失败：{e}")))?;

            let client: IAudioClient = device
                .Activate(CLSCTX_ALL, None)
                .map_err(|e| CaptureError::Wasapi(format!("IAudioClient::Activate 失败：{e}")))?;

            let mix_ptr = client
                .GetMixFormat()
                .map_err(|e| CaptureError::Wasapi(format!("GetMixFormat 失败：{e}")))?;
            let parsed = parse_mix_format(mix_ptr);

            let mut default_period_hns: i64 = 0;
            let mut min_period_hns: i64 = 0;
            let _ =
                client.GetDevicePeriod(Some(&mut default_period_hns), Some(&mut min_period_hns));

            let device_fmt = parsed?;

            // 采样率对不上就直接失败。理由见 format::formats_compatible 的文档：
            // 悄悄变调比明确报错难查得多。
            if !formats_compatible(&device_fmt, format) {
                CoTaskMemFree(Some(mix_ptr as *const c_void));
                return Err(CaptureError::RateMismatch {
                    device_rate: device_fmt.sample_rate,
                    requested_rate: format.sample_rate,
                });
            }

            // 采样率一致时，Initialize 直接吃设备自己的混音格式：
            // 共享模式不接受任意格式，硬塞会失败或被引擎偷偷加一层转换缓冲。
            // 传原样格式 = 我们和引擎之间没有任何隐藏的重采样。
            let flags = if is_loopback {
                AUDCLNT_STREAMFLAGS_LOOPBACK
            } else {
                0
            };
            // hnsBufferDuration = 0 → 让引擎用默认设备周期（实测 10ms）。
            // 给固定值会在某些驱动上被拒绝，0 是唯一到处都能过的写法。
            let init = client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 0, 0, mix_ptr, None);
            CoTaskMemFree(Some(mix_ptr as *const c_void));
            init.map_err(|e| {
                CaptureError::UnsupportedFormat(format!(
                    "IAudioClient::Initialize 失败（loopback={is_loopback}，{}）：{e}",
                    device_fmt
                ))
            })?;

            let kind = device_fmt.sample_kind().map_err(CaptureError::Format)?;

            let buffer_frames = client
                .GetBufferSize()
                .map_err(|e| CaptureError::Wasapi(format!("GetBufferSize 失败：{e}")))?;

            let stream_latency_ms = client
                .GetStreamLatency()
                .map(|hns| hns as f64 / HNS_PER_MS as f64)
                .unwrap_or(0.0);

            let capture: IAudioCaptureClient = client.GetService().map_err(|e| {
                CaptureError::Wasapi(format!("GetService(IAudioCaptureClient) 失败：{e}"))
            })?;

            client
                .Start()
                .map_err(|e| CaptureError::Wasapi(format!("IAudioClient::Start 失败：{e}")))?;

            Ok(WasapiCapture {
                client,
                capture,
                device: device_fmt,
                kind,
                in_format: format,
                buffer_frames,
                is_loopback,
                stream_latency_ms,
                default_period_ms: default_period_hns as f64 / HNS_PER_MS as f64,
                min_period_ms: min_period_hns as f64 / HNS_PER_MS as f64,
                last_pending_frames: 0,
                samples_captured: 0,
                samples_dropped: 0,
                expected_pos: None,
                open_elapsed_us: t0.elapsed().as_secs_f64() * 1e6,
                _com: com,
            })
        }
    }

    /// 设备混音格式
    pub fn device_format(&self) -> DeviceFormat {
        self.device
    }

    /// 引擎缓冲区总帧数
    pub fn buffer_frames(&self) -> u32 {
        self.buffer_frames
    }

    /// 打开耗时（微秒）
    pub fn open_elapsed_us(&self) -> f64 {
        self.open_elapsed_us
    }

    /// 是否 loopback
    pub fn is_loopback(&self) -> bool {
        self.is_loopback
    }

    /// 已产出的采样数（i16 个数）
    pub fn samples_captured(&self) -> u64 {
        self.samples_captured
    }

    /// 丢弃的采样数（i16 个数）
    pub fn samples_dropped(&self) -> u64 {
        self.samples_dropped
    }

    /// `GetStreamLatency` 原值（毫秒）
    pub fn stream_latency_ms(&self) -> f64 {
        self.stream_latency_ms
    }

    /// 设备周期 `(默认, 最小)`（毫秒）
    pub fn device_period_ms(&self) -> (f64, f64) {
        (self.default_period_ms, self.min_period_ms)
    }

    /// 采集侧估计延迟（毫秒）
    ///
    /// = `max(流的固有延迟, 一个设备周期) + 上一次 read 开始时排队的帧时长`。
    ///
    /// # 为什么要和设备周期取 max
    ///
    /// `GetStreamLatency` 在本机 Realtek 驱动上返回 0，直接信它会得到
    /// 「延迟 = 排队帧数」的乐观估计 —— 队列排空时就是 0ms，那是假的：
    /// 声音先进引擎缓冲、引擎按周期交给驱动，物理上不可能低于一个周期。
    ///
    /// 排队帧数用最近一次 `read` 的**起始**值（不是结束值）：
    /// 结束值恒为 0（我们已经把它抽干了），那不是延迟，那是我们自己的处理速度。
    pub fn latency_ms(&self) -> f64 {
        let queued_ms =
            self.last_pending_frames as f64 * 1000.0 / self.device.sample_rate.max(1) as f64;
        self.stream_latency_ms.max(self.default_period_ms) + queued_ms
    }

    /// 抽干引擎缓冲，把 PCM 追加进 `out`
    ///
    /// 返回追加的**采样数**（i16 个数）。**绝不阻塞**：只轮询
    /// `GetNextPacketSize`，没有数据就立刻返回 0。
    pub fn read(&mut self, out: &mut Vec<i16>) -> Result<usize, CaptureError> {
        let dev_ch = self.device.channels.max(1) as usize;
        let req_ch = self.in_format.channels.max(1) as usize;
        let before = out.len();

        // 第一次查询既用来判断「有没有数据」，也顺手记下「此刻引擎里积压了多少」——
        // 后者才是延迟，而不是我们抽干之后的 0。
        // 两次调用拿的是同一个数，所以这里只查一次。
        // SAFETY: capture 由 self 持有，调用期间存活。
        let mut next = match unsafe { self.capture.GetNextPacketSize() } {
            Ok(n) => n,
            Err(e) => return Err(CaptureError::Wasapi(format!("GetNextPacketSize 失败：{e}"))),
        };
        self.last_pending_frames = next;

        for _ in 0..MAX_PACKETS_PER_READ {
            if next == 0 {
                break;
            }

            let mut data: *mut u8 = std::ptr::null_mut();
            let mut frames: u32 = 0;
            let mut flags: u32 = 0;
            let mut pos: u64 = 0;

            // SAFETY: 四个出参都是本栈上的有效变量。device position 是可选输出，
            // 传 Some(&mut pos) 让驱动顺手填一个 —— 这是判断「有没有丢帧」的唯一
            // 硬证据（我们自己数不出被引擎覆盖掉的部分）。
            if let Err(e) = unsafe {
                self.capture
                    .GetBuffer(&mut data, &mut frames, &mut flags, Some(&mut pos), None)
            } {
                return Err(CaptureError::Wasapi(format!("GetBuffer 失败：{e}")));
            }

            if frames > 0 {
                self.account_discontinuity(flags, pos, frames, req_ch);

                // 坑 3：SILENT 时 data 是 NULL，但「数据」确实存在，就是零。
                // 不解引用，直接填 0。
                let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
                if silent || data.is_null() {
                    push_silence(out, frames as usize, req_ch);
                } else {
                    let n = frames as usize * dev_ch;
                    // SAFETY: GetBuffer 保证 data 至少 frames × device_channels 个
                    // 采样可读；容器类型由 mix format 决定，与 kind 一致。
                    // WASAPI 的缓冲区至少按 16 字节对齐，满足 f32/i16 的对齐要求。
                    match self.kind {
                        SampleKind::F32 => {
                            let slice =
                                unsafe { std::slice::from_raw_parts(data as *const f32, n) };
                            convert_frames(out, DeviceSamples::F32(slice), dev_ch, req_ch);
                        }
                        SampleKind::I16 => {
                            let slice =
                                unsafe { std::slice::from_raw_parts(data as *const i16, n) };
                            convert_frames(out, DeviceSamples::I16(slice), dev_ch, req_ch);
                        }
                    }
                }
            }

            // ReleaseBuffer 必须与每一次成功的 GetBuffer 配对 ——
            // 漏掉的话引擎缓冲永远不归还，几秒后整个设备静默。
            // frames == 0 也要调用（驱动可能真的给了空包）。
            if let Err(e) = unsafe { self.capture.ReleaseBuffer(frames) } {
                return Err(CaptureError::Wasapi(format!("ReleaseBuffer 失败：{e}")));
            }

            // 问下一包。**必须在 ReleaseBuffer 之后**：归还之前引擎
            // 不会把新数据挂上来，提前查会永远拿到上一包的大小。
            // SAFETY: capture 由 self 持有，调用期间存活。
            next = match unsafe { self.capture.GetNextPacketSize() } {
                Ok(n) => n,
                Err(e) => return Err(CaptureError::Wasapi(format!("GetNextPacketSize 失败：{e}"))),
            };
        }

        let appended = out.len() - before;
        self.samples_captured += appended as u64;
        Ok(appended)
    }

    /// 用 device position 的跳变推算被引擎覆盖掉的帧数
    ///
    /// # 为什么要这么算
    ///
    /// loopback / 麦克风在调用方 tick 变慢时，引擎的环形缓冲会绕回，
    /// 旧数据被新数据覆盖 —— 这部分**我们从来没有机会读**，所以
    /// 光靠计数是发现不了的。驱动会置 `AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY`，
    /// 并且 device position 会跳变。两个证据一起用：
    /// 有 DISCONTINUITY 且 position 确实跳了，差额才算丢弃。
    ///
    /// 只信 DISCONTINUITY 不够（有些驱动无脑置位），只信 position 也不够
    /// （loopback 的 position 在无播放时不推进）。
    fn account_discontinuity(&mut self, flags: u32, pos: u64, frames: u32, req_ch: usize) {
        let discontinuity = flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0;
        if let Some(expected) = self.expected_pos {
            if discontinuity && pos > expected {
                let gap_frames = (pos - expected) as usize;
                // 丢弃量按**输出**口径折算：调用方看到的是 i16 单声道，
                // 报设备帧数会让日志里的数字对不上。
                self.samples_dropped += (gap_frames * req_ch) as u64;
            }
        }
        self.expected_pos = Some(pos.saturating_add(frames as u64));
    }
}

impl Drop for WasapiCapture {
    fn drop(&mut self) {
        // SAFETY: client 由 self 持有；Stop 对未启动的流也安全。
        unsafe {
            let _ = self.client.Stop();
        }
    }
}

/// 解析 `GetMixFormat` 的返回值
fn parse_mix_format(ptr: *mut WAVEFORMATEX) -> Result<DeviceFormat, CaptureError> {
    if ptr.is_null() {
        return Err(CaptureError::Wasapi("GetMixFormat 返回空指针".into()));
    }
    // SAFETY: ptr 由 GetMixFormat 分配且非空；WAVEFORMATEX 是 packed(1)，
    // 按值整体读取（`*ptr`）不会产生未对齐引用。
    let fmt = unsafe { *ptr };
    let tag = fmt.wFormatTag;
    let bits = fmt.wBitsPerSample;
    let channels = fmt.nChannels;
    let rate = fmt.nSamplesPerSec;
    let cb_size = fmt.cbSize;

    if channels == 0 || rate == 0 {
        return Err(CaptureError::UnsupportedFormat(format!(
            "设备混音格式非法：{rate}Hz/{channels}ch"
        )));
    }

    // 0xFFFE = WAVEFORMATEXTENSIBLE，真正的类型藏在 SubFormat 里。
    // 现代 Windows 的混音格式**几乎总是**这个 —— 直接看 wFormatTag
    // 会把 float32 误判成整数，然后听到满幅白噪声。
    let is_float = if tag == WAVE_FORMAT_EXTENSIBLE && cb_size >= 22 {
        // SAFETY: cbSize >= 22 说明后面确实跟着扩展部分；同样是 packed(1)，
        // 按值读 SubFormat 安全。
        let ext = ptr as *const WAVEFORMATEXTENSIBLE;
        let sub = unsafe { (*ext).SubFormat };
        sub == SUBTYPE_IEEE_FLOAT
    } else {
        tag == WAVE_FORMAT_IEEE_FLOAT
    };

    Ok(DeviceFormat::new(rate, channels, bits, is_float))
}

/// 取某个数据流方向默认端点的 id（诊断用）
fn default_endpoint_id(dataflow: windows::Win32::Media::Audio::EDataFlow) -> Option<String> {
    let com = ComGuard::init().ok()?;
    // SAFETY: COM 调用；返回的 PWSTR 由 CoTaskMemFree 释放。
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
        let device = enumerator
            .GetDefaultAudioEndpoint(dataflow, eConsole)
            .ok()?;
        let id = device.GetId().ok()?;
        let s = id.to_string().ok();
        CoTaskMemFree(Some(id.0 as *const c_void));
        drop(com);
        s
    }
}

/// 默认采集（麦克风）端点的 id
pub fn default_capture_device_id() -> Option<String> {
    default_endpoint_id(eCapture)
}

/// 默认渲染（播放）端点的 id —— loopback 采集的就是它
pub fn default_render_device_id() -> Option<String> {
    default_endpoint_id(eRender)
}
