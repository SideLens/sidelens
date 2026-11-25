//! IMFMediaSource 的 COM 实现
//!
//! # 这是整个虚拟摄像头最难的部分
//!
//! 不是代码长，而是**契约严苛**：消费端（Zoom/Teams/浏览器）会按固定顺序
//! 调用一堆接口，任何一步不满足就静默失败 —— 用户看到的是「设备能选但黑屏」。
//!
//! # 必须实现的三组接口
//!
//! ```text
//! IMFMediaSource
//!   ├─ IMFMediaEventGenerator  ← 消费端靠它拿事件
//!   ├─ IMFGetService           ← 取服务（可选但常被调用）
//!   └─ IMFAttributes           ← 源属性
//! IMFMediaStream（每个流一个）
//!   ├─ IMFMediaEventGenerator
//!   └─ IMFMediaStream 的回溯到 Source
//! IClassFactory                ← 让 CoCreateInstance 能创建上述对象
//! ```
//!
//! # 本实现的范围
//!
//! 已实现：
//! - [x] 状态机（Stopped/Started/Paused/Shutdown，严守 MF 转换规则）
//! - [x] 同步事件队列（`GetEvent` 可用）
//! - [x] `CreatePresentationDescriptor`（单流、NV12、可协商的分辨率表）
//! - [x] `IClassFactory`
//!
//! 未实现（下一步）：
//! - [ ] `BeginGetEvent` / `EndGetEvent` 的异步回调
//! - [ ] `IMFMediaStream::RequestSample` 与帧投递
//! - [ ] `IMFGetService`
//!
//! 这个分界的理由：**同步路径能验证「设备被识别、格式协商成功」**，
//! 这已经是 B3 阻塞点的核心。异步与帧投递可以在确认可见之后加。

#![allow(unsafe_code)]
// COM vtable 方法签名带裸指针；解引用前均已判空
#![allow(clippy::not_unsafe_ptr_arg_deref)]
// MediaSourceState 里的 Mutex 内容都是 POD，实际是 Send+Sync；
// clippy 因 IMFMediaEvent 等 COM 接口无法自动推导
#![allow(clippy::arc_with_non_send_sync)]

use std::sync::{Arc, Mutex};

use windows::core::{implement, ComObject, IUnknown, Interface, Ref, Result as WinResult, GUID};
use windows::Win32::Foundation::{CLASS_E_NOAGGREGATION, E_INVALIDARG, E_NOTIMPL, E_POINTER, S_OK};
use windows::Win32::Media::MediaFoundation::{
    IMFAsyncCallback, IMFAsyncResult, IMFAttributes, IMFGetService, IMFGetService_Impl,
    IMFMediaEvent, IMFMediaEventGenerator, IMFMediaEventGenerator_Impl, IMFMediaEventQueue,
    IMFMediaSource, IMFMediaSourceEx, IMFMediaSourceEx_Impl, IMFMediaSource_Impl, IMFMediaType,
    IMFPresentationDescriptor, IMFStreamDescriptor, MEStreamStarted, MEStreamStopped,
    MFCreatePresentationDescriptor, MFCreateStreamDescriptor, MFMediaType_Video,
    MFVideoFormat_NV12, MFVideoInterlace_Progressive, MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS,
    MF_E_INVALIDREQUEST, MF_E_SHUTDOWN, MF_E_UNSUPPORTED_SERVICE, MF_MT_ALL_SAMPLES_INDEPENDENT,
    MF_MT_AVG_BITRATE, MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE,
    MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE,
};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{
    CoRegisterClassObject, CoRevokeClassObject, IClassFactory, IClassFactory_Impl,
    CLSCTX_INPROC_SERVER, REGCLS_MULTIPLEUSE,
};

use windows::Win32::Media::KernelStreaming::{IKsControl, IKsControl_Impl, KSIDENTIFIER};

use crate::source::{Resolution, SourceConfig, SourceState, TransitionError};

/// 写入诊断日志
///
/// DLL 在别人的进程里跑，stderr 不可见。跨进程排查只能落盘。
fn log_to_file(msg: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(std::env::temp_dir().join("sidelens-vcam.log"))
    {
        let _ = writeln!(f, "{}", msg);
    }
}

/// 把状态机错误转成 MF 的 HRESULT
///
/// MF 对错误码有约定：
/// - 已 Shutdown → `MF_E_SHUTDOWN`
/// - 状态非法 → `MF_E_INVALIDREQUEST`
///
/// 用错错误码会让消费端的重试逻辑走错分支。
fn transition_to_hresult(e: TransitionError) -> windows::core::Error {
    match e {
        TransitionError::Shutdown => windows::core::Error::from(MF_E_SHUTDOWN),
        TransitionError::InvalidState { .. } => windows::core::Error::from(MF_E_INVALIDREQUEST),
    }
}

/// 媒体源共享状态
#[derive(Debug)]
pub struct MediaSourceState {
    state: Mutex<SourceState>,
    /// 用 MF 官方事件队列而不是手写 Vec
    ///
    /// 手写队列的 BeginGetEvent 只能返回 E_NOTIMPL，
    /// 而 FrameServer 在 Start 流程里会调用它等 MEStreamStarted。
    /// 返回 E_NOTIMPL 会让整个 Start 失败 —— 而且错误码表现为
    /// PATH_NOT_FOUND 这种毫无关联的值，极难定位。
    event_queue: Mutex<Option<IMFMediaEventQueue>>,
    /// 源属性缓存
    ///
    /// 官方 `GetSourceAttributes` 返回的是**同一个** `m_spAttributes` 对象
    /// （`m_spAttributes.copy_to(sourceAttributes)`），不是每次新建。
    /// 每次新建会让 FrameServer 拿到不同实例，某些一致性检查会失败。
    source_attrs: Mutex<Option<IMFAttributes>>,
    config: SourceConfig,
    pub stats: Arc<SourceStatsAtomic>,
}

