//! 布局计算（纯逻辑，任何平台可测）
//!
//! # 为什么布局要从 Win32 里拆出来
//!
//! 「窗口变大 200 像素后按钮该在哪」是一道**算术题**，不是 Win32 题。
//! 把它和 `MoveWindow` 混在一起写，就只能在 Windows 桌面上靠肉眼验证；
//! 拆成纯函数后，Linux CI 上就能用一组 (设计尺寸, 实际尺寸, DPI) 把边界钉死。
//!
//! # 坐标系
//!
//! 本模块**全部是逻辑像素**（96 DPI 下的像素）。转成物理像素是
//! [`scale_rect`] 的活，由 `win32` 模块在真正调 `MoveWindow` 前做一次。
//! 混着用是这类代码最常见的 bug 来源：设计稿量出来的 320 拿去当物理像素，
//! 在 150% 缩放下就会短三分之一。

use crate::dpi::scale_len;

/// 矩形（逻辑像素）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    /// 左
    pub x: i32,
    /// 上
    pub y: i32,
    /// 宽
    pub w: i32,
    /// 高
    pub h: i32,
}

impl Rect {
    /// 构造
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self { x, y, w, h }
    }

    /// 右边界（不含）
    pub const fn right(&self) -> i32 {
        self.x + self.w
    }

    /// 下边界（不含）
    pub const fn bottom(&self) -> i32 {
        self.y + self.h
    }

    /// 是否没有面积
    ///
    /// 用 `<=` 而不是 `<`：宽或高为 0 的控件在 Win32 里也是不可见的，
    /// 把它当「有面积」会让 `FillRect` 这类调用画出 0 像素的边框。
    pub const fn is_empty(&self) -> bool {
        self.w <= 0 || self.h <= 0
    }

    /// 拆成 `(x, y, w, h)`，便于直接喂给渲染层 / 日志
    pub const fn to_tuple(self) -> (i32, i32, i32, i32) {
        (self.x, self.y, self.w, self.h)
    }
}

/// 控件在窗口变大时贴着哪个角
///
/// 只有四个角，没有「居中」「按比例」：SideLens 的界面是
/// 「上面一堆状态文字 + 中间视频 + 下面一排按钮」，这四种已经够用，
/// 完整布局引擎（弹性盒 / 网格）的复杂度换不来任何体验提升。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Anchor {
    /// 贴左上（默认）：窗口变大时控件不动
    #[default]
    TopLeft,
    /// 贴右上：窗口变宽时向右平移
    TopRight,
    /// 贴左下：窗口变高时向下平移
    BottomLeft,
    /// 贴右下：同时向右向下平移
    BottomRight,
}

/// 窗口尺寸变化后，控件矩形该平移到哪（**尺寸不变**）
///
/// `design` 是设计时的客户区尺寸，`actual` 是当前客户区尺寸，
/// 两者都是逻辑像素。差值就是控件要平移的量。
pub fn stretch(rect: Rect, design: (i32, i32), actual: (i32, i32), anchor: Anchor) -> Rect {
    let dx = actual.0 - design.0;
    let dy = actual.1 - design.1;
    let (ox, oy) = match anchor {
        Anchor::TopLeft => (0, 0),
        Anchor::TopRight => (dx, 0),
        Anchor::BottomLeft => (0, dy),
        Anchor::BottomRight => (dx, dy),
    };
    Rect::new(rect.x + ox, rect.y + oy, rect.w, rect.h)
}

/// 窗口尺寸变化后，矩形该长到多大（**左上角不动**，用于视频区）
///
/// 宽高按客户区增量同步增长，但不允许缩成负数：窗口被拖到比设计尺寸还小时，
/// 视频区退化成 0 面积（[Rect::is_empty`]），由绘制层跳过，而不是画出
/// 一个坐标反向的矩形（`FillRect` 对 left>right 的行为是未定义的观感）。
pub fn grow(rect: Rect, design: (i32, i32), actual: (i32, i32)) -> Rect {
    let dw = (actual.0 - design.0).max(-rect.w);
    let dh = (actual.1 - design.1).max(-rect.h);
    Rect::new(rect.x, rect.y, rect.w + dw, rect.h + dh)
}

/// 把窗口外框夹进屏幕工作区
///
/// # 为什么需要这个
///
/// 1920x1080 的屏幕配 200% 缩放，工作区只有 1920x1040 物理像素；
/// 而「900x640 逻辑像素」的客户区在 200% 下外框已经是 1832x1336 ——
/// **比屏幕还高**，底部的按钮行和复选框会被挤到屏幕外，用户根本点不到。
///
/// 这不是边角情况：4K 笔记本外接 1080p 显示器、或者老人把缩放调到 200%
/// 都会命中。所以建窗口时先把外框夹进工作区，宁可客户区比设计尺寸小
/// （布局会自己重排），也不能让控件跑到屏幕外。
///
/// 参数与返回值都是**物理像素**（工作区本来就是物理像素）。
pub fn fit_to_work_area(
    width: i32,
    height: i32,
    work: (i32, i32, i32, i32),
    min: (i32, i32),
) -> (i32, i32) {
    let (work_w, work_h) = (work.2, work.3);
    // 工作区本身可能比 min 还小（超小屏 + 大缩放），这时以工作区为准，
    // 否则窗口会被系统夹到比 min 还小，行为和我们的意图不一致。
    let max_w = work_w.max(1);
    let max_h = work_h.max(1);
    let w = width.min(max_w).max(min.0.min(max_w));
    let h = height.min(max_h).max(min.1.min(max_h));
    (w, h)
}

