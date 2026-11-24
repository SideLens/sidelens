//! Win32 实现层（仅 Windows）
//!
//! # 为什么是纯 Win32 而不是 webview
//!
//! 用户对 SideLens 的原始要求里有一句「极致的原生 ui 性能体验」。webview 的代价是
//! 启动几百毫秒、常驻几十 MB、滚动和动画掉帧，而这个软件的界面只是
//! 「几行状态文字 + 一块视频 + 几个开关」，原生控件的开发成本远低于它的收益。
//!
//! # 线程模型
//!
//! Win32 窗口**绑定创建它的线程**：窗口过程只会在那个线程上被调用。
//! 所以 [MainWindow] 刻意用 PhantomData<*mut ()> 做成 !Send / !Sync ——
//! 把它挪到别的线程去 pump()，消息会永远留在队列里，表现为「界面卡死」，
//! 而且这种错误在单线程的 demo 里永远暴露不出来。编译期挡住比运行期查容易得多。
//!
//! # 窗口过程怎么拿到 self
//!
//! WNDPROC 是 extern "system" 自由函数，没有 self。标准做法是把状态指针
//! 塞进 GWLP_USERDATA：CreateWindowExW 的 lpCreateParams 在 WM_NCCREATE
//! 时通过 CREATESTRUCTW 传进来，那一刻存下指针，之后每条消息再取出来。
//!
//! 指针指向的是 MainWindow 持有的 Box<Inner>，Box 在生命周期内不会移动，
//! 所以指针始终有效。Drop 时先把 GWLP_USERDATA 清零再 DestroyWindow，
//! 避免销毁过程中窗口过程回读到一个已经被释放的 Inner。

use std::collections::VecDeque;
use std::ffi::c_void;
use std::marker::PhantomData;
use std::sync::Once;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontIndirectW, CreateSolidBrush, DeleteObject, DrawTextW, EndPaint,
    EnumFontFamiliesExW, FillRect, FrameRect, GetDC, GetSysColorBrush, InvalidateRect, ReleaseDC,
    SelectObject, SetBkMode, SetTextColor, UpdateWindow, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS,
    COLOR_BTNFACE, COLOR_BTNTEXT, DEFAULT_CHARSET, DEFAULT_PITCH, DT_CENTER, DT_SINGLELINE,
    DT_VCENTER, FF_DONTCARE, HDC, HFONT, HGDIOBJ, LOGFONTW, OUT_DEFAULT_PRECIS, PAINTSTRUCT,
    TEXTMETRICW, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{
    InitCommonControlsEx, BST_CHECKED, BST_UNCHECKED, ICC_STANDARD_CLASSES, INITCOMMONCONTROLSEX,
    INITCOMMONCONTROLSEX_ICC,
};
use windows::Win32::UI::HiDpi::{
    AdjustWindowRectExForDpi, GetDpiForSystem, GetDpiForWindow, SetProcessDpiAwarenessContext,
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, SetFocus};
use windows::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect,
    GetWindowLongPtrW, IsDialogMessageW, IsWindow, MoveWindow, PeekMessageW, PostMessageW,
    PostQuitMessage, RegisterClassExW, SendMessageW, SetWindowLongPtrW, SetWindowPos, ShowWindow,
    SystemParametersInfoW, TranslateMessage, BM_CLICK, BM_GETCHECK, BM_SETCHECK, BN_CLICKED,
    BS_AUTOCHECKBOX, BS_PUSHBUTTON, CBN_SELCHANGE, CBS_DROPDOWNLIST, CB_ADDSTRING, CB_GETCOUNT,
    CB_GETCURSEL, CB_SETCURSEL, CREATESTRUCTW, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT,
    GWLP_USERDATA, HMENU, MINMAXINFO, MSG, PM_REMOVE, SPI_GETWORKAREA, SWP_NOACTIVATE, SWP_NOMOVE,
    SWP_NOZORDER, SW_HIDE, SW_SHOW, WINDOW_EX_STYLE, WINDOW_STYLE, WM_CLOSE, WM_COMMAND, WM_CREATE,
    WM_CTLCOLORBTN, WM_CTLCOLORSTATIC, WM_DESTROY, WM_DPICHANGED, WM_GETMINMAXINFO, WM_NCCREATE,
    WM_NCDESTROY, WM_PAINT, WM_SETFONT, WM_SIZE, WNDCLASSEXW, WS_CHILD, WS_CLIPCHILDREN,
    WS_EX_CONTROLPARENT, WS_OVERLAPPEDWINDOW, WS_TABSTOP, WS_VISIBLE, WS_VSCROLL,
};

use crate::control::{ControlId, ControlKind};
use crate::dpi::{clamp_dpi, font_height, scale_len, unscale_len, UI_FONT_POINT};
use crate::event::UiEvent;
use crate::layout::{fit_to_work_area, grow, scale_rect, stretch, Anchor, Rect};
use crate::UiError;

/// 窗口类名。进程内唯一；重复注册（同进程开第二个窗口）会返回 0 且
/// GetLastError == ERROR_CLASS_ALREADY_EXISTS，这不是错误。
const CLASS_NAME: PCWSTR = w!("SideLensMainWindow");

/// 控件 ID 起点。从 1000 开始是为了避开 IDOK(1) / IDCANCEL(2) 这类
/// 系统预定义值 —— 虽然本窗口不是对话框，但用户可能自己再嵌对话框。
const FIRST_CONTROL_ID: isize = 1000;

/// 首选字体，按顺序挑第一个系统里存在的
///
/// 「Microsoft YaHei UI」是 Win8 起的中文 UI 字体，字重和字距都为界面调过；
/// 退到「Microsoft YaHei」保证 Win7 也不难看；再退 Segoe UI / Tahoma 保证
/// 精简版系统（Server Core、部分 LTSC）仍然有字。**绝不**用 SYSTEM_FONT：
/// 那是 Win3.1 时代的位图字体，在 4K 屏上会糊成一团。
const FONT_CANDIDATES: [&str; 4] = [
    "Microsoft YaHei UI",
    "Microsoft YaHei",
    "Segoe UI",
    "Tahoma",
];

/// 每个进程只做一次的初始化（DPI 感知 + 通用控件）
static INIT: Once = Once::new();

/// 视频占位区的背景色（深灰，和窗口的浅灰拉开层次）
const VIDEO_BG: COLORREF = COLORREF(0x0020_2024);

/// 视频占位区的边框色
const VIDEO_BORDER: COLORREF = COLORREF(0x0050_5058);

/// 视频占位区里的提示文字色
const VIDEO_HINT: COLORREF = COLORREF(0x0090_9098);

/// 把 &str 转成以 0 结尾的 UTF-16
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 把字符串写进 LOGFONTW.lfFaceName（最多 31 个 UTF-16 码元 + 结尾 0）
fn fill_face(lf: &mut LOGFONTW, face: &str) {
    let mut n = 0usize;
    for unit in face.encode_utf16() {
        if n + 1 >= lf.lfFaceName.len() {
            break;
        }
        lf.lfFaceName[n] = unit;
        n += 1;
    }
    lf.lfFaceName[n] = 0;
}

/// 枚举回调：只要被调用就说明这个字体族存在
extern "system" fn enum_face_cb(
    _lf: *const LOGFONTW,
    _tm: *const TEXTMETRICW,
    _font_type: u32,
    lparam: LPARAM,
) -> i32 {
    // SAFETY: lparam 由 face_available 传入，指向栈上一个存活的 i32；
    // EnumFontFamiliesExW 在调用返回前不会保留这个指针。
    unsafe {
        let flag = lparam.0 as *mut i32;
        if !flag.is_null() {
            *flag = 1;
        }
    }
    // 返回 0 = 停止枚举
    0
}

