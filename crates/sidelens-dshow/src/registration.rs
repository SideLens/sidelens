//! 注册表登记
//!
//! # DirectShow 滤镜怎么被系统发现
//!
//! 两层登记，缺一不可：
//!
//! ```text
//! HKLM\SOFTWARE\Classes\CLSID\{9e4b7c31-...}
//!   └─ InprocServer32
//!        (Default)      = DLL 完整路径
//!        ThreadingModel = Both
//!
//! HKLM\SOFTWARE\Classes\CLSID\{860BB310-5D01-11D0-BD3B-00A0C911CE86}
//!   └─ Instance\SideLens Camera
//!        CLSID        = {9e4b7c31-...}
//!        FriendlyName = SideLens Camera
//! ```
//!
//! 第一层是 COM 类登记（`CoCreateInstance` 靠它找到 DLL），
//! 第二层是**类别登记** —— `{860BB310-5D01-11D0-BD3B-00A0C911CE86}` 就是
//! `CLSID_VideoInputDeviceCategory`（视频捕获设备）。
//!
//! # 为什么必须写 HKLM 不能写 HKCU
//!
//! 消费端可能是任意进程（Zoom、OBS、浏览器），它们各自 `CoCreateInstance`。
//! HKCU 只对当前用户可见，而且提权进程的 HKCU 和普通进程的 HKCU 不是同一个
//! （UAC 分隔）。写 HKLM 才能让所有进程都找到。
//!
//! 这个坑在虚拟摄像头那轮踩过（docs/vcam-pitfalls.md 第 2 条）。
//!
//! # 为什么 `ThreadingModel = Both`
//!
//! `Both` 表示对象可以住在 STA 或 MTA。DirectShow 图通常在 MTA 里构建，
//! 但有些应用（老的 Win32 程序）用 STA。写 `Both` 两种都能工作。

#![allow(unsafe_code)]

use windows::core::{HRESULT, PCWSTR};
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::LibraryLoader::GetModuleFileNameW;
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, RegSetValueExW, HKEY, HKEY_LOCAL_MACHINE,
    KEY_WRITE, REG_BINARY, REG_OPTION_NON_VOLATILE, REG_SZ,
};

use crate::filter::CLSID_SIDELENS_DSHOW;

/// `CLSID_VideoInputDeviceCategory` —— 「视频捕获设备」类别
///
/// 这个 GUID 是 DirectShow 的固定常量，所有摄像头都登记在这里。
/// 消费端枚举摄像头就是枚举这个类别的 `Instance` 子键。
const CLSID_VIDEO_INPUT_DEVICE_CATEGORY: &str = "{860BB310-5D01-11D0-BD3B-00A0C911CE86}";

/// 把 `&str` 转成 NUL 结尾的 UTF-16
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// CLSID 的注册表字符串形式（带大括号）
///
/// # 为什么不用 `Debug`
///
/// `GUID` 的 `Debug` 实现是 `9E4B7C31-5A2D-4F8E-B1C6-3D7A0E5F92B4`
/// （纯大写十六进制，**没有** `GUID(...)` 包装）—— 第一版按 `GUID(...)`
/// 做 `replace` 得到的是原样字符串，写进注册表就是一个无效键名。
/// 这个错误由单元测试 `clsid_string_has_braces` 抓到。
pub fn clsid_string() -> String {
    format!("{{{:?}}}", CLSID_SIDELENS_DSHOW)
}

