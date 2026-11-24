//! SideLens 虚拟摄像头
//!
//! # 为什么这个模块值得单独一个 crate
//!
//! 虚拟摄像头是**平台专属**能力，且要求进程外 COM 服务器常驻。
//! 把它和协议/缓冲逻辑混在一起，会让核心 crate 绑上 Windows 依赖，
//! 破坏「Linux 上只编 Rust 核心」这个前提。
//!
//! # Windows 的两条路
//!
//! | 方案 | 最低系统 | 签名要求 | 用户操作 |
//! | --- | --- | --- | --- |
//! | **MFCreateVirtualCamera** | Win11 Build 22000+ | 无 | 无 |
//! | DirectShow 滤镜 | 全版本 | 需注册 COM | regsvr32 |
//!
//! 选前者：不需要内核驱动、不需要 EV 证书、不需要重启。
//! 代价是只支持 Win11 —— 取舍是「接受 Win11+ 门槛换取零安装摩擦」。
//!
//! # 跨平台边界（这一节是踩坑后补的）
//!
//! 第一版把 10 个模块全部无条件 `pub mod`，结果是**在 Linux / macOS /
//! Android / iOS 上编译出 142 个错误**。而这些错误看起来像「Windows API
//! 不可用」，实际原因是**根本没写 cfg 守卫** —— 见
//! [`docs/cross-platform.md`](../../../docs/cross-platform.md) 第 5.2 节。
//!
//! 现在的分层：
//!
//! | 模块 | 平台 | 理由 |
//! | --- | --- | --- |
//! | [`detect`] | 全平台 | 非 Windows 返回 `NotWindows`，让调用方能优雅降级 |
//! | [`source`] | 全平台 | 纯状态机（`SourceState` / `Resolution` / `FrameRate`），零 Windows 依赖 |
//! | 其余 8 个 | 仅 Windows | 全部直接调 COM / Media Foundation |
//!
//! **判据**：模块里只要出现一处 `windows::`，就必须门控。
//! 「反正这个 crate 只在 Windows 用」不是理由 —— 那会把整个 crate 变成
//! 不可移植的孤岛，连带挡住 `sidelens-codec` 这类真正平台无关的层。
//!
//! # 当前实现进度
//!
//! - [x] 系统能力检测（Build 版本、注册表兜底）
//! - [x] 注册虚拟摄像头设备
//! - [x] 实现 IMFMediaSource（提供视频流）
//! - [ ] 帧注入与格式协商（FrameServer 未调用 `Start`，见 B3）

// Windows API 调用必须 unsafe；本 crate 的 unsafe 全部集中在对 Win32 的直接调用，
// 每处都有 SAFETY 注释说明前提条件。
#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_debug_implementations)]

// 全平台可用：状态机与能力检测
pub mod detect;
pub mod source;

// 仅 Windows：这些模块内部无条件使用 `windows` crate 与 `sidelens_ipc`，
// 在非 Windows target 上会以 E0433 炸掉整个 crate。
#[cfg(windows)]
pub mod activate;
#[cfg(windows)]
pub mod callback;
#[cfg(windows)]
pub mod clsid;
#[cfg(windows)]
pub mod com;
#[cfg(windows)]
pub mod devprop;
#[cfg(windows)]
pub mod frame_source;
#[cfg(windows)]
pub mod media_source;
#[cfg(windows)]
pub mod register;
#[cfg(windows)]
pub mod stream;

pub use detect::{probe, CapabilityReport, VirtualCameraSupport, MIN_BUILD};
pub use source::{FrameRate, Operation, Resolution, SourceConfig, SourceState, TransitionError};

#[cfg(windows)]
pub use com::{InitError, MediaFoundation};
#[cfg(windows)]
pub use frame_source::{FramePoll, FrameSource, SourceStatsSnapshot};
#[cfg(windows)]
pub use register::{
    fsm_virtual_camera_category, parse_clsid, video_camera_category, CameraSpec, RegisterError,
    VirtualCamera, SIDELENS_SOURCE_CLSID,
};
