//! SideLens 渲染层：把解码出来的 NV12 画到屏幕上
//!
//! # 为什么单独一个 crate
//!
//! 解码（`sidelens-codec-mf`）与显示是两件事，耦合在一起会带来两个问题：
//!
//! 1. 解码 crate 是 **Windows 专属**的（Media Foundation）。渲染里有一大半逻辑
//!    （NV12 → RGBA 的色彩转换、coded/display 裁剪）其实是**纯数学**，
//!    不该被平台 API 绑死 —— 否则 Linux CI 上一行都测不到。
//! 2. 虚拟摄像头（`sidelens-vcam`）将来也要吃同一份 NV12。转换逻辑放在
//!    渲染 crate 里、以纯函数暴露，两个消费者共用一套，不会分叉出两份
//!    「有一份颜色偏绿」的实现。
//!
//! 所以本 crate 一刀切成两层：
//!
//! | 层 | 模块 | 编译条件 | 内容 |
//! | --- | --- | --- | --- |
//! | 跨平台 | [`convert`] | 无条件 | `Nv12View` / `to_rgba` / `crop_to_display` |
//! | Windows | [`win32`] | `cfg(windows)` | 最简窗口 + GDI 显示 + headless |
//!
//! 非 Windows 上本 crate 依然能编译、能跑转换测试，只是没有 [`Renderer`]。
//!
//! # 快速上手
//!
//! ```no_run
//! use sidelens_render::{Nv12View, Renderer};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! // coded 640x368 / display 640x360 —— H264Decoder 的两个尺寸方法各给一个
//! let nv12 = vec![16u8; 640 * 368 * 3 / 2];
//! let frame = Nv12View::new(&nv12, (640, 368), (640, 360))?;
//!
//! let mut renderer = Renderer::new("SideLens", 640, 360)?;
//! while renderer.pump_events() {
//!     renderer.present(&frame)?;
//! }
//! # Ok(())
//! # }
//! ```
//!
//! # coded vs display：这个 crate 存在的理由
//!
//! H.264 的编码尺寸必须是 16 的整数倍，`H264Decoder` 输出的是 **coded size**
//! （640x360 → 640x**368**、1920x1080 → 1920x**1088**），缓冲也按 coded size 分配。
//! 但真正该显示的只有 SPS cropping 之后的那块，底部多出来的行是编码器的宏块
//! 填充 —— 直接按 coded 尺寸出图，画面底部会有一条绿边。
//!
//! [`Nv12View`] 因此同时携带两个尺寸，[`Nv12View::to_rgba`] **只转 display 区域**。
//! `tests/convert.rs` 里有 640x368 → 640x360 的裁剪断言守着。

#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_debug_implementations)]

pub mod convert;

#[cfg(windows)]
pub mod win32;

pub use convert::{ConvertError, Matrix, Nv12View};

#[cfg(windows)]
pub use win32::{RenderError, Renderer, RendererMode};