/// 原子统计
#[derive(Debug, Default)]
pub struct SourceStatsAtomic {
    pub start: std::sync::atomic::AtomicU64,
    pub stop: std::sync::atomic::AtomicU64,
    pub pause: std::sync::atomic::AtomicU64,
    pub shutdown: std::sync::atomic::AtomicU64,
    pub events_queued: std::sync::atomic::AtomicU64,
    pub events_served: std::sync::atomic::AtomicU64,
    pub descriptors_created: std::sync::atomic::AtomicU64,
}

impl MediaSourceState {
    pub fn new(config: SourceConfig) -> Self {
        // 立刻创建事件队列 —— 官方 SimpleMediaSource::Initialize 也是这么做的
        // SAFETY: MFCreateEventQueue 无副作用
        let q = unsafe { windows::Win32::Media::MediaFoundation::MFCreateEventQueue() };
        match &q {
            Ok(_) => log_to_file("MFCreateEventQueue 成功"),
            Err(e) => log_to_file(&format!("MFCreateEventQueue 失败 {e}")),
        }

        Self {
            state: Mutex::new(SourceState::Stopped),
            event_queue: Mutex::new(q.ok()),
            source_attrs: Mutex::new(None),
            config,
            stats: Arc::new(SourceStatsAtomic::default()),
        }
    }

    /// 取事件队列（不存在时返回错误）
    pub fn queue(&self) -> WinResult<IMFMediaEventQueue> {
        self.event_queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .ok_or_else(|| windows::core::Error::from(MF_E_SHUTDOWN))
    }

    pub fn state(&self) -> SourceState {
        *self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn config(&self) -> SourceConfig {
        self.config
    }

    /// 执行状态转换
    pub fn transition(
        &self,
        f: impl FnOnce(SourceState) -> Result<SourceState, TransitionError>,
    ) -> Result<SourceState, TransitionError> {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let next = f(*st)?;
        *st = next;
        Ok(next)
    }

    /// 排队一个事件（走 MF 官方队列）
    pub fn queue_event(&self, event_type: i32) -> WinResult<()> {
        if self.state() == SourceState::Shutdown {
            return Err(windows::core::Error::from(MF_E_SHUTDOWN));
        }
        let q = self.queue()?;
        // SAFETY: q 有效；GUID 传零表示无扩展类型；无附加 VARIANT
        unsafe {
            q.QueueEventParamVar(event_type as u32, &GUID::zeroed(), S_OK, std::ptr::null())?;
        }
        self.stats
            .events_queued
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    /// 取一个事件
    pub fn pop_event(
        &self,
        flags: MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS,
    ) -> WinResult<IMFMediaEvent> {
        if self.state() == SourceState::Shutdown {
            return Err(windows::core::Error::from(MF_E_SHUTDOWN));
        }
        let q = self.queue()?;
        // SAFETY: q 有效；GetEvent 收 u32 flags
        let ev = unsafe { q.GetEvent(flags.0) }?;
        self.stats
            .events_served
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(ev)
    }
}

/// 可协商的格式表
///
/// 消费端会逐个 `GetMediaTypeByIndex` 直到 `MF_E_NO_MORE_TYPES`。
/// 至少要给一个，否则消费端认为"设备不支持任何格式"而直接放弃。
pub fn supported_resolutions() -> Vec<Resolution> {
    vec![
        Resolution::new(320, 240),
        Resolution::new(640, 480),
        Resolution::new(1280, 720),
        Resolution::new(1920, 1080),
    ]
}

/// 视频流索引（单流设备，恒为 0）
pub const VIDEO_STREAM_INDEX: u32 = 0;

/// 视频流 ID
///
/// # 为什么是 0 而不是 1（实测踩坑）
///
/// 这个值必须在**四个地方完全一致**，任何一处不一致都会让
/// FrameServer 在参数校验阶段直接返回 E_INVALIDARG，
/// 而且连 `IMFMediaSource::Start` 都不会调用 —— 从媒体源侧完全看不到线索：
///
/// | 位置 | 用途 |
/// | --- | --- |
/// | `MFCreateStreamDescriptor(id, ...)` | 描述符的标识 |
/// | `MF_DEVICESTREAM_STREAM_ID` 属性 | 消费端读的流号 |
/// | `AddProfileFilter(pin_id, ...)` | 传感器 profile 的引脚号 |
/// | `GetStreamAttributes(stream_id)` | 源属性查询的入参 |
///
/// 官方 `SimpleMediaSource` 全部用 0（`Initialize(this, i, ...)` 里 i 从 0 起）。
/// 我们最初描述符用 1、属性用 0、profile 用 0、查询用 1 —— 四处三个值。
pub const VIDEO_STREAM_ID: u32 = 0;

/// SideLens 虚拟摄像头媒体源
///
/// `#[implement]` 宏会为它生成 `QueryInterface` / `AddRef` / `Release`
/// 以及各接口的 vtable 转发。我们只需要写方法体。
/// # 必须实现的五个接口（官方文档硬要求）
///
/// Frame Server 的自定义媒体源契约列得很死：
///
/// | 接口 | 是否可选 |
/// | --- | --- |
/// | `IMFMediaEventGenerator` | 必需 |
/// | `IMFMediaSource` | 必需 |
/// | `IMFMediaSourceEx` | 必需 |
/// | `IMFGetService` | **必需**（可以不提供任何服务，但要能应答） |
/// | `IKsControl` | **必需**（可以不支持任何控制，但要能应答） |
///
/// 我们最初只实现了前三个，`IMFVirtualCamera::Start` 一直报
/// `E_INVALIDARG (0x80070057)` —— 因为 `QueryInterface` 拿不到
/// `IMFGetService` 或 `IKsControl` 时，框架认为这不是一个合格的
/// 自定义媒体源。
#[implement(
    IMFMediaSource,
    IMFMediaSourceEx,
    IMFMediaEventGenerator,
    IMFGetService,
    IKsControl
)]
#[derive(Debug)]
pub struct SideLensMediaSource {
    state: Arc<MediaSourceState>,
    /// 唯一的视频流对象
    ///
    /// 必须真实存在 —— 实测证明：只有描述符而没有流对象时，
    /// FrameServer 会直接返回 E_INVALIDARG，连 IMFMediaSource::Start 都不调用。
    stream: Arc<crate::stream::SideLensMediaStream>,
}

