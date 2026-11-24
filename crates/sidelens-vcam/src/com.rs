//! COM 与 Media Foundation 初始化
//!
//! # 为什么必须显式初始化
//!
//! `MFCreateVirtualCamera` 是个 COM 工厂函数。**没调用过 `CoInitializeEx`
//! 的线程调用它会直接失败**，错误码 `0x800401F0`（CO_E_NOTINITIALIZED）。
//!
//! 本项目实测踩过：能力检测（纯注册表读取）一切正常，
//! 一到 `MFCreateVirtualCamera` 就报「尚未调用 CoInitialize」。
//! 这说明**检测通过 ≠ 能创建**，两者是独立的门槛。
//!
//! # 还要 MFStartup
//!
//! Media Foundation 平台本身需要 `MFStartup`。缺了它，
//! COM 初始化正确的情况下后续的 `GetMediaSource` / 媒体类型协商仍会失败。
//!
//! # STA 还是 MTA
//!
//! 选 **MTA**（`COINIT_MULTITHREADED`）：
//!
//! - 虚拟摄像头会被多个消费端（Zoom、Teams、浏览器）同时连接
//! - STA 需要消息泵，而 tokio 的运行时不泵 Windows 消息
//! - MTA 下 COM 对象可以在任意线程被调用，省掉整套线程亲和性管理
//!
//! 代价是对象必须自己保证线程安全（我们要做的），换来的是不用为每个
//! 消费端维护一个专门的 STA 线程。

#![allow(unsafe_code)]

use std::fmt;

use windows::Win32::Media::MediaFoundation::{MFShutdown, MFStartup, MF_VERSION};
use windows::Win32::System::Com::{
    CoInitializeEx, CoUninitialize, COINIT_DISABLE_OLE1DDE, COINIT_MULTITHREADED,
};

/// 初始化错误
#[derive(Debug)]
pub struct InitError {
    pub stage: &'static str,
    pub detail: String,
}

impl fmt::Display for InitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} 失败：{}", self.stage, self.detail)
    }
}

impl std::error::Error for InitError {}

/// COM + Media Foundation 的作用域守卫
///
/// 构造时初始化，Drop 时按相反顺序清理。
/// 每个需要 COM 的线程都要有一个（COM 初始化是**按线程**的）。
///
/// # 用法
///
/// ```no_run
/// # use sidelens_vcam::com::MediaFoundation;
/// let _mf = MediaFoundation::init().expect("初始化失败");
/// // 此后才能调用 MFCreateVirtualCamera
/// ```
#[derive(Debug)]
pub struct MediaFoundation {
    /// 重复初始化时不能重复反初始化
    com_owned: bool,
    mf_owned: bool,
}

impl MediaFoundation {
    /// 在当前线程初始化 COM（MTA）与 Media Foundation
    pub fn init() -> Result<Self, InitError> {
        // COM 可能已被别处初始化（比如 tokio 的某个工作线程）。
        // RPC_E_CHANGED_MODE 表示"已初始化但模式不同"——不是致命错误，
        // 因为 COM 是进程级共享，我们只是要"能用"，不强求模式。
        let com_hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED | COINIT_DISABLE_OLE1DDE) };

        let com_owned = if com_hr.is_ok() {
            true
        } else if com_hr.0 == 0x8001_01F9u32 as i32 {
            // RPC_E_CHANGED_MODE：已以 STA 初始化。可以继续用，但不反初始化。
            false
        } else {
            return Err(InitError {
                stage: "CoInitializeEx",
                detail: format!("HRESULT 0x{:08X}", com_hr.0),
            });
        };

        // MF_VERSION 已由 windows crate 按编译时 SDK 版本填好
        if let Err(e) = unsafe { MFStartup(MF_VERSION, 0) } {
            if com_owned {
                unsafe { CoUninitialize() };
            }
            return Err(InitError {
                stage: "MFStartup",
                detail: format!("{e}"),
            });
        }

        Ok(Self {
            com_owned,
            mf_owned: true,
        })
    }

    /// 是否由本守卫持有 COM 初始化
    pub fn owns_com(&self) -> bool {
        self.com_owned
    }
}

impl Drop for MediaFoundation {
    fn drop(&mut self) {
        if self.mf_owned {
            // SAFETY: 与 MFStartup 成对
            unsafe { MFShutdown().ok() };
        }
        if self.com_owned {
            // SAFETY: 与 CoInitializeEx 成对
            unsafe { CoUninitialize() };
        }
    }
}
