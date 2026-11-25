//! 控件标识与种类
//!
//! # 为什么 `ControlId` 直接就是 Win32 的子窗口 ID
//!
//! Win32 的 `WM_COMMAND` 只带回来 `LOWORD(wParam)`，也就是**创建子窗口时传进去的
//! 那个整数**，不带你自己的任何上下文。第一版想用「Vec 下标」当 ID，结果窗口过程里
//! 拿到 0..N 根本对不上表 —— 因为按钮、静态文本、下拉框的 ID 空间是共用的。
//!
//! 现在 `ControlId` 的裸值**就是**创建控件时塞给 `CreateWindowExW` 的 `hMenu` 参数，
//! 于是窗口过程不需要任何额外映射就能把消息翻回 `ControlId`。代价是 ID 必须在窗口内
//! 唯一 —— 由 [`crate::MainWindow`] 内部从 1000 开始自增保证（避开 `IDOK` 这类系统值）。

/// 控件句柄包装（内部就是 Win32 子窗口 ID）
///
/// 不直接暴露 `isize` 是为了让 API 不会被误传成窗口句柄或控件种类；
/// 需要和别的层（比如把控件坐标交给渲染层）交换时用 [`ControlId::raw`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ControlId(isize);

impl ControlId {
    /// 从裸值构造
    ///
    /// 只给「从 Win32 消息里还原 ID」这类场景用。业务代码不应该凭空造 ID：
    /// 造出来的值不会出现在控件表里，所有操作都会静默失败。
    pub const fn from_raw(raw: isize) -> Self {
        Self(raw)
    }

    /// 取出裸值
    pub const fn raw(self) -> isize {
        self.0
    }

    /// 是否是「无效 ID」（创建失败的控件返回它）
    pub const fn is_invalid(self) -> bool {
        self.0 < 0
    }
}

impl std::fmt::Display for ControlId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.is_invalid() {
            write!(f, "控件#无效")
        } else {
            write!(f, "控件#{}", self.0)
        }
    }
}

/// 控件种类
///
/// 窗口过程只拿到「ID + 通知码」，而 `BN_CLICKED`（0）和 `CBN_SELCHANGE`（1）
/// 都是小整数、含义却完全不同，只能靠控件种类区分。种类在创建时记进控件表。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ControlKind {
    /// 静态文本（STATIC）
    Label,
    /// 普通按钮（BUTTON + BS_PUSHBUTTON）
    Button,
    /// 复选框（BUTTON + BS_AUTOCHECKBOX，自己维护勾选状态）
    Checkbox,
    /// 下拉框（COMBOBOX + CBS_DROPDOWNLIST）
    Combo,
    /// 原生滑块（TRACKBAR）
    Slider,
}

impl ControlKind {
    /// 中文名，日志 / 报告里用
    pub fn as_str(self) -> &'static str {
        match self {
            ControlKind::Label => "静态文本",
            ControlKind::Button => "按钮",
            ControlKind::Checkbox => "复选框",
            ControlKind::Combo => "下拉框",
            ControlKind::Slider => "滑块",
        }
    }
}
