//! 虚拟摄像头能力检测
//!
//! 在真正写 COM 实现之前，先回答一个更基础的问题：
//! **这台机器能不能装虚拟摄像头？**
//!
//! 这个模块不调用任何 COM，只做注册表与环境探测，
//! 所以它能在任何 Windows 上安全运行，也容易写测试。

use std::fmt;

/// 虚拟摄像头支持等级
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum VirtualCameraSupport {
    /// 不是 Windows：不可用
    NotWindows,
    /// Windows 10 或更早：没有 MFCreateVirtualCamera
    UnsupportedVersion,
    /// Win11 但系统组件缺失
    MissingComponents,
    /// 可用
    Available,
}

impl VirtualCameraSupport {
    pub fn is_available(self) -> bool {
        self == VirtualCameraSupport::Available
    }
}

impl fmt::Display for VirtualCameraSupport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            VirtualCameraSupport::NotWindows => "非 Windows 平台，虚拟摄像头需要平台专属实现",
            VirtualCameraSupport::UnsupportedVersion => {
                "Windows 版本低于 Build 22000，MFCreateVirtualCamera 不可用"
            }
            VirtualCameraSupport::MissingComponents => "系统版本达标但缺少 Media Foundation 组件",
            VirtualCameraSupport::Available => "可用",
        };
        f.write_str(s)
    }
}

/// 检测结果
#[derive(Debug, Clone)]
pub struct CapabilityReport {
    pub support: VirtualCameraSupport,
    /// Windows Build 号；非 Windows 时为 None
    pub build: Option<u32>,
    /// 系统里已有的摄像头类设备名（含第三方虚拟摄像头）
    pub existing_cameras: Vec<String>,
    /// 诊断信息
    pub notes: Vec<String>,
}

impl CapabilityReport {
    /// 该机器能否运行虚拟摄像头
    pub fn can_run(&self) -> bool {
        self.support.is_available()
    }
}

/// MFCreateVirtualCamera 的最低 Build 号
pub const MIN_BUILD: u32 = 22000;

/// 检测当前机器
///
/// 函数不叫 `detect` 是因为与模块名同名会导致 `use sidelens_vcam::detect;`
/// 之后无法直接调用 —— Rust 的模块与函数在不同命名空间，但导入时只取一个。
pub fn probe() -> CapabilityReport {
    detect_impl()
}

#[cfg(not(windows))]
fn detect_impl() -> CapabilityReport {
    CapabilityReport {
        support: VirtualCameraSupport::NotWindows,
        build: None,
        existing_cameras: Vec::new(),
        notes: vec!["非 Windows 平台".into()],
    }
}

#[cfg(windows)]
fn detect_impl() -> CapabilityReport {
    let mut notes = Vec::new();

    // 顺序很重要：
    //
    // 1. RtlGetVersion —— 唯一能拿到真实版本的 API
    // 2. 注册表 —— 兜底
    // 3. GetVersionExW —— **不能用**，见下面注释
    //
    // 为什么不用 GetVersionExW：没有应用 manifest 时它一律返回
    // Windows 8（6.2 / Build 9200），这是微软从 Win8.1 起引入的"版本谎言"机制。
    // 本项目实测踩过：GetVersionExW 报 9200，而实际是 26300。
    let build = rtl_get_build().or_else(read_build_from_registry);

    if build.is_none() {
        notes.push("无法确定系统版本".into());
    }

    let support = match build {
        None => VirtualCameraSupport::UnsupportedVersion,
        Some(b) if b < MIN_BUILD => VirtualCameraSupport::UnsupportedVersion,
        Some(_) => VirtualCameraSupport::Available,
    };

    CapabilityReport {
        support,
        build,
        existing_cameras: Vec::new(), // 由上层用 SetupAPI 填充（需额外依赖，暂不引入）
        notes,
    }
}

/// `RtlGetVersion` 的 OSVERSIONINFOW 布局
///
/// 自己的结构体而不是复用 `windows` crate 的 `OSVERSIONINFOW`：
/// 因为 `RtlGetVersion` 在 ntdll 里没有官方 Rust 绑定，
/// 需要自己声明，而它的参数布局与 `OSVERSIONINFOW` 一致。
#[cfg(windows)]
#[repr(C)]
struct RtlOsVersionInfoW {
    dw_os_version_info_size: u32,
    dw_major_version: u32,
    dw_minor_version: u32,
    dw_build_number: u32,
    dw_platform_id: u32,
    sz_csd_version: [u16; 128],
}

#[cfg(windows)]
extern "system" {
    fn RtlGetVersion(info: *mut RtlOsVersionInfoW) -> i32;
}

/// 用 `RtlGetVersion` 拿真实 Build 号
///
/// 这是唯一不受 manifest 影响的版本 API。
#[cfg(windows)]
fn rtl_get_build() -> Option<u32> {
    let mut info = RtlOsVersionInfoW {
        dw_os_version_info_size: std::mem::size_of::<RtlOsVersionInfoW>() as u32,
        dw_major_version: 0,
        dw_minor_version: 0,
        dw_build_number: 0,
        dw_platform_id: 0,
        sz_csd_version: [0; 128],
    };

    // SAFETY: info 是合法且大小正确的可写结构体，size 字段已按实际大小初始化。
    // RtlGetVersion 成功返回 0（STATUS_SUCCESS）。
    let status = unsafe { RtlGetVersion(&mut info) };
    if status == 0 {
        Some(info.dw_build_number)
    } else {
        None
    }
}

#[cfg(windows)]
fn read_build_from_registry() -> Option<u32> {
    use windows::core::HSTRING;
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};

    let subkey = HSTRING::from("SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion");
    let value = HSTRING::from("CurrentBuildNumber");

    let mut buf = [0u16; 32];
    let mut size = (buf.len() * 2) as u32;

    // SAFETY: buf 与 size 描述一块合法的可写内存，size 以其字节长度初始化。
    let ok = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            &subkey,
            &value,
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr() as *mut _),
            Some(&mut size),
        )
    };
    if ok.is_err() {
        return None;
    }

    let len = (size as usize / 2).saturating_sub(1);
    let s = String::from_utf16_lossy(&buf[..len.min(buf.len())]);
    s.trim().parse().ok()
}
