//! Windows 专属示例（非 Windows 上是一段空壳）。
//!
//! # 为什么包一层模块
//!
//! 直接写 `#![cfg(windows)]` 会让整个文件在非 Windows 上消失 ——
//! 包括 `main`，于是 `cargo check --all-targets` 报
//! `E0601: main function not found`。这个错误看起来像"忘了写 main"，
//! 实际是 cfg 把 main 一起删了（见 docs/cross-platform.md P2）。
//!
//! 正确写法：内容进 `mod imp`，外层留两个 `main` 薄壳。

#[cfg(windows)]
mod imp {
    //! 验证 CLSID 注册表登记对 Start 的影响

    #![allow(unsafe_code)]

    use windows::core::{implement, ComObject, Ref, Result as WinResult, HSTRING, PCWSTR};
    use windows::Win32::Media::MediaFoundation::{
        IMFAsyncCallback, IMFAsyncCallback_Impl, IMFAsyncResult, IMFVirtualCamera,
        MFCreateVirtualCamera, MFStartup, MFVirtualCameraAccess_CurrentUser,
        MFVirtualCameraLifetime_Session, MFVirtualCameraType_SoftwareCameraSource, MF_VERSION,
    };
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

    #[implement(IMFAsyncCallback)]
    #[derive(Debug)]
    struct Cb;
    impl IMFAsyncCallback_Impl for Cb_Impl {
        fn GetParameters(&self, f: *mut u32, q: *mut u32) -> WinResult<()> {
            if !f.is_null() {
                unsafe { *f = 0 };
            }
            if !q.is_null() {
                unsafe { *q = 1 };
            }
            Ok(())
        }
        fn Invoke(&self, _r: Ref<IMFAsyncResult>) -> WinResult<()> {
            Ok(())
        }
    }

    pub fn main() {
        // SAFETY: 进程级初始化
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let _ = MFStartup(MF_VERSION, 0);
        }

        let friendly = HSTRING::from("SideLens Probe");
        let src = HSTRING::from("{7A3E1C42-5D88-4F6B-9E20-1B4D8C3A9F57}");

        // SAFETY: 字符串在调用期间存活
        let cam: Result<IMFVirtualCamera, _> = unsafe {
            MFCreateVirtualCamera(
                MFVirtualCameraType_SoftwareCameraSource,
                MFVirtualCameraLifetime_Session,
                MFVirtualCameraAccess_CurrentUser,
                PCWSTR(friendly.as_ptr()),
                PCWSTR(src.as_ptr()),
                None,
            )
        };

        let cam = match cam {
            Ok(c) => c,
            Err(e) => {
                println!("Create 失败: {e}");
                return;
            }
        };

        let cb: IMFAsyncCallback = ComObject::new(Cb).into_interface();
        // SAFETY: cam 与 cb 有效
        match unsafe { cam.Start(&cb) } {
            Ok(()) => println!("✅ Start 成功"),
            Err(e) => println!("Start 失败: {e}"),
        }
    }
}

#[cfg(windows)]
fn main() {
    imp::main();
}

#[cfg(not(windows))]
fn main() {
    eprintln!("此示例仅支持 Windows —— 虚拟摄像头是 Windows 专属能力");
}
