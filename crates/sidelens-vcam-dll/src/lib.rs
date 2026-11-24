//! SideLens 虚拟摄像头 DLL
//!
//! # 为什么必须是 DLL
//!
//! 实测结论（详见 [docs/vcam-pitfalls.md](../../../docs/vcam-pitfalls.md)）：
//!
//! ```text
//! 不登记注册表      → CLASSNOTREG (0x80040154)
//! 登记 exe 路径     → DLL 中出错 (0x800401F9)
//! ```
//!
//! `IMFVirtualCamera::Start` 由 **FrameServer**（独立的 svchost 进程）发起，
//! 它按注册表里的 `InprocServer32` 路径 `LoadLibrary` 我们的模块，
//! 再调 `DllGetClassObject` 拿类工厂。
//!
//! 所以本 crate 编译成 **cdylib**，导出四个标准 COM 入口：
//!
//! | 导出 | 作用 |
//! | --- | --- |
//! | `DllGetClassObject` | FrameServer 用它拿类工厂 |
//! | `DllCanUnloadNow` | 没人引用时允许卸载 |
//! | `DllRegisterServer` | regsvr32 调用（写注册表） |
//! | `DllUnregisterServer` | regsvr32 /u 调用（清注册表） |
//!
//! # 跨平台边界
//!
//! 整个 crate 只有 Windows 有实体（COM 服务器在非 Windows 上不存在）。
//! 非 Windows target 上编译出一个**空 cdylib**，只为让
//! `cargo check --workspace --target <其他平台>` 不因这个 crate 整体失败
//! （见 [docs/cross-platform.md](../../../docs/cross-platform.md) P2）。
//!
//! 注意：**不能**用 `#![cfg(windows)]` 一刀切 —— cdylib 在没有 crate 根
//! 内容时会报「no library targets found」之类的怪错误。正确写法是
//! 「所有内容进 `mod imp`，模块本身 cfg 门控」，crate 根保留 `no_mangle`
//! 导出所需的最小骨架。
//!
//! # 当前状态
//!
//! - [x] 四个导出函数
//! - [x] 类工厂转发到 `sidelens-vcam`
//! - [x] regsvr32 可注册/反注册
//! - [x] 命名管道与 EXE 通信（帧来源）
//! - [ ] FrameServer 真正调用 `IMFMediaSource::Start`（B3 卡点）

#![allow(unsafe_code)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]
// COM 导出函数名由 Windows 规范固定（DllGetClassObject 等），
// 不能改成 snake_case —— 改了 FrameServer 就找不到。
#![allow(non_snake_case)]

#[cfg(windows)]
mod imp;

#[cfg(windows)]
pub use imp::*;

// 非 Windows 上没有 COM 服务器可导。留一个显式说明，避免有人误以为
// 「能编过 = 能用」。
#[cfg(not(windows))]
pub const NOT_SUPPORTED: &str = "虚拟摄像头 DLL 仅在 Windows 上存在";