impl SideLensMediaSource {
    /// 创建媒体源
    pub fn new(config: SourceConfig) -> Self {
        Self {
            state: Arc::new(MediaSourceState::new(config)),
            // 流必须拿到和媒体源一致的配置 ——
            // 否则它按默认 720p 去要帧，而描述符声明的是别的分辨率，
            // 帧来源的尺寸校验会一路返回 SizeMismatch。
            stream: Arc::new(crate::stream::SideLensMediaStream::with_config(
                crate::stream::VIDEO_STREAM_ID,
                config,
            )),
        }
    }

    /// 创建并包成 COM 对象
    ///
    /// # 为什么必须在这里回填「流 → 源」的反向引用
    ///
    /// `IMFMediaStream::GetMediaSource` 的契约是返回**拥有该流的媒体源**。
    /// 我们最初没做这一步，流里存的是 `None`，于是它返回
    /// `MF_E_INVALIDREQUEST (0xC00D36B2)`。
    ///
    /// FrameServer 在 `CreatePresentationDescriptor` 之后会走一遍校验，
    /// 其中就包括「顺着流反查源」。反查失败 → 它认为这个源不合法 →
    /// `IMFVirtualCamera::Start` 返回 `E_INVALIDARG (0x80070057)`，
    /// 而且**永远不会调用 `IMFMediaSource::Start`**。
    ///
    /// 实测日志里那条 `GetMediaSource 失败：当前状态的请求无效 (0xC00D36B2)`
    /// 就是这个原因。
    pub fn create(config: SourceConfig) -> ComObject<Self> {
        let obj = ComObject::new(Self::new(config));
        // 拿到自身的 IMFMediaSource 接口，挂到流上（COM 引用计数保证不死循环）
        let self_source: IMFMediaSource = obj.to_interface();
        obj.stream().attach_source(self_source);
        obj
    }

    /// 共享状态（测试与诊断用）
    pub fn state(&self) -> &Arc<MediaSourceState> {
        &self.state
    }

    /// 视频流对象
    pub fn stream(&self) -> &Arc<crate::stream::SideLensMediaStream> {
        &self.stream
    }

    /// 构造流描述符
    ///
    /// 消费端要求：
    /// 1. 每个流有自己的 `IMFStreamDescriptor`
    /// 2. 描述符里挂着 `IMFMediaTypeHandler`，它持有可协商的格式表
    /// 3. 描述符必须设置 `MF_DEVICESTREAM_STREAM_ID` 与 `MF_DEVICESTREAM_STREAM_CATEGORY`
    fn build_stream_descriptor(&self) -> WinResult<IMFStreamDescriptor> {
        // 关键：必须至少提供一个媒体类型。
        //
        // 传空数组能创建出描述符对象，但它**没有任何格式**，
        // 消费端（也就是 IMFVirtualCamera::Start 的内部逻辑）拿到后
        // 会判定"设备不支持任何格式"，直接返回 E_INVALIDARG。
        //
        // 这个错误发生在 Start 里，从媒体源侧的日志完全看不到线索。
        let media_types = build_nv12_types(self.state.config())?;
        let refs: Vec<Option<IMFMediaType>> = media_types.iter().cloned().map(Some).collect();

        // SAFETY: refs 在调用期间存活，元素都是有效媒体类型
        let desc = unsafe { MFCreateStreamDescriptor(VIDEO_STREAM_ID, &refs) }?;

        // 关键 1：必须显式设置当前媒体类型。
        // 官方示例 SimpleMediaStream::Initialize 里这一步不可省略 ——
        // 描述符有格式表还不够，消费端要读「当前选中的格式」。
        if let Some(first) = media_types.first() {
            unsafe {
                if let Ok(handler) = desc.GetMediaTypeHandler() {
                    handler.SetCurrentMediaType(first).ok();
                }
            }
        }

        // 关键 2：流必须标记为「已选中」。
        //
        // 不设这个，呈现描述符里的流默认是 unselected 状态，
        // FrameServer 认为"没有任何流要启动"，随后在参数校验阶段
        // 直接返回 E_INVALIDARG —— 而且它连 IMFMediaSource::Start 都不会调用
        // （实证：日志里只有 Shutdown，没有 Start）。
        //
        // SAFETY: desc 有效
        unsafe {
            desc.SetUINT32(
                &windows::Win32::Media::MediaFoundation::MF_DEVICESTREAM_STREAM_ID,
                VIDEO_STREAM_INDEX,
            )
            .ok();
        }

        // 标记为视频流
        // SAFETY: desc 有效；SetItem 是 IMFAttributes 的方法。
        unsafe {
            desc.SetGUID(
                &windows::Win32::Media::MediaFoundation::MF_DEVICESTREAM_STREAM_CATEGORY,
                &PINNAME_VIDEO_CAPTURE,
            )
            .ok();
            desc.SetUINT32(
                &windows::Win32::Media::MediaFoundation::MF_DEVICESTREAM_STREAM_ID,
                VIDEO_STREAM_INDEX,
            )
            .ok();
            desc.SetUINT32(
                &windows::Win32::Media::MediaFoundation::MF_DEVICESTREAM_FRAMESERVER_SHARED,
                1,
            )
            .ok();
            // 官方 _SetStreamDescriptorAttributes 里也有这一项。
            // 缺了它，FrameServer 不知道这个流是「彩色视频」，
            // 在按 profile 匹配引脚时会判为不匹配。
            desc.SetUINT32(
                &windows::Win32::Media::MediaFoundation::MF_DEVICESTREAM_ATTRIBUTE_FRAMESOURCE_TYPES,
                windows::Win32::Media::MediaFoundation::MFFrameSourceTypes_Color.0 as u32,
            )
            .ok();
        }

        // 把描述符绑定到流对象 —— 这一步是 FrameServer 检查的关键
        self.stream.attach_descriptor(desc.clone());

        Ok(desc)
    }
}