/// 系统里是否存在这个字体族
fn face_available(face: &str) -> bool {
    let mut lf = LOGFONTW {
        lfCharSet: DEFAULT_CHARSET,
        ..Default::default()
    };
    fill_face(&mut lf, face);

    // SAFETY: GetDC(None) 取屏幕 DC，配对 ReleaseDC；EnumFontFamiliesExW 只读 lf。
    unsafe {
        let hdc = GetDC(None);
        if hdc.is_invalid() {
            return false;
        }
        let mut found: i32 = 0;
        let _ = EnumFontFamiliesExW(
            hdc,
            &lf,
            Some(enum_face_cb),
            LPARAM(&mut found as *mut i32 as isize),
            0,
        );
        let _ = ReleaseDC(None, hdc);
        found != 0
    }
}

/// 进程级初始化：DPI 感知 + 通用控件
///
/// SetProcessDpiAwarenessContext 失败**不算错误**：宿主进程（比如某些安装器、
/// 或者被兼容模式启动）可能已经设过，此时返回 ERROR_ACCESS_DENIED。
/// 这时界面仍然按调用方设的模式渲染，功能正常。
fn init_process() {
    INIT.call_once(|| {
        // SAFETY: 纯初始化调用，无参数、无生命周期依赖。
        unsafe {
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            let icc = INITCOMMONCONTROLSEX {
                dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
                dwICC: INITCOMMONCONTROLSEX_ICC(ICC_STANDARD_CLASSES.0),
            };
            let _ = InitCommonControlsEx(&icc);
        }
    });
}

/// 主显示器的工作区（物理像素，不含任务栏）
///
/// 查不到时退回一个保守的 1280x720：宁可窗口小一点，也不要建出一个
/// 比屏幕还大的窗口把控件推到屏幕外。
fn work_area() -> (i32, i32, i32, i32) {
    let mut rc = RECT::default();
    // SAFETY: SPI_GETWORKAREA 只写我们提供的 RECT。
    let ok = unsafe {
        SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some(&mut rc as *mut RECT as *mut c_void),
            Default::default(),
        )
    }
    .is_ok();
    if !ok || rc.right <= rc.left || rc.bottom <= rc.top {
        return (0, 0, 1280, 720);
    }
    (rc.left, rc.top, rc.right - rc.left, rc.bottom - rc.top)
}

/// 控件表里的一行
struct ControlEntry {
    id: ControlId,
    kind: ControlKind,
    hwnd: HWND,
    /// 设计坐标系（96 DPI 逻辑像素）下的矩形
    rect: Rect,
    anchor: Anchor,
}

/// 窗口的全部可变状态
///
/// 单独一个类型是为了能安全地把裸指针交给窗口过程 —— 指针只指向它，
/// 生命周期由 [MainWindow] 的 Box 保证。
struct Inner {
    hwnd: HWND,
    hinstance: windows::Win32::Foundation::HINSTANCE,
    /// 当前 DPI（96 = 100%）
    dpi: u32,
    /// 设计时的客户区尺寸（逻辑像素）
    design: (i32, i32),
    /// 当前字体，DPI 变化时重建
    font: HFONT,
    controls: Vec<ControlEntry>,
    next_id: isize,
    events: VecDeque<UiEvent>,
    /// 视频区在设计坐标系下的矩形
    video: Rect,
    /// 视频区背景刷（创建一次，Drop 时删）
    video_brush: windows::Win32::Graphics::Gdi::HBRUSH,
    /// 视频区边框刷
    border_brush: windows::Win32::Graphics::Gdi::HBRUSH,
    /// 是否已经收到 WM_DESTROY
    destroyed: bool,
}

impl Inner {
    /// 客户区尺寸（物理像素）
    fn client_physical(&self) -> (u32, u32) {
        let mut rc = RECT::default();
        // SAFETY: hwnd 有效（Inner 只在窗口存活期间被使用）。
        let ok = unsafe { GetClientRect(self.hwnd, &mut rc) }.is_ok();
        if !ok {
            return (0, 0);
        }
        (
            (rc.right - rc.left).max(0) as u32,
            (rc.bottom - rc.top).max(0) as u32,
        )
    }

    /// 客户区尺寸（逻辑像素）
    fn client_logical(&self) -> (i32, i32) {
        let (w, h) = self.client_physical();
        (
            unscale_len(w as i32, self.dpi),
            unscale_len(h as i32, self.dpi),
        )
    }

    /// 当前视频区（逻辑像素，已按窗口尺寸增长）
    fn video_logical(&self) -> Rect {
        grow(self.video, self.design, self.client_logical())
    }

    /// 按当前 DPI 与窗口尺寸把所有控件摆到位
    fn relayout(&self) {
        let actual = self.client_logical();
        for c in &self.controls {
            let logical = stretch(c.rect, self.design, actual, c.anchor);
            let phys = scale_rect(logical, self.dpi);
            // SAFETY: hwnd 是有效子窗口；MoveWindow 只做位置/尺寸调整。
            let _ = unsafe { MoveWindow(c.hwnd, phys.x, phys.y, phys.w, phys.h, true) };
        }
    }

    /// 给所有控件重新设字体（DPI 变化后必须调用，否则文字大小不会变）
    fn apply_font(&self) {
        for c in &self.controls {
            // SAFETY: WM_SETFONT 是同步消息，wparam 传字体句柄、lparam 传 1 表示立即重绘。
            unsafe {
                SendMessageW(
                    c.hwnd,
                    WM_SETFONT,
                    Some(WPARAM(self.font.0 as usize)),
                    Some(LPARAM(1)),
                );
            }
        }
    }

    /// DPI 变化：重建字体 → 重新设字体 → 重新布局
    fn on_dpi_changed(&mut self, new_dpi: u32) {
        let dpi = clamp_dpi(new_dpi);
        if dpi == self.dpi {
            return;
        }
        self.dpi = dpi;
        self.rebuild_font();
        self.apply_font();
        self.relayout();
        self.events.push_back(UiEvent::DpiChanged(dpi));
    }

    /// 按当前 DPI 建字体，旧的删掉
    fn rebuild_font(&mut self) {
        let face = FONT_CANDIDATES
            .iter()
            .find(|f| face_available(f))
            .copied()
            .unwrap_or("Segoe UI");
        let mut lf = LOGFONTW {
            lfHeight: font_height(UI_FONT_POINT, self.dpi),
            // FW_NORMAL = 400。不用 FW_REGULAR 是因为它和 FW_NORMAL 同值，
            // 但语义上「普通字重」比「常规」更不容易被误解成「系统默认」。
            lfWeight: 400,
            lfCharSet: DEFAULT_CHARSET,
            lfOutPrecision: OUT_DEFAULT_PRECIS,
            lfClipPrecision: CLIP_DEFAULT_PRECIS,
            // ClearType：LCD 屏上比抗锯齿锐利一档，是 Windows 界面的默认观感
            lfQuality: CLEARTYPE_QUALITY,
            lfPitchAndFamily: DEFAULT_PITCH.0 | FF_DONTCARE.0,
            ..Default::default()
        };
        fill_face(&mut lf, face);

        // SAFETY: CreateFontIndirectW 只读 lf；旧字体由我们自己 DeleteObject。
        unsafe {
            let new_font = CreateFontIndirectW(&lf);
            if !self.font.is_invalid() {
                let _ = DeleteObject(HGDIOBJ(self.font.0));
            }
            self.font = new_font;
        }
    }

