//! UI 事件
//!
//! # 为什么是「轮询出来的一队事件」而不是回调
//!
//! SideLens 的主循环是「1ms tick：收包 → 解码 → 渲染 → 再收包」，
//! 控件回调如果直接在里面做业务（比如点「开始」就同步建连接），
//! 一次阻塞就会让视频卡成幻灯片。所以窗口过程只把事件塞进队列，
//! 由主循环在**自己方便的时刻** [`crate::MainWindow::pump`] 取走处理。

use crate::ControlId;

/// UI 事件
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiEvent {
    /// 按钮被点击
    ButtonClicked(ControlId),
    /// 复选框状态变化（第二个参数是**变化后**的状态）
    CheckboxToggled(ControlId, bool),
    /// 下拉框选中项变化（第二个参数是新的下标，从 0 开始）
    ComboChanged(ControlId, usize),
    /// 滑块值变化（范围 0..=100）
    SliderChanged(ControlId, i32),
    /// 窗口被关闭（点 X、Alt+F4，或调用了 `DestroyWindow`）
    Closed,
    /// 客户区尺寸变化
    Resized(u32, u32),
    /// 窗口所在显示器的 DPI 变化
    DpiChanged(u32),
    /// 注册的全局热键被按下（参数为注册时的 ID）
    Hotkey(i32),
    /// 托盘菜单里的「随 Windows 登录启动」被切换（第二个参数是新的状态）
    TrayAutostartToggled(bool),
}

impl std::fmt::Display for UiEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UiEvent::ButtonClicked(id) => write!(f, "按钮点击 {id}"),
            UiEvent::CheckboxToggled(id, on) => {
                write!(f, "复选框 {id} -> {}", if *on { "勾选" } else { "取消" })
            }
            UiEvent::ComboChanged(id, index) => write!(f, "下拉框 {id} -> 第 {index} 项"),
            UiEvent::SliderChanged(id, value) => write!(f, "滑块 {id} -> {value}"),
            UiEvent::Closed => write!(f, "窗口关闭"),
            UiEvent::Resized(w, h) => write!(f, "窗口尺寸 {w}x{h}"),
            UiEvent::DpiChanged(dpi) => write!(f, "DPI 变化 {dpi}"),
            UiEvent::Hotkey(id) => write!(f, "全局快捷键 {id}"),
            UiEvent::TrayAutostartToggled(on) => {
                write!(f, "托盘自启动 -> {}", if *on { "启用" } else { "停用" })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 自启动托盘事件在两种状态下都有清晰文案() {
        assert!(UiEvent::TrayAutostartToggled(true)
            .to_string()
            .contains("启用"));
        assert!(UiEvent::TrayAutostartToggled(false)
            .to_string()
            .contains("停用"));
    }
}