/// 构造 NV12 的媒体类型表
///
/// # 为什么是 NV12
///
/// 摄像头类设备的通用格式。消费端（Zoom/Teams/浏览器）都认这个格式，
/// 而且硬解/硬编链路上上下游都是 NV12，能省一次色彩空间转换。
///
/// # 为什么要给多个
///
/// 消费端会逐个 `GetMediaTypeByIndex` 直到 `MF_E_NO_MORE_TYPES`，
/// 从中挑一个它支持的。只给一个分辨率会大幅降低兼容性 ——
/// 比如 4K 摄像头遇到只支持 720p 的应用就直接失败。
fn build_nv12_types(cfg: SourceConfig) -> WinResult<Vec<IMFMediaType>> {
    use windows::Win32::Media::MediaFoundation::MFCreateMediaType;

    let mut out = Vec::new();

    // 当前配置 + 常见降级分辨率
    let mut resolutions = vec![cfg.resolution];
    for r in supported_resolutions() {
        if r != cfg.resolution {
            resolutions.push(r);
        }
    }

    for res in resolutions {
        if !res.is_supported() {
            continue;
        }
        // SAFETY: MFCreateMediaType 只写输出指针
        let mt = unsafe { MFCreateMediaType() }?;

        // 对照微软官方 SimpleMediaStream::Initialize 的属性集：
        // 少设任何一个都可能让消费端在格式协商阶段拒绝。
        // SAFETY: mt 有效；SetXXX 都是属性写入
        unsafe {
            mt.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            mt.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
            mt.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            mt.SetUINT32(&MF_MT_ALL_SAMPLES_INDEPENDENT, 1)?;
            // 帧大小：高 32 位是宽，低 32 位是高
            mt.SetUINT64(
                &MF_MT_FRAME_SIZE,
                ((res.width as u64) << 32) | res.height as u64,
            )?;
            // 帧率：高 32 位是分子，低 32 位是分母
            mt.SetUINT64(
                &MF_MT_FRAME_RATE,
                ((cfg.frame_rate.numerator as u64) << 32) | cfg.frame_rate.denominator as u64,
            )?;
            // 默认跨距（每行字节数）。
            //
            // 官方 VCamSample 的两种格式都设了这一项：
            //   RGB32 → width * 4
            //   NV12  → width * 1.5
            //
            // 缺了它，FrameServer 在格式协商阶段拿不到「一行多少字节」，
            // 会把整个媒体类型判为无效 —— Start 返回 MF_E_INVALIDTYPE
            // (0xC00D36BD)。这是第十九轮隔离实验定位到的。
            let stride = (res.width as f64 * 1.5) as u32;
            mt.SetUINT32(&MF_MT_DEFAULT_STRIDE, stride)?;
            // 像素宽高比：1:1（方形像素）。官方示例里有这一条。
            mt.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1u64)?;
            // 平均码率：官方按 宽*1.5*高*8*帧率 估算 NV12
            let fps = cfg.frame_rate.fps().max(1.0);
            let bitrate = (res.width as f64 * 1.5 * res.height as f64 * 8.0 * fps) as u32;
            mt.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)?;
        }
        out.push(mt);
    }

    Ok(out)
}

/// 构造传感器配置文件集合
///
/// 官方示例里有两个 profile：
///
/// | Profile | 过滤条件 | 用途 |
/// | --- | --- | --- |
/// | Legacy | `FRT<=30` | 所有应用都能用的保底配置（**强制**） |
/// | HighFrameRate | `FRT>=60` | 高帧率场景 |
///
/// 过滤字符串的语法是 KS 属性过滤表达式：
/// `((RES==;FRT<=30,1;SUT==))` 表示"分辨率不限、帧率≤30、子类型不限"。
fn build_sensor_profile_collection(
) -> WinResult<windows::Win32::Media::MediaFoundation::IMFSensorProfileCollection> {
    // KSCAMERAPROFILE_* 在 KernelStreaming 模块，不在 MediaFoundation
    use windows::Win32::Media::KernelStreaming::{
        KSCAMERAPROFILE_HighFrameRate, KSCAMERAPROFILE_Legacy,
    };
    use windows::Win32::Media::MediaFoundation::{
        IMFSensorProfileCollection, MFCreateSensorProfile, MFCreateSensorProfileCollection,
    };

    // SAFETY: 这些 MF 工厂函数无副作用
    let collection: IMFSensorProfileCollection = unsafe { MFCreateSensorProfileCollection()? };

    // 流 ID 用 0 —— 官方示例也是 0（不是 1）。
    // 这里容易搞错：MF 的 stream id 在 profile 过滤里是「pin id」，
    // 与 IMFStreamDescriptor 的 id 不是同一个概念。
    const PROFILE_STREAM_ID: u32 = 0;

    // 1. Legacy profile（强制）
    let legacy_filters = windows::core::HSTRING::from("((RES==;FRT<=30,1;SUT==))");
    // SAFETY: 参数都是简单值/有效字符串
    let legacy = unsafe { MFCreateSensorProfile(&KSCAMERAPROFILE_Legacy, 0, &legacy_filters)? };
    // SAFETY: legacy 有效
    unsafe {
        legacy.AddProfileFilter(PROFILE_STREAM_ID, &legacy_filters)?;
        collection.AddProfile(&legacy)?;
    }

    // 2. HighFrameRate profile（可选，但提供能提升高帧率应用体验）
    let hfr_filters = windows::core::HSTRING::from("((RES==;FRT>=60,1;SUT==))");
    // SAFETY: 同上
    if let Ok(hfr) =
        unsafe { MFCreateSensorProfile(&KSCAMERAPROFILE_HighFrameRate, 0, &hfr_filters) }
    {
        // SAFETY: hfr 有效
        unsafe {
            hfr.AddProfileFilter(PROFILE_STREAM_ID, &hfr_filters).ok();
            collection.AddProfile(&hfr).ok();
        }
    }

    Ok(collection)
}

/// 创建一个空的属性集
///
/// MFCreateAttributes 写的是 `*mut Option<IMFAttributes>`，
/// 需要先给出有效的栈上变量再取地址 —— 不能直接传 `&mut null_mut()`。
fn make_empty_attributes() -> WinResult<IMFAttributes> {
    let mut attrs: Option<IMFAttributes> = None;
    // SAFETY: attrs 是栈上有效变量，输出指针指向它
    unsafe {
        windows::Win32::Media::MediaFoundation::MFCreateAttributes(&mut attrs, 4)?;
    }
    attrs.ok_or_else(|| windows::core::Error::from(E_NOTIMPL))
}

