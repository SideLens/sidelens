//! 注册表与 DirectShow 滤镜注册。
//!
//! 两个职责：
//!
//! 1. **卸载项**：写 `HKCU\...\Uninstall\SideLens`，让「应用和功能」里能
//!    看到 SideLens 并能卸载。per-user 安装写 HKCU —— 同一台机器上另一个
//!    用户看不到、也卸不掉，这正是 per-user 的语义。
//! 2. **DirectShow 滤镜注册**：直接 `LoadLibrary` + `DllRegisterServer`，
//!    而不是起 `regsvr32` 子进程。理由见下面的长注释。
//!
//! 非 Windows 上整个模块退化成返回 "不支持"，让 `cargo test` 在 Linux 上照跑。

#[cfg(windows)]
mod imp {
    use std::path::Path;

    use windows::core::{HRESULT, PCWSTR};
    use windows::Win32::Foundation::{CloseHandle, ERROR_SUCCESS, HANDLE};
    use windows::Win32::Security::{
        GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
    };
    use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
    use windows::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, RegSetValueExW, HKEY, HKEY_CURRENT_USER,
        KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    use sidelens_update::layout::UNINSTALL_KEY;

    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// 当前进程是否已提权。
    ///
    /// 不用「能否写 HKLM 试一下」来判断：那会在注册表里留下试探性的脏键。
    /// `TokenElevation` 是只读查询，没有副作用。
    ///
    /// 判断失败时返回 `false`（按未提权处理）：宁可少注册一次滤镜，
    /// 也不要因为误判为已提权而在写 HKLM 时抛出一串错误。
    pub fn is_elevated() -> bool {
        unsafe {
            let mut token = HANDLE::default();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
                return false;
            }
            let mut elevation = TOKEN_ELEVATION::default();
            let mut returned = 0u32;
            let result = GetTokenInformation(
                token,
                TokenElevation,
                Some(&mut elevation as *mut _ as *mut core::ffi::c_void),
                std::mem::size_of::<TOKEN_ELEVATION>() as u32,
                &mut returned,
            );
            let _ = CloseHandle(token);
            result.is_ok() && elevation.TokenIsElevated != 0
        }
    }

    fn create_key(path: &str, hkey: HKEY) -> windows::core::Result<HKEY> {
        let path_w = wide(path);
        let mut key = HKEY::default();
        let result = unsafe {
            RegCreateKeyExW(
                hkey,
                PCWSTR(path_w.as_ptr()),
                None,
                PCWSTR::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_WRITE,
                None,
                &mut key,
                None,
            )
        };
        if result == ERROR_SUCCESS {
            Ok(key)
        } else {
            Err(windows::core::Error::from_hresult(HRESULT::from_win32(
                result.0,
            )))
        }
    }

    fn set_string(key: HKEY, name: &str, value: &str) -> windows::core::Result<()> {
        let name_w = wide(name);
        let value_w = wide(value);
        let bytes = unsafe {
            std::slice::from_raw_parts(
                value_w.as_ptr() as *const u8,
                value_w.len() * std::mem::size_of::<u16>(),
            )
        };
        let result =
            unsafe { RegSetValueExW(key, PCWSTR(name_w.as_ptr()), None, REG_SZ, Some(bytes)) };
        if result == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(windows::core::Error::from_hresult(HRESULT::from_win32(
                result.0,
            )))
        }
    }

    /// 写/更新卸载项
    pub fn write_uninstall(values: &[(String, String)]) -> Result<(), String> {
        let key = create_key(UNINSTALL_KEY, HKEY_CURRENT_USER)
            .map_err(|e| format!("打开卸载注册表项失败：{e}"))?;
        let mut first_error = None;
        for (name, value) in values {
            if let Err(e) = set_string(key, name, value) {
                first_error.get_or_insert(format!("写 {name} 失败：{e}"));
            }
        }
        unsafe {
            let _ = RegCloseKey(key);
        }
        match first_error {
            None => Ok(()),
            Some(e) => Err(e),
        }
    }

    /// 删除卸载项（整棵子树）
    pub fn remove_uninstall() -> Result<(), String> {
        let path = wide(UNINSTALL_KEY);
        let result = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(path.as_ptr())) };
        // 键本来就不存在视为成功：重复卸载不该报错。
        if result == ERROR_SUCCESS || result == windows::Win32::Foundation::ERROR_FILE_NOT_FOUND {
            Ok(())
        } else {
            Err(format!("删除卸载注册表项失败：{:?}", result))
        }
    }

    /// 直接调用 DLL 的 `DllRegisterServer`。
    ///
    /// # 为什么不用 `regsvr32`
    ///
    /// `regsvr32` 有三个具体问题：
    ///
    /// 1. **拿不到 HRESULT**：它把结果塞进一个模态对话框，脚本化只能靠
    ///    猜退出码，而它的退出码在「DLL 内部返回失败」时仍然是 0。
    /// 2. **多一次进程启动**：安装器要立刻知道成功与否才能写
    ///    `installed.json` 里的 `registered_dshow`。
    /// 3. **路径含空格要额外引号处理**：`regsvr32 "/s <path>"` 的引号规则
    ///    与 `CreateProcess` 不同，是一个独立的坑。
    ///
    /// 我们的 DLL 已经导出了 `DllRegisterServer`（见 `crates/sidelens-dshow`），
    /// 直接 `LoadLibraryW` + `GetProcAddress` 就能拿到真实返回值。
    pub fn register_dshow(dll: &Path) -> Result<(), String> {
        if !is_elevated() {
            return Err("注册 DirectShow 滤镜需要管理员权限".into());
        }
        let path_w = wide(&dll.display().to_string());
        unsafe {
            let module = LoadLibraryW(PCWSTR(path_w.as_ptr()))
                .map_err(|e| format!("加载 {dll:?} 失败：{e}"))?;
            let name = b"DllRegisterServer\0";
            let proc = GetProcAddress(module, windows::core::PCSTR(name.as_ptr()))
                .ok_or_else(|| format!("{dll:?} 没有导出 DllRegisterServer"))?;
            let function: unsafe extern "system" fn() -> HRESULT = std::mem::transmute(proc);
            let result = function();
            if result.is_ok() {
                Ok(())
            } else {
                Err(format!("DllRegisterServer 返回 0x{:08X}", result.0))
            }
        }
    }

    /// 反注册
    pub fn unregister_dshow(dll: &Path) -> Result<(), String> {
        if !is_elevated() {
            return Err("反注册 DirectShow 滤镜需要管理员权限".into());
        }
        let path_w = wide(&dll.display().to_string());
        unsafe {
            let module = LoadLibraryW(PCWSTR(path_w.as_ptr()))
                .map_err(|e| format!("加载 {dll:?} 失败：{e}"))?;
            let name = b"DllUnregisterServer\0";
            let proc = GetProcAddress(module, windows::core::PCSTR(name.as_ptr()))
                .ok_or_else(|| format!("{dll:?} 没有导出 DllUnregisterServer"))?;
            let function: unsafe extern "system" fn() -> HRESULT = std::mem::transmute(proc);
            let result = function();
            if result.is_ok() {
                Ok(())
            } else {
                Err(format!("DllUnregisterServer 返回 0x{:08X}", result.0))
            }
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use std::path::Path;

    pub fn is_elevated() -> bool {
        false
    }
    pub fn write_uninstall(_values: &[(String, String)]) -> Result<(), String> {
        Err("注册表仅在 Windows 上可用".into())
    }
    pub fn remove_uninstall() -> Result<(), String> {
        Err("注册表仅在 Windows 上可用".into())
    }
    pub fn register_dshow(_dll: &Path) -> Result<(), String> {
        Err("DirectShow 仅在 Windows 上可用".into())
    }
    pub fn unregister_dshow(_dll: &Path) -> Result<(), String> {
        Err("DirectShow 仅在 Windows 上可用".into())
    }
}

pub use imp::{is_elevated, register_dshow, remove_uninstall, unregister_dshow, write_uninstall};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 卸载项的值列表来自发布层而不是本地重写() {
        use sidelens_update::layout::{uninstall_values, InstalledRecord, Layout};
        use sidelens_update::Version;

        let layout = Layout::new(r"C:\x\SideLens");
        let record = InstalledRecord::new(Version::new(0, 2, 0), &layout, String::new());
        let values = uninstall_values(&record, std::path::Path::new("setup.exe"));
        // 只要发布层给出非空列表，这里就照着写；本模块不自己拼文案。
        assert!(!values.is_empty());
        assert!(values.iter().any(|(k, _)| k == "UninstallString"));
    }

    #[test]
    fn 非_windows_上注册表操作返回可读错误而不是_panic() {
        if cfg!(windows) {
            return;
        }
        assert!(remove_uninstall().is_err());
    }
}