/// 逻辑像素矩形 → 物理像素矩形
///
/// 按**两个角**分别取整再相减，而不是「左上角取整 + 宽高各自取整」：
/// 后者会让相邻控件的右边界和下一个控件的左边界差出 1 像素（125% 缩放下很常见），
/// 视觉上就是一条随机出现的缝。
pub fn scale_rect(rect: Rect, dpi: u32) -> Rect {
    let x0 = scale_len(rect.x, dpi);
    let y0 = scale_len(rect.y, dpi);
    let x1 = scale_len(rect.right(), dpi);
    let y1 = scale_len(rect.bottom(), dpi);
    Rect::new(x0, y0, (x1 - x0).max(0), (y1 - y0).max(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_边界计算正确() {
        let r = Rect::new(10, 20, 30, 40);
        assert_eq!(r.right(), 40);
        assert_eq!(r.bottom(), 60);
        assert_eq!(r.to_tuple(), (10, 20, 30, 40));
        assert!(!r.is_empty());
    }

    #[test]
    fn 零面积矩形算空() {
        assert!(Rect::new(0, 0, 0, 10).is_empty());
        assert!(Rect::new(0, 0, 10, 0).is_empty());
        assert!(Rect::new(0, 0, -5, 10).is_empty());
    }

    #[test]
    fn 左上锚点不随窗口移动() {
        let r = Rect::new(16, 16, 100, 20);
        let moved = stretch(r, (800, 600), (1200, 900), Anchor::TopLeft);
        assert_eq!(moved, r);
    }

    #[test]
    fn 右下锚点跟随增量平移() {
        let r = Rect::new(600, 500, 100, 20);
        let moved = stretch(r, (800, 600), (1200, 900), Anchor::BottomRight);
        assert_eq!(moved, Rect::new(1000, 800, 100, 20));
    }

    #[test]
    fn 右上与左下锚点各自只动一个方向() {
        let r = Rect::new(10, 10, 100, 20);
        assert_eq!(
            stretch(r, (800, 600), (1000, 900), Anchor::TopRight),
            Rect::new(210, 10, 100, 20)
        );
        assert_eq!(
            stretch(r, (800, 600), (1000, 900), Anchor::BottomLeft),
            Rect::new(10, 310, 100, 20)
        );
    }

    #[test]
    fn 窗口变小时控件跟着往回走() {
        let r = Rect::new(600, 500, 100, 20);
        let moved = stretch(r, (800, 600), (400, 300), Anchor::BottomRight);
        assert_eq!(moved, Rect::new(200, 200, 100, 20));
    }

    #[test]
    fn 视频区随窗口一起长() {
        let r = Rect::new(16, 96, 768, 400);
        let grown = grow(r, (800, 600), (1200, 900));
        assert_eq!(grown, Rect::new(16, 96, 1168, 700));
    }

    #[test]
    fn 视频区缩到零就停住不出现负宽() {
        let r = Rect::new(16, 96, 768, 400);
        let grown = grow(r, (800, 600), (0, 0));
        assert_eq!(grown.w, 0);
        assert_eq!(grown.h, 0);
        assert!(grown.is_empty());
    }

    #[test]
    fn 一百二十五百分比下相邻矩形不裂缝() {
        // 96 → 120：x1.25。左右相接的两个 100 宽矩形必须仍然相接。
        let left = scale_rect(Rect::new(0, 0, 100, 20), 120);
        let right = scale_rect(Rect::new(100, 0, 100, 20), 120);
        assert_eq!(left.right(), right.x);
    }

    #[test]
    fn 超高窗口被夹进工作区() {
        // 200% 缩放下的 1080p：工作区 1920x1040，窗口想要 1832x1336
        let (w, h) = fit_to_work_area(1832, 1336, (0, 0, 1920, 1040), (600, 400));
        assert_eq!(w, 1832);
        assert_eq!(h, 1040, "高度必须被夹到工作区高度");
    }

    #[test]
    fn 正常窗口不被改动() {
        let (w, h) = fit_to_work_area(900, 640, (0, 0, 1920, 1040), (600, 400));
        assert_eq!((w, h), (900, 640));
    }

    #[test]
    fn 工作区比最小尺寸还小时以工作区为准() {
        let (w, h) = fit_to_work_area(2000, 2000, (0, 0, 320, 240), (600, 400));
        assert_eq!((w, h), (320, 240));
    }

    #[test]
    fn 逻辑矩形按dpi放大() {
        let r = Rect::new(10, 10, 100, 20);
        assert_eq!(scale_rect(r, 96), r);
        assert_eq!(scale_rect(r, 192), Rect::new(20, 20, 200, 40));
    }
}
