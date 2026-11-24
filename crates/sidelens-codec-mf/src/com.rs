//! Media Foundation / COM 生命周期
//!
//! # 为什么必须集中管理
//!
//! 两个 API 都是**按线程**计数的：
//!
//! | API | 配对 | 重复调用 |
//! | --- | --- | --- |
//! | CoInitializeEx | CoUninitialize | 返回 S_FALSE，仍需配对一次 |
//! | MFStartup | MFShutdown | 内部自己计数，但漏调会拖住进程退出 |
//!
//! 发送端里编码器只有一个实例，但测试与离线工具可能同时开多个。
//! 用一个进程级引用计数把配对关系收在一处，比每个调用点自己
//! CoInitializeEx 安全得多。

use std::sync::Mutex;

use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
use windows::Win32::Media::MediaFoundation::{MFShutdown, MFStartup, MFSTARTUP_LITE, MF_VERSION};
use windows::Win32::System::Com::{
    CoInitializeEx, CoUninitialize, COINIT_DISABLE_OLE1DDE, COINIT_MULTITHREADED,
};

/// 进程级引用计数
///
/// Mutex::new 是 const 的，可以直接做静态初始化，不需要 OnceLock。
static REFS: Mutex<u32> = Mutex::new(0);

/// Media Foundation 的初始化守卫
///
/// 拿到它就代表「MFStartup 已成功」，drop 时按引用计数决定是否真正
/// MFShutdown。
#[derive(Debug)]
pub struct MediaFoundation {
    /// 本线程是否**真正**初始化了 COM 公寓
    ///
    /// RPC_E_CHANGED_MODE（线程已在别的公寓里）时是 false ——
    /// 那种情况下调用 CoUninitialize 会破坏别人的公寓。
    owns_apartment: bool,
}

impl MediaFoundation {
    /// 初始化（幂等）
    pub fn init() -> Result<Self, InitError> {
        let mut refs = REFS.lock().unwrap_or_else(|e| e.into_inner());

        if *refs == 0 {
            // SAFETY: 无参数副作用；返回 HRESULT 由下面判断
            let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED | COINIT_DISABLE_OLE1DDE) };
            // S_OK(0) 与 S_FALSE(1) 都表示「本线程现在处于 MTA」，
            // 两者都需要配对的 CoUninitialize。
            let owns_apartment = hr.is_ok();
            if !owns_apartment && hr.0 != RPC_E_CHANGED_MODE.0 {
                return Err(InitError::new("CoInitializeEx", hr.0));
            }

            // SAFETY: 与 Drop 里的 MFShutdown 配对
            if let Err(e) = unsafe { MFStartup(MF_VERSION, MFSTARTUP_LITE) } {
                if owns_apartment {
                    // SAFETY: 上面刚成功初始化
                    unsafe { CoUninitialize() };
                }
                return Err(InitError::new("MFStartup", e.code().0));
            }

            *refs = 1;
            return Ok(Self { owns_apartment });
        }

        *refs += 1;
        Ok(Self {
            owns_apartment: false,
        })
    }
}

impl Drop for MediaFoundation {
    fn drop(&mut self) {
        let mut refs = REFS.lock().unwrap_or_else(|e| e.into_inner());
        if *refs == 0 {
            return;
        }
        *refs -= 1;
        if *refs > 0 {
            return;
        }
        // SAFETY: 与 init 里的 MFStartup 严格配对
        unsafe {
            let _ = MFShutdown();
        }
        // 只有当初真正建立公寓的那一次才反初始化。
        //
        // 这里是「进程级」而不是「线程级」的简化 —— MFStartup 本身是
        // 进程级的，编码器也都跑在同一个 runtime 线程上。若将来把编码器
        // 挪到别的线程，需要改成 thread_local 计数。
        if self.owns_apartment {
            // SAFETY: 与 init 里的 CoInitializeEx 严格配对
            unsafe { CoUninitialize() };
        }
    }
}

/// 初始化失败
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitError {
    stage: &'static str,
    hresult: i32,
}

impl InitError {
    pub(crate) fn new(stage: &'static str, hresult: i32) -> Self {
        Self { stage, hresult }
    }

    /// 失败的阶段
    pub fn stage(&self) -> &'static str {
        self.stage
    }

    /// 原始 HRESULT
    pub fn hresult(&self) -> i32 {
        self.hresult
    }
}

impl std::fmt::Display for InitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} 失败（HRESULT 0x{:08X}）",
            self.stage, self.hresult as u32
        )
    }
}

impl std::error::Error for InitError {}
