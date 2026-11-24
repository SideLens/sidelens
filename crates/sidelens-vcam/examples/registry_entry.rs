//! 实验：用 `AddRegistryEntry` 设 `IsVirtualCamera`
//!
//! # 为什么需要这个实验
//!
//! `AddProperty(DEVPKEY_DeviceInterface_IsVirtualCamera, BOOLEAN, 1)` 在
//! **管理员权限下**仍然返回 `ACCESS_DENIED`（0x80070005）。
//!
//! 而同一次调用里 `AddProperty(DEVPKEY_DeviceInterface_IsWindowsCameraEffectAvailable,
//! BOOLEAN, 0)` **成功**了 —— 同一个 GUID、同样的 DEVPROPTYPE、同样的权限。
//!
//! 这说明 `IsVirtualCamera` 不是权限问题，而是**这个特定属性被系统保护**
//! （很可能由系统自己写，用户态不能设）。
//!
//! `AddRegistryEntry` 是另一条路：它写的是**设备接口节点下的注册表值**，
//! 走的是不同的代码路径（由系统代写，不经过 DEVPROP 的权限检查）。
//!
//! 这个实验验证它能不能绕过去。

#![allow(unsafe_code)]

//! # 跨平台门控（第四十轮补）
//!
//! 本文件原来**没有** `cfg` 门控，导致 `cargo check --all-targets
//! --target x86_64-unknown-linux-gnu` 报 3 个 `E0433`（`windows` crate 在
//! 非 Windows target 上不存在）。同目录的 `probe_start.rs` / `reg_probe.rs`
//! 都用了 `mod imp` 范式，只有这个文件漏了。
//!
//! 现在统一：内容进 `mod imp`，外层留两个 `main` 薄壳。

#[cfg(windows)]
mod imp {
    #![allow(unsafe_code)]

    use windows::core::{HSTRING, PCWSTR};
    use windows::Win32::Media::MediaFoundation::{
        IMFVirtualCamera, MFCreateVirtualCamera, MFStartup, MFVirtualCameraAccess_AllUsers,
        MFVirtualCameraLifetime_Session, MFVirtualCameraType_SoftwareCameraSource, MF_VERSION,
    };
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

    const CLSID: &str = "{7A3E1C42-5D88-4F6B-9E20-1B4D8C3A9F57}";

    /// 注册表值类型
    const REG_SZ: u32 = 1;
    const REG_DWORD: u32 = 4;
    const REG_BINARY: u32 = 3;

    fn try_entry(
        cam: &IMFVirtualCamera,
        label: &str,
        entry: &str,
        subkey: Option<&str>,
        ty: u32,
        data: &[u8],
    ) {
        let e = HSTRING::from(entry);
        let s = subkey.map(HSTRING::from);
        let sub = s
            .as_ref()
            .map(|h| PCWSTR(h.as_ptr()))
            .unwrap_or(PCWSTR::null());

        // SAFETY: cam 有效；e / sub / data 在调用期间存活
        let r = unsafe { cam.AddRegistryEntry(PCWSTR(e.as_ptr()), sub, ty, data) };
        match r {
            Ok(()) => println!("  ✅ {label}"),
            Err(err) => println!("  ❌ {label} : {err} (0x{:08X})", err.code().0),
        }
    }

    pub fn main() {
        // 命令行参数：只设指定的那一项，用于 A/B 定位
        let mode = std::env::args().nth(1).unwrap_or_else(|| "all".into());
        println!("模式：{mode}");

        // SAFETY: 进程级初始化
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let _ = MFStartup(MF_VERSION, 0);
        }

        let friendly = HSTRING::from("SideLens Probe");
        let src = HSTRING::from(CLSID);

        // SAFETY: 两个 HSTRING 在调用期间存活
        let cam: Result<IMFVirtualCamera, _> = unsafe {
            MFCreateVirtualCamera(
                MFVirtualCameraType_SoftwareCameraSource,
                MFVirtualCameraLifetime_Session,
                MFVirtualCameraAccess_AllUsers,
                PCWSTR(friendly.as_ptr()),
                PCWSTR(src.as_ptr()),
                None,
            )
        };

        let cam = match cam {
            Ok(c) => c,
            Err(e) => {
                println!("MFCreateVirtualCamera 失败: {e}");
                return;
            }
        };
        println!("MFCreateVirtualCamera 成功");

        let clsid_sz: Vec<u8> = CLSID
            .encode_utf16()
            .chain(std::iter::once(0))
            .flat_map(|u| u.to_le_bytes())
            .collect();

