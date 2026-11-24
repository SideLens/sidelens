//! CLSID 注册：让 FrameServer 能通过注册表找到我们的媒体源
//!
//! # 为什么 CoRegisterClassObject 不够
//!
//! `CoRegisterClassObject` 只让**本进程内**的 `CoCreateInstance` 找到类工厂。
//! 但 `IMFVirtualCamera::Start` 是由 **FrameServer（svchost 里的独立进程）**
//! 发起激活的 —— 它跨进程查 `HKCR\CLSID\{...}\InprocServer32`，
//! 找对应的 DLL 去加载。
//!
//! 实测错误链：
//!
//! ```text
//! 只 CoRegisterClassObject → Start 报 ERROR_PATH_NOT_FOUND (0x80070003)
//! 补上注册表登记         → 待验证
//! ```
//!
//! # 本实现的限制
//!
//! 正确做法是把媒体源编成 DLL（`VirtualCameraMediaSource.def` 导出
//! `DllGetClassObject` / `DllCanUnloadNow`），MSI 注册它。
//! 当前是**单进程可执行**，无法提供 DLL 给 FrameServer 加载。
//!
//! 所以这里做的是：
//! 1. 提供注册表读写能力（供 DLL 形态使用）
//! 2. 检测当前形态是否支持 FrameServer 激活
//! 3. 在不支持时给出明确诊断，而不是静默失败

#![allow(unsafe_code)]

use std::fmt;

use windows::core::{GUID, HSTRING, PCWSTR};
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, RegQueryValueExW, RegSetValueExW, HKEY,
    HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ,
};

/// 注册错误
#[derive(Debug)]
pub enum RegError {
    Win32(String),
    NotFound,
}

impl fmt::Display for RegError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RegError::Win32(s) => write!(f, "注册表操作失败：{s}"),
            RegError::NotFound => f.write_str("注册表项不存在"),
        }
    }
}

impl std::error::Error for RegError {}

/// CLSID 在 HKCU 下的注册路径
///
/// 用 HKCU 而不是 HKLM：无需管理员权限。
/// FrameServer 查 CLSID 时会同时查 HKLM 和 HKCU 的 Classes 映射。
pub fn clsid_key_path(clsid: &str) -> String {
    format!("Software\\Classes\\CLSID\\{clsid}\\InprocServer32")
}

/// 把 CLSID 登记到当前用户的注册表
///
/// `dll_path` 必须是 **DLL** 的绝对路径 —— FrameServer 会 `LoadLibrary` 它。
/// 传 exe 路径不会被加载（exe 没有 `DllGetClassObject` 导出）。
pub fn register_inproc_server(clsid: &str, dll_path: &str) -> Result<(), RegError> {
    let subkey = HSTRING::from(clsid_key_path(clsid));

    let mut key = HKEY::default();
    // SAFETY: 所有指针都是有效的栈变量；subkey 在调用期间存活
    let status = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            &subkey,
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            &mut key,
            None,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(RegError::Win32(format!("RegCreateKeyExW: {status:?}")));
    }

    // 默认值 = DLL 路径（空值名）
    let wide: Vec<u16> = dll_path.encode_utf16().chain(std::iter::once(0)).collect();
    let bytes = unsafe { std::slice::from_raw_parts(wide.as_ptr() as *const u8, wide.len() * 2) };

    // SAFETY: key 有效；bytes 描述一块有效内存
    let status = unsafe { RegSetValueExW(key, PCWSTR::null(), None, REG_SZ, Some(bytes)) };
    // SAFETY: key 由 RegCreateKeyExW 打开，成对关闭
    unsafe {
        let _ = RegCloseKey(key);
    }

    if status != ERROR_SUCCESS {
        return Err(RegError::Win32(format!("RegSetValueExW: {status:?}")));
    }
    Ok(())
}