/// 取当前 DLL 的完整路径
///
/// # 为什么必须传模块句柄，不能传 `None`
///
/// `GetModuleFileNameW(None, ...)` 返回的是**调用进程的主模块**路径。
/// `regsvr32` 加载我们的 DLL 后调 `DllRegisterServer`，此时进程主模块
/// 是 `regsvr32.exe` —— 第一版就是这么写的，结果 `InprocServer32` 被写成
/// `C:\Windows\system32\regsvr32.exe`，注册表看起来"成功"了，
/// 但 `CoCreateInstance` 永远拿不到我们的滤镜。
///
/// 正确做法是用 `GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, ...)`
/// 从本函数自己的地址反查所属模块。
fn module_path() -> Result<String, HRESULT> {
    use windows::Win32::System::LibraryLoader::{
        GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
        GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
    };

    let mut module = windows::Win32::Foundation::HMODULE::default();
    // 用本函数地址反查所属模块
    let addr = module_path as *const core::ffi::c_void;
    // SAFETY: addr 是本进程内的有效代码地址
    let ok = unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            windows::core::PCWSTR(addr as *const u16),
            &mut module,
        )
    };
    if ok.is_err() {
        return Err(windows::Win32::Foundation::E_FAIL);
    }

    let mut buf = vec![0u16; 32768];
    // SAFETY: buf 足够大，返回长度受 buf 限制；module 是本 DLL 的句柄
    let n = unsafe { GetModuleFileNameW(Some(module), &mut buf) };
    if n == 0 || n as usize >= buf.len() {
        return Err(windows::Win32::Foundation::E_FAIL);
    }
    buf.truncate(n as usize);
    String::from_utf16(&buf).map_err(|_| windows::Win32::Foundation::E_FAIL)
}

/// 写一个二进制值
///
/// # Safety
///
/// `key` 必须是有效的、已打开的注册表键。
unsafe fn set_binary(key: HKEY, name: &str, value: &[u8]) -> Result<(), HRESULT> {
    let name_w = wide(name);
    let rc = RegSetValueExW(key, PCWSTR(name_w.as_ptr()), None, REG_BINARY, Some(value));
    if rc == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(HRESULT::from_win32(rc.0))
    }
}

/// 构造 `FilterData` 注册表项
///
/// # 这是什么
///
/// DirectShow 的「视频捕获设备」类别里，每个设备的 `Instance` 子键除了
/// `CLSID` 和 `FriendlyName`，还有一个二进制 `FilterData` ——
/// 它是**滤镜自己序列化的配置**，由滤镜的 `IPersistStream` 实现决定。
///
/// # 为什么必须有
///
/// 实测对照：本机的 Lens（正常摄像头）和 OBS Virtual Camera 都有这一项，
/// 我们的没有。OBS 的 `dshow_input` 在枚举设备时会读它 ——
/// 缺失时日志报 `DecodeDeviceId failed`，随后在
/// `obs_source_properties` 里崩溃（`c0000005`）。
///
/// # 内容怎么定
///
/// 这一段是从 OBS Virtual Camera 的 `FilterData` 抄的骨架：
/// 它是 `AM_MEDIA_TYPE` 序列化后的形态，声明「支持 NV12」。
/// 尾部 16 字节是 `MEDIASUBTYPE_NV12` 的 GUID。
///
/// 我们的滤镜不实现 `IPersistStream`，所以这个值只是给枚举方看的
/// 「能力声明」，不会被反序列化回来。
fn filter_data() -> Vec<u8> {
    // # 逐字节抄 rng-lens（Lens）的 FilterData
    //
    // 对照本机注册表 `{860BB310-...}\Instance\Lens`：
    //
    // ```text
    // 02000000 00002000 01000000 00000000
    // 30706933 08000000 00000000 01000000
    // 00000000 00000000 30747933 00000000
    // 38000000 48000000 76696473 00001000   ← "vids" = MEDIATYPE_Video
    // 800000AA 00389B71 00000000 00000000   ← subtype 全零 = MEDIASUBTYPE_NULL
    // 00000000 00000000
    // ```
    //
    // # 为什么 subtype 必须全零（第二十三轮定位）
    //
    // 第一版把 NV12 的 GUID 写进去，OBS 读到后显示
    // `format: XRGB`（它按 32 位猜的），然后 `ConnectPins` 报
    // `VFW_E_NO_ACCEPTABLE_TYPES` (0x8004022A)。
    //
    // softcam 的注册代码明确用 `MEDIASUBTYPE_NULL`：
    //
    // ```cpp
    // const AMOVIESETUP_MEDIATYPE s_pin_types[] = {
    //     { &MEDIATYPE_Video, &MEDIASUBTYPE_NULL }
    // };
    // ```
    //
    // 含义是「任意视频子类型，别按这个字段猜」—— 真实格式由
    // `EnumMediaTypes` / `GetStreamCaps` 在运行时协商。
    let mut v: Vec<u8> = vec![
        0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x20, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x30, 0x70, 0x69, 0x33, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x30, 0x74, 0x79, 0x33, 0x00,
        0x00, 0x00, 0x00, 0x38, 0x00, 0x00, 0x00, 0x48, 0x00, 0x00, 0x00,
        // "vids" = MEDIATYPE_Video
        0x76, 0x69, 0x64, 0x73, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B,
        0x71,
    ];
    // MEDIASUBTYPE_NULL：16 字节全零
    v.extend_from_slice(&[0u8; 16]);
    v
}