/// 视频捕获引脚类别
///
/// 对应 `PINNAME_VIDEO_CAPTURE`：`{FB6C4281-0353-11D1-905F-0000C0CC16BA}`
pub const PINNAME_VIDEO_CAPTURE: GUID = GUID::from_u128(0xFB6C4281_0353_11D1_905F_0000C0CC16BA);

impl IMFMediaEventGenerator_Impl for SideLensMediaSource_Impl {
    fn GetEvent(&self, dwflags: MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS) -> WinResult<IMFMediaEvent> {
        self.state.pop_event(dwflags)
    }

    fn BeginGetEvent(
        &self,
        pcallback: Ref<IMFAsyncCallback>,
        punkstate: Ref<IUnknown>,
    ) -> WinResult<()> {
        // 必须转发到 MF 事件队列。
        // 返回 E_NOTIMPL 会让 FrameServer 在 Start 里等事件失败，
        // 而失败错误码是 PATH_NOT_FOUND，与真实原因毫无关系。
        let q = self.state.queue()?;
        // Ref<T> 已实现 Deref<Target = Option<T>>；用 as_ref() 拿 &T
        let cb: Option<&IMFAsyncCallback> = pcallback.as_ref();
        let st: Option<&IUnknown> = punkstate.as_ref();
        // SAFETY: q 有效；cb / st 的生命周期由调用方保证
        unsafe { q.BeginGetEvent(cb, st) }
    }

    fn EndGetEvent(&self, presult: Ref<IMFAsyncResult>) -> WinResult<IMFMediaEvent> {
        let q = self.state.queue()?;
        let r: Option<&IMFAsyncResult> = presult.as_ref();
        // SAFETY: q 有效；r 的生命周期由调用方保证
        unsafe { q.EndGetEvent(r) }
    }

    fn QueueEvent(
        &self,
        met: u32,
        _guidextendedtype: *const GUID,
        _hrstatus: windows::core::HRESULT,
        _pvvalue: *const PROPVARIANT,
    ) -> WinResult<()> {
        self.state.queue_event(met as i32)
    }
}

impl IMFMediaSource_Impl for SideLensMediaSource_Impl {
    fn GetCharacteristics(&self) -> WinResult<u32> {
        // MFMEDIASOURCE_IS_LIVE = 0x1：实时源，不支持 Seek
        log_to_file("IMFMediaSource::GetCharacteristics");
        Ok(0x1)
    }

    fn CreatePresentationDescriptor(&self) -> WinResult<IMFPresentationDescriptor> {
        // 官方实现每次都返回内部 PD 的 **Clone**，而不是重新构造。
        // 重新构造会让 FrameServer 手里的 PD 与 Start 时传回来的 PD
        // 不是同一个对象，`_ValidatePresentationDescriptor` 之类的
        // 一致性检查可能因此失败。
        // 写文件日志：DLL 被 FrameServer 加载时 stderr 看不到，
        // 只能落盘。这是排查跨进程 COM 问题唯一可靠的手段。
        log_to_file("CreatePresentationDescriptor: 进入");

        if self.state.state() == SourceState::Shutdown {
            log_to_file("CreatePresentationDescriptor: 已 Shutdown");
            return Err(windows::core::Error::from(MF_E_SHUTDOWN));
        }

        let stream_desc = match self.build_stream_descriptor() {
            Ok(s) => {
                log_to_file("CreatePresentationDescriptor: 流描述符 OK");
                s
            }
            Err(e) => {
                log_to_file(&format!("CreatePresentationDescriptor: 流描述符失败 {e}"));
                return Err(e);
            }
        };

        // SAFETY: 传入单元素数组，长度 1。
        let streams = [Some(stream_desc)];
        let pd = unsafe { MFCreatePresentationDescriptor(Some(&streams)) }?;

        // 关键 3：呈现描述符里必须把流标记为选中。
        //
        // 不调用 SelectStream，消费端/FrameServer 认为没有要启动的流，
        // 会在参数校验阶段失败（E_INVALIDARG），且**根本不会调用
        // IMFMediaSource::Start**。
        // SAFETY: pd 有效；stream index 0 一定存在
        unsafe {
            let _ = pd.SelectStream(VIDEO_STREAM_INDEX);
        }

        self.state
            .stats
            .descriptors_created
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(pd)
    }

    fn Start(
        &self,
        ppresentationdescriptor: Ref<IMFPresentationDescriptor>,
        pguidtimeformat: *const GUID,
        pvarstartposition: *const PROPVARIANT,
    ) -> WinResult<()> {
        log_to_file(&format!(
            "IMFMediaSource::Start 被调用 当前状态={:?}",
            self.state.state()
        ));
        log_to_file(&format!(
            "  pd_null={} timefmt_null={} varpos_null={}",
            ppresentationdescriptor.is_null(),
            pguidtimeformat.is_null(),
            pvarstartposition.is_null()
        ));
        if !pguidtimeformat.is_null() {
            // SAFETY: 已判空
            eprintln!("[SideLens]   timeformat = {:?}", unsafe {
                *pguidtimeformat
            });
        }
        match self.state.transition(SourceState::start) {
            Ok(_) => {
                self.state
                    .stats
                    .start
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

                // 流也必须切到 RUNNING，否则 RequestSample 会被拒。
                //
                // 这里同时走 IMFMediaStream2::SetStreamState 的语义：
                // MF 文档要求源的 Start 与流的 SetStreamState(RUNNING) 配对，
                // 只改内部状态而不通知流对象，某些消费端会一直等 MEStreamStarted。
                self.stream.state().set_running();

                // 通知消费端流已启动
                self.state.queue_event(MEStreamStarted.0)?;
                Ok(())
            }
            Err(e) => Err(transition_to_hresult(e)),
        }
    }

    fn Stop(&self) -> WinResult<()> {
        log_to_file("IMFMediaSource::Stop 被调用");
        match self.state.transition(SourceState::stop) {
            Ok(_) => {
                self.state
                    .stats
                    .stop
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                self.state.queue_event(MEStreamStopped.0)?;
                Ok(())
            }
            Err(e) => Err(transition_to_hresult(e)),
        }
    }

