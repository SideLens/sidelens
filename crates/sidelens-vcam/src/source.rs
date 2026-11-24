//! 虚拟摄像头媒体源的状态机（不含 COM 接口实现）
//!
//! # 为什么先只做状态机
//!
//! Windows 媒体源的接口契约要求状态转换严格合法：
//!
//! | 当前状态 | Start | Stop | Pause | Shutdown |
//! | --- | --- | --- | --- | --- |
//! | Stopped | ✅ | ❌ (E_FAIL) | ❌ | ✅ |
//! | Started | ❌ (E_FAIL) | ✅ | ✅ | ✅ |
//! | Paused | ✅ | ✅ | ❌ | ✅ |
//! | Shutdown | ❌ (MF_E_SHUTDOWN) | ❌ | ❌ | ❌ |
//!
//! 这套规则**可以脱离 COM 单独测试**，而 COM 的样板代码（Q I/AddRef/Release）
//! 用 `#[implement]` 宏生成，没有逻辑。所以先把有逻辑的部分抽出来测透，
//! 再接 COM —— 否则每个状态 bug 都要在真实消费端里复现，成本极高。
//!
//! # 与真实消费端的对应
//!
//! Zoom / Teams / 浏览器在连接时大致会走：
//!
//! ```text
//! GetMediaSource → GetStreamDescriptors → CreatePresentationDescriptor
//!   → Start → BeginGetEvent 等 MEStreamStarted
//!   → BeginGetEvent 等 MEMediaSample（帧）
//! ```
//!
//! 任何一步状态不对就会失败，且错误位置离真正原因很远。

use std::fmt;

/// 媒体源状态
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SourceState {
    /// 刚创建，未 Start
    #[default]
    Stopped,
    /// 运行中
    Started,
    /// 暂停
    Paused,
    /// 已 Shutdown，不再可用
    Shutdown,
}

/// 状态转换结果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransitionError {
    /// 当前状态下不允许该操作（对应 MF_E_INVALIDREQUEST / E_FAIL）
    InvalidState { from: SourceState, op: Operation },
    /// 已 Shutdown，任何操作都失败（对应 MF_E_SHUTDOWN）
    Shutdown,
}

impl fmt::Display for TransitionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TransitionError::InvalidState { from, op } => {
                write!(f, "在 {from:?} 状态下不能执行 {op:?}")
            }
            TransitionError::Shutdown => f.write_str("媒体源已 Shutdown"),
        }
    }
}

impl std::error::Error for TransitionError {}

/// 操作类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    Start,
    Stop,
    Pause,
    Shutdown,
}

impl SourceState {
    /// 执行 Start
    pub fn start(self) -> Result<Self, TransitionError> {
        match self {
            SourceState::Stopped | SourceState::Paused => Ok(SourceState::Started),
            SourceState::Started => Err(TransitionError::InvalidState {
                from: self,
                op: Operation::Start,
            }),
            SourceState::Shutdown => Err(TransitionError::Shutdown),
        }
    }

    /// 执行 Stop
    pub fn stop(self) -> Result<Self, TransitionError> {
        match self {
            SourceState::Started | SourceState::Paused => Ok(SourceState::Stopped),
            SourceState::Stopped => Err(TransitionError::InvalidState {
                from: self,
                op: Operation::Stop,
            }),
            SourceState::Shutdown => Err(TransitionError::Shutdown),
        }
    }

    /// 执行 Pause
    pub fn pause(self) -> Result<Self, TransitionError> {
        match self {
            SourceState::Started => Ok(SourceState::Paused),
            SourceState::Stopped | SourceState::Paused => Err(TransitionError::InvalidState {
                from: self,
                op: Operation::Pause,
            }),
            SourceState::Shutdown => Err(TransitionError::Shutdown),
        }
    }

    /// 执行 Shutdown（幂等）
    pub fn shutdown(self) -> Result<Self, TransitionError> {
        match self {
            SourceState::Shutdown => Err(TransitionError::Shutdown),
            _ => Ok(SourceState::Shutdown),
        }
    }

    /// 是否可以投递帧
    pub fn can_deliver_frames(self) -> bool {
        // Stop 后不投递（MF 要求）；Pause 也不投递
        matches!(self, SourceState::Started)
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, SourceState::Shutdown)
    }
}

impl fmt::Display for SourceState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            SourceState::Stopped => "Stopped",
            SourceState::Started => "Started",
            SourceState::Paused => "Paused",
            SourceState::Shutdown => "Shutdown",
        };
        f.write_str(s)
    }
}

/// 支持的分辨率
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resolution {
    pub width: u32,
    pub height: u32,
}

impl Resolution {
    pub const fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }

    /// 该分辨率是否被支持
    ///
    /// 限制来自两条：
    /// - 太小的分辨率对摄像头没意义（< 160x120）
    /// - 太大超出常见消费端的能力（> 4K）
    pub fn is_supported(self) -> bool {
        self.width >= 160
            && self.height >= 120
            && self.width <= 3840
            && self.height <= 2160
            && self.width % 2 == 0
            && self.height % 2 == 0
    }

    /// NV12 格式的缓冲区大小
    ///
    /// Y 平面是 w*h，UV 平面是 w*h/2（交错），合计 w*h*3/2。
    pub fn nv12_size(self) -> usize {
        (self.width as usize) * (self.height as usize) * 3 / 2
    }

    /// RGB32（BGRA）格式的缓冲区大小
    pub fn rgb32_size(self) -> usize {
        (self.width as usize) * (self.height as usize) * 4
    }
}

impl fmt::Display for Resolution {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}x{}", self.width, self.height)
    }
}

/// 支持的帧率范围
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameRate {
    pub numerator: u32,
    pub denominator: u32,
}

impl FrameRate {
    pub const fn new(num: u32, den: u32) -> Self {
        Self {
            numerator: num,
            denominator: den,
        }
    }

    pub fn fps(self) -> f64 {
        if self.denominator == 0 {
            return 0.0;
        }
        self.numerator as f64 / self.denominator as f64
    }

    /// 是否在合理范围（1–120 fps）
    pub fn is_supported(self) -> bool {
        let f = self.fps();
        (1.0..=120.0).contains(&f)
    }
}

impl fmt::Display for FrameRate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}/{} ({:.2} fps)",
            self.numerator,
            self.denominator,
            self.fps()
        )
    }
}

/// 媒体源的默认配置
#[derive(Debug, Clone, Copy)]
pub struct SourceConfig {
    pub resolution: Resolution,
    pub frame_rate: FrameRate,
}

impl Default for SourceConfig {
    fn default() -> Self {
        Self {
            resolution: Resolution::new(1280, 720),
            frame_rate: FrameRate::new(30, 1),
        }
    }
}

impl SourceConfig {
    /// 每帧间隔（微秒）
    pub fn frame_interval_us(&self) -> u64 {
        let fps = self.frame_rate.fps();
        if fps <= 0.0 {
            return 0;
        }
        (1_000_000.0 / fps) as u64
    }

    pub fn validate(&self) -> Result<(), String> {
        if !self.resolution.is_supported() {
            return Err(format!("不支持的分辨率：{}", self.resolution));
        }
        if !self.frame_rate.is_supported() {
            return Err(format!("不支持的帧率：{}", self.frame_rate));
        }
        Ok(())
    }
}