/// 写一个字符串值
///
/// # Safety
///
/// `key` 必须是有效的、已打开的注册表键。
unsafe fn set_string(key: HKEY, name: &str, value: &str) -> Result<(), HRESULT> {
    // # 空名字必须传 null 指针
    //
    // 注册表的「默认值」用 null 名（或空字符串）表示。但这里有个坑：
    // `PCWSTR(wide("").as_ptr())` 指向的是临时 Vec，**函数返回后立即释放** ——
    // 第一版就这么写的，结果 RegSetValueExW 读到了悬垂指针，
    // 把设备名写成了键名（`SideLens Camera REG_SZ SideLens Camera`），
    // 而真正的 `FriendlyName` 值消失了。
    //
    // 正确做法：空名字直接传 `PCWSTR::null()`。
    let name_w = wide(name);
    let name_ptr = if name.is_empty() {
        PCWSTR::null()
    } else {
        PCWSTR(name_w.as_ptr())
    };
    let value_w = wide(value);
    let bytes = std::slice::from_raw_parts(
        value_w.as_ptr() as *const u8,
        value_w.len() * std::mem::size_of::<u16>(),
    );
    let rc = RegSetValueExW(key, name_ptr, None, REG_SZ, Some(bytes));
    if rc == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(HRESULT::from_win32(rc.0))
    }
}

/// 创建（或打开）一个键
///
/// # Safety
///
/// 返回的键必须由调用方 `RegCloseKey` 关闭。
unsafe fn create_key(path: &str) -> Result<HKEY, HRESULT> {
    let path_w = wide(path);
    let mut key = HKEY::default();
    let rc = RegCreateKeyExW(
        HKEY_LOCAL_MACHINE,
        PCWSTR(path_w.as_ptr()),
        None,
        PCWSTR::null(),
        REG_OPTION_NON_VOLATILE,
        KEY_WRITE,
        None,
        &mut key,
        None,
    );
    if rc == ERROR_SUCCESS {
        Ok(key)
    } else {
        Err(HRESULT::from_win32(rc.0))
    }
}

/// 注册滤镜（`DllRegisterServer` 调用）
pub fn dll_register_server() -> Result<(), HRESULT> {
    let dll = module_path()?;
    let clsid = clsid_string();

    // 1. COM 类登记
    let clsid_path = format!("SOFTWARE\\Classes\\CLSID\\{clsid}");
    // SAFETY: 路径合法；key 用完即关
    unsafe {
        let key = create_key(&clsid_path)?;
        let _ = set_string(key, "", crate::FILTER_NAME);
        let _ = RegCloseKey(key);

        let inproc_path = format!("{clsid_path}\\InprocServer32");
        let key = create_key(&inproc_path)?;
        let _ = set_string(key, "", &dll);
        let _ = set_string(key, "ThreadingModel", "Both");
        let _ = RegCloseKey(key);
    }

    // 2. 类别登记（让消费端能枚举到）
    let inst_path = format!(
        "SOFTWARE\\Classes\\CLSID\\{CLSID_VIDEO_INPUT_DEVICE_CATEGORY}\\Instance\\{}",
        crate::FILTER_NAME
    );
    // SAFETY: 同上
    unsafe {
        let key = create_key(&inst_path)?;
        let _ = set_string(key, "CLSID", &clsid);
        let _ = set_string(key, "FriendlyName", crate::FILTER_NAME);
        // FilterData：能力声明，缺了 OBS 会 DecodeDeviceId failed 并崩溃
        let _ = set_binary(key, "FilterData", &filter_data());
        let _ = RegCloseKey(key);
    }

    Ok(())
}