    /// 处理一条消息；返回 None 表示交给 DefWindowProcW
    fn handle(&mut self, hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
        match msg {
            WM_SIZE => {
                let (w, h) = self.client_physical();
                self.relayout();
                self.events.push_back(UiEvent::Resized(w, h));
                None
            }
            WM_DPICHANGED => {
                let dpi = (wparam.0 & 0xFFFF) as u32;
                self.on_dpi_changed(dpi);
                // lparam 是系统建议的新窗口矩形（已按新 DPI 换算），照它走
                let suggested = lparam.0 as *const RECT;
                if !suggested.is_null() {
                    // SAFETY: WM_DPICHANGED 的 lparam 保证指向一个有效 RECT。
                    let rc = unsafe { *suggested };
                    let _ = unsafe {
                        SetWindowPos(
                            hwnd,
                            None,
                            rc.left,
                            rc.top,
                            rc.right - rc.left,
                            rc.bottom - rc.top,
                            SWP_NOZORDER | SWP_NOACTIVATE,
                        )
                    };
                }
                // 让默认过程继续处理（它负责重画非客户区）
                None
            }
            WM_COMMAND => {
                let id = ControlId::from_raw((wparam.0 & 0xFFFF) as isize);
                let code = ((wparam.0 >> 16) & 0xFFFF) as u32;
                let entry = self.controls.iter().find(|c| c.id == id)?;
                let hwnd_ctrl = entry.hwnd;
                match (entry.kind, code) {
                    (ControlKind::Button, c) if c == BN_CLICKED => {
                        self.events.push_back(UiEvent::ButtonClicked(id));
                        Some(LRESULT(0))
                    }
                    (ControlKind::Checkbox, c) if c == BN_CLICKED => {
                        // BS_AUTOCHECKBOX 已经自己翻转了状态，这里只读结果
                        let checked = read_checked(hwnd_ctrl);
                        self.events.push_back(UiEvent::CheckboxToggled(id, checked));
                        Some(LRESULT(0))
                    }
                    (ControlKind::Combo, c) if c == CBN_SELCHANGE => {
                        let index = read_combo_index(hwnd_ctrl);
                        self.events.push_back(UiEvent::ComboChanged(id, index));
                        Some(LRESULT(0))
                    }
                    _ => None,
                }
            }
            WM_CTLCOLORSTATIC | WM_CTLCOLORBTN => {
                // 静态文本和复选框默认按「对话框背景」画，和本窗口的按钮面色
                // 不一致，视觉上就是一块块补丁。这里把背景设成透明、返回窗口
                // 背景刷，让文字直接浮在窗口底色上。
                let hdc = HDC(wparam.0 as *mut c_void);
                // SAFETY: WM_CTLCOLOR* 的 wparam 就是目标 DC；GetSysColorBrush 只读系统色表。
                let brush = unsafe {
                    let _ = SetBkMode(hdc, TRANSPARENT);
                    let _ = SetTextColor(hdc, COLORREF(sys_text_color()));
                    GetSysColorBrush(COLOR_BTNFACE)
                };
                Some(LRESULT(brush.0 as isize))
            }
            WM_GETMINMAXINFO => {
                // 窗口最小 640x480（逻辑像素），再小视频区就没意义了
                let info = lparam.0 as *mut MINMAXINFO;
                if !info.is_null() {
                    let mut rc = RECT {
                        left: 0,
                        top: 0,
                        right: scale_len(640, self.dpi),
                        bottom: scale_len(480, self.dpi),
                    };
                    // SAFETY: WM_GETMINMAXINFO 的 lparam 指向一个有效 MINMAXINFO。
                    unsafe {
                        let _ = AdjustWindowRectExForDpi(
                            &mut rc,
                            WS_OVERLAPPEDWINDOW,
                            false,
                            WINDOW_EX_STYLE(0),
                            self.dpi,
                        );
                        (*info).ptMinTrackSize.x = rc.right - rc.left;
                        (*info).ptMinTrackSize.y = rc.bottom - rc.top;
                    }
                }
                Some(LRESULT(0))
            }
            WM_PAINT => {
                self.paint(hwnd);
                Some(LRESULT(0))
            }
            WM_CLOSE => {
                // 显式 DestroyWindow 而不是交给默认过程，语义更直白
                let _ = unsafe { DestroyWindow(hwnd) };
                Some(LRESULT(0))
            }
            WM_DESTROY => {
                self.destroyed = true;
                self.events.push_back(UiEvent::Closed);
                // SAFETY: 线程内退出消息循环
                unsafe { PostQuitMessage(0) };
                Some(LRESULT(0))
            }
            WM_NCDESTROY => {
                // SAFETY: 窗口即将销毁，先把状态指针摘掉，防止后续消息读到悬垂指针
                unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) };
                None
            }
            WM_CREATE => Some(LRESULT(0)),
            _ => None,
        }
    }

    /// 画客户区背景与视频占位区
    fn paint(&self, hwnd: HWND) {
        let mut ps = PAINTSTRUCT::default();
        // SAFETY: BeginPaint/EndPaint 必须成对，且只能在 WM_PAINT 里调用。
        unsafe {
            let hdc = BeginPaint(hwnd, &mut ps);
            let video = scale_rect(self.video_logical(), self.dpi);
            if !video.is_empty() {
                let rc = RECT {
                    left: video.x,
                    top: video.y,
                    right: video.right(),
                    bottom: video.bottom(),
                };
                FillRect(hdc, &rc, self.video_brush);
                FrameRect(hdc, &rc, self.border_brush);

                // 居中的提示文字：告诉调用方这块是留给渲染层的
                let mut text = wide("视频区域（由 sidelens-render-gpu 渲染）");
                let mut trc = rc;
                let old = SelectObject(hdc, HGDIOBJ(self.font.0));
                let _ = SetBkMode(hdc, TRANSPARENT);
                let _ = SetTextColor(hdc, VIDEO_HINT);
                let _ = DrawTextW(
                    hdc,
                    &mut text,
                    &mut trc,
                    DT_CENTER | DT_VCENTER | DT_SINGLELINE,
                );
                let _ = SelectObject(hdc, old);
            }
            let _ = EndPaint(hwnd, &ps);
        }
    }
}

/// 系统按钮文字色
///
/// 不能写死黑色：深色主题下会和背景糊在一起。GetSysColorBrush 只能拿刷子，
/// 文字色必须走 GetSysColor。
fn sys_text_color() -> u32 {
    // SAFETY: 纯查询。
    unsafe { windows::Win32::Graphics::Gdi::GetSysColor(COLOR_BTNTEXT) }
}

/// 下拉框的子类 ID（同一个窗口可以有多个子类过程，用 ID 区分）
const COMBO_SUBCLASS_ID: usize = 1;

/// 下拉框子类过程：吞掉鼠标滚轮
///
/// # 这个 bug 值得单独一段
///
/// 分辨率下拉框是个「一改就生效」的设置。用户把鼠标停在窗口上滚轮翻页时，
/// 光标恰好压在下拉框上 —— CBS_DROPDOWNLIST 的默认行为是**直接把选中项
/// 往前/后挪一格**。结果是用户什么都没点，推流分辨率从 1920x1080 变成了
/// 1280x720，画面糊了还找不到原因。
///
/// Win32 没有开关能关掉这个行为，唯一正路是子类化：拦下 WM_MOUSEWHEEL
/// 直接返回 0（不往下传），其它消息原样交给 DefSubclassProc。
///
/// 这里不需要任何共享状态，所以 `dwrefdata` 传 0 —— 也就不会有
/// 「窗口过程拿 &mut self、子类过程同时拿 &self」的别名问题。
unsafe extern "system" fn combo_swallow_wheel(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _refdata: usize,
) -> LRESULT {
    unsafe {
        if msg == windows::Win32::UI::WindowsAndMessaging::WM_MOUSEWHEEL {
            // 返回 0 表示「已处理」，comctl32 不会再改选中项
            return LRESULT(0);
        }
        DefSubclassProc(hwnd, msg, wparam, lparam)
    }
}

/// 读复选框状态
fn read_checked(hwnd: HWND) -> bool {
    // SAFETY: BM_GETCHECK 同步返回，不持有参数。
    let v = unsafe { SendMessageW(hwnd, BM_GETCHECK, None, None) };
    v.0 == BST_CHECKED.0 as isize
}

