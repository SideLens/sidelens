//! IMFAsyncCallback 实现
//!
//! # 为什么必须有
//!
//! `IMFVirtualCamera::Start(callback)` 的 callback 参数**不能传 null**。
//! 实测传 null 返回 `E_INVALIDARG`（0x80070057），
//! 而且这个失败发生在 `IMFMediaSource::Start` 被调用之前 ——
//! 从媒体源侧的日志里完全看不到线索，这个坑卡了很久。
//!
//! 框架用这个回调通知「消费端已连接 / 已断开」这类设备级事件。
//!
//! # 线程模型
//!
//! callback 可能在任意 MTA 线程被调用，所以实现必须线程安全（本实现只计数）。

#![allow(unsafe_code)]
// COM 回调签名带裸指针，解引用前已判空；不标 unsafe fn 以免污染调用方
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use windows::core::{implement, ComObject, Ref, Result as WinResult};
use windows::Win32::Media::MediaFoundation::{
    IMFAsyncCallback, IMFAsyncCallback_Impl, IMFAsyncResult,
};

/// 回调统计
#[derive(Debug, Default)]
pub struct CallbackStats {
    pub invoked: AtomicU64,
}

/// 空的异步回调
///
/// 只记录被调用次数，不做处理 —— 当前实现不依赖设备级事件。
/// 但**必须存在**，否则 `IMFVirtualCamera::Start` 直接拒绝。
#[implement(IMFAsyncCallback)]
#[derive(Debug)]
pub struct VcamCallback {
    pub stats: Arc<CallbackStats>,
}

impl VcamCallback {
    pub fn new() -> Self {
        Self {
            stats: Arc::new(CallbackStats::default()),
        }
    }

    /// 创建并转成 IMFAsyncCallback 接口
    pub fn create() -> IMFAsyncCallback {
        let obj: ComObject<VcamCallback> = ComObject::new(Self::new());
        obj.into_interface()
    }
}

impl Default for VcamCallback {
    fn default() -> Self {
        Self::new()
    }
}

impl IMFAsyncCallback_Impl for VcamCallback_Impl {
    fn GetParameters(&self, pdwflags: *mut u32, pdwqueue: *mut u32) -> WinResult<()> {
        // 关键：queue 不能是 0。
        //
        // MFASYNC_CALLBACK_QUEUE_UNDEFINED = 0 是**非法值**，
        // 返回 0 会让调用方（IMFVirtualCamera::Start）认为参数无效，
        // 直接报 E_INVALIDARG (0x80070057) —— 这个错误发生在 Start 内部，
        // 而不是 GetParameters 本身，所以很难定位。
        //
        // MFASYNC_CALLBACK_QUEUE_STANDARD = 1（平台默认队列）
        const MFASYNC_CALLBACK_QUEUE_STANDARD: u32 = 1;

        if !pdwflags.is_null() {
            // SAFETY: 已判空
            unsafe { *pdwflags = 0 };
        }
        if !pdwqueue.is_null() {
            // SAFETY: 已判空
            unsafe { *pdwqueue = MFASYNC_CALLBACK_QUEUE_STANDARD };
        }
        Ok(())
    }

    fn Invoke(&self, _pasyncresult: Ref<IMFAsyncResult>) -> WinResult<()> {
        self.stats.invoked.fetch_add(1, Ordering::Relaxed);
        // FrameServer 通过这个回调把「设备级事件」推给我们。
        // 落盘一份，方便确认回调有没有真的被用起来。
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(std::env::temp_dir().join("sidelens-vcam.log"))
        {
            let _ = writeln!(
                f,
                "VcamCallback::Invoke（第 {} 次）",
                self.stats.invoked.load(Ordering::Relaxed)
            );
        }
        Ok(())
    }
}