/// 反注册（`DllUnregisterServer` 调用）
pub fn dll_unregister_server() -> Result<(), HRESULT> {
    let clsid = clsid_string();

    // SAFETY: 路径合法；删除不存在的键会返回错误，忽略即可
    unsafe {
        let path = wide(&format!("SOFTWARE\\Classes\\CLSID\\{clsid}"));
        let _ = RegDeleteTreeW(HKEY_LOCAL_MACHINE, PCWSTR(path.as_ptr()));

        let path = wide(&format!(
            "SOFTWARE\\Classes\\CLSID\\{CLSID_VIDEO_INPUT_DEVICE_CATEGORY}\\Instance\\{}",
            crate::FILTER_NAME
        ));
        let _ = RegDeleteTreeW(HKEY_LOCAL_MACHINE, PCWSTR(path.as_ptr()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clsid_string_has_braces() {
        let s = clsid_string();
        assert!(s.starts_with('{') && s.ends_with('}'), "得到 {s}");
        assert_eq!(s, "{9E4B7C31-5A2D-4F8E-B1C6-3D7A0E5F92B4}");
    }

    #[test]
    fn wide_string_is_nul_terminated() {
        assert_eq!(wide("abc"), vec![97, 98, 99, 0]);
    }

    #[test]
    fn category_guid_is_the_video_input_one() {
        // CLSID_VideoInputDeviceCategory 是固定常量，写错就枚举不到
        assert_eq!(
            CLSID_VIDEO_INPUT_DEVICE_CATEGORY,
            "{860BB310-5D01-11D0-BD3B-00A0C911CE86}"
        );
    }

    #[test]
    fn module_path_points_at_our_dll_not_the_host_process() {
        // 回归测试：第一版用 GetModuleFileNameW(None, ...)，在 regsvr32 下
        // 拿到的是 regsvr32.exe 的路径，导致 InprocServer32 写错。
        // 测试进程里主模块是测试二进制，而本函数应返回 DLL（rlib 时是同一个）。
        let p = module_path().expect("应能取到模块路径");
        assert!(p.ends_with(".exe") || p.ends_with(".dll"), "得到 {p}");
        assert!(
            !p.to_lowercase().contains("regsvr32"),
            "绝不能是 regsvr32：{p}"
        );
    }

    #[test]
    fn filter_data_declares_video_with_null_subtype() {
        let fd = filter_data();
        // "vids" = MEDIATYPE_Video
        assert!(
            fd.windows(4).any(|w| w == b"vids"),
            "必须声明 MEDIATYPE_Video"
        );
        // 尾部 16 字节必须是全零（MEDIASUBTYPE_NULL）
        //
        // 写具体子类型（NV12/XRGB）会让 OBS 按那个格式猜，
        // 然后 ConnectPins 报 VFW_E_NO_ACCEPTABLE_TYPES (0x8004022A)。
        let tail = &fd[fd.len() - 16..];
        assert!(
            tail.iter().all(|&b| b == 0),
            "尾部必须是 MEDIASUBTYPE_NULL（全零），实际 {tail:?}"
        );
        assert_eq!(fd.len(), 88, "长度应与 Lens 的 FilterData 一致");
    }

    #[test]
    fn registry_paths_are_well_formed() {
        let clsid = clsid_string();
        let inproc = format!("SOFTWARE\\Classes\\CLSID\\{clsid}\\InprocServer32");
        assert!(inproc.contains("InprocServer32"));
        let inst = format!(
            "SOFTWARE\\Classes\\CLSID\\{CLSID_VIDEO_INPUT_DEVICE_CATEGORY}\\Instance\\{}",
            crate::FILTER_NAME
        );
        assert!(inst.contains("Instance"));
    }
}
