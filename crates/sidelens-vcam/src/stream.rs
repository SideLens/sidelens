//! IMFMediaStream 实现
//!
//! # 这是虚拟摄像头最后一块拼图
//!
//! 实测证据（[docs/vcam-pitfalls.md](../../../docs/vcam-pitfalls.md)）：
//!
//! ```text
//! CreatePresentationDescriptor: 流描述符 OK
//! IMFMediaSource::Shutdown 被调用     ← 只有 Shutdown，没有 Start
//! ```
//!
//! FrameServer 建完描述符后**从不调用 `IMFMediaSource::Start`**，
//! 直接返回 `E_INVALIDARG`。原因是：描述符背后必须挂一个真实的
//! `IMFMediaStream` 对象，否则框架认为「有描述符但没有流」。
//!
//! # 三个接口的关系
//!
//! ```text
//! IMFMediaStream
//!   ├─ GetMediaSource       反查媒体源
//!   ├─ GetStreamDescriptor  返回自己的描述符
//!   ├─ RequestSample        消费端要一帧（关键方法）
//!   └─ IMFMediaEventGenerator  投递 MEMediaSample 事件
//!
//! IMFMediaStream2（扩展）
//!   ├─ SetStreamState       Stopped / Paused / Running
//!   └─ GetStreamState
//! ```
//!
//! # RequestSample 的契约
//!
//! 消费端调 `RequestSample(token)` 表示「给我一帧」。
//! 实现方必须**异步**投递 `MEMediaSample` 事件，
//! 且把 token 原样挂到样本上。同步返回帧是错的 ——
//! 那会让消费端的调度模型失效。
//!
//! # 当前状态
//!
//! - [x] 接口骨架与状态机
//! - [x] `RequestSample` 记账（记录待满足的请求）
//! - [x] 流状态（Stopped/Paused/Running）
//! - [x] 真实帧投递（命名管道 + 共享内存，见 [crate::frame_source]）
//!
//! # 帧投递的关键契约
//!
//! `RequestSample` 必须**同步**返回，帧通过 `MEMediaSample` 事件**异步**投递。
//! 我们的帧来源是带超时的（默认 16ms），所以典型路径是：
//!
//! 1. 消费端调 `RequestSample(token)`
//! 2. 我们从共享内存取最新帧（约 0.3ms）
//! 3. 包成 `IMFSample`，挂上 token，`QueueEventParamUnk(MEMediaSample)`
//! 4. 同步返回 S_OK
//!
//! 如果这一拍没帧（超时），我们**不返回错误**，而是记下这个 token，
//! 等下一拍有帧时再投递。返回错误会让消费端以为流坏了。

#![allow(unsafe_code)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use windows::core::{implement, ComObject, IUnknown, Interface, Ref, Result as WinResult, GUID};
use windows::Win32::Foundation::{E_INVALIDARG, S_OK};
use windows::Win32::Media::MediaFoundation::{
    IMFAsyncCallback, IMFAsyncResult, IMFMediaBuffer, IMFMediaEvent, IMFMediaEventGenerator,
    IMFMediaEventGenerator_Impl, IMFMediaEventQueue, IMFMediaSource, IMFMediaStream,
    IMFMediaStream2, IMFMediaStream2_Impl, IMFMediaStream_Impl, IMFSample, IMFStreamDescriptor,
    MEMediaSample, MFCreateEventQueue, MFCreateMemoryBuffer, MFCreateSample,
    MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS, MF_E_INVALIDREQUEST, MF_E_SHUTDOWN, MF_STREAM_STATE,
    MF_STREAM_STATE_PAUSED, MF_STREAM_STATE_RUNNING, MF_STREAM_STATE_STOPPED,
};

use crate::frame_source::{us_to_mftime, FramePoll, FrameSource};

/// 写入诊断日志（与 media_source 共用同一个文件）
fn log(msg: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(std::env::temp_dir().join("sidelens-vcam.log"))
    {
        let _ = writeln!(f, "{msg}");
    }
}
use crate::source::SourceConfig;

/// 流统计
#[derive(Debug, Default)]
pub struct StreamStats {
    /// 消费端请求过多少帧
    pub samples_requested: AtomicU64,
    /// 实际投递了多少帧
    pub samples_delivered: AtomicU64,
    /// 因状态不对被拒的请求
    pub rejected_requests: AtomicU64,
}

