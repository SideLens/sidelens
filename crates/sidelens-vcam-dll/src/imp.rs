//! Windows 实现：四个 COM 导出 + 类工厂转发
//!
//! 整块被 `#[cfg(windows)]` 门控，见 crate 文档「跨平台边界」。

use std::sync::atomic::{AtomicU32, Ordering};

use windows::core::{IUnknown, Interface, GUID};
use windows::Win32::Foundation::{
    CLASS_E_CLASSNOTAVAILABLE, E_INVALIDARG, E_POINTER, HMODULE, S_FALSE, S_OK,
};
use windows::Win32::System::Com::IClassFactory;

use sidelens_vcam::media_source::MediaSourceFactory;
use sidelens_vcam::register::{parse_clsid, SIDELENS_SOURCE_CLSID};
use sidelens_vcam::source::SourceConfig;

/// 当前存活的 COM 对象数（用于 DllCanUnloadNow）
static OBJECT_COUNT: AtomicU32 = AtomicU32::new(0);
/// 服务器锁计数（LockServer 调用）
static LOCK_COUNT: AtomicU32 = AtomicU32::new(0);

/// 增加对象计数（类工厂创建对象时调用）
pub fn add_ref() {
    OBJECT_COUNT.fetch_add(1, Ordering::Relaxed);
}

/// 减少对象计数
pub fn release() {
    OBJECT_COUNT.fetch_sub(1, Ordering::Relaxed);
}

/// 是否可以卸载
pub fn can_unload() -> bool {
    OBJECT_COUNT.load(Ordering::Relaxed) == 0 && LOCK_COUNT.load(Ordering::Relaxed) == 0
}

/// 模块句柄（DllMain 时记录，注册表操作需要）
static mut MODULE_HANDLE: HMODULE = HMODULE(std::ptr::null_mut());

/// # Safety
///
/// 由系统在 DLL 加载/卸载时调用，参数由系统保证有效。
///
/// 注意：DllMain 不需要 .def 导出 —— 系统按名字直接找它。
#[no_mangle]
pub unsafe extern "system" fn DllMain(
    hmodule: HMODULE,
    reason: u32,
    _reserved: *mut core::ffi::c_void,
) -> i32 {
    // DLL_PROCESS_ATTACH = 1
    if reason == 1 {
        // SAFETY: 单线程赋值，且仅在 ATTACH 时发生
        MODULE_HANDLE = hmodule;
    }
    1 // TRUE
}

/// FrameServer 用这个拿类工厂
///
/// # Safety
/// `rclsid` / `riid` 必须是有效指针，`ppv` 必须是可写的出参指针。
#[no_mangle]
pub unsafe extern "system" fn DllGetClassObject(
    rclsid: *const GUID,
    riid: *const GUID,
    ppv: *mut *mut core::ffi::c_void,
) -> windows::core::HRESULT {
    if rclsid.is_null() || riid.is_null() || ppv.is_null() {
        return E_POINTER;
    }
    *ppv = std::ptr::null_mut();

    // SAFETY: 三个指针都已判空
    let clsid = *rclsid;
    let our_clsid = match parse_clsid(SIDELENS_SOURCE_CLSID) {
        Some(g) => g,
        None => return E_INVALIDARG,
    };

    if clsid != our_clsid {
        return CLASS_E_CLASSNOTAVAILABLE;
    }

    // 创建类工厂并查询请求的接口
    let factory: IClassFactory = MediaSourceFactory::new(SourceConfig::default()).into();
    let unknown: IUnknown = factory.into();
    // SAFETY: riid 与 ppv 已判空
    let _ = unknown.query(riid, ppv as *mut _);
    S_OK
}

/// 没人引用时允许卸载
///
/// 本身是安全函数，但保持 `extern "system"` 的 COM 约定导出。
#[no_mangle]
pub extern "system" fn DllCanUnloadNow() -> windows::core::HRESULT {
    if can_unload() {
        S_OK
    } else {
        S_FALSE
    }
}

/// regsvr32 注册：把 CLSID 写到 HKCU
///
/// # Safety
/// 由 regsvr32 调用，无参数。
#[no_mangle]
pub unsafe extern "system" fn DllRegisterServer() -> windows::core::HRESULT {
    match register_self() {
        Ok(()) => S_OK,
        Err(e) => {
            eprintln!("[SideLens] DllRegisterServer 失败：{e}");
            windows::Win32::Foundation::E_FAIL
        }
    }
}

/// regsvr32 /u 反注册
///
/// # Safety
///
/// 由 `regsvr32 /u` 调用，无参数；只写当前用户的注册表。
#[no_mangle]
pub unsafe extern "system" fn DllUnregisterServer() -> windows::core::HRESULT {
    match sidelens_vcam::clsid::unregister(SIDELENS_SOURCE_CLSID) {
        Ok(()) => S_OK,
        Err(e) => {
            eprintln!("[SideLens] DllUnregisterServer 失败：{e}");
            windows::Win32::Foundation::E_FAIL
        }
    }
}

/// 把本 DLL 的路径写进注册表
fn register_self() -> Result<(), String> {
    // 取当前模块路径
    let mut buf = [0u16; 1024];
    // SAFETY: buf 与 handle 都是有效值
    let len = unsafe {
        windows::Win32::System::LibraryLoader::GetModuleFileNameW(Some(MODULE_HANDLE), &mut buf)
    };
    if len == 0 {
        return Err("GetModuleFileNameW 失败".into());
    }
    let path = String::from_utf16_lossy(&buf[..len as usize]);

    sidelens_vcam::clsid::register_inproc_server_full(SIDELENS_SOURCE_CLSID, &path)
        .map_err(|e| e.to_string())
}

/// 供测试：读回已登记的路径
pub fn registered_path() -> Option<String> {
    sidelens_vcam::clsid::query_inproc_server(SIDELENS_SOURCE_CLSID).ok()
}
