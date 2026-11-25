//! SideLens DirectShow 虚拟摄像头
//!
//! # 为什么是 DirectShow 而不是 MFCreateVirtualCamera
//!
//! 第十九轮用 7 组调用参数组合 + 6 组注册表 A/B 验证过：`MFCreateVirtualCamera`
//! 在当前 Windows build（26300.9711）上走不通 —— FrameServer 完整走完 13 步、
//! `ActivateObject` 成功，然后 `IMFVirtualCamera::Start` 报 `MF_E_INVALIDTYPE`
//! (0xC00D36BD)，从未调用 `IMFMediaSource::Start`。
//!
//! 详见 [docs/vcam-summary.md](../../docs/vcam-summary.md)。
//!
//! DirectShow 源滤镜是 OBS 用了十几年的方案：
//!
//! | 维度 | MFCreateVirtualCamera | DirectShow 源滤镜 |
//! | --- | --- | --- |
//! | 注册方式 | `MFCreateVirtualCamera` + `Start` | `regsvr32` |
//! | 需要驱动 | 否 | 否 |
//! | 需要签名 | 否 | 否（Authenticode 可选） |
//! | 最低系统 | Win11 22000+ | 全版本 |
//! | 权限 | **管理员** | 管理员（仅注册时） |
//! | 生命周期 | 进程退出即消失 | 注册后常驻 |
//!
//! # 架构
//!
//! ```text
//! sidelensd (生产者)               本 DLL (消费者)
//! ─────────────────               ─────────────────
//! 采集 → 编码 → NV12 帧
//!   ↓ 命名管道（帧元数据）
//!   ↓ 共享内存环（像素）
//!                                  FrameSource
//!                                    ↓ poll
//!                                  PushSource 滤镜
//!                                    ↓ FillBuffer
//!                                  消费端（Zoom/OBS/浏览器）
//! ```
//!
//! 传帧层（`sidelens-ipc` + `FrameSource`）与 `sidelens-vcam` 完全共用 ——
//! 那部分不依赖任何虚拟摄像头 API，只做「管道 + 共享内存 → NV12 字节」。
//!
//! # 注册
//!
//! ```text
//! regsvr32 /s sidelens_dshow.dll      # 注册（管理员）
//! regsvr32 /u /s sidelens_dshow.dll   # 反注册
//! ```
//!
//! 注册后出现在「视频捕获设备」类别，任何用 DirectShow 或 Media Foundation
//! 枚举摄像头的应用都能看到（MF 在 Win7+ 有 DShow 桥接）。

#![allow(unsafe_code)]

#[cfg(windows)]
mod filter;
#[cfg(windows)]
pub mod graph_test;
#[cfg(windows)]
mod pin;
#[cfg(windows)]
mod registration;
#[cfg(windows)]
mod source;

#[cfg(windows)]
pub use filter::{SideLensFilter, CLSID_SIDELENS_DSHOW};
#[cfg(windows)]
pub use source::FrameSource;

/// 写日志到文件
///
/// # 为什么必须写文件
///
/// `eprintln!` 在 GUI 进程（OBS / Zoom）里**看不到** ——
/// 那些进程没有控制台，标准错误直接被丢弃。
///
/// 第二十三轮排查时加了大量 `eprintln`，结果一条都没看到，
/// 浪费了一轮时间。现在统一走文件日志。
///
/// 路径：`%TEMP%\sidelens-dshow.log`
pub fn log_to_file(msg: &str) {
    use std::io::Write;
    let path = std::env::temp_dir().join("sidelens-dshow.log");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let line = format!("[{}] {msg}\n", std::process::id());
        let _ = f.write_all(line.as_bytes());
    }
}

/// 滤镜显示名（出现在应用的摄像头列表里）
pub const FILTER_NAME: &str = "SideLens Camera";

/// 单例类工厂导出
///
/// DirectShow 滤镜是 in-proc COM 服务器，必须导出这四个函数，
/// `regsvr32` 和消费端都靠它们。
#[cfg(windows)]
mod exports {
    use std::ffi::c_void;

    use windows::core::{IUnknown, Interface, GUID, HRESULT};
    use windows::Win32::Foundation::{CLASS_E_NOAGGREGATION, E_POINTER, S_FALSE, S_OK};
    use windows::Win32::System::Com::IClassFactory;

    use crate::filter::{SideLensFilter, CLSID_SIDELENS_DSHOW};
    use crate::registration::{dll_register_server, dll_unregister_server};

    /// 类工厂：只创建 `SideLensFilter`
    #[windows::core::implement(IClassFactory)]
    struct FilterFactory;

    impl windows::Win32::System::Com::IClassFactory_Impl for FilterFactory_Impl {
        fn CreateInstance(
            &self,
            punkouter: windows::core::Ref<IUnknown>,
            riid: *const GUID,
            ppv: *mut *mut c_void,
        ) -> windows::core::Result<()> {
            unsafe {
                if punkouter.as_ref().is_some() {
                    return Err(CLASS_E_NOAGGREGATION.into());
                }
                if ppv.is_null() || riid.is_null() {
                    return Err(E_POINTER.into());
                }
                *ppv = std::ptr::null_mut();

                let filter: IUnknown = SideLensFilter::new().into_interface();
                filter.query(riid, ppv).ok()
            }
        }

        fn LockServer(&self, _flock: windows::core::BOOL) -> windows::core::Result<()> {
            Ok(())
        }
    }

    /// `DllGetClassObject`
    ///
    /// # Safety
    ///
    /// 由 COM 运行时调用，指针有效性由调用方保证。
    #[no_mangle]
    pub unsafe extern "system" fn DllGetClassObject(
        rclsid: *const GUID,
        riid: *const GUID,
        ppv: *mut *mut c_void,
    ) -> HRESULT {
        if rclsid.is_null() || riid.is_null() || ppv.is_null() {
            return E_POINTER;
        }
        *ppv = std::ptr::null_mut();
        if *rclsid != CLSID_SIDELENS_DSHOW {
            return windows::Win32::Foundation::CLASS_E_CLASSNOTAVAILABLE;
        }
        let factory: IClassFactory = FilterFactory.into();
        factory.query(riid, ppv)
    }

    /// `DllCanUnloadNow`
    #[no_mangle]
    pub extern "system" fn DllCanUnloadNow() -> HRESULT {
        S_FALSE
    }

    /// `DllRegisterServer`
    #[no_mangle]
    pub extern "system" fn DllRegisterServer() -> HRESULT {
        match dll_register_server() {
            Ok(()) => S_OK,
            Err(e) => e,
        }
    }

    /// `DllUnregisterServer`
    #[no_mangle]
    pub extern "system" fn DllUnregisterServer() -> HRESULT {
        match dll_unregister_server() {
            Ok(()) => S_OK,
            Err(e) => e,
        }
    }
}