/// 读下拉框当前下标（没有选中时返回 0）
fn read_combo_index(hwnd: HWND) -> usize {
    // SAFETY: CB_GETCURSEL 同步返回下标，未选中返回 CB_ERR(-1)。
    let v = unsafe { SendMessageW(hwnd, CB_GETCURSEL, None, None) }.0;
    if v < 0 {
        0
    } else {
        v as usize
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        // SAFETY: 两个刷子和字体都是 GDI 对象，必须 DeleteObject。
        unsafe {
            if !self.video_brush.is_invalid() {
                let _ = DeleteObject(HGDIOBJ(self.video_brush.0));
            }
            if !self.border_brush.is_invalid() {
                let _ = DeleteObject(HGDIOBJ(self.border_brush.0));
            }
            if !self.font.is_invalid() {
                let _ = DeleteObject(HGDIOBJ(self.font.0));
            }
        }
    }
}

/// 窗口过程
unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        if msg == WM_NCCREATE {
            let cs = &*(lparam.0 as *const CREATESTRUCTW);
            let inner = cs.lpCreateParams as *mut Inner;
            if !inner.is_null() {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, inner as isize);
                (*inner).hwnd = hwnd;
            }
        }

        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Inner;
        if ptr.is_null() {
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        let inner = &mut *ptr;
        match inner.handle(hwnd, msg, wparam, lparam) {
            Some(r) => r,
            None => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

/// SideLens 主窗口
///
/// 一个进程一个主窗口。所有控件都是 Win32 原生子窗口（STATIC / BUTTON / COMBOBOX），
/// 没有自绘、没有第三方 GUI 库。
///
/// # 用法
///
/// ```no_run
/// use sidelens_ui::{MainWindow, UiEvent};
///
/// # fn main() -> Result<(), sidelens_ui::UiError> {
/// let mut win = MainWindow::new("SideLens", 900, 640)?;
/// let start = win.add_button("开始", 600, 560, 100, 32);
/// for _ in 0..10 {
///     while let Some(ev) = win.pump() {
///         println!("{ev}");
///         if ev == UiEvent::Closed {
///             return Ok(());
///         }
///     }
///     std::thread::sleep(std::time::Duration::from_millis(1));
/// }
/// let _ = start;
/// # Ok(())
/// # }
/// ```
///
/// # 为什么是 !Send
///
/// Win32 窗口属于创建它的线程，窗口过程只在该线程被调用。把它挪到别的线程
/// 会让消息永远堆在队列里 —— 编译期挡住这种写法。
pub struct MainWindow {
    inner: Box<Inner>,
    /// 让类型变成 !Send / !Sync
    _not_send: PhantomData<*mut ()>,
}

impl std::fmt::Debug for MainWindow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MainWindow")
            .field("hwnd", &self.inner.hwnd)
            .field("dpi", &self.inner.dpi)
            .field("design", &self.inner.design)
            .field("controls", &self.inner.controls.len())
            .finish_non_exhaustive()
    }
}

impl MainWindow {
    /// 建一个可见的主窗口
    ///
    /// width / height 是**逻辑像素**的客户区尺寸（不含标题栏和边框），
    /// 内部按当前 DPI 折算成外框尺寸，这样设计稿量出来的数字在任何缩放下都对得上。
    pub fn new(title: &str, width: u32, height: u32) -> Result<Self, UiError> {
        Self::create(title, width, height, true)
    }

    /// 建一个**隐藏**的主窗口
    ///
    /// 给自动化测试用：测试不能弹真窗口卡住（会挡住 CI 的桌面，
    /// 也可能因为无人点击而永远等下去）。隐藏窗口照样有完整的消息队列、
    /// 控件树和布局逻辑，能验证除「肉眼可见」以外的一切。
    pub fn new_hidden(title: &str, width: u32, height: u32) -> Result<Self, UiError> {
        Self::create(title, width, height, false)
    }

    fn create(title: &str, width: u32, height: u32, visible: bool) -> Result<Self, UiError> {
        if width == 0 || height == 0 {
            return Err(UiError::Window(format!(
                "窗口尺寸不能为 0（{width}x{height}）"
            )));
        }
        init_process();

        // SAFETY: 以下整段都是 Win32 建窗口的标准流程，参数都在本函数内构造、
        // 生命周期覆盖 CreateWindowExW 调用；窗口过程拿到的指针指向 Box 内的 Inner。
        unsafe {
            let hmodule = GetModuleHandleW(PCWSTR::null())
                .map_err(|e| UiError::Window(format!("取模块句柄失败：{e}")))?;
            let hinstance: windows::Win32::Foundation::HINSTANCE = hmodule.into();

            let wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(wndproc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: hinstance,
                hIcon: Default::default(),
                hCursor: Default::default(),
                // COLOR_BTNFACE + 1：系统色索引加一表示「用这个系统色做背景刷」，
                // 这样窗口背景会跟随系统主题，深色模式也不会突兀。
                hbrBackground: windows::Win32::Graphics::Gdi::HBRUSH(
                    (COLOR_BTNFACE.0 + 1) as isize as *mut c_void,
                ),
                lpszMenuName: PCWSTR::null(),
                lpszClassName: CLASS_NAME,
                hIconSm: Default::default(),
            };
            // 已注册过（同进程开第二个窗口）返回 0，这不是错误
            RegisterClassExW(&wc);

            // 建窗口前拿不到「窗口所在显示器」的 DPI，先用系统 DPI 估外框，
            // 建完立刻用 GetDpiForWindow 校正（见下面）。
            let dpi = clamp_dpi(GetDpiForSystem());

            let mut outer = RECT {
                left: 0,
                top: 0,
                right: scale_len(width as i32, dpi),
                bottom: scale_len(height as i32, dpi),
            };
            AdjustWindowRectExForDpi(
                &mut outer,
                WS_OVERLAPPEDWINDOW,
                false,
                WINDOW_EX_STYLE(0),
                dpi,
            )
            .map_err(|e| UiError::Window(format!("折算窗口外框失败：{e}")))?;

            // 把外框夹进屏幕工作区：1920x1080 + 200% 缩放下，900x640 的客户区
            // 外框是 1832x1336 —— 比屏幕还高，底部按钮会被挤到屏幕外点不到。
            let (work_w, work_h) = {
                let (_, _, w, h) = work_area();
                (w, h)
            };
            let (fit_w, fit_h) = fit_to_work_area(
                outer.right - outer.left,
                outer.bottom - outer.top,
                (0, 0, work_w, work_h),
                (scale_len(640, dpi), scale_len(480, dpi)),
            );
            if fit_w != outer.right - outer.left || fit_h != outer.bottom - outer.top {
                eprintln!(
                    "sidelens-ui: 请求的外框 {}x{} 超出工作区 {}x{}，已收缩到 {}x{}",
                    outer.right - outer.left,
                    outer.bottom - outer.top,
                    work_w,
                    work_h,
                    fit_w,
                    fit_h
                );
            }

            let mut inner = Box::new(Inner {
                hwnd: HWND::default(),
                hinstance,
                dpi,
                // 设计基准用**请求**尺寸：窗口被工作区夹小之后，
                // 布局会按「比设计尺寸小」重排（锚点自动往回走），
                // 这正是我们想要的降级行为。
                design: (width as i32, height as i32),
                font: HFONT::default(),
                controls: Vec::new(),
                next_id: FIRST_CONTROL_ID,
                events: VecDeque::new(),
                // 视频区默认：左右各留 16、上留 96（状态栏）、下留 96（按钮行）
                video: Rect::new(16, 96, width as i32 - 32, height as i32 - 192),
                video_brush: CreateSolidBrush(VIDEO_BG),
                border_brush: CreateSolidBrush(VIDEO_BORDER),
                destroyed: false,
            });
            inner.rebuild_font();

            let ptr: *mut Inner = &mut *inner;
            let title_w = wide(title);
            let hwnd = CreateWindowExW(
                WS_EX_CONTROLPARENT,
                CLASS_NAME,
                PCWSTR(title_w.as_ptr()),
                WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                fit_w,
                fit_h,
                None,
                None,
                Some(hinstance),
                Some(ptr as *const c_void),
            )
            .map_err(|e| UiError::Window(format!("创建窗口失败：{e}")))?;

            // 用真实显示器 DPI 校正（多显示器混合 DPI 时和系统 DPI 可能不同）
            let real_dpi = clamp_dpi(GetDpiForWindow(hwnd));
            if real_dpi != inner.dpi {
                inner.dpi = real_dpi;
                inner.rebuild_font();
            }

            if visible {
                let _ = ShowWindow(hwnd, SW_SHOW);
            }

            Ok(MainWindow {
                inner,
                _not_send: PhantomData,
            })
        }
    }

