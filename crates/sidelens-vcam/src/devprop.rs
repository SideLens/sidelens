//! 虚拟摄像头创建所需的设备属性键
//!
//! # 为什么这一步不可省略
//!
//! 官方 `VCamUtils::RegisterVirtualCamera` 在 `Start` 之前会做 **4 次 AddProperty**：
//!
//! | 属性 | 类型 | 值 |
//! | --- | --- | --- |
//! | `VCamCreate_SourceId` | STRING | sourceId（CLSID 字符串） |
//! | `VCamCreate_FriendlyName` | STRING | 显示名 |
//! | `VCamCreate_Lifetime` | INT32 | 0=Session / 1=System |
//! | `VCamCreate_Access` | INT32 | 0=CurrentUser / 1=AllUsers |
//!
//! 缺任何一个，FrameServer 在创建设备接口时拿不到必要信息，
//! 表现为 `ERROR_PATH_NOT_FOUND`（0x80070003）—— 与真实原因毫无关系的错误码。
//!
//! # 这些 GUID 从哪来
//!
//! Windows SDK 的 `devpkey.h` 在本机没找到（可能是 SDK 版本差异），
//! 从微软官方示例源码里直接抄的常量：
//!
//! ```cpp
//! const winrt::hstring _DEVPKEY_DeviceInterface_VCamCreate_SourceId(
//!     L"{6ac1fbf7-45f7-4e06-bda7-f817ebfa04d1}, 4");
//! ```
//!
//! 属性 ID 从 4 开始，连续到 7。

#![allow(unsafe_code)]

use windows::Win32::Foundation::DEVPROPKEY;

/// 虚拟摄像头属性键的 GUID 部分
///
/// 所有 `VCamCreate_*` 属性共用一个 GUID，用 pid 区分。
pub const VCAM_CREATE_GUID: u128 = 0x6ac1fbf7_45f7_4e06_bda7_f817ebfa04d1;

/// 构造一个 DEVPROPKEY
///
/// `DEVPROPKEY` 是 `{fmtid, pid}` 结构，对应 C++ 里的 `{GUID}, N` 写法。
const fn devpropkey(pid: u32) -> DEVPROPKEY {
    DEVPROPKEY {
        fmtid: windows::core::GUID::from_u128(VCAM_CREATE_GUID),
        pid,
    }
}

/// 媒体源 ID（CLSID 字符串）
pub const DEVPROPKEY_VCAM_CREATE_SOURCE_ID: DEVPROPKEY = devpropkey(4);
/// 设备显示名
pub const DEVPROPKEY_VCAM_CREATE_FRIENDLY_NAME: DEVPROPKEY = devpropkey(5);
/// 生命周期（Session / System）
pub const DEVPROPKEY_VCAM_CREATE_LIFETIME: DEVPROPKEY = devpropkey(6);
/// 访问范围（CurrentUser / AllUsers）
pub const DEVPROPKEY_VCAM_CREATE_ACCESS: DEVPROPKEY = devpropkey(7);

/// 设备接口属性的 GUID（**官方 SDK 定义**）
///
/// 来自 `mfvirtualcamera.h` 第 80-85 行：
///
/// ```cpp
/// // {6EDC630D-C2E3-43B7-B2D1-20525A1AF120}, 3
/// DEFINE_DEVPROPKEY(DEVPKEY_DeviceInterface_IsVirtualCamera, ...);
/// ```
///
/// # 为什么这组属性比 `VCamCreate_*` 重要
///
/// `VCamCreate_*`（`6ac1fbf7`）在 SDK 头文件、`devpkey.h`、以及本机 10 个
/// MF DLL 里**全部查不到** —— 它是官方示例自造的约定值。
///
/// 而这一组是 **SDK 明确定义的系统属性**，系统自己会读。
pub const DEVICE_INTERFACE_GUID: u128 = 0x6EDC630D_C2E3_43B7_B2D1_20525A1AF120;

/// 构造设备接口的 DEVPROPKEY
const fn interface_propkey(pid: u32) -> DEVPROPKEY {
    DEVPROPKEY {
        fmtid: windows::core::GUID::from_u128(DEVICE_INTERFACE_GUID),
        pid,
    }
}

/// 这个设备接口是不是虚拟摄像头（BOOLEAN）
///
/// # 实测：我们的设备上这个属性是空的
///
/// ```text
/// Get-PnpDeviceProperty -KeyName '{6EDC630D-...} 3'
///   → 值 = （空）  类型 = Empty
/// ```
///
/// 类型是 `Empty` 说明属性**根本不存在**，不是「值为 false」。
///
/// 一个虚拟摄像头却没有 `IsVirtualCamera = true` 标记 ——
/// 这很可能就是 FrameServer 判定「这不是合格的虚拟摄像头」的直接依据。
pub const DEVPKEY_DEVICE_INTERFACE_IS_VIRTUAL_CAMERA: DEVPROPKEY = interface_propkey(3);

/// 这个接口是否支持 Windows 相机效果（BOOLEAN）
pub const DEVPKEY_DEVICE_INTERFACE_IS_WIN_CAMERA_EFFECT_AVAILABLE: DEVPROPKEY =
    interface_propkey(4);

/// 关联的物理摄像头符号链接列表（STRING_LIST）
pub const DEVPKEY_DEVICE_INTERFACE_VIRTUAL_CAMERA_ASSOCIATED_CAMERAS: DEVPROPKEY =
    interface_propkey(5);

/// DEVPROP_TYPE_STRING
pub const DEVPROP_TYPE_STRING: u32 = 0x0000_0019;
/// DEVPROP_TYPE_INT32
pub const DEVPROP_TYPE_INT32: u32 = 0x0000_0005;
/// DEVPROP_TYPE_BOOLEAN
///
/// 注意：DEVPROP_TYPE_BOOLEAN 的**值 0 表示 true**（与直觉相反）。
/// 参见 `devpropdef.h`：`#define DEVPROP_TRUE ((DEVPROPTYPE)-1)` 是类型层面，
/// 而布尔值本身是 `0x01`。
pub const DEVPROP_TYPE_BOOLEAN: u32 = 0x0000_0001;

/// 把字符串转成 AddProperty 需要的字节序列
///
/// MF 要求 UTF-16 **含结尾 NUL**，长度按字节算（不是字符数）。
/// 少写那个 NUL 会让属性值被截断。
pub fn string_prop_bytes(s: &str) -> Vec<u8> {
    let wide: Vec<u16> = s.encode_utf16().chain(std::iter::once(0)).collect();
    let mut out = Vec::with_capacity(wide.len() * 2);
    for u in wide {
        out.extend_from_slice(&u.to_le_bytes());
    }
    out
}

/// 把 i32 转成 AddProperty 需要的字节序列
pub fn i32_prop_bytes(v: i32) -> Vec<u8> {
    v.to_le_bytes().to_vec()
}