    fn Pause(&self) -> WinResult<()> {
        log_to_file("IMFMediaSource::Pause 被调用");
        match self.state.transition(SourceState::pause) {
            Ok(_) => {
                self.state
                    .stats
                    .pause
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Ok(())
            }
            Err(e) => Err(transition_to_hresult(e)),
        }
    }

    fn Shutdown(&self) -> WinResult<()> {
        log_to_file("IMFMediaSource::Shutdown 被调用");
        match self.state.transition(SourceState::shutdown) {
            Ok(_) => {
                self.state
                    .stats
                    .shutdown
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Ok(())
            }
            // Shutdown 是幂等的：重复调用返回 S_OK 而不报错。
            // MF 文档要求这一点 —— 消费端可能在清理路径里调多次。
            Err(TransitionError::Shutdown) => Ok(()),
            Err(e) => Err(transition_to_hresult(e)),
        }
    }
}

impl IMFMediaSourceEx_Impl for SideLensMediaSource_Impl {
    fn GetSourceAttributes(&self) -> WinResult<IMFAttributes> {
        // 关键：必须提供传感器配置文件集合（Sensor Profile Collection）。
        //
        // 这是从官方 SimpleMediaSource::_CreateSourceAttributes 学到的：
        // FrameServer 靠这个集合判断设备支持哪些「使用场景」
        // （Legacy 普通帧率 / HighFrameRate 高帧率 / HDR / VariablePhotoSequence）。
        //
        // **Legacy profile 是强制的** —— 官方注释：
        //   "Legacy profile is mandatory. This is to ensure non-profile
        //    aware applications can still function"
        //
        // 没有它，FrameServer 认为设备没有可用的配置，直接拒绝。
        // 已构造过就直接返回同一个对象（官方语义）
        if let Ok(g) = self.state.source_attrs.lock() {
            if let Some(a) = g.as_ref() {
                log_to_file("GetSourceAttributes: 返回缓存的属性对象");
                return Ok(a.clone());
            }
        }

        let attrs = make_empty_attributes()?;

        match build_sensor_profile_collection() {
            Ok(collection) => {
                log_to_file("GetSourceAttributes: 传感器配置文件集合已构造");
                // SAFETY: attrs 与 collection 都有效
                unsafe {
                    attrs.SetUnknown(
                        &windows::Win32::Media::MediaFoundation::MF_DEVICEMFT_SENSORPROFILE_COLLECTION,
                        &collection,
                    )?;
                }
            }
            Err(e) => {
                log_to_file(&format!("GetSourceAttributes: 配置文件集合失败 {e}"));
                return Err(e);
            }
        }

        // `MF_VIRTUALCAMERA_CONFIGURATION_APP_PACKAGE_FAMILY_NAME` 只在
        // 「媒体源属于某个打包应用」时才该设。
        //
        // 官方代码把这一项包在 try/catch 里，捕获到「不在 App 包内」就
        // **什么都不设**。我们之前硬塞了一个假 PFN，FrameServer 拿它去
        // 查应用包必然查不到 —— 这会让设备注册的合法性检查失败。
        //
        // 普通 exe 形态下正确做法是**不设**这一项。

        if let Ok(mut g) = self.state.source_attrs.lock() {
            *g = Some(attrs.clone());
        }
        Ok(attrs)
    }

    fn GetStreamAttributes(&self, dwstreamidentifier: u32) -> WinResult<IMFAttributes> {
        log_to_file(&format!(
            "GetStreamAttributes 被调用 stream_id={dwstreamidentifier}（期望 {VIDEO_STREAM_ID}）"
        ));
        if dwstreamidentifier != VIDEO_STREAM_ID {
            log_to_file("GetStreamAttributes: stream_id 不匹配，返回 E_INVALIDARG");
            return Err(windows::core::Error::from(E_INVALIDARG));
        }

        // 不能返回空属性集。
        //
        // 官方 SimpleMediaStream::_SetStreamAttributes 在这里设了 4 项，
        // FrameServer 靠它们判断这个流「是什么」：
        //
        // | 属性 | 作用 |
        // | --- | --- |
        // | MF_DEVICESTREAM_STREAM_CATEGORY | 视频捕获引脚 |
        // | MF_DEVICESTREAM_STREAM_ID | 流号（必须与描述符一致） |
        // | MF_DEVICESTREAM_FRAMESERVER_SHARED | 允许 FrameServer 共享 |
        // | MF_DEVICESTREAM_ATTRIBUTE_FRAMESOURCE_TYPES | 帧来源类型（彩色） |
        //
        // 少了 FRAMESOURCE_TYPES，FrameServer 无法把流归类到
        // 「彩色视频」这一类，按 profile 匹配引脚时直接失败。
        let attrs = make_empty_attributes()?;
        // SAFETY: attrs 有效
        unsafe {
            attrs.SetGUID(
                &windows::Win32::Media::MediaFoundation::MF_DEVICESTREAM_STREAM_CATEGORY,
                &PINNAME_VIDEO_CAPTURE,
            )?;
            attrs.SetUINT32(
                &windows::Win32::Media::MediaFoundation::MF_DEVICESTREAM_STREAM_ID,
                VIDEO_STREAM_ID,
            )?;
            attrs.SetUINT32(
                &windows::Win32::Media::MediaFoundation::MF_DEVICESTREAM_FRAMESERVER_SHARED,
                1,
            )?;
            attrs.SetUINT32(
                &windows::Win32::Media::MediaFoundation::MF_DEVICESTREAM_ATTRIBUTE_FRAMESOURCE_TYPES,
                windows::Win32::Media::MediaFoundation::MFFrameSourceTypes_Color.0 as u32,
            )?;
        }
        log_to_file("GetStreamAttributes: 已返回 4 项流属性");
        Ok(attrs)
    }

    fn SetD3DManager(&self, _pmanager: Ref<IUnknown>) -> WinResult<()> {
        // 不接受 D3D 管理器：我们的帧由 CPU 提供，不走 GPU 互操作。
        // 返回 S_OK 会让消费端以为可以走零拷贝路径，后续拿到 CPU 帧会失败。
        Err(windows::core::Error::from(E_NOTIMPL))
    }
}