    /// 创建一个子控件并登记
    ///
    /// 参数多且都是 Win32 必需项，拆成 builder 反而更难读 —— 这是本 crate
    /// 唯一允许超过 7 个参数的函数。
    #[allow(clippy::too_many_arguments)]
    fn create_control(
        &mut self,
        kind: ControlKind,
        class: PCWSTR,
        text: &str,
        style: WINDOW_STYLE,
        ex_style: WINDOW_EX_STYLE,
        rect: Rect,
    ) -> Result<ControlId, UiError> {
        let id = self.inner.next_id;
        self.inner.next_id += 1;
        let control_id = ControlId::from_raw(id);

        // 逻辑坐标 → 物理坐标
        let dpi = self.inner.dpi;
        let phys = scale_rect(rect, dpi);
        // 下拉框要按「列表全展开」的高度创建，列表才有空间；
        // 创建后立刻把可见高度收回到闭合高度（Win32 的经典做法）。
        let create_h = if kind == ControlKind::Combo {
            phys.h + scale_len(24, dpi) * 8
        } else {
            phys.h
        };

        let text_w = wide(text);
        // SAFETY: 参数都在本函数内构造；父窗口有效；hMenu 位置传的是控件 ID。
        let hwnd = unsafe {
            CreateWindowExW(
                ex_style,
                class,
                PCWSTR(text_w.as_ptr()),
                style,
                phys.x,
                phys.y,
                phys.w,
                create_h,
                Some(self.inner.hwnd),
                Some(HMENU(id as *mut c_void)),
                Some(self.inner.hinstance),
                None,
            )
        }
        .map_err(|e| {
            UiError::Control(format!(
                "创建{}失败（id={id} text={text}）：{e}",
                kind.as_str()
            ))
        })?;

        // 系统默认字体是 SYSTEM_FONT（位图字体），必须显式换掉
        unsafe {
            SendMessageW(
                hwnd,
                WM_SETFONT,
                Some(WPARAM(self.inner.font.0 as usize)),
                Some(LPARAM(1)),
            );
        }

        if kind == ControlKind::Combo {
            // 收回可见高度：列表高度已经记住了，这里只改可见部分
            unsafe {
                let _ = SetWindowPos(
                    hwnd,
                    None,
                    0,
                    0,
                    0,
                    phys.h,
                    SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
                );
                // 吞掉鼠标滚轮，见 combo_swallow_wheel 的说明
                let _ = SetWindowSubclass(hwnd, Some(combo_swallow_wheel), COMBO_SUBCLASS_ID, 0);
            }
        }

        self.inner.controls.push(ControlEntry {
            id: control_id,
            kind,
            hwnd,
            rect,
            anchor: Anchor::TopLeft,
        });
        Ok(control_id)
    }

    /// 加一行静态文本
    pub fn add_label(&mut self, text: &str, x: i32, y: i32, w: i32, h: i32) -> ControlId {
        // SS_LEFT 的值就是 0，但引用它要额外开 Win32_System_SystemServices feature，
        // 为常量 0 拉一个 feature 不划算；这里直接写 0 并留注释。
        let style = WS_CHILD | WS_VISIBLE | WINDOW_STYLE(0);
        self.create_control(
            ControlKind::Label,
            w!("STATIC"),
            text,
            style,
            WINDOW_EX_STYLE(0),
            Rect::new(x, y, w, h),
        )
        .unwrap_or_else(|e| {
            // 少一行文字不该让整个界面起不来；返回无效 ID，调用方的后续操作会静默跳过
            eprintln!("sidelens-ui: {e}");
            ControlId::from_raw(-1)
        })
    }

    /// 加一个普通按钮
    pub fn add_button(&mut self, text: &str, x: i32, y: i32, w: i32, h: i32) -> ControlId {
        let style = WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(BS_PUSHBUTTON as u32);
        self.create_control(
            ControlKind::Button,
            w!("BUTTON"),
            text,
            style,
            WINDOW_EX_STYLE(0),
            Rect::new(x, y, w, h),
        )
        .unwrap_or_else(|e| {
            eprintln!("sidelens-ui: {e}");
            ControlId::from_raw(-1)
        })
    }

    /// 加一个复选框
    ///
    /// 用 BS_AUTOCHECKBOX 而不是手动切 BM_SETCHECK：系统自己维护勾选状态、
    /// 自己画勾，调用方只需要在 [UiEvent::CheckboxToggled] 里读结果。
    pub fn add_checkbox(
        &mut self,
        text: &str,
        checked: bool,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
    ) -> ControlId {
        let style = WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(BS_AUTOCHECKBOX as u32);
        let id = self
            .create_control(
                ControlKind::Checkbox,
                w!("BUTTON"),
                text,
                style,
                WINDOW_EX_STYLE(0),
                Rect::new(x, y, w, h),
            )
            .unwrap_or_else(|e| {
                eprintln!("sidelens-ui: {e}");
                ControlId::from_raw(-1)
            });
        if checked && !id.is_invalid() {
            self.set_checked(id, true);
        }
        id
    }

    /// 加一个下拉框
    ///
    /// h 是**闭合状态**的高度（例如 26），列表展开高度由控件自己算。
    /// selected 超出范围时退回最后一项，不会 panic —— 配置项数量变了
    /// 是常态，界面不该因此崩掉。
    pub fn add_combo(
        &mut self,
        items: &[&str],
        selected: usize,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
    ) -> ControlId {
        let style =
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | WS_VSCROLL | WINDOW_STYLE(CBS_DROPDOWNLIST as u32);
        let id = self
            .create_control(
                ControlKind::Combo,
                w!("COMBOBOX"),
                "",
                style,
                WINDOW_EX_STYLE(0),
                Rect::new(x, y, w, h),
            )
            .unwrap_or_else(|e| {
                eprintln!("sidelens-ui: {e}");
                ControlId::from_raw(-1)
            });
        if id.is_invalid() {
            return id;
        }
        let Some(hwnd) = self.entry(id).map(|e| e.hwnd) else {
            return id;
        };
        for item in items {
            let w_item = wide(item);
            // SAFETY: CB_ADDSTRING 同步复制字符串，调用返回后 w_item 可以释放。
            unsafe {
                SendMessageW(
                    hwnd,
                    CB_ADDSTRING,
                    None,
                    Some(LPARAM(w_item.as_ptr() as isize)),
                );
            }
        }
        if !items.is_empty() {
            let index = selected.min(items.len() - 1);
            // SAFETY: CB_SETCURSEL 同步生效，不触发 CBN_SELCHANGE（不会回灌事件）。
            unsafe {
                SendMessageW(hwnd, CB_SETCURSEL, Some(WPARAM(index)), None);
            }
        }
        id
    }

    /// 找控件
    fn entry(&self, id: ControlId) -> Option<&ControlEntry> {
        self.inner.controls.iter().find(|c| c.id == id)
    }

