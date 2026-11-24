//! SideLens GPU 渲染层：D3D11 + DXGI flip-model，NV12 零拷贝上屏
//!
//! # 为什么需要这个 crate
//!
//! 用户对 SideLens 的原始要求是「超低的连接延迟 + **极致的原生 ui 性能体验**」。
//! 旧的 `sidelens-render` 只有 GDI `StretchDIBits`，链路是：
//!
//! ~~~text
//! NV12 (GPU 解码输出) -> CPU 转 RGBA -> 拷到 GDI DIB -> StretchDIBits -> DWM -> 屏幕
//!                        ↑ convert.rs      ↑ 又一次全帧拷贝
//! ~~~
//!
//! 三次全帧拷贝，1080p 下单是 blit 就要好几毫秒。本 crate 换成：
//!
//! ~~~text
//! NV12 (CPU 缓冲) -> Map staging + memcpy -> NV12 纹理
//!                                            │
//!                     ID3D11VideoProcessorBlt（GPU 固定功能单元，YUV→RGB）
//!                                            ▼
//!                        flip-model swap chain -> DWM -> 屏幕
//! ~~~
//!
//! 色彩转换**零 CPU 参与**，呈现走 flip-model（不经过 DWM 的额外拷贝）。
//! 实测数字见 `examples/bench_present.rs`。
//!
//! # 分层
//!
//! | 层 | 模块 | 编译条件 | 内容 |
//! | --- | --- | --- | --- |
//! | 跨平台 | [`layout`] | 无条件 | NV12 几何、裁剪矩形、staging 打包 |
//! | 跨平台 | [`stats`] | 无条件 | p50 / p95 耗时统计 |
//! | 跨平台 | [`color`] | 无条件 | 色彩空间位域拼装 |
//! | 跨平台 | [`error`] | 无条件 | 错误类型 |
//! | Windows | [`win32`] | `cfg(windows)` | D3D11 / DXGI / 窗口 / 读回 |
//!
//! 跨平台层承载了所有**会算错**的逻辑（尺寸、裁剪、位域、分位数），
//! 所以 Linux / macOS CI 上就能把这些钉死；Windows 层只剩「调 API」。
//!
//! # 快速上手
//!
//! ~~~no_run
//! use sidelens_render_gpu::GpuRenderer;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! // coded 1920x1088 / display 1920x1080 —— 解码器的两个尺寸方法各给一个
//! let nv12 = vec![16u8; 1920 * 1088 * 3 / 2];
//! let mut r = GpuRenderer::new("SideLens", 1920, 1080)?;
//! while r.pump_events() {
//!     r.present(&nv12, (1920, 1088), (1920, 1080))?;
//! }
//! # Ok(())
//! # }
//! ~~~
//!
//! # 降级策略
//!
//! [`GpuRenderer::new`] / [`GpuRenderer::offscreen`] 失败时调用方应当
//! 降级到 `sidelens_render::Renderer`（GDI）。失败原因会在
//! [`GpuError`] 里带原始 HRESULT，足以定位是设备、swap chain 还是
//! 视频处理器的问题。
//!
//! 没有 GPU 的 CI 环境用 [`GpuRenderer::headless`]：不碰任何 D3D 对象，
//! 但仍然做完整的输入校验，所以尺寸相关的测试照样能跑。

#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_debug_implementations)]

pub mod color;
pub mod error;
pub mod layout;
pub mod stats;

#[cfg(windows)]
pub mod win32;

pub use color::ColorSpace;
pub use error::GpuError;
pub use layout::{fill_rect_nv12, solid_nv12, LayoutError, Nv12Layout, Rect};
pub use stats::FrameStats;

#[cfg(windows)]
pub use win32::readback::ReadbackImage;
#[cfg(windows)]
pub use win32::renderer::{GpuMode, GpuRenderer};
#[cfg(windows)]
pub use win32::swapchain::SWAPCHAIN_FORMAT_NAME;

/// 本 crate 的版本
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// 非 Windows 上仍然存在的模式枚举
///
/// 为什么要在非 Windows 也定义一份：调用方（主控的接线代码）要写
/// `match renderer.mode()`，如果这个类型只在 Windows 存在，那些代码就得
/// 整段 `cfg` 门控，可读性很差。而 `headless` 这个概念本身是跨平台的
/// （它不碰 GPU），所以这里给它一个跨平台的枚举，Windows 上再用
/// [`win32::renderer::GpuMode`] 覆盖。
#[cfg(not(windows))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuMode {
    /// 真窗口（非 Windows 上不可用）
    Window,
    /// 离屏（非 Windows 上不可用）
    Offscreen,
    /// 无 GPU
    Headless,
}

#[cfg(not(windows))]
impl GpuMode {
    /// 人类可读的名字
    pub fn as_str(self) -> &'static str {
        match self {
            GpuMode::Window => "window",
            GpuMode::Offscreen => "offscreen",
            GpuMode::Headless => "headless",
        }
    }
}

/// 非 Windows 上的渲染器：**只有 headless 能用**
///
/// 存在的理由是「让调用方代码不用整段 cfg」+「让尺寸校验的测试在
/// Linux / macOS CI 上也能跑」。真窗口和离屏在非 Windows 上直接返回
/// 错误，而不是 `unimplemented!()` —— 后者会在运行期炸掉整个进程，
/// 而降级逻辑本来就该处理「GPU 不可用」这种情况。
#[cfg(not(windows))]
pub struct GpuRenderer {
    mode: GpuMode,
    width: u32,
    height: u32,
    stats: FrameStats,
}

