//! 错误类型
//!
//! 分成四个变体而不是一个字符串，是因为调用方对不同失败的处理**确实不同**：
//! 建窗口失败（多半是没有交互桌面 / 会话 0）要退化成命令行模式；
//! 建控件失败要记录并继续跑（少一个复选框不影响推流）；
//! 绘制失败可以吞掉（下一帧重画就好）；消息泵失败要上报。

/// SideLens UI 层的错误
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiError {
    /// 窗口创建 / 注册 / 显示失败
    Window(String),
    /// 控件创建或操作失败
    Control(String),
    /// 绘制失败
    Paint(String),
    /// 消息泵 / 消息分发失败
    Message(String),
}

impl UiError {
    /// 错误类别的中文名，日志里比 `Debug` 更好读
    pub fn kind(&self) -> &'static str {
        match self {
            UiError::Window(_) => "窗口",
            UiError::Control(_) => "控件",
            UiError::Paint(_) => "绘制",
            UiError::Message(_) => "消息",
        }
    }

    /// 具体原因（不含类别前缀）
    pub fn detail(&self) -> &str {
        match self {
            UiError::Window(s) | UiError::Control(s) | UiError::Paint(s) | UiError::Message(s) => s,
        }
    }
}

impl std::fmt::Display for UiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}失败：{}", self.kind(), self.detail())
    }
}

impl std::error::Error for UiError {}