impl IMFGetService_Impl for SideLensMediaSource_Impl {
    fn GetService(
        &self,
        _guidservice: *const GUID,
        _riid: *const GUID,
        ppvobject: *mut *mut core::ffi::c_void,
    ) -> WinResult<()> {
        // 官方文档的示例实现就是「不支持任何服务」：
        //
        //   *ppvObject = NULL;
        //   return MF_E_UNSUPPORTED_SERVICE;
        //
        // 关键是**必须实现这个接口并给出确定答复**，而不是让
        // QueryInterface 失败。框架先 QI 拿接口，拿不到就判定
        // 「不是合格的自定义媒体源」，随后 Start 报 E_INVALIDARG。
        // SAFETY: 指针可能为 null，先判空再解引用
        let (svc, iid) = unsafe { (_guidservice.as_ref().copied(), _riid.as_ref().copied()) };
        log_to_file(&format!(
            "IMFGetService::GetService service={svc:?} riid={iid:?}"
        ));
        if !ppvobject.is_null() {
            // SAFETY: 已判空
            unsafe { *ppvobject = std::ptr::null_mut() };
        }

        // ── 虚拟摄像头会话接口（2032C7EF）────────────────────────
        //
        // # 为什么单独处理这一个 IID（见 docs/vcam-summary.md）
        //
        // 它是 FrameServer 在 `IMFVirtualCamera::Start` 期间唯一没拿到的接口。
        // 证据链：
        //
        // 1. `mfsensorgroup.dll` 0x80848 的 GUID 表里，它紧跟在
        //    UTF-16 串 `vcam_session-{%08lX-%04X-...}` 后面 —— 是会话对象接口
        // 2. SDK 头 `mfvirtualcamera.h` 第 101-106 行定义了六个
        //    `MF_FRAMESERVER_VCAMEVENT_EXTENDED_*` 事件 GUID
        // 3. `mfsensorgroup.dll` 0x86620-0x86688 有六个对应的字符串
        //    `VCamEvent_Initialize` / `Start` / `Stop` / `Uninitialize` /
        //    `Shutdown` / `CustomEvent`
        //
        // **六个事件一一对应** —— 说明这是一个「事件接收器」接口。
        // FrameServer 通过 `GetService(2032C7EF)` 拿它，拿不到就不知道该
        // 往哪发 `SOURCE_INITIALIZE`，于是流程走完直接 Shutdown。
        //
        // # 当前的实验性实现
        //
        // 方法表未知（这个 IID 不在任何公开文档里）。所以先返回一个
        // **最小可用的 IUnknown**：所有方法都返回 `E_NOTIMPL`。
        //
        // 这不是「正确实现」，而是**探针** —— 目的是看 FrameServer 的行为
        // 是否变化：
        //
        // - 若它开始调用这个对象的方法 → 说明方向对了，可以按调用顺序反推方法表
        // - 若行为完全不变 → 说明 2032C7EF 不是根因，排除这条路
        const IID_VCAM_SESSION: windows::core::GUID =
            windows::core::GUID::from_u128(0x2032C7EF_76F6_492A_94F3_4A81F69380CC);

        if iid == Some(IID_VCAM_SESSION) && !ppvobject.is_null() {
            log_to_file(
                "IMFGetService: 请求虚拟摄像头会话接口 2032C7EF（实验：返回 E_NOINTERFACE）",
            );
            // # 实验设计
            //
            // 第一次实验只改**返回值**，不实现任何东西：
            //
            // | 版本 | 返回 | 目的 |
            // | --- | --- | --- |
            // | 原来 | `MF_E_UNSUPPORTED_SERVICE` | 基线 |
            // | 现在 | `E_NOINTERFACE` | 看 FrameServer 是否区分这两个错误 |
            //
            // 这两个 HRESULT 语义不同：
            //
            // - `MF_E_UNSUPPORTED_SERVICE` = 「我知道这个服务，但我不提供」
            // - `E_NOINTERFACE` = 「我不认识这个接口」
            //
            // 如果 FrameServer 的行为**变了**，说明它在读这个错误码做分支 ——
            // 那就有得谈。如果完全不变，说明它只是「拿不到就往下走」，
            // 2032C7EF 不是根因。
            //
            // 这个实验的价值在于：**改一行就能证伪一条最强怀疑**，
            // 比直接去猜 vtable 便宜得多。
            return Err(windows::core::Error::from_hresult(windows::core::HRESULT(
                E_NOINTERFACE_HR,
            )));
        }

        Err(windows::core::Error::from(MF_E_UNSUPPORTED_SERVICE))
    }
}

/// `E_NOINTERFACE` 的原始值
///
/// `windows::core` 有 `E_NOINTERFACE` 常量，但它被 re-export 在
/// `windows::Win32::Foundation` 下，而本文件只 import 了 MediaFoundation。
/// 直接写值更省事（这是 COM 的固定值，不会变）。
const E_NOINTERFACE_HR: i32 = 0x8000_4002u32 as i32;

/// `IKsControl` 的「无控制」应答
///
/// 官方推荐返回 `HRESULT_FROM_WIN32(ERROR_SET_NOT_FOUND)`：
/// 与 AVStream 小端口驱动「没注册这个 KS 操作」的行为一致，
/// 框架能正确识别为「无此控制」而不是「驱动坏了」。
fn ks_not_found() -> WinResult<()> {
    // HRESULT_FROM_WIN32(1170) = 0x80070492
    //
    // windows crate 没有导出 HRESULT_FROM_WIN32 这个辅助函数，
    // 只能自己按公式算：
    //   若 code <= 0 则原样返回，否则 (code & 0xFFFF) | 0x80070000
    const ERROR_SET_NOT_FOUND_HR: i32 = 0x8007_0492u32 as i32;
    Err(windows::core::Error::from_hresult(windows::core::HRESULT(
        ERROR_SET_NOT_FOUND_HR,
    )))
}