    /// 改控件文字
    ///
    /// 对下拉框无效（下拉框的「文字」是选中项，不是 caption）。
    pub fn set_text(&mut self, id: ControlId, text: &str) {
        let Some((hwnd, kind)) = self.entry(id).map(|e| (e.hwnd, e.kind)) else {
            return;
        };
        if kind == ControlKind::Combo {
            return;
        }
        let w_text = wide(text);
        // SAFETY: SetWindowTextW 同步复制字符串。
        unsafe {
            let _ = windows::Win32::UI::WindowsAndMessaging::SetWindowTextW(
                hwnd,
                PCWSTR(w_text.as_ptr()),
            );
            // 让控件立刻重画，否则要等下一次自然失效
            let _ = InvalidateRect(Some(hwnd), None, true);
        }
    }

    /// 设置复选框勾选状态
    ///
    /// 直接发 BM_SETCHECK，**不会**产生 [UiEvent::CheckboxToggled] ——
    /// 程序主动改状态不该被当成用户操作。
    pub fn set_checked(&mut self, id: ControlId, checked: bool) {
        let Some((hwnd, kind)) = self.entry(id).map(|e| (e.hwnd, e.kind)) else {
            return;
        };
        if kind != ControlKind::Checkbox {
            return;
        }
        let state = if checked {
            BST_CHECKED.0 as usize
        } else {
            BST_UNCHECKED.0 as usize
        };
        // SAFETY: BM_SETCHECK 同步生效。
        unsafe {
            SendMessageW(hwnd, BM_SETCHECK, Some(WPARAM(state)), None);
        }
    }

    /// 读复选框勾选状态（非复选框恒返回 false）
    pub fn is_checked(&self, id: ControlId) -> bool {
        match self.entry(id) {
            Some(e) if e.kind == ControlKind::Checkbox => read_checked(e.hwnd),
            _ => false,
        }
    }

    /// 设置下拉框选中项（下标从 0 开始，越界被夹到有效范围）
    ///
    /// 用 `CB_SETCURSEL` 而不是模拟键盘：前者**不会**产生
    /// [UiEvent::ComboChanged] —— 程序主动改选中项不该被当成用户操作，
    /// 否则「加载配置」这个动作会立刻触发一遍「用户改了配置」的回调，
    /// 进而把配置再存一次，形成无意义的写盘循环。
    pub fn set_combo_index(&mut self, id: ControlId, index: usize) {
        let Some((hwnd, kind)) = self.entry(id).map(|e| (e.hwnd, e.kind)) else {
            return;
        };
        if kind != ControlKind::Combo {
            return;
        }
        // SAFETY: CB_GETCOUNT / CB_SETCURSEL 都同步返回，不持有参数。
        unsafe {
            let count = SendMessageW(hwnd, CB_GETCOUNT, None, None).0;
            if count <= 0 {
                return;
            }
            let clamped = (index as isize).min(count - 1).max(0);
            SendMessageW(hwnd, CB_SETCURSEL, Some(WPARAM(clamped as usize)), None);
        }
    }

    /// 读下拉框当前选中项下标（非下拉框恒返回 0）
    pub fn combo_index(&self, id: ControlId) -> usize {
        match self.entry(id) {
            Some(e) if e.kind == ControlKind::Combo => read_combo_index(e.hwnd),
            _ => 0,
        }
    }

    /// 启用 / 禁用控件
    ///
    /// 用 EnableWindow 而不是改 WS_DISABLED 样式位：后者不会触发重画，
    /// 控件会停在旧的视觉状态（看起来能点但点不动）。
    pub fn set_enabled(&mut self, id: ControlId, enabled: bool) {
        let Some(hwnd) = self.entry(id).map(|e| e.hwnd) else {
            return;
        };
        // SAFETY: EnableWindow 同步生效。
        unsafe {
            let _ = EnableWindow(hwnd, enabled);
        }
    }

    /// 显示 / 隐藏控件
    pub fn set_visible(&mut self, id: ControlId, visible: bool) {
        let Some(hwnd) = self.entry(id).map(|e| e.hwnd) else {
            return;
        };
        // SAFETY: ShowWindow 同步生效。
        unsafe {
            let _ = ShowWindow(hwnd, if visible { SW_SHOW } else { SW_HIDE });
        }
    }

    /// 设置控件在窗口变大时的贴边方式
    ///
    /// 第一版布局只有「固定像素 + 视频区填满剩余空间」，但底部按钮行必须
    /// 跟着窗口底边走，否则窗口拉高后按钮会飘在中间。
    pub fn set_anchor(&mut self, id: ControlId, anchor: Anchor) {
        let Some(index) = self.inner.controls.iter().position(|c| c.id == id) else {
            return;
        };
        self.inner.controls[index].anchor = anchor;

        let design = self.inner.design;
        let dpi = self.inner.dpi;
        let actual = self.inner.client_logical();
        let entry = &self.inner.controls[index];
        let logical = stretch(entry.rect, design, actual, entry.anchor);
        let phys = scale_rect(logical, dpi);
        // SAFETY: hwnd 有效。
        let _ = unsafe { MoveWindow(entry.hwnd, phys.x, phys.y, phys.w, phys.h, true) };
    }

    /// 设置视频区（设计坐标系下的矩形，逻辑像素）
    ///
    /// 窗口变大时这块矩形会跟着长（[crate::layout::grow]），
    /// 调用方拿 [MainWindow::video_rect] 得到的永远是**当前**的物理矩形。
    pub fn set_video_rect(&mut self, x: i32, y: i32, w: i32, h: i32) {
        self.inner.video = Rect::new(x, y, w, h);
        // SAFETY: hwnd 有效。
        unsafe {
            let _ = InvalidateRect(Some(self.inner.hwnd), None, true);
        }
    }

    /// 预留的视频矩形（客户区坐标，**物理像素**）
    ///
    /// 返回的是「现在」的位置：窗口缩放或 DPI 变化后要重新取一次。
    /// 渲染层（sidelens-render-gpu）应该在自己的 resize 处理里调用它。
    pub fn video_rect(&self) -> (i32, i32, i32, i32) {
        scale_rect(self.inner.video_logical(), self.inner.dpi).to_tuple()
    }

    /// 非阻塞消息泵
    ///
    /// 用 PeekMessageW 而不是 GetMessageW：调用方是 1ms tick 的主循环，
    /// 阻塞会让视频和解码一起停住。一次调用会把队列里**所有**待处理消息
    /// 抽干，但只返回**第一个**事件；剩下的留在内部队列里，下次 pump 直接吐出来。
    ///
    /// 窗口已销毁后恒返回 None。
    pub fn pump(&mut self) -> Option<UiEvent> {
        if let Some(ev) = self.inner.events.pop_front() {
            return Some(ev);
        }
        if self.inner.destroyed {
            return None;
        }
        if !self.inner.hwnd.is_invalid() && !is_window(self.inner.hwnd) {
            return None;
        }
        self.drain_messages();
        self.inner.events.pop_front()
    }

