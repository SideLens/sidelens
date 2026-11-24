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
    //! 探测 IMFVirtualCamera::Start 的合法参数组合
    //!
    //! 用穷举法找出哪组 (Lifetime, Access, callback_queue) 能让 Start 成功。
    //! 这比查文档快 —— MSDN 对 Start 的参数约束语焉不详。

    #![allow(unsafe_code)]

    use windows::core::HSTRING;
    use windows::core::{implement, ComObject, Ref, Result as WinResult, PCWSTR};
    use windows::Win32::Media::MediaFoundation::{
        IMFAsyncCallback, IMFAsyncCallback_Impl, IMFAsyncResult, IMFVirtualCamera,
        MFCreateVirtualCamera, MFStartup, MFVirtualCameraAccess_AllUsers,
        MFVirtualCameraAccess_CurrentUser, MFVirtualCameraLifetime_Session,
        MFVirtualCameraLifetime_System, MFVirtualCameraType_SoftwareCameraSource, MF_VERSION,
    };
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
    use windows::Win32::System::Registry::{RegCloseKey, RegDeleteTreeW, HKEY_CURRENT_USER};

    /// 可配置队列 ID 的回调
    #[implement(IMFAsyncCallback)]
    #[derive(Debug)]
    struct TestCallback {
        queue: u32,
        flags: u32,
    }

    impl IMFAsyncCallback_Impl for TestCallback_Impl {
        fn GetParameters(&self, pdwflags: *mut u32, pdwqueue: *mut u32) -> WinResult<()> {
            if !pdwflags.is_null() {
                unsafe { *pdwflags = self.flags };
            }
            if !pdwqueue.is_null() {
                unsafe { *pdwqueue = self.queue };
            }
            Ok(())
        }
        fn Invoke(&self, _r: Ref<IMFAsyncResult>) -> WinResult<()> {
            Ok(())
        }
    }

    fn try_combo(
        lifetime: &str,
        lif: windows::Win32::Media::MediaFoundation::MFVirtualCameraLifetime,
        access_name: &str,
        acc: windows::Win32::Media::MediaFoundation::MFVirtualCameraAccess,
        queue: u32,
        clsid_str: &str,
    ) {
        let friendly = HSTRING::from("SideLens Probe");
        let src = HSTRING::from(clsid_str);

        // SAFETY: 全流程 COM 已初始化；字符串在调用期间存活
        let cam: Result<IMFVirtualCamera, _> = unsafe {
            MFCreateVirtualCamera(
                MFVirtualCameraType_SoftwareCameraSource,
                lif,
                acc,
                PCWSTR(friendly.as_ptr()),
                PCWSTR(src.as_ptr()),
                None,
            )
        };

        let cam = match cam {
            Ok(c) => c,
            Err(e) => {
                println!("  {lifetime:8} {access_name:12} queue={queue} → Create 失败: {e}");
                return;
            }
        };

        let cb: IMFAsyncCallback =
            ComObject::new(TestCallback { queue, flags: 0 }).into_interface();
        // SAFETY: cam 与 cb 有效
        let r = unsafe { cam.Start(&cb) };
        match r {
            Ok(()) => {
                println!("  {lifetime:8} {access_name:12} queue={queue} → ✅ Start 成功");
                // SAFETY: 成功过，Stop 清理
                unsafe {
                    let _ = cam.Stop();
                    let _ = cam.Remove();
                }
            }
            Err(e) => println!("  {lifetime:8} {access_name:12} queue={queue} → Start 失败: {e}"),
        }
    }

    pub fn main() {
        // SAFETY: 初始化 COM 与 MF
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let _ = MFStartup(MF_VERSION, 0);
        }

        let clsid = "{7A3E1C42-5D88-4F6B-9E20-1B4D8C3A9F57}";

        println!("探测 (Lifetime × Access × callback queue):");
        for queue in [1u32, 2, 0] {
            try_combo(
                "Session",
                MFVirtualCameraLifetime_Session,
                "CurrentUser",
                MFVirtualCameraAccess_CurrentUser,
                queue,
                clsid,
            );
            try_combo(
                "System",
                MFVirtualCameraLifetime_System,
                "CurrentUser",
                MFVirtualCameraAccess_CurrentUser,
                queue,
                clsid,
            );
            try_combo(
                "Session",
                MFVirtualCameraLifetime_Session,
                "AllUsers",
                MFVirtualCameraAccess_AllUsers,
                queue,
                clsid,
            );
        }

        // 清理注册表里可能残留的设备记录
        let sub = HSTRING::from("Software\\Microsoft\\Windows Media Foundation\\VirtualCamera");
        // SAFETY: 注册表键路径固定，删除失败（不存在）也无害
        unsafe {
            let _ = RegDeleteTreeW(HKEY_CURRENT_USER, &sub);
            let _ = RegCloseKey(HKEY_CURRENT_USER);
        }
        println!("已清理 HKCU 下的虚拟摄像头注册表项");
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