/// 写 `ThreadingModel`
///
/// # 为什么少了这一项 COM 会激活失败
///
/// `InprocServer32` 的默认值只是「DLL 在哪」。COM 还要知道**用什么套间模型**
/// 来调用 `DllGetClassObject`：
///
/// | 值 | 含义 |
/// | --- | --- |
/// | （缺省） | Single，只能从 STA 线程激活 |
/// | `Apartment` | STA |
/// | `Free` | MTA |
/// | `Both` | 两者都行，由对象自己保证线程安全 |
///
/// FrameServer 在 MTA 线程池里激活，而我们的媒体源内部全是 `Mutex` + 原子，
/// 两种套间都安全，所以用 `Both`。
///
/// 缺这一项时 COM 会按 Single 处理，从 MTA 激活直接报
/// `CO_E_ATTEMPT_TO_CREATE_OUTSIDE_CLIENT_CONTEXT` 或干脆找不到类。
pub fn register_threading_model(clsid: &str, model: &str) -> Result<(), RegError> {
    let subkey = HSTRING::from(clsid_key_path(clsid));
    let mut key = HKEY::default();
    // SAFETY: 指针都是有效栈变量；subkey 在调用期间存活
    let status = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            &subkey,
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            &mut key,
            None,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(RegError::Win32(format!("RegCreateKeyExW: {status:?}")));
    }

    let wide: Vec<u16> = model.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: wide 是有效内存块
    let bytes = unsafe { std::slice::from_raw_parts(wide.as_ptr() as *const u8, wide.len() * 2) };
    let name = HSTRING::from("ThreadingModel");
    // SAFETY: key 有效；bytes 描述有效内存
    let status = unsafe { RegSetValueExW(key, &name, None, REG_SZ, Some(bytes)) };
    // SAFETY: key 成对关闭
    unsafe {
        let _ = RegCloseKey(key);
    }
    if status != ERROR_SUCCESS {
        return Err(RegError::Win32(format!(
            "RegSetValueExW(ThreadingModel): {status:?}"
        )));
    }
    Ok(())
}

/// 完整登记：DLL 路径 + ThreadingModel
pub fn register_inproc_server_full(clsid: &str, dll_path: &str) -> Result<(), RegError> {
    register_inproc_server(clsid, dll_path)?;
    register_threading_model(clsid, "Both")
}

/// 登记到 **HKLM**（全机器，所有用户与会话可见）
///
/// # 为什么 HKCU 不够（实测踩坑）
///
/// `IMFVirtualCamera::Start` 是由 **FrameServer** 发起激活的，它跑在
/// `svchost.exe -k Camera`，也就是 **session 0 的系统服务上下文**。
///
/// `HKCU\Software\Classes` 只属于**当前登录用户**：
///
/// | 位置 | 谁读得到 |
/// | --- | --- |
/// | `HKCU\Software\Classes\CLSID` | 只有当前用户进程 |
/// | `HKLM\Software\Classes\CLSID` | 所有进程（含服务） |
///
/// 实测：HKCU 里登记得好好的（路径与 ThreadingModel 都能读回来），
/// `IMFVirtualCamera::Start` 仍然报 `REGDB_E_CLASSNOTREG (0x80040154)`。
/// 因为 FrameServer 根本看不到 HKCU。
///
/// 需要管理员权限。
pub fn register_inproc_server_machine(clsid: &str, dll_path: &str) -> Result<(), RegError> {
    let subkey = HSTRING::from(clsid_key_path(clsid));
    let mut key = HKEY::default();
    // SAFETY: 指针都是有效栈变量；subkey 在调用期间存活
    let status = unsafe {
        RegCreateKeyExW(
            HKEY_LOCAL_MACHINE,
            &subkey,
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            &mut key,
            None,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(RegError::Win32(format!(
            "RegCreateKeyExW(HKLM): {status:?}（需要管理员权限）"
        )));
    }

    let wide: Vec<u16> = dll_path.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: wide 是有效内存块
    let bytes = unsafe { std::slice::from_raw_parts(wide.as_ptr() as *const u8, wide.len() * 2) };
    // SAFETY: key 有效
    let mut status = unsafe { RegSetValueExW(key, PCWSTR::null(), None, REG_SZ, Some(bytes)) };

    if status == ERROR_SUCCESS {
        let name = HSTRING::from("ThreadingModel");
        let model: Vec<u16> = "Both".encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: model 是有效内存块
        let mbytes =
            unsafe { std::slice::from_raw_parts(model.as_ptr() as *const u8, model.len() * 2) };
        // SAFETY: key 有效
        status = unsafe { RegSetValueExW(key, &name, None, REG_SZ, Some(mbytes)) };
    }

    // SAFETY: key 成对关闭
    unsafe {
        let _ = RegCloseKey(key);
    }
    if status != ERROR_SUCCESS {
        return Err(RegError::Win32(format!("RegSetValueExW(HKLM): {status:?}")));
    }
    Ok(())
}

/// 从 HKLM 撤销登记
pub fn unregister_machine(clsid: &str) -> Result<(), RegError> {
    let root = HSTRING::from(format!("Software\\Classes\\CLSID\\{clsid}"));
    // SAFETY: 路径固定；不存在时返回 FILE_NOT_FOUND 也无害
    let status = unsafe { RegDeleteTreeW(HKEY_LOCAL_MACHINE, &root) };
    if status != ERROR_SUCCESS && status != windows::Win32::Foundation::ERROR_FILE_NOT_FOUND {
        return Err(RegError::Win32(format!("RegDeleteTreeW(HKLM): {status:?}")));
    }
    Ok(())
}

/// 查询已登记的 DLL 路径
pub fn query_inproc_server(clsid: &str) -> Result<String, RegError> {
    let subkey = HSTRING::from(clsid_key_path(clsid));

    let mut key = HKEY::default();
    // SAFETY: 指针有效
    let status = unsafe {
        windows::Win32::System::Registry::RegOpenKeyExW(
            HKEY_CURRENT_USER,
            &subkey,
            None,
            KEY_READ,
            &mut key,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(RegError::NotFound);
    }

    let mut buf = [0u16; 512];
    let mut size = (buf.len() * 2) as u32;
    // SAFETY: buf 与 size 描述有效可写内存
    let status = unsafe {
        RegQueryValueExW(
            key,
            PCWSTR::null(),
            None,
            None,
            Some(buf.as_mut_ptr() as *mut u8),
            Some(&mut size),
        )
    };
    // SAFETY: 成对关闭
    unsafe {
        let _ = RegCloseKey(key);
    }

    if status != ERROR_SUCCESS {
        return Err(RegError::Win32(format!("RegQueryValueExW: {status:?}")));
    }

    let len = (size as usize / 2).saturating_sub(1);
    Ok(String::from_utf16_lossy(&buf[..len.min(buf.len())]))
}

/// 撤销登记
pub fn unregister(clsid: &str) -> Result<(), RegError> {
    let root = HSTRING::from(format!("Software\\Classes\\CLSID\\{clsid}"));
    // SAFETY: 路径固定，删除失败（不存在）也无害
    let status = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, &root) };
    if status != ERROR_SUCCESS && status != windows::Win32::Foundation::ERROR_FILE_NOT_FOUND {
        return Err(RegError::Win32(format!("RegDeleteTreeW: {status:?}")));
    }
    Ok(())
}

