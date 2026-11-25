//! Windows 登录自启动。
//!
//! 只写当前用户的 `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`，
//! 不需要管理员权限，也不会影响同一台电脑上的其他用户。值名固定，重复启用
//! 会覆盖成当前可执行文件路径，便于程序升级或移动目录后自动纠正旧路径。
//!
//! 命令使用 `--gui --background`：登录时只启动接收器并驻留托盘，不抢焦点；
//! 用户手动双击程序时仍走普通的 `--gui`，窗口照常显示。

#![cfg(windows)]

use std::path::Path;

use windows::core::PCWSTR;
use windows::Win32::Foundation::ERROR_FILE_NOT_FOUND;
use windows::Win32::System::Registry::{
    RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY,
    HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_SZ,
};

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE_NAME: &str = "SideLens";

/// 生成注册表里的启动命令。
///
/// Windows 的 `Run` 值按命令行解析，路径包含空格时**必须**加引号，否则
/// `C:\Program Files\SideLens\sidelensd.exe` 会被拆成 `C:\Program` 和
/// `Files\SideLens\sidelensd.exe`。这里只负责引号，不负责规范化路径：
/// 调用方传入什么就用什么，方便测试和诊断。
pub fn command_line(exe: &Path) -> String {
    format!("\"{}\" --gui --background", exe.display())
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

fn open_run_key(access: windows::Win32::System::Registry::REG_SAM_FLAGS) -> Result<HKEY, String> {
    let path = wide(RUN_KEY);
    let mut key = HKEY::default();
    // SAFETY: path 在调用期间存活；成功时 key 由调用方关闭。
    let status = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(path.as_ptr()),
            None,
            access,
            &mut key,
        )
    };
    if status == windows::Win32::Foundation::ERROR_SUCCESS {
        Ok(key)
    } else {
        Err(format!("打开 HKCU\\{RUN_KEY} 失败：{status:?}"))
    }
}

/// 读取当前用户登记的启动命令；值不存在时返回 `None`。
pub fn registered_command() -> Result<Option<String>, String> {
    let key = open_run_key(KEY_QUERY_VALUE)?;
    let name = wide(VALUE_NAME);
    let mut kind = windows::Win32::System::Registry::REG_VALUE_TYPE::default();
    let mut bytes = 0u32;
    // SAFETY: name 有效；第一次调用只查询长度，不传数据缓冲区。
    let mut status = unsafe {
        RegQueryValueExW(
            key,
            PCWSTR(name.as_ptr()),
            None,
            Some(&mut kind),
            None,
            Some(&mut bytes),
        )
    };
    if status == ERROR_FILE_NOT_FOUND {
        // SAFETY: key 与 open_run_key 成对关闭。
        unsafe {
            let _ = RegCloseKey(key);
        }
        return Ok(None);
    }
    if status != windows::Win32::Foundation::ERROR_SUCCESS {
        // SAFETY: key 与 open_run_key 成对关闭。
        unsafe {
            let _ = RegCloseKey(key);
        }
        return Err(format!("读取 SideLens 自启动项失败：{status:?}"));
    }

    // REG_SZ 的字节数包含结尾 NUL；多分配两个 u16 是防御性余量，
    // 防止注册表里存在不规范数据时第二次查询需要更大缓冲区。
    let mut data = vec![0u16; (bytes as usize / 2).saturating_add(1)];
    let mut data_bytes = (data.len() * 2) as u32;
    // SAFETY: data 缓冲区大小由 data_bytes 准确描述。
    status = unsafe {
        RegQueryValueExW(
            key,
            PCWSTR(name.as_ptr()),
            None,
            Some(&mut kind),
            Some(data.as_mut_ptr() as *mut u8),
            Some(&mut data_bytes),
        )
    };
    // SAFETY: key 与 open_run_key 成对关闭。
    unsafe {
        let _ = RegCloseKey(key);
    }
    if status != windows::Win32::Foundation::ERROR_SUCCESS {
        return Err(format!("读取 SideLens 启动命令失败：{status:?}"));
    }
    if kind != REG_SZ {
        return Err(format!("SideLens 启动项类型异常：{kind:?}"));
    }
    let units = (data_bytes as usize / 2).min(data.len());
    let end = data[..units]
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(units);
    Ok(Some(String::from_utf16_lossy(&data[..end])))
}

/// 当前用户是否已经启用指向当前 exe 的 SideLens 登录自启动。
pub fn is_enabled(exe: &Path) -> Result<bool, String> {
    Ok(registered_command()?.is_some_and(|command| command == command_line(exe)))
}

/// 启用或停用当前用户的登录自启动。
///
/// `enable=true` 时写入当前 exe 的完整路径；`enable=false` 时删除值。
/// 值不存在视为“已经停用”，这样重复点击不会报错。
pub fn set_enabled(enable: bool, exe: &Path) -> Result<(), String> {
    let key = open_run_key(KEY_SET_VALUE)?;
    let name = wide(VALUE_NAME);
    let status = if enable {
        let command = command_line(exe);
        let data = wide(&command);
        let bytes = unsafe {
            std::slice::from_raw_parts(
                data.as_ptr() as *const u8,
                data.len() * std::mem::size_of::<u16>(),
            )
        };
        // SAFETY: key/name/data 在调用期间全部有效。
        unsafe { RegSetValueExW(key, PCWSTR(name.as_ptr()), None, REG_SZ, Some(bytes)) }
    } else {
        // SAFETY: key/name 在调用期间有效。
        unsafe { RegDeleteValueW(key, PCWSTR(name.as_ptr())) }
    };
    // SAFETY: key 与 open_run_key 成对关闭。
    unsafe {
        let _ = RegCloseKey(key);
    }

    if status == windows::Win32::Foundation::ERROR_SUCCESS
        || (!enable && status == ERROR_FILE_NOT_FOUND)
    {
        Ok(())
    } else if enable {
        Err(format!("写入 SideLens 自启动项失败：{status:?}"))
    } else {
        Err(format!("删除 SideLens 自启动项失败：{status:?}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_line_quotes_spaces_and_requests_background_gui() {
        let command = command_line(Path::new(r"C:\Program Files\SideLens\sidelensd.exe"));
        assert_eq!(
            command,
            r#""C:\Program Files\SideLens\sidelensd.exe" --gui --background"#
        );
    }

    #[test]
    fn command_line_keeps_simple_path_quoted() {
        let command = command_line(Path::new(r"D:\apps\sidelensd.exe"));
        assert_eq!(command, r#""D:\apps\sidelensd.exe" --gui --background"#);
    }

    #[test]
    fn enabled_state_requires_the_exact_current_command_line() {
        let exe = Path::new(r"C:\Program Files\SideLens\sidelensd.exe");
        let expected = command_line(exe);
        assert_eq!(expected, command_line(exe));
        assert_ne!(expected, command_line(Path::new(r"D:\old\sidelensd.exe")));
        assert_ne!(
            expected,
            r#""C:\Program Files\SideLens\sidelensd.exe" --gui"#
        );
    }

    #[test]
    fn command_line_does_not_duplicate_arguments() {
        let command = command_line(Path::new(r"C:\SideLens\sidelensd.exe"));
        assert_eq!(command.matches("--gui").count(), 1);
        assert_eq!(command.matches("--background").count(), 1);
    }
}
