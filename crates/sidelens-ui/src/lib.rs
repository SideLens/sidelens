//! SideLens 原生 UI 层：纯 Win32 控件 + DPI 感知 + 非阻塞消息泵
//!
//! # 为什么不用 webview / Electron / Tauri
//!
//! 用户对 SideLens 的原始要求是「超低的连接延迟、**极致的原生 ui 性能体验**，
//! 哪怕不同系统的 ui 界面用不同的框架来写都可以」。三条硬指标直接排除 webview：
//!
//! | 方案 | 启动 | 常驻内存 | 滚动/动画 |
//! | --- | --- | --- | --- |
//! | webview（WebView2） | 数百毫秒（首次更久） | 几十 MB | 合成器掉帧，1ms tick 下尤其明显 |
//! | 纯 Win32 控件 | 数十毫秒 | 几 MB | 系统原生，无掉帧 |
//!
//! 而这个软件的界面复杂度是「几行状态文字 + 一块视频 + 几个开关」——
//! 原生控件的开发成本并不高，收益却是实打实的。所以本 crate **不引任何
//! 第三方 GUI 库**，只有 windows crate 的 FFI 绑定。
//!
//! # 分层
//!
//! | 层 | 模块 | 编译条件 | 内容 |
//! | --- | --- | --- | --- |
//! | 跨平台 | [`dpi`] | 无条件 | 逻辑/物理像素换算、字体磅值换算 |
//! | 跨平台 | [`layout`] | 无条件 | 锚点平移、视频区增长、矩形缩放 |
//! | 跨平台 | [`control`] / [`event`] / [`error`] | 无条件 | 句柄、事件、错误 |
//! | Windows | [`win32`] | `cfg(windows)` | 真窗口、真控件、真消息泵 |
//!
//! 前四层是**纯函数**，在 Linux CI 上能跑全部单元测试。这不是洁癖：
//! DPI 换算错一次，界面在 200% 缩放下就会小一半，而这种错误在开发机的
//! 100% 屏幕上永远看不出来 —— 只有把算术从 Win32 里拆出来，才可能用
//! 一组 DPI 值把边界钉死。
//!
//! # 与渲染层的关系
//!
//! 本 crate 是**外壳**：它只负责窗口、控件、事件，**不画视频**。
//! 视频区域由 [`MainWindow::video_rect`] 暴露一块客户区矩形（物理像素），
//! 由 `sidelens-render-gpu` 在那块矩形上建 swap chain 并出画。
//! 这样 UI 层不需要知道 D3D11 的存在，渲染层也不需要知道按钮在哪。
//!
//! # 快速上手
//!
//! ```no_run
//! use sidelens_ui::{MainWindow, UiEvent};
//!
//! # fn main() -> Result<(), sidelens_ui::UiError> {
//! let mut win = MainWindow::new("SideLens", 900, 640)?;
//! let start = win.add_button("开始", 660, 560, 100, 32);
//!
//! loop {
//!     while let Some(ev) = win.pump() {
//!         match ev {
//!             UiEvent::ButtonClicked(id) if id == start => println!("开始推流"),
//!             UiEvent::Closed => return Ok(()),
//!             other => println!("{other}"),
//!         }
//!     }
//!     // 这里放 1ms tick 的视频 / 网络逻辑；pump 不会阻塞
//!     std::thread::sleep(std::time::Duration::from_millis(1));
//! }
//! # }
//! ```
//!
//! # 集成须知（给 apps/ 下的真实客户端）
//!
//! ## 1. 必须嵌 comctl32 v6 的 manifest
//!
//! 没有 manifest 的进程加载的是 comctl32 **v5**，按钮/复选框/下拉框会退回
//! Win95 经典外观（方块灰、无圆角、标题栏深蓝渐变）。本 crate 的 `build.rs`
//! 只给**自己的 examples** 嵌了 manifest —— 库没有链接阶段，管不到最终 exe。
//!
//! 真实客户端要在自己的 `build.rs` 里做同样的事（`/MANIFESTINPUT:` +
//! `/MANIFEST:EMBED`，见本 crate `build.rs` 里的 XML 模板）。
//!
//! ## 2. 首帧闪烁：用 `new_hidden` + `show`
//!
//! [`MainWindow::new`] 建完立刻显示。如果后面还要建十几个控件，
//! 用户会先看到空窗口再看到控件一个个冒出来。正确顺序是
//! [`MainWindow::new_hidden`] → 建控件 → [`MainWindow::show`]。
//!
//! ## 3. 视频区要给渲染层
//!
//! [`MainWindow::video_rect`] 返回**物理像素**的客户区矩形，每次 resize /
//! DPI 变化后都要重新取。`sidelens-render-gpu` 用它建 swap chain 的子窗口。
//!
//! # 非 Windows 上怎么办
//!
//! 本 crate 在非 Windows 上仍然能编译、能跑 [`dpi`] / [`layout`] 的全部测试，
//! 但没有 [`MainWindow`]。调用方应该用 `#[cfg(windows)]` 门控 UI 分支，
//! 其它平台退回命令行模式 —— 而不是给整个 crate 加 `#![cfg(windows)]`
//! （那会让 Linux CI 一行都测不到）。

#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_debug_implementations)]

pub mod control;
pub mod dpi;
pub mod error;
pub mod event;
pub mod layout;

#[cfg(windows)]
mod win32;

pub use control::{ControlId, ControlKind};
pub use error::UiError;
pub use event::UiEvent;
pub use layout::{Anchor, Rect};

#[cfg(windows)]
pub use win32::MainWindow;

/// 本 crate 是否带原生窗口实现（编译期常量）
///
/// 调用方用它决定要不要走图形界面分支，比在业务代码里散落
/// `#[cfg(windows)]` 更好读。
pub const HAS_NATIVE_WINDOW: bool = cfg!(windows);