        // 按 mode 只设一项
        match mode.as_str() {
            "none" => println!("(不设任何属性 —— 基线)"),
            "iscam_dword" => {
                println!("\n=== 只设 IsVirtualCamera (REG_DWORD) ===");
                try_entry(
                    &cam,
                    "IsVirtualCamera=1",
                    "{6EDC630D-C2E3-43B7-B2D1-20525A1AF120}, 3",
                    None,
                    REG_DWORD,
                    &1u32.to_le_bytes(),
                );
            }
            "iscam_binary" => {
                println!("\n=== 只设 IsVirtualCamera (REG_BINARY under Properties) ===");
                try_entry(
                    &cam,
                    "pid3 binary",
                    "00000003",
                    Some("Properties\\{6EDC630D-C2E3-43B7-B2D1-20525A1AF120}"),
                    REG_BINARY,
                    &[0x01, 0x00, 0x00, 0x00, 0x01],
                );
            }
            "vcamclsid" => {
                println!("\n=== 只设 VCamClsid ===");
                try_entry(&cam, "VCamClsid", "VCamClsid", None, REG_SZ, &clsid_sz);
            }
            "customclsid" => {
                println!("\n=== 只设 CustomCaptureSourceClsid ===");
                try_entry(
                    &cam,
                    "CustomCaptureSourceClsid",
                    "CustomCaptureSourceClsid",
                    None,
                    REG_SZ,
                    &clsid_sz,
                );
            }
            "all" => {
                run_all(&cam, &clsid_sz);
            }
            other => {
                println!("未知模式 {other}，跑 all");
                run_all(&cam, &clsid_sz);
            }
        }

        println!("\n=== 现在试 Start ===");
        // SAFETY: cam 有效
        match unsafe { cam.Start(None) } {
            Ok(()) => println!("  ✅ Start 成功！"),
            Err(e) => println!("  ❌ Start 失败: {e} (0x{:08X})", e.code().0),
        }

        // SAFETY: 清理
        unsafe {
            let _ = cam.Stop();
            let _ = cam.Remove();
        }
    }

    fn run_all(cam: &IMFVirtualCamera, clsid_sz: &[u8]) {
        let _ = cam;
        let _ = clsid_sz;
        // 各种写法的组合，看哪种被接受
        println!("\n=== 尝试 1：SubkeyPath = null，写字符串 ===");
        try_entry(
            cam,
            "IsVirtualCamera = \"1\" (REG_SZ, no subkey)",
            "{6EDC630D-C2E3-43B7-B2D1-20525A1AF120}, 3",
            None,
            REG_SZ,
            b"1\0",
        );

        println!("\n=== 尝试 2：SubkeyPath = Properties\\<guid> ===");
        try_entry(
            cam,
            "pid 3 (REG_BINARY, DEVPROPTYPE prefix)",
            "00000003",
            Some("Properties\\{6EDC630D-C2E3-43B7-B2D1-20525A1AF120}"),
            REG_BINARY,
            // DEVPROP_TYPE_BOOLEAN (0x0001) 小端 4 字节 + 值 1
            &[0x01, 0x00, 0x00, 0x00, 0x01],
        );

        println!("\n=== 尝试 3：VCamClsid（小写，mfsensorgroup 里的名字）===");
        try_entry(
            cam,
            "VCamClsid = our CLSID (REG_SZ)",
            "VCamClsid",
            None,
            REG_SZ,
            &CLSID
                .encode_utf16()
                .chain(std::iter::once(0))
                .flat_map(|u| u.to_le_bytes())
                .collect::<Vec<_>>(),
        );

        println!("\n=== 尝试 4：CustomCaptureSourceClsid（已知无效，做对照）===");
        try_entry(
            cam,
            "CustomCaptureSourceClsid (REG_SZ)",
            "CustomCaptureSourceClsid",
            None,
            REG_SZ,
            &CLSID
                .encode_utf16()
                .chain(std::iter::once(0))
                .flat_map(|u| u.to_le_bytes())
                .collect::<Vec<_>>(),
        );

        println!("\n=== 尝试 5：IsVirtualCamera 用 REG_DWORD ===");
        try_entry(
            cam,
            "IsVirtualCamera = 1 (REG_DWORD)",
            "{6EDC630D-C2E3-43B7-B2D1-20525A1AF120}, 3",
            None,
            REG_DWORD,
            &1u32.to_le_bytes(),
        );
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