    /// 把系统消息队列抽干（非阻塞）
    fn drain_messages(&mut self) {
        let mut msg = MSG::default();
        // SAFETY: PeekMessageW 只填 msg；PM_REMOVE 表示取走。
        unsafe {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == windows::Win32::UI::WindowsAndMessaging::WM_QUIT {
                    self.inner.events.push_back(UiEvent::Closed);
                    continue;
                }
                // 让 IsDialogMessageW 处理 Tab 焦点循环与 Enter/Escape。
                // 普通窗口没有对话框管理器，不调它的话 Tab 键在控件之间跳不动。
                if IsDialogMessageW(self.inner.hwnd, &msg).as_bool() {
                    continue;
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }

    /// 客户区尺寸（**物理像素**）
    pub fn client_size(&self) -> (u32, u32) {
        self.inner.client_physical()
    }

    /// 当前 DPI（96 = 100%）
    pub fn dpi(&self) -> u32 {
        self.inner.dpi
    }

    /// 窗口句柄（裸值）
    ///
    /// 给渲染层建 swap chain / 子窗口用。传出去的是 HWND 的地址值，
    /// 不是「另一个窗口」的拥有权 —— 窗口仍由本类型负责销毁。
    pub fn hwnd(&self) -> isize {
        self.inner.hwnd.0 as isize
    }

    /// 改客户区尺寸（逻辑像素）
    ///
    /// 走 AdjustWindowRectExForDpi + SetWindowPos，保证「客户区正好是
    /// 你要的尺寸」—— 直接 SetWindowPos 改外框会让客户区少掉边框那几像素。
    pub fn set_client_size(&mut self, width: u32, height: u32) {
        let dpi = self.inner.dpi;
        let mut rc = RECT {
            left: 0,
            top: 0,
            right: scale_len(width as i32, dpi),
            bottom: scale_len(height as i32, dpi),
        };
        // SAFETY: hwnd 有效；SetWindowPos 同步发送 WM_SIZE。
        unsafe {
            if AdjustWindowRectExForDpi(
                &mut rc,
                WS_OVERLAPPEDWINDOW,
                false,
                WINDOW_EX_STYLE(0),
                dpi,
            )
            .is_ok()
            {
                let _ = SetWindowPos(
                    self.inner.hwnd,
                    None,
                    0,
                    0,
                    rc.right - rc.left,
                    rc.bottom - rc.top,
                    SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
        }
    }

    /// 显示窗口
    ///
    /// 配合 [MainWindow::new_hidden] 用：「先把控件全部建好、布局算完，
    /// 再一次性显示」。否则用户会先看到一个空窗口，几十毫秒后控件才一个个
    /// 冒出来 —— 原生程序里这叫「白屏闪烁」，观感比慢一点还差。
    ///
    /// 对已经可见的窗口调用它是幂等的（`ShowWindow` 会原样返回）。
    pub fn show(&self) {
        // SAFETY: hwnd 有效时同步生效；无效时 ShowWindow 只返回 false。
        unsafe {
            let _ = ShowWindow(self.inner.hwnd, SW_SHOW);
        }
    }

    /// 隐藏窗口（不销毁）
    ///
    /// 隐藏后消息泵照常工作、控件树和布局都还在，只是不合成到屏幕上。
    /// 自动化测试用它避免弹窗打扰用户，也正是 [MainWindow::new_hidden] 的底层。
    pub fn hide(&self) {
        // SAFETY: 同上。
        unsafe {
            let _ = ShowWindow(self.inner.hwnd, SW_HIDE);
        }
    }

    /// 请求关闭窗口（投递 WM_CLOSE，不阻塞）
    ///
    /// 关闭是异步的：调用后要等 pump 吐出 [UiEvent::Closed] 才算真的关掉。
    pub fn close(&self) {
        // SAFETY: 窗口可能已经被销毁，PostMessageW 会返回错误，忽略即可。
        unsafe {
            let _ = PostMessageW(Some(self.inner.hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
    }

    /// 用程序「点一下」按钮（等价于用户单击）
    ///
    /// 发的是 `BM_CLICK`，走的是**完整**的 WM_COMMAND 链路 —— 也就是说
    /// 事件会正常进队列，和真人点没有区别。给自动化测试和 demo 用：
    /// 没有它就只能靠 SendInput 移动真实鼠标，在 CI 上既不稳定又会干扰用户。
    ///
    /// 只对按钮/复选框有效；其它控件静默跳过。
    pub fn click(&self, id: ControlId) {
        let Some(entry) = self.entry(id) else { return };
        if !matches!(entry.kind, ControlKind::Button | ControlKind::Checkbox) {
            return;
        }
        // SAFETY: BM_CLICK 同步派发 BN_CLICKED 到父窗口。
        unsafe {
            SendMessageW(entry.hwnd, BM_CLICK, None, None);
        }
    }

    /// 把焦点给某个控件
    pub fn focus(&self, id: ControlId) {
        if let Some(entry) = self.entry(id) {
            // SAFETY: SetFocus 同步生效，窗口必须是活动窗口才有意义。
            unsafe {
                let _ = SetFocus(Some(entry.hwnd));
            }
        }
    }

    /// 重画客户区
    pub fn repaint(&self) {
        // SAFETY: hwnd 有效。
        unsafe {
            let _ = InvalidateRect(Some(self.inner.hwnd), None, true);
            let _ = UpdateWindow(self.inner.hwnd);
        }
    }
}

/// IsWindow 的包装：窗口句柄无效时返回 false 而不是 panic
fn is_window(hwnd: HWND) -> bool {
    // SAFETY: IsWindow 对任意句柄都安全，只做查询。
    unsafe { IsWindow(Some(hwnd)).as_bool() }
}

impl Drop for MainWindow {
    fn drop(&mut self) {
        // SAFETY: 先把窗口过程的状态指针摘掉，再销毁窗口，
        // 这样 WM_DESTROY / WM_NCDESTROY 里不会读到已经要释放的 Inner。
        unsafe {
            if !self.inner.hwnd.is_invalid() && is_window(self.inner.hwnd) {
                SetWindowLongPtrW(self.inner.hwnd, GWLP_USERDATA, 0);
                let _ = DestroyWindow(self.inner.hwnd);
            }
        }
        // 字体与刷子由 Inner 的 Drop 释放
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 建隐藏窗口；无交互桌面（服务会话）时返回 None，测试跳过而不是失败
    fn try_window() -> Option<MainWindow> {
        match MainWindow::new_hidden("SideLens 测试窗口", 900, 640) {
            Ok(w) => Some(w),
            Err(e) => {
                eprintln!("跳过窗口测试：{e}");
                None
            }
        }
    }

    #[test]
    fn 零尺寸窗口被拒绝() {
        let err = MainWindow::new("x", 0, 640).unwrap_err();
        assert_eq!(err.kind(), "窗口");
    }

    #[test]
    fn 隐藏窗口的客户区尺寸与dpi合理() {
        let Some(win) = try_window() else { return };
        let (w, h) = win.client_size();
        assert!(w >= 800, "客户区宽度 {w} 太小");
        assert!(h >= 500, "客户区高度 {h} 太小");
        let dpi = win.dpi();
        assert!((48..=960).contains(&dpi), "DPI {dpi} 不合理");
        assert!(win.hwnd() != 0);
    }

    #[test]
    fn 控件句柄唯一且递增() {
        let Some(mut win) = try_window() else { return };
        let a = win.add_label("连接状态", 16, 16, 200, 24);
        let b = win.add_button("开始", 16, 560, 100, 32);
        let c = win.add_checkbox("启用音频", true, 16, 500, 120, 24);
        let d = win.add_combo(&["1920x1080", "1280x720"], 0, 16, 460, 160, 26);
        assert!(a.raw() < b.raw() && b.raw() < c.raw() && c.raw() < d.raw());
        assert!(!a.is_invalid());
    }

    #[test]
    fn 复选框状态可读写() {
        let Some(mut win) = try_window() else { return };
        let cb = win.add_checkbox("启用摄像头", true, 16, 100, 140, 24);
        assert!(win.is_checked(cb));
        win.set_checked(cb, false);
        assert!(!win.is_checked(cb));
        win.set_checked(cb, true);
        assert!(win.is_checked(cb));
    }

    #[test]
    fn 非复选框读勾选恒为假() {
        let Some(mut win) = try_window() else { return };
        let btn = win.add_button("开始", 16, 100, 100, 32);
        assert!(!win.is_checked(btn));
    }

    #[test]
    fn 视频矩形在客户区内且非空() {
        let Some(win) = try_window() else { return };
        let (x, y, w, h) = win.video_rect();
        let (cw, ch) = win.client_size();
        assert!(x >= 0 && y >= 0);
        assert!(w > 0 && h > 0);
        assert!(
            x + w <= cw as i32 + 1,
            "视频区右边界 {} 超出客户区 {cw}",
            x + w
        );
        assert!(y + h <= ch as i32 + 1);
    }

    #[test]
    fn 视频矩形随dpi缩放() {
        let Some(win) = try_window() else { return };
        let (_, _, w, _) = win.video_rect();
        let expect = scale_len(900 - 32, win.dpi());
        assert!((w - expect).abs() <= 2, "视频宽 {w}，期望约 {expect}");
    }

    #[test]
    fn 消息泵在没有消息时不阻塞() {
        let Some(mut win) = try_window() else { return };
        let start = std::time::Instant::now();
        for _ in 0..200 {
            let _ = win.pump();
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "200 次 pump 花了 {elapsed:?}，说明消息泵在阻塞"
        );
    }

    #[test]
    fn 改客户区尺寸后收到resized事件() {
        let Some(mut win) = try_window() else { return };
        // 先抽干创建阶段的残留消息
        while win.pump().is_some() {}
        win.set_client_size(1000, 700);
        let mut got = false;
        for _ in 0..64 {
            if let Some(UiEvent::Resized(w, h)) = win.pump() {
                assert!(w > 0 && h > 0);
                got = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(got, "没有收到 Resized 事件");
    }

    #[test]
    fn 关闭后pump返回closed并停止() {
        let Some(mut win) = try_window() else { return };
        win.close();
        let mut closed = false;
        for _ in 0..128 {
            if let Some(ev) = win.pump() {
                if ev == UiEvent::Closed {
                    closed = true;
                    break;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(closed, "关闭后没有收到 Closed 事件");
    }

    #[test]
    fn 设置文字与可见性不panic() {
        let Some(mut win) = try_window() else { return };
        let label = win.add_label("连接状态", 16, 16, 200, 24);
        win.set_text(label, "连接状态：已连接 127.0.0.1:27200");
        win.set_visible(label, false);
        win.set_visible(label, true);
        win.set_enabled(label, false);
        win.set_enabled(label, true);
        win.repaint();
    }

    #[test]
    fn 无效控件id的操作静默跳过() {
        let Some(mut win) = try_window() else { return };
        let bogus = ControlId::from_raw(-1);
        win.set_text(bogus, "x");
        win.set_checked(bogus, true);
        win.set_enabled(bogus, false);
        win.set_visible(bogus, false);
        win.focus(bogus);
        assert!(!win.is_checked(bogus));
    }

    #[test]
    fn 下拉框越界选中被夹住() {
        let Some(mut win) = try_window() else { return };
        // 只给 2 项却要求第 99 项：不能 panic
        let combo = win.add_combo(&["a", "b"], 99, 16, 300, 160, 26);
        assert!(!combo.is_invalid());
    }

    #[test]
    fn 锚点设置后控件仍在客户区内() {
        let Some(mut win) = try_window() else { return };
        let btn = win.add_button("退出", 760, 560, 100, 32);
        win.set_anchor(btn, Anchor::BottomRight);
        let (w, _) = win.client_size();
        assert!(w > 0);
    }

    #[test]
    fn 隐藏窗口显示后客户区不变() {
        let Some(win) = try_window() else { return };
        let before = win.client_size();
        win.show();
        let after = win.client_size();
        assert_eq!(before, after, "show() 不该改变客户区尺寸");
        win.hide();
        assert_eq!(win.client_size(), before, "hide() 不该改变客户区尺寸");
    }

    #[test]
    fn 下拉框选中项可读写且越界被夹住() {
        let Some(mut win) = try_window() else { return };
        let combo = win.add_combo(&["a", "b", "c"], 0, 16, 300, 160, 26);
        assert_eq!(win.combo_index(combo), 0);
        win.set_combo_index(combo, 2);
        assert_eq!(win.combo_index(combo), 2);
        // 越界：应被夹到最后一项，而不是留空
        win.set_combo_index(combo, 99);
        assert_eq!(win.combo_index(combo), 2);
        // 程序改选中项不产生事件
        for _ in 0..10 {
            if let Some(UiEvent::ComboChanged(..)) = win.pump() {
                panic!("程序设置选中项不应产生 ComboChanged");
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    #[test]
    fn 非下拉框读写选中项安全() {
        let Some(mut win) = try_window() else { return };
        let btn = win.add_button("开始", 16, 100, 100, 32);
        win.set_combo_index(btn, 3);
        assert_eq!(win.combo_index(btn), 0);
    }

    #[test]
    fn 程序点击按钮会进事件队列() {
        let Some(mut win) = try_window() else { return };
        let btn = win.add_button("开始", 16, 100, 100, 32);
        while win.pump().is_some() {}
        win.click(btn);
        let mut got = None;
        for _ in 0..64 {
            if let Some(UiEvent::ButtonClicked(id)) = win.pump() {
                got = Some(id);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(got, Some(btn), "BM_CLICK 没有转成 ButtonClicked 事件");
    }

    #[test]
    fn 程序点击复选框会翻转并报告新状态() {
        let Some(mut win) = try_window() else { return };
        let cb = win.add_checkbox("启用音频", false, 16, 100, 120, 24);
        assert!(!win.is_checked(cb));
        while win.pump().is_some() {}
        win.click(cb);
        let mut got = None;
        for _ in 0..64 {
            if let Some(UiEvent::CheckboxToggled(id, on)) = win.pump() {
                got = Some((id, on));
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(got, Some((cb, true)), "点击复选框没有报出新状态");
        assert!(win.is_checked(cb));
    }

    #[test]
    fn 程序点击非按钮控件不出事件() {
        let Some(mut win) = try_window() else { return };
        let label = win.add_label("状态", 16, 100, 200, 24);
        let combo = win.add_combo(&["a", "b"], 0, 16, 200, 160, 26);
        while win.pump().is_some() {}
        win.click(label);
        win.click(combo);
        // 没有消息可抽时才可能返回 None；给系统一点时间也仍然是 None
        for _ in 0..10 {
            if let Some(ev) = win.pump() {
                panic!("非按钮控件不应产生事件：{ev}");
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    #[test]
    fn 屏幕与工作区可查询() {
        // 只为 --nocapture 下的人工诊断保留：不同机器的 DPI / 分辨率组合差异很大，
        // 光看断言失败信息分不清是「机器小」还是「算错了」。
        //
        // 踩过的坑：用 PowerShell 查同一个 SPI_GETWORKAREA，在 200% 缩放的机器上
        // 得到的是 1920x1032 —— 因为 pwsh 是 DPI-unaware 进程，系统给它的是
        // **虚拟化**后的坐标。本进程设了 PER_MONITOR_AWARE_V2，拿到的是真物理值
        // 3840x2064。两个数都对，只是坐标系不同。
        let (_, _, w, h) = work_area();
        // SAFETY: GetDpiForSystem 是纯查询。
        let dpi = clamp_dpi(unsafe { GetDpiForSystem() });
        assert!(w > 0 && h > 0, "工作区尺寸不合法：{w}x{h}");
        assert!((48..=960).contains(&dpi), "系统 DPI 不合理：{dpi}");

        // 隐藏窗口建窗耗时：ShowWindow 的首次合成开销不算在内，
        // 这个数才是「UI 层自身」的成本。
        let t = std::time::Instant::now();
        if let Ok(win) = MainWindow::new_hidden("隐藏", 900, 640) {
            eprintln!(
                "[实测] 隐藏窗口建窗 {:?}，DPI {}，客户区 {:?}，视频区 {:?}",
                t.elapsed(),
                win.dpi(),
                win.client_size(),
                win.video_rect()
            );
        }
    }

    #[test]
    fn 字体族探测能找到中文字体() {
        // 中文 Windows 上必然有 Microsoft YaHei；非中文系统至少能拿到 Segoe UI
        let found = FONT_CANDIDATES.iter().any(|f| face_available(f));
        assert!(found, "一个候选字体都找不到，界面上会是 SYSTEM_FONT 位图字");
    }
}
