//! 渲染层错误
//!
//! # 为什么把 HRESULT 塞进字符串而不是保留 `windows::core::Error`
//!
//! 调用方（`apps/sidelensd`）只做一件事：看到错误就降级到 GDI 并打一条日志。
//! 它不需要 match 具体的 HRESULT 分支，但**排障时必须有原始 HRESULT** ——
//! 否则「GPU 路径打不开」就成了一句没法查的废话。
//!
//! 所以这里选择「分类枚举 + 格式化字符串里带 HRESULT」，兼顾分类处理与排障。
//! 五个变体对应链路上的五个阶段，日志里一眼能看出卡在哪一步：
//!
//! | 变体 | 卡在哪 |
//! | --- | --- |
//! | [`Device`] | D3D11 设备 / 适配器 |
//! | [`SwapChain`] | DXGI swap chain 创建与 present |
//! | [`VideoProcessor`] | 视频处理器 enumerator / processor / blt |
//! | [`Texture`] | NV12 / 输出纹理与 staging 拷贝 |
//! | [`Window`] | 窗口创建 / 消息循环 |

use std::fmt;

/// 渲染层错误
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GpuError {
    /// D3D11 设备或适配器层面的失败
    Device(String),
    /// DXGI swap chain 层面的失败
    SwapChain(String),
    /// 视频处理器（YUV→RGB）层面的失败
    VideoProcessor(String),
    /// 纹理创建 / 上传 / 读回层面的失败
    Texture(String),
    /// 窗口层面的失败
    Window(String),
    /// 输入帧不合法（尺寸 / 缓冲），与 GPU 无关
    Layout(crate::layout::LayoutError),
    /// 尺寸为 0
    ZeroSize {
        /// 宽
        width: u32,
        /// 高
        height: u32,
    },
    /// 窗口已销毁还在 present
    WindowGone,
    /// 该模式下不支持这个操作（例如 headless 上读回像素）
    Unsupported {
        /// 哪个模式
        mode: &'static str,
        /// 想做什么
        what: &'static str,
    },
    /// 设备丢失（`DXGI_ERROR_DEVICE_REMOVED` / `DEVICE_HUNG` / `DEVICE_RESET`）
    ///
    /// # 为什么单独一个变体
    ///
    /// 设备丢失是**唯一可恢复**的 GPU 错误 —— 其余（格式不匹配、尺寸非法）
    /// 重试一万次也一样失败。调用方需要能区分「该重建」和「该放弃」：
    ///
    /// ```text
    /// match err {
    ///     DeviceRemoved => renderer.recover(),   // 重建
    ///     _ => 降级到 GDI,                       // 放弃 GPU
    /// }
    /// ```
    ///
    /// 塞进 `Device(String)` 也能跑，但调用方只能靠字符串匹配判断，
    /// 那是脆弱的（错误信息一改就失效）。
    DeviceRemoved {
        /// 原始 HRESULT（排障用）
        hresult: u32,
    },
}

impl fmt::Display for GpuError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GpuError::Device(m) => write!(f, "D3D11 设备失败：{m}"),
            GpuError::SwapChain(m) => write!(f, "交换链失败：{m}"),
            GpuError::VideoProcessor(m) => write!(f, "视频处理器失败：{m}"),
            GpuError::Texture(m) => write!(f, "纹理操作失败：{m}"),
            GpuError::Window(m) => write!(f, "窗口操作失败：{m}"),
            GpuError::Layout(e) => write!(f, "帧布局不合法：{e}"),
            GpuError::ZeroSize { width, height } => {
                write!(f, "尺寸不能为 0（{width}x{height}）")
            }
            GpuError::WindowGone => write!(f, "窗口已销毁，不能再 present"),
            GpuError::Unsupported { mode, what } => {
                write!(f, "{mode} 模式不支持 {what}")
            }
            GpuError::DeviceRemoved { hresult } => write!(
                f,
                "GPU 设备丢失（HRESULT 0x{hresult:08X}）—— 需要重建渲染器"
            ),
        }
    }
}

impl std::error::Error for GpuError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            GpuError::Layout(e) => Some(e),
            _ => None,
        }
    }
}

impl From<crate::layout::LayoutError> for GpuError {
    fn from(e: crate::layout::LayoutError) -> Self {
        GpuError::Layout(e)
    }
}

// `windows` 是 target-specific 依赖，非 Windows 上根本不存在 ——
// 这两项必须 cfg 门控，否则 `cargo check --target x86_64-unknown-linux-gnu` 会
// 报 "cannot find module or crate `windows`"。这就是「Windows-only 逻辑
// 不要出现在跨平台文件里」的具体代价，踩过才知道。
#[cfg(windows)]
impl From<windows::core::Error> for GpuError {
    fn from(e: windows::core::Error) -> Self {
        GpuError::Device(hresult("Windows API", &e))
    }
}

/// 把 HRESULT 格式化成「阶段: 消息 (0xXXXXXXXX)」
///
/// 每个阶段各自包一层，这样日志里能区分「swap chain 的 E_INVALIDARG」
/// 和「视频处理器的 E_INVALIDARG」—— 它们的成因完全不同。
#[cfg(windows)]
pub(crate) fn hresult(stage: &str, e: &windows::core::Error) -> String {
    format!("{stage}: {} (0x{:08X})", e.message(), e.code().0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::LayoutError;

    #[test]
    fn layout_error_wraps_and_keeps_source() {
        let e = GpuError::Layout(LayoutError::ZeroDimension {
            what: "coded",
            width: 0,
            height: 1080,
        });
        assert!(matches!(e, GpuError::Layout(_)));
        assert!(std::error::Error::source(&e).is_some());
        assert!(e.to_string().contains("0x0") || e.to_string().contains("0"));
    }

    #[test]
    fn display_mentions_the_stage() {
        assert!(GpuError::Device("x".into()).to_string().contains("D3D11"));
        assert!(GpuError::SwapChain("x".into())
            .to_string()
            .contains("交换链"));
        assert!(GpuError::VideoProcessor("x".into())
            .to_string()
            .contains("视频处理器"));
        assert!(GpuError::Texture("x".into()).to_string().contains("纹理"));
        assert!(GpuError::Window("x".into()).to_string().contains("窗口"));
    }

    #[test]
    fn zero_size_and_window_gone_are_readable() {
        assert!(GpuError::ZeroSize {
            width: 0,
            height: 0
        }
        .to_string()
        .contains("0x0"));
        assert!(GpuError::WindowGone.to_string().contains("窗口已销毁"));
    }

    #[test]
    fn unsupported_names_mode_and_operation() {
        let e = GpuError::Unsupported {
            mode: "headless",
            what: "读回像素",
        };
        let s = e.to_string();
        assert!(s.contains("headless"));
        assert!(s.contains("读回像素"));
    }
}