#[cfg(not(windows))]
impl std::fmt::Debug for GpuRenderer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GpuRenderer")
            .field("mode", &self.mode)
            .field("width", &self.width)
            .field("height", &self.height)
            .finish_non_exhaustive()
    }
}

#[cfg(not(windows))]
impl GpuRenderer {
    /// 非 Windows 上恒失败：没有 D3D11
    pub fn new(_title: &str, _width: u32, _height: u32) -> Result<Self, GpuError> {
        Err(GpuError::Device(
            "GPU 渲染只在 Windows 上可用（D3D11 是 Windows 专属 API）".into(),
        ))
    }

    /// 非 Windows 上恒失败：没有 D3D11
    ///
    /// 保留这个签名是为了让调用方代码不用整段 `cfg` —— 与 [`Self::new`]
    /// 同一个理由。
    pub fn new_child(
        _parent: isize,
        _title: &str,
        _x: i32,
        _y: i32,
        _width: u32,
        _height: u32,
    ) -> Result<Self, GpuError> {
        Err(GpuError::Device(
            "GPU 子窗口渲染只在 Windows 上可用（D3D11 是 Windows 专属 API）".into(),
        ))
    }

    /// 非 Windows 上恒失败：没有 D3D11
    pub fn offscreen(_width: u32, _height: u32) -> Result<Self, GpuError> {
        Err(GpuError::Device(
            "离屏 GPU 渲染只在 Windows 上可用（D3D11 是 Windows 专属 API）".into(),
        ))
    }

    /// 无 GPU 模式：跨平台可用，只做输入校验与计数
    pub fn headless(width: u32, height: u32) -> Self {
        GpuRenderer {
            mode: GpuMode::Headless,
            width,
            height,
            stats: FrameStats::new(),
        }
    }

    /// 当前模式
    pub fn mode(&self) -> GpuMode {
        self.mode
    }

    /// 请求的尺寸
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// 已 present 的帧数
    pub fn frames(&self) -> u64 {
        self.stats.count()
    }

    /// 平均呈现耗时（微秒）—— 非 Windows 上恒为 0
    pub fn avg_present_us(&self) -> f64 {
        self.stats.avg_us()
    }

    /// 平均 GPU 耗时（微秒）—— 非 Windows 上恒为 0
    pub fn avg_gpu_us(&self) -> f64 {
        0.0
    }

    /// 适配器名字
    pub fn adapter_name(&self) -> &str {
        "非 Windows（无 D3D11）"
    }

    /// swap chain 格式名
    pub fn swapchain_format(&self) -> &'static str {
        "none"
    }

    /// 恒返回 `true`：没有窗口可关
    pub fn pump_events(&mut self) -> bool {
        true
    }

    /// 只做输入校验并计数
    pub fn present(
        &mut self,
        nv12: &[u8],
        coded: (u32, u32),
        display: (u32, u32),
    ) -> Result<(), GpuError> {
        let layout = Nv12Layout::new(coded, display)?;
        if layout.frame_len() > nv12.len() {
            return Err(GpuError::Layout(LayoutError::BufferTooSmall {
                need: layout.frame_len(),
                have: nv12.len(),
            }));
        }
        self.stats.record(0.0);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_not_empty() {
        assert!(!VERSION.is_empty());
    }

    #[test]
    fn headless_counts_frames_and_accepts_valid_input() {
        let mut r = GpuRenderer::headless(64, 64);
        assert_eq!(r.mode(), GpuMode::Headless);
        assert_eq!(r.frames(), 0);
        let frame = solid_nv12(Nv12Layout::new((64, 64), (64, 64)).expect("合法"), 16, 128);
        r.present(&frame, (64, 64), (64, 64)).expect("必须成功");
        r.present(&frame, (64, 64), (64, 64)).expect("必须成功");
        assert_eq!(r.frames(), 2);
        assert!(r.pump_events(), "headless 没有窗口，恒返回 true");
    }

    #[test]
    fn headless_rejects_bad_layout() {
        let mut r = GpuRenderer::headless(64, 64);
        let frame = vec![0u8; 64 * 64 * 3 / 2];
        let e = r.present(&frame, (65, 64), (64, 64)).unwrap_err();
        assert!(matches!(
            e,
            GpuError::Layout(LayoutError::OddDimension { .. })
        ));
        assert_eq!(r.frames(), 0, "失败的 present 不能计数");
    }

    #[test]
    fn headless_rejects_short_buffer() {
        let mut r = GpuRenderer::headless(64, 64);
        let frame = vec![0u8; 10];
        let e = r.present(&frame, (64, 64), (64, 64)).unwrap_err();
        assert!(matches!(
            e,
            GpuError::Layout(LayoutError::BufferTooSmall { .. })
        ));
    }

    #[test]
    fn headless_reports_its_limits() {
        let r = GpuRenderer::headless(100, 200);
        assert_eq!(r.size(), (100, 200));
        assert_eq!(r.swapchain_format(), "none");
        assert_eq!(r.avg_gpu_us(), 0.0);
        assert!(!r.adapter_name().is_empty());
    }
}
