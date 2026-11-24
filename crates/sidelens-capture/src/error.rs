//! 采集错误类型
//!
//! # 为什么要自己定义而不是 anyhow
//!
//! 屏幕采集失败的排查方向差得非常远：`DuplicateOutput` 失败可能是远程桌面、
//! 显卡驱动、会话 0、或者已经被别的进程占用；`BitBlt` 失败基本只有
//! 「DC 无效 / 窗口已销毁」两种。只给一句笼统的错误，调用方没法决定
//! **该降级到哪个后端**。
//!
//! 所以这里保留三件事：**失败的阶段**（stage）、**原始 HRESULT**、
//! **人类可读消息**。与 `sidelens-codec-mf::EncoderError` 同构 ——
//! 整个 workspace 的 Windows 错误风格保持一致，日志解析脚本只写一套。

use std::fmt;

/// 采集错误
///
/// 变体刻意做得很少：调用方真正需要区分的只有「平台不支持（换后端）」
/// 「源不支持（换源）」「参数错了（改调用）」「系统调用失败（看 HRESULT）」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureError {
    /// 当前平台没有这个后端
    ///
    /// 非 Windows 上调用 [`crate::ScreenCapture::new_gdi`] /
    /// [`crate::ScreenCapture::new_dxgi`] 会得到它。调用方应该降级到
    /// [`crate::ScreenCapture::synthetic`]。
    UnsupportedPlatform {
        /// 后端名（"GDI" / "DXGI"）
        backend: &'static str,
    },

    /// 这个采集源不被该后端支持
    ///
    /// 典型场景：DXGI 的 Desktop Duplication 是**按输出（显示器）**复制的，
    /// 没有「复制某个窗口」的能力；旋转过的显示器 DXGI 也不替我们转。
    /// 这两种都应该降级到 GDI。
    UnsupportedSource {
        /// 后端名
        backend: &'static str,
        /// 为什么不行（含降级建议）
        reason: String,
    },

    /// 参数非法（尺寸为 0、缓冲区太小、显示器序号越界……）
    InvalidArgument(String),

    /// Win32 / COM / D3D 调用失败
    Win32 {
        /// 失败的阶段（"DuplicateOutput" / "BitBlt" / "Map" ……）
        stage: &'static str,
        /// 原始 HRESULT 或 Win32 错误码
        hresult: Option<i32>,
        /// 系统给的错误描述
        message: String,
    },
}

impl CaptureError {
    /// 构造一个不带 HRESULT 的错误
    pub fn new(stage: &'static str, message: impl Into<String>) -> Self {
        Self::Win32 {
            stage,
            hresult: None,
            message: message.into(),
        }
    }

    /// 从 `windows::core::Error` 构造
    ///
    /// 保留 HRESULT 原始值而不是只留字符串：`DXGI_ERROR_WAIT_TIMEOUT` 与
    /// `DXGI_ERROR_ACCESS_LOST` 的 message 都是 "The operation completed
    /// successfully" 一类毫无信息量的文本，只能靠码值区分。
    #[cfg(windows)]
    pub fn from_win(stage: &'static str, err: windows::core::Error) -> Self {
        Self::Win32 {
            stage,
            hresult: Some(err.code().0),
            message: err.message(),
        }
    }

    /// 失败的阶段
    pub fn stage(&self) -> &'static str {
        match self {
            Self::UnsupportedPlatform { backend } => backend,
            Self::UnsupportedSource { backend, .. } => backend,
            Self::InvalidArgument(_) => "validate",
            Self::Win32 { stage, .. } => stage,
        }
    }

    /// 原始 HRESULT（不是 Windows 错误则为 None）
    pub fn hresult(&self) -> Option<i32> {
        match self {
            Self::Win32 { hresult, .. } => *hresult,
            _ => None,
        }
    }

    /// HRESULT 的十六进制形式（`0x887A0026`）
    ///
    /// 报告里必须贴这个 —— 十进制码值没法直接和 MSDN 对照。
    pub fn hresult_hex(&self) -> Option<String> {
        self.hresult().map(|h| format!("0x{:08X}", h as u32))
    }
}

impl fmt::Display for CaptureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedPlatform { backend } => {
                write!(f, "{backend} 后端在当前平台不可用")
            }
            Self::UnsupportedSource { backend, reason } => {
                write!(f, "{backend} 不支持该采集源：{reason}")
            }
            Self::InvalidArgument(msg) => write!(f, "参数非法：{msg}"),
            Self::Win32 {
                stage,
                hresult,
                message,
            } => match hresult {
                Some(h) => write!(f, "{stage} 失败（HRESULT 0x{:08X}）：{message}", *h as u32),
                None => write!(f, "{stage} 失败：{message}"),
            },
        }
    }
}

impl std::error::Error for CaptureError {}

/// 本 crate 的 Result 别名
pub type Result<T> = std::result::Result<T, CaptureError>;