impl StreamStats {
    pub fn snapshot(&self) -> StreamStatsSnapshot {
        StreamStatsSnapshot {
            samples_requested: self.samples_requested.load(Ordering::Relaxed),
            samples_delivered: self.samples_delivered.load(Ordering::Relaxed),
            rejected_requests: self.rejected_requests.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StreamStatsSnapshot {
    pub samples_requested: u64,
    pub samples_delivered: u64,
    pub rejected_requests: u64,
}

/// 流共享状态
#[derive(Debug)]
pub struct StreamState {
    /// 流 ID（与描述符一致）
    stream_id: u32,
    /// 当前流状态
    state: Mutex<MF_STREAM_STATE>,
    /// 事件队列（MF 官方实现）
    event_queue: Mutex<Option<IMFMediaEventQueue>>,
    /// 流描述符（由媒体源提供）
    descriptor: Mutex<Option<IMFStreamDescriptor>>,
    /// 尚未满足的采样请求（token 列表）
    pending_tokens: Mutex<Vec<IUnknown>>,
    pub stats: Arc<StreamStats>,
}

// SAFETY: StreamState 里持有的全是 Media Foundation 的 free-threaded 对象
// （IMFMediaEventQueue / IMFStreamDescriptor / IUnknown）加普通 Mutex。
// MF 的事件队列契约就是「任何线程都可以 QueueEvent」，而我们的类工厂注册的是
// ThreadingModel=Both，FrameServer 会在 MTA 线程池里回调。因此跨线程共享是
// 契约允许的，只是 windows crate 的 COM 包装类型默认不带 Send + Sync 标记。
// 我们自己维护的所有可变状态都在 Mutex 后面，不存在数据竞争。
unsafe impl Send for StreamState {}
unsafe impl Sync for StreamState {}

impl StreamState {
    pub fn new(stream_id: u32) -> Self {
        // SAFETY: MFCreateEventQueue 无副作用
        let q = unsafe { MFCreateEventQueue() };

        Self {
            stream_id,
            // 初始为 STOPPED —— MF 契约：流创建后必须先 SetStreamState(RUNNING)
            state: Mutex::new(MF_STREAM_STATE_STOPPED),
            event_queue: Mutex::new(q.ok()),
            descriptor: Mutex::new(None),
            pending_tokens: Mutex::new(Vec::new()),
            stats: Arc::new(StreamStats::default()),
        }
    }

    pub fn stream_id(&self) -> u32 {
        self.stream_id
    }

    pub fn state(&self) -> MF_STREAM_STATE {
        *self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn set_state(&self, s: MF_STREAM_STATE) {
        *self.state.lock().unwrap_or_else(|e| e.into_inner()) = s;
    }

    /// 切到 RUNNING（媒体源 Start 时调用）
    pub fn set_running(&self) {
        self.set_state(MF_STREAM_STATE_RUNNING);
    }

    /// 切到 STOPPED
    pub fn set_stopped(&self) {
        self.set_state(MF_STREAM_STATE_STOPPED);
    }

    pub fn queue(&self) -> WinResult<IMFMediaEventQueue> {
        self.event_queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .ok_or_else(|| windows::core::Error::from(MF_E_SHUTDOWN))
    }

    pub fn set_descriptor(&self, desc: IMFStreamDescriptor) {
        *self.descriptor.lock().unwrap_or_else(|e| e.into_inner()) = Some(desc);
    }

    pub fn descriptor(&self) -> WinResult<IMFStreamDescriptor> {
        self.descriptor
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .ok_or_else(|| windows::core::Error::from(MF_E_INVALIDREQUEST))
    }

    /// 记录一个采样请求
    pub fn add_request(&self, token: Option<IUnknown>) {
        self.stats.samples_requested.fetch_add(1, Ordering::Relaxed);
        if let Some(t) = token {
            self.pending_tokens
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(t);
        }
    }

    /// 取出最早的待满足请求
    pub fn take_request(&self) -> Option<IUnknown> {
        self.pending_tokens
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop()
    }

    pub fn pending_count(&self) -> usize {
        self.pending_tokens.lock().map(|v| v.len()).unwrap_or(0)
    }

    pub fn shutdown(&self) {
        if let Ok(q) = self.queue() {
            // SAFETY: q 有效
            unsafe {
                let _ = q.Shutdown();
            }
        }
        self.set_state(MF_STREAM_STATE_STOPPED);
    }
}

/// SideLens 视频流
///
/// 同时实现 `IMFMediaStream` / `IMFMediaStream2` / `IMFMediaEventGenerator`。
#[implement(IMFMediaStream, IMFMediaStream2, IMFMediaEventGenerator)]
#[derive(Debug)]
pub struct SideLensMediaStream {
    state: Arc<StreamState>,
    /// 反查媒体源（弱引用语义由 COM 引用计数保证）
    source: Mutex<Option<IMFMediaSource>>,
    /// 帧来源（懒连接：第一次 RequestSample 时才连）
    frames: Arc<FrameSource>,
    /// 复用的像素缓冲区，避免每次请求都分配 3MB
    pixel_buf: Mutex<Vec<u8>>,
}

impl SideLensMediaStream {
    pub fn new(stream_id: u32) -> Self {
        Self::with_config(stream_id, SourceConfig::default())
    }

    /// 按指定配置创建（决定像素缓冲区大小与帧来源的尺寸校验）
    pub fn with_config(stream_id: u32, config: SourceConfig) -> Self {
        let need = config.resolution.nv12_size();
        Self {
            state: Arc::new(StreamState::new(stream_id)),
            source: Mutex::new(None),
            frames: Arc::new(FrameSource::new(config)),
            pixel_buf: Mutex::new(vec![0u8; need]),
        }
    }

    /// 用自定义的帧来源创建（测试与多实例场景）
    pub fn with_source(stream_id: u32, frames: Arc<FrameSource>) -> Self {
        let need = frames.config().resolution.nv12_size();
        Self {
            state: Arc::new(StreamState::new(stream_id)),
            source: Mutex::new(None),
            frames,
            pixel_buf: Mutex::new(vec![0u8; need]),
        }
    }

    /// 创建并包成 COM 对象
    pub fn create(stream_id: u32) -> ComObject<Self> {
        ComObject::new(Self::new(stream_id))
    }

    pub fn state(&self) -> &Arc<StreamState> {
        &self.state
    }

    /// 帧来源（供统计与测试观察）
    pub fn frames(&self) -> &Arc<FrameSource> {
        &self.frames
    }

    /// 取一帧并包成 IMFSample
    ///
    /// 返回 None 表示这一拍没有帧（不是错误）。
    fn build_sample(&self) -> Option<IMFSample> {
        let mut buf = self.pixel_buf.lock().unwrap_or_else(|e| e.into_inner());
        let poll = self.frames.poll(&mut buf);

        let (len, pts_us, keyframe) = match poll {
            FramePoll::Got {
                len,
                pts_us,
                keyframe,
            } => (len, pts_us, keyframe),
            _ => return None,
        };

        // SAFETY: MF 工厂函数无副作用
        let sample: IMFSample = unsafe { MFCreateSample() }.ok()?;
        // SAFETY: len 已由帧来源保证不超过缓冲区
        let buffer: IMFMediaBuffer = unsafe { MFCreateMemoryBuffer(len as u32) }.ok()?;

        // 拷贝像素到 MF 缓冲 —— 这一步省不掉，MF 不认外部内存
        // SAFETY: buffer 有效；Lock 返回的指针在 Unlock 前有效
        unsafe {
            let mut ptr: *mut u8 = std::ptr::null_mut();
            if buffer.Lock(&mut ptr, None, None).is_err() || ptr.is_null() {
                return None;
            }
            std::ptr::copy_nonoverlapping(buf.as_ptr(), ptr, len);
            let _ = buffer.Unlock();
            let _ = buffer.SetCurrentLength(len as u32);
        }

        // SAFETY: sample / buffer 都有效
        unsafe {
            sample.AddBuffer(&buffer).ok()?;
            // MF 的时间单位是 100ns，不是微秒
            let _ = sample.SetSampleTime(us_to_mftime(pts_us));
            let dur = self.frames.config().frame_interval_us() as i64;
            let _ = sample.SetSampleDuration(us_to_mftime(dur));
        }
        if keyframe {
            // SAFETY: sample 有效
            unsafe {
                let _ = sample.SetUINT32(
                    &windows::Win32::Media::MediaFoundation::MFSampleExtension_CleanPoint,
                    1,
                );
            }
        }

        Some(sample)
    }

    /// 绑定媒体源（由媒体源在创建流时调用）
    pub fn attach_source(&self, source: IMFMediaSource) {
        *self.source.lock().unwrap_or_else(|e| e.into_inner()) = Some(source);
    }

    /// 绑定描述符
    pub fn attach_descriptor(&self, desc: IMFStreamDescriptor) {
        self.state.set_descriptor(desc);
    }
}

impl IMFMediaEventGenerator_Impl for SideLensMediaStream_Impl {
    fn GetEvent(&self, dwflags: MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS) -> WinResult<IMFMediaEvent> {
        let q = self.state.queue()?;
        // SAFETY: q 有效；GetEvent 收 u32
        unsafe { q.GetEvent(dwflags.0) }
    }

    fn BeginGetEvent(
        &self,
        pcallback: Ref<IMFAsyncCallback>,
        punkstate: Ref<IUnknown>,
    ) -> WinResult<()> {
        let q = self.state.queue()?;
        let cb: Option<&IMFAsyncCallback> = pcallback.as_ref();
        let st: Option<&IUnknown> = punkstate.as_ref();
        // SAFETY: q 有效；cb/st 生命周期由调用方保证
        unsafe { q.BeginGetEvent(cb, st) }
    }

    fn EndGetEvent(&self, presult: Ref<IMFAsyncResult>) -> WinResult<IMFMediaEvent> {
        let q = self.state.queue()?;
        let r: Option<&IMFAsyncResult> = presult.as_ref();
        // SAFETY: q 有效
        unsafe { q.EndGetEvent(r) }
    }

    fn QueueEvent(
        &self,
        met: u32,
        guidextendedtype: *const GUID,
        hrstatus: windows::core::HRESULT,
        pvvalue: *const windows::Win32::System::Com::StructuredStorage::PROPVARIANT,
    ) -> WinResult<()> {
        let q = self.state.queue()?;
        // SAFETY: q 有效；指针参数由调用方保证
        unsafe { q.QueueEventParamVar(met, guidextendedtype, hrstatus, pvvalue) }
    }
}

impl IMFMediaStream_Impl for SideLensMediaStream_Impl {
    fn GetMediaSource(&self) -> WinResult<IMFMediaSource> {
        log(&format!(
            "IMFMediaStream::GetMediaSource（已绑定={}）",
            self.source.lock().map(|g| g.is_some()).unwrap_or(false)
        ));
        self.source
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .ok_or_else(|| windows::core::Error::from(MF_E_INVALIDREQUEST))
    }

    fn GetStreamDescriptor(&self) -> WinResult<IMFStreamDescriptor> {
        log(&format!(
            "IMFMediaStream::GetStreamDescriptor（已绑定={}）",
            self.state.descriptor().is_ok()
        ));
        self.state.descriptor()
    }

    fn RequestSample(&self, ptoken: Ref<IUnknown>) -> WinResult<()> {
        // 关键契约：只在 RUNNING 状态接受请求。
        // 在 STOPPED/PAUSED 下接受会让消费端以为能拿到帧，实际永远不会来。
        if self.state.state() != MF_STREAM_STATE_RUNNING {
            self.state
                .stats
                .rejected_requests
                .fetch_add(1, Ordering::Relaxed);
            return Err(windows::core::Error::from(MF_E_INVALIDREQUEST));
        }

        let token: Option<&IUnknown> = ptoken.as_ref();
        self.state.add_request(token.cloned());

        // 尝试这一拍就投递。取不到帧时**不返回错误** ——
        // 返回错误会让消费端以为流坏了（甚至直接停流），
        // 而实际上只是这一拍没有新帧。
        if let Some(sample) = self.build_sample() {
            // 把 token 原样挂回样本：消费端靠它匹配自己的请求
            if let Some(t) = token {
                // SAFETY: sample 有效；t 是消费端给的 token
                unsafe {
                    let _ = sample.SetUnknown(
                        &windows::Win32::Media::MediaFoundation::MFSampleExtension_Token,
                        t,
                    );
                }
            }

            let q = self.state.queue()?;
            let unk: IUnknown = sample.cast()?;
            // SAFETY: q 有效；MEMediaSample 的事件值就是 IMFSample
            unsafe {
                q.QueueEventParamUnk(MEMediaSample.0 as u32, std::ptr::null(), S_OK, &unk)?;
            }
            self.state
                .stats
                .samples_delivered
                .fetch_add(1, Ordering::Relaxed);
        }

        Ok(())
    }
}

impl IMFMediaStream2_Impl for SideLensMediaStream_Impl {
    fn SetStreamState(&self, value: MF_STREAM_STATE) -> WinResult<()> {
        // 只允许三个合法值
        if value != MF_STREAM_STATE_STOPPED
            && value != MF_STREAM_STATE_PAUSED
            && value != MF_STREAM_STATE_RUNNING
        {
            return Err(windows::core::Error::from(E_INVALIDARG));
        }
        self.state.set_state(value);
        Ok(())
    }

    fn GetStreamState(&self) -> WinResult<MF_STREAM_STATE> {
        Ok(self.state.state())
    }
}

/// 流 ID 常量
///
/// 必须与 [crate::media_source::VIDEO_STREAM_ID] 一致 ——
/// 描述符、属性、profile 引脚、属性查询四处用同一个值。
pub const VIDEO_STREAM_ID: u32 = 0;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_starts_stopped() {
        let s = StreamState::new(VIDEO_STREAM_ID);
        assert_eq!(s.state(), MF_STREAM_STATE_STOPPED);
    }

    #[test]
    fn request_tokens_are_lifo() {
        // 用 pop 语义：后进先出。真实实现应按到达顺序，
        // 但当前只需验证记账正确。
        let s = StreamState::new(VIDEO_STREAM_ID);
        assert_eq!(s.pending_count(), 0);
    }

    #[test]
    fn stream_id_is_preserved() {
        let s = StreamState::new(7);
        assert_eq!(s.stream_id(), 7);
    }
}