/// 当前形态能否被 FrameServer 激活
///
/// 只有在「媒体源编成 DLL 并已登记」时才可能成功。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActivationReadiness {
    /// 已登记 DLL，可以尝试 Start
    Ready { dll_path: String },
    /// 未登记
    NotRegistered,
    /// 登记了但文件不存在
    DllMissing { dll_path: String },
}

impl ActivationReadiness {
    pub fn can_activate(&self) -> bool {
        matches!(self, ActivationReadiness::Ready { .. })
    }
}

impl fmt::Display for ActivationReadiness {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ActivationReadiness::Ready { dll_path } => {
                write!(f, "已就绪，DLL = {dll_path}")
            }
            ActivationReadiness::NotRegistered => {
                f.write_str("CLSID 未登记（FrameServer 找不到媒体源实现）")
            }
            ActivationReadiness::DllMissing { dll_path } => {
                write!(f, "已登记但 DLL 不存在：{dll_path}")
            }
        }
    }
}

/// 检查激活准备状态
pub fn check_readiness(clsid: &str) -> ActivationReadiness {
    match query_inproc_server(clsid) {
        Ok(path) => {
            if std::path::Path::new(&path).exists() {
                ActivationReadiness::Ready { dll_path: path }
            } else {
                ActivationReadiness::DllMissing { dll_path: path }
            }
        }
        Err(_) => ActivationReadiness::NotRegistered,
    }
}

/// 便捷：从 GUID 取字符串形式
pub fn guid_to_clsid_string(g: GUID) -> String {
    format!(
        "{{{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}}}",
        g.data1,
        g.data2,
        g.data3,
        g.data4[0],
        g.data4[1],
        g.data4[2],
        g.data4[3],
        g.data4[4],
        g.data4[5],
        g.data4[6],
        g.data4[7]
    )
}