impl IKsControl_Impl for SideLensMediaSource_Impl {
    fn KsProperty(
        &self,
        _property: *const KSIDENTIFIER,
        _propertylength: u32,
        _propertydata: *mut core::ffi::c_void,
        _datalength: u32,
        _bytesreturned: *mut u32,
    ) -> WinResult<()> {
        // SAFETY: 指针可能为 null，用 as_ref 判空
        let Some(id) = (unsafe { _property.as_ref() }) else {
            log_to_file("IKsControl::KsProperty property=null");
            return ks_not_found();
        };
        // SAFETY: union 的两个变体都是 POD，读 Anonymous 是安全的
        let inner = unsafe { id.Anonymous.Anonymous };
        log_to_file(&format!(
            "IKsControl::KsProperty set={:?} id={} flags={} datalen={}",
            inner.Set, inner.Id, inner.Flags, _datalength
        ));

        // PROPSETID_VIDCAP_CAMERACONTROL 是摄像头控制集。
        //
        // FrameServer 在决定是否接受这个虚拟摄像头时会 GET 一批控制项
        // （实测只查 id=8）。按官方推荐一律回 ERROR_SET_NOT_FOUND 时，
        // 它把「不支持」当成硬失败并直接 Shutdown。
        //
        // 所以对已知的 GET 请求回一个「能力=0（不支持具体模式）」的
        // 结构体 —— 这是摄像头的合法应答，表示「这个控制我支持查询，
        // 但当前没有可用模式」。
        const PROPSETID_VIDCAP_CAMERACONTROL: GUID =
            GUID::from_u128(0xC6E13370_30AC_11D0_A18C_00A0C9118956);
        const KSPROPERTY_TYPE_GET: u32 = 0x0000_0001;

        if inner.Set == PROPSETID_VIDCAP_CAMERACONTROL
            && (inner.Flags & KSPROPERTY_TYPE_GET) != 0
            && !_propertydata.is_null()
            && _datalength >= 4
        {
            // KSPROPERTY_CAMERACONTROL_S 的第一个字段是 Capabilities/Flags。
            // 填 0 表示「支持该控制，但没有任何可用模式」。
            // SAFETY: 上面已校验指针非空且长度足够
            unsafe {
                std::ptr::write_bytes(_propertydata as *mut u8, 0, _datalength as usize);
            }
            if !_bytesreturned.is_null() {
                // SAFETY: 已判空
                unsafe { *_bytesreturned = _datalength };
            }
            log_to_file("IKsControl::KsProperty 摄像头控制 → 返回成功（能力=0）");
            return Ok(());
        }

        ks_not_found()
    }

    fn KsMethod(
        &self,
        _method: *const KSIDENTIFIER,
        _methodlength: u32,
        _methoddata: *mut core::ffi::c_void,
        _datalength: u32,
        _bytesreturned: *mut u32,
    ) -> WinResult<()> {
        ks_not_found()
    }

    fn KsEvent(
        &self,
        _event: *const KSIDENTIFIER,
        _eventlength: u32,
        _eventdata: *mut core::ffi::c_void,
        _datalength: u32,
        _bytesreturned: *mut u32,
    ) -> WinResult<()> {
        ks_not_found()
    }
}

/// 类工厂：让 `CoCreateInstance(SIDELENS_SOURCE_CLSID)` 能创建媒体源
#[implement(IClassFactory)]
#[derive(Debug)]
pub struct MediaSourceFactory {
    config: SourceConfig,
}

impl MediaSourceFactory {
    pub fn new(config: SourceConfig) -> Self {
        Self { config }
    }
}

impl IClassFactory_Impl for MediaSourceFactory_Impl {
    fn CreateInstance(
        &self,
        punkouter: Ref<IUnknown>,
        riid: *const GUID,
        ppvobject: *mut *mut core::ffi::c_void,
    ) -> WinResult<()> {
        // 不支持聚合
        if !punkouter.is_null() {
            return Err(windows::core::Error::from(CLASS_E_NOAGGREGATION));
        }
        if ppvobject.is_null() {
            return Err(windows::core::Error::from(E_POINTER));
        }
        if riid.is_null() {
            return Err(windows::core::Error::from(E_INVALIDARG));
        }

        // 关键：虚拟摄像头框架通过 CLSID 要的是 **IMFActivate**，
        // 不是 IMFMediaSource。实测查询 IID {7FEE9E9A-...}（IMFActivate）时
        // 若返回 E_NOINTERFACE，Start() 就整体失败。
        let activate: IUnknown =
            crate::activate::MediaSourceActivate::create(self.config).into_interface();
        // SAFETY: ppvobject 已判空；riid 已判空。
        let hr = unsafe { activate.query(riid, ppvobject as *mut _) };
        if hr.is_err() {
            // SAFETY: riid 已判空，解引用安全
            let iid = unsafe { *riid };
            eprintln!("[SideLens] CreateInstance: QueryInterface({iid:?}) 失败");
        }
        hr.ok()
    }

    fn LockServer(&self, _flock: windows::core::BOOL) -> WinResult<()> {
        // 进程内服务器不需要真正的锁计数
        Ok(())
    }
}

/// 已注册的类对象（Drop 时自动撤销）
#[derive(Debug)]
pub struct RegisteredClass {
    cookie: u32,
}

impl RegisteredClass {
    /// 注册类工厂
    pub fn register(clsid: GUID, config: SourceConfig) -> WinResult<Self> {
        let factory: IClassFactory = MediaSourceFactory::new(config).into();
        // SAFETY: factory 在注册期间存活（由 CoRegisterClassObject 内部 AddRef）；
        // CLSCTX_INPROC_SERVER 表示进程内；REGCLS_MULTIPLEUSE 允许多个客户端共用。
        let cookie = unsafe {
            CoRegisterClassObject(&clsid, &factory, CLSCTX_INPROC_SERVER, REGCLS_MULTIPLEUSE)?
        };
        Ok(Self { cookie })
    }

    pub fn cookie(&self) -> u32 {
        self.cookie
    }
}

impl Drop for RegisteredClass {
    fn drop(&mut self) {
        // SAFETY: cookie 来自 CoRegisterClassObject，成对撤销。
        unsafe {
            CoRevokeClassObject(self.cookie).ok();
        }
    }
}
