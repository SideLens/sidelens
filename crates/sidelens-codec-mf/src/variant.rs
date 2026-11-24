//! VARIANT 构造辅助（ICodecAPI 用）
//!
//! # 为什么单独一个模块
//!
//! `ICodecAPI::SetValue` 只吃 `*const VARIANT`，而 windows crate 的 VARIANT
//! 只有 `Default`，没有任何 `From<u32>` / `From<bool>` 实现
//! （grep 过 Win32/System/Variant/mod.rs，只有 Default/Clone）。
//! 编码器与解码器都要设 CODECAPI_*，各写一份手搓联合体的代码迟早会分叉，
//! 所以收在这里。
//!
//! # `VT_BOOL` 与 `VT_UI4` 不能混用（实测踩到的坑）
//!
//! `CODECAPI_AVLowLatencyMode` 的**文档**写的是 `BOOL`，看起来应该用
//! `VT_BOOL`（VARIANT_BOOL，-1 = true）。**实测不是**：
//!
//! ```text
//! SetValue(AVLowLatencyMode, VT_BOOL(-1))  -> 0x80070057 (E_INVALIDARG)
//! SetValue(AVLowLatencyMode, VT_UI4(1))    -> S_OK
//! GetValue(AVLowLatencyMode)               -> vt=19 (VT_UI4) ulVal=0
//! ```
//!
//! 也就是说这个参数在 msmpeg2vdec 系解码器里存的是 `VT_UI4`。
//! 而 `CODECAPI_AVDecNumWorkerThreads` 反过来只吃 `VT_I4`：
//!
//! ```text
//! SetValue(AVDecNumWorkerThreads, VT_UI4(1)) -> 0x80070057
//! SetValue(AVDecNumWorkerThreads, VT_I4(1))  -> S_OK
//! GetValue(AVDecNumWorkerThreads)            -> vt=3 (VT_I4) ulVal=0xFFFFFFFF
//! ```
//!
//! 两者都返回 `E_INVALIDARG` 而不是「不支持」，所以**错误码看不出问题**，
//! 只有实测延迟才暴露。这个坑很贵，写在这里。

use std::mem::ManuallyDrop;

use windows::Win32::System::Variant::{VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0};

/// 构造一个 VT_BOOL 的 VARIANT（VARIANT_BOOL：-1 = true，0 = false）
///
/// 编码器侧用（`CODECAPI_AVEncCommonLowLatency` 吃这个类型）。
pub(crate) fn variant_bool(value: bool) -> VARIANT {
    use windows::Win32::System::Variant::VT_BOOL;
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_BOOL,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: VARIANT_0_0_0 {
                    boolVal: windows::Win32::Foundation::VARIANT_BOOL(if value { -1 } else { 0 }),
                },
            }),
        },
    }
}

/// 构造一个 VT_UI4 的 VARIANT
///
/// 解码器侧的 `CODECAPI_AVLowLatencyMode` 与编码器侧的码控参数都吃这个。
pub(crate) fn variant_u32(value: u32) -> VARIANT {
    use windows::Win32::System::Variant::VT_UI4;
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_UI4,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: VARIANT_0_0_0 { ulVal: value },
            }),
        },
    }
}

/// 构造一个 VT_I4 的 VARIANT
///
/// 解码器侧的 `CODECAPI_AVDecNumWorkerThreads` **只**吃这个
/// （VT_UI4 会返回 E_INVALIDARG，见模块文档）。
pub(crate) fn variant_i32(value: i32) -> VARIANT {
    use windows::Win32::System::Variant::VT_I4;
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_I4,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: VARIANT_0_0_0 { lVal: value },
            }),
        },
    }
}
