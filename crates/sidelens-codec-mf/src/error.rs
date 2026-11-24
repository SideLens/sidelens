//! 编码器错误类型
//!
//! # 为什么要自己定义而不是直接用 anyhow
//!
//! 编码失败时必须能回答两个问题：**卡在哪一步**、**HRESULT 是多少**。
//! 只给一句 E_FAIL 的 anyhow 错误，在 MF 上等于什么都没说 ——
//! 同一个 E_FAIL 可能来自 SetInputType、ProcessInput 或
//! ActivateObject，排查方向完全不同。

use std::fmt;

/// 编码器错误
///
/// stage 是**失败的那一步**（SetInputType / ProcessInput / …），
/// hresult 是原始的 HRESULT 值（不是 Windows 错误则为 None）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncoderError {
    stage: &'static str,
    hresult: Option<i32>,
    message: String,
}

impl EncoderError {
    /// 构造一个不带 HRESULT 的错误
    pub fn new(stage: &'static str, message: impl Into<String>) -> Self {
        Self {
            stage,
            hresult: None,
            message: message.into(),
        }
    }

    /// 带 HRESULT 的构造
    pub fn with_hresult(stage: &'static str, hresult: i32, message: impl Into<String>) -> Self {
        Self {
            stage,
            hresult: Some(hresult),
            message: message.into(),
        }
    }

    /// 从 Windows 错误构造
    #[cfg(windows)]
    pub fn from_win(stage: &'static str, err: windows::core::Error) -> Self {
        Self {
            stage,
            hresult: Some(err.code().0),
            message: err.message(),
        }
    }

    /// 失败的阶段
    pub fn stage(&self) -> &'static str {
        self.stage
    }

    /// 原始 HRESULT
    pub fn hresult(&self) -> Option<i32> {
        self.hresult
    }

    /// HRESULT 的十六进制形式（0x80070057），没有则为 None
    pub fn hresult_hex(&self) -> Option<String> {
        self.hresult.map(|h| format!("0x{:08X}", h as u32))
    }
}

impl fmt::Display for EncoderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.hresult {
            Some(h) => write!(
                f,
                "{} 失败（HRESULT 0x{:08X}）：{}",
                self.stage, h as u32, self.message
            ),
            None => write!(f, "{} 失败：{}", self.stage, self.message),
        }
    }
}

impl std::error::Error for EncoderError {}

/// 本 crate 的 Result 别名
pub type Result<T> = std::result::Result<T, EncoderError>;
