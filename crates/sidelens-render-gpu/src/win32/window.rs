//! 最简 Win32 窗口 + 消息泵
//!
//! 和 `sidelens-render` 的 `win32.rs` 保持同构：同样的窗口类名策略、
//! 同样的 `pump_events` 语义、同样只处理 WM_DESTROY / WM_PAINT / WM_ERASEBKGND。
//! 主控要把两条渲染路径按同一个模式接线，行为不一致会立刻变成 bug。
//!
//! # 为什么关窗标志挂在 `GWLP_USERDATA` 上，而不是进程级静态
//!
//! `sidelens-render` 用一个 `static QUIT: AtomicBool` 把「用户关窗」传给
//! 调用方 —— 窗口过程是 `extern "system"` 自由函数，签名里没有 `self`。
//! 那个做法在「进程里只有一个窗口」时是对的，但**同一进程里有两个渲染器**
//! 时会立刻出错：A 窗口关闭把全局标志置位，B 窗口下一帧就以为自己也被关了。
//!
//! 这不是假想场景：`cargo test` 默认多线程跑，两个窗口测试并行时就互相踩；
//! 主控的降级逻辑（GPU 失败建 GDI）也可能让两个窗口短暂共存。
//!
//! 所以这里改成标准做法：把 `Arc<AtomicBool>` 的裸指针塞进
//! `GWLP_USERDATA`，窗口过程里取回来。每个窗口有自己的标志，互不干扰。
//! `Arc` 保证指针在窗口存活期间有效（`Window` 持有克隆）。
//!
//! # 为什么不用 WM_PAINT 重绘
//!
//! 画面完全由 `present()` 主动推。窗口被遮挡再露出来时系统会发
//! `WM_PAINT`，我们只把消息消费掉（`BeginPaint`/`EndPaint` 配对），
//! 不重绘 —— 因为 flip-model 的后缓冲内容在被 Present 之前是**未定义**的，
//! 用 WM_PAINT 重绘只会闪一下。真正的恢复靠下一帧 Present。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{BeginPaint, EndPaint, PAINTSTRUCT};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRect, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GetClientRect, GetWindowLongPtrW, MoveWindow, PeekMessageW, PostQuitMessage, RegisterClassExW,
    SetParent, SetWindowLongPtrW, ShowWindow, TranslateMessage, CS_HREDRAW, CS_VREDRAW,
    CW_USEDEFAULT, GWLP_USERDATA, MSG, PM_REMOVE, SW_SHOW, WM_DESTROY, WM_ERASEBKGND, WM_PAINT,
    WM_QUIT, WNDCLASSEXW, WS_CLIPCHILDREN, WS_CLIPSIBLINGS, WS_OVERLAPPEDWINDOW, WS_POPUP,
};

use crate::error::GpuError;

/// 窗口类名（全局唯一，注册一次）
const CLASS_NAME: PCWSTR = windows::core::w!("SideLensGpuRenderWindow");

/// 一个最小的 Win32 窗口
pub struct Window {
    hwnd: HWND,
    width: u32,
    height: u32,
    /// 子窗口模式下的 `(父句柄, x, y)`；顶层窗口为 `None`
    ///
    /// 设备丢失后重建时要还原同样的挂载位置 —— 否则重建出来的窗口
    /// 会变成独立的顶层窗口，盖住 UI 面板。
    child: Option<(isize, i32, i32)>,
    /// 用户关窗标志
    ///
    /// 每个窗口一个，通过 `GWLP_USERDATA` 传给窗口过程 —— 见模块文档
    /// 「为什么关窗标志挂在 GWLP_USERDATA 上」。
    quit: Arc<AtomicBool>,
}

impl std::fmt::Debug for Window {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Window")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("quit", &self.quit.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl Window {
    /// 创建并显示窗口
    ///
    /// `width` / `height` 是**客户区**尺寸（不含标题栏和边框），
    /// 内部用 `AdjustWindowRect` 折算成外框尺寸 —— 不算的话画面四周会被
    /// 边框吃掉一圈，1080p 显示不全。
    pub fn new(title: &str, width: u32, height: u32) -> Result<Self, GpuError> {
        Self::build(None, title, 0, 0, width, height)
    }

    /// 创建**子窗口**：嵌进 `parent` 的客户区 `(x, y)` 处
    ///
    /// # 为什么需要子窗口模式（第五十四轮）
    ///
    /// 第五十三轮接入原生 UI 后暴露一个问题：GPU 渲染器建的是
    /// `WS_OVERLAPPEDWINDOW` 顶层窗口，会**盖住**状态面板的标签和按钮。
    ///
    /// 面板已经算好了视频区矩形（实测 200% DPI 下是 `(32, 192, 1736, 928)`），
    /// 但渲染层没有「嵌进去」的能力 —— 只能开一个独立窗口。
    ///
    /// 子窗口模式的三个关键点：
    ///
    /// 1. **`WS_CHILD` 而不是 `WS_OVERLAPPEDWINDOW`**：子窗口没有标题栏、
    ///    不能独立移动、坐标相对父窗口客户区
    /// 2. **`WS_CLIPCHILDREN` 要设在父窗口上**（不是这里）：否则父窗口
    ///    重绘时会盖住子窗口。这一条由调用方（`sidelens-ui`）负责
    /// 3. **`AdjustWindowRect` 不能用**：子窗口没有边框，客户区尺寸就是
    ///    给定尺寸，折算反而会缩水
    ///
    /// # 坐标系
    ///
    /// `x` / `y` / `width` / `height` 全是**父窗口客户区的物理像素**。
    /// `sidelens-ui` 的 `video_rect()` 正好给这个口径，可以直接用。
    pub fn new_child(
        parent: isize,
        title: &str,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
    ) -> Result<Self, GpuError> {
        if parent == 0 {
            return Err(GpuError::Window("new_child 的 parent 句柄不能为 0".into()));
        }
        Self::build(Some((parent, x, y)), title, x, y, width, height)
    }

    /// 统一的构建路径
    ///
    /// `child` 为 `Some((parent_hwnd, x, y))` 时建子窗口，否则建顶层窗口。
    /// 两条路径共享 90% 的代码（注册类、清残留 WM_QUIT、挂 quit 指针），
    /// 分开写会让「清残留 WM_QUIT」这种关键逻辑只在一处生效 ——
    /// 而那个坑（见上方注释）一旦复发，表现是「新窗口第一轮 pump 就退出」。
    fn build(
        child: Option<(isize, i32, i32)>,
        title: &str,
        _x: i32,
        _y: i32,
        width: u32,
        height: u32,
    ) -> Result<Self, GpuError> {
        if width == 0 || height == 0 {
            return Err(GpuError::ZeroSize { width, height });
        }

        let title_w: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
        let quit = Arc::new(AtomicBool::new(false));

        unsafe {
            let hinstance = GetModuleHandleW(PCWSTR::null())
                .map_err(|e| GpuError::Window(format!("GetModuleHandleW：{}", e.message())))?;

            // 注册窗口类。同进程第二次 new 会返回 0 且
            // GetLastError == ERROR_CLASS_ALREADY_EXISTS —— 这不是错误，
            // 所以不检查返回值，只把结构体填对。
            let wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(wndproc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: hinstance.into(),
                hIcon: Default::default(),
                hCursor: Default::default(),
                // 不设背景刷：背景由 swap chain 填满，设了反而多一次填充
                hbrBackground: Default::default(),
                lpszMenuName: PCWSTR::null(),
                lpszClassName: CLASS_NAME,
                hIconSm: Default::default(),
            };
            RegisterClassExW(&wc);

            // 清掉队列里**残留的 WM_QUIT**。
            //
            // 踩过的坑：任何 Win32 窗口被 DestroyWindow 时，它的窗口过程
            // 通常会调 PostQuitMessage(0)，把一条 WM_QUIT 塞进**线程**队列
            // （不是窗口队列）。于是「先建 GDI 渲染器、销毁、再建 GPU 渲染器」
            // 这种顺序下，新窗口的第一次 pump_events 就会读到那条陈旧的
            // WM_QUIT 并立刻退出 —— 表现为「GPU 窗口一闪而过、0 帧」。
            //
            // 实测：`examples/bench_present.rs` 先跑 GDI 再跑 GPU，GPU 侧
            // 报「0 帧」。这条 drain 修掉了它。
            // # 为什么只丢 WM_QUIT，其它消息必须 Dispatch
            //
            // 第一版是「`PeekMessageW(None, ...)` 把队列里所有消息全丢掉」。
            // 在**独立进程**里没问题（那时队列确实只有别人的垃圾）。
            //
            // 但第五十四轮接进 daemon 后，同一个线程上**已经有一个 UI 面板**
            // 在跑。它的 `WM_PAINT` / `WM_SIZE` 等消息被这里吃掉且没有
            // `DispatchMessageW`，于是面板的窗口过程永远收不到它们 ——
            // 表现为整个进程卡死（不是崩溃，是消息永远不派发）。
            //
            // 实测：daemon 打到「GPU 渲染器嵌入面板视频区」后就再无输出。
            //
            // 正确做法：只丢弃 `WM_QUIT`，其余消息照常派发。
            {
                let mut stale = MSG::default();
                while PeekMessageW(&mut stale, None, 0, 0, PM_REMOVE).as_bool() {
                    if stale.message == WM_QUIT {
                        // 陈旧退出信号，丢弃
                        continue;
                    }
                    // 别人的消息：必须派发，否则那个窗口永远收不到
                    let _ = TranslateMessage(&stale);
                    DispatchMessageW(&stale);
                }
            }

            // 顶层窗口要折算外框（否则画面四周被边框吃掉一圈）；
            // 子窗口没有边框，客户区尺寸就是给定尺寸，不能折算。
            let (style, pos_x, pos_y, out_w, out_h, parent) = match child {
                Some((_ph, cx, cy)) => (
                    // # 为什么先建 WS_POPUP 而不是 WS_CHILD
                    //
                    // `CreateSwapChainForHwnd` 在**真正的 WS_CHILD 窗口**上会
                    // 同步给窗口过程发消息并等待处理完 —— 而窗口过程跑在本线程，
                    // 本线程正卡在那个调用里，于是**永久死锁**。
                    //
                    // 实测症状：daemon 打到「GPU 渲染器嵌入面板视频区」后就再无
                    // 输出，进程活着但不干活（不是崩溃）。加 `pump_once` 也救不了，
                    // 因为消息是在 CreateSwapChainForHwnd **内部**才发出的。
                    //
                    // 所以先建成无边框顶层窗口（`WS_POPUP`），让 swap chain 正常创建，
                    // 再由 `attach_to_parent` 用 `SetParent` 挂到面板上。
                    // `SetParent` 之后窗口变成子窗口，样式由系统自动加 `WS_CHILD`。
                    WS_POPUP | WS_CLIPSIBLINGS,
                    cx,
                    cy,
                    width as i32,
                    height as i32,
                    None,
                ),
                None => {
                    let mut rect = RECT {
                        left: 0,
                        top: 0,
                        right: width as i32,
                        bottom: height as i32,
                    };
                    let _ = AdjustWindowRect(&mut rect, WS_OVERLAPPEDWINDOW, false);
                    (
                        WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
                        CW_USEDEFAULT,
                        CW_USEDEFAULT,
                        rect.right - rect.left,
                        rect.bottom - rect.top,
                        None,
                    )
                }
            };

            let hwnd = CreateWindowExW(
                Default::default(),
                CLASS_NAME,
                PCWSTR(title_w.as_ptr()),
                style,
                pos_x,
                pos_y,
                out_w,
                out_h,
                parent,
                None,
                Some(hinstance.into()),
                None,
            )
            .map_err(|e| GpuError::Window(format!("CreateWindowExW：{}", e.message())))?;

            // 把 quit 的裸指针交给窗口过程。
            // Arc::into_raw 增加引用计数，Window 的 Drop 里用 from_raw 收回，
            // 所以指针在窗口整个生命周期内都有效。
            let raw = Arc::into_raw(Arc::clone(&quit));
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, raw as isize);

            // 子窗口已带 `WS_VISIBLE`，再 ShowWindow 是多余的；
            // 顶层窗口必须显式显示（CreateWindowExW 默认不显示）。
            if child.is_none() {
                let _ = ShowWindow(hwnd, SW_SHOW);
            }

            Ok(Window {
                hwnd,
                width,
                height,
                child,
                quit,
            })
        }
    }

    /// 把本窗口挂到 `parent` 的客户区 `(x, y)` 处，变成子窗口
    ///
    /// # 为什么必须延后到 swap chain 建好之后
    ///
    /// `CreateSwapChainForHwnd` 在真 `WS_CHILD` 窗口上会同步发消息并等
    /// 处理完，而窗口过程跑在本线程 —— 死锁。所以创建时用 `WS_POPUP`，
    /// 建完 swap chain 再挂上去。
    ///
    /// # `SetParent` 会自动补 `WS_CHILD`
    ///
    /// 系统在 `SetParent` 时给子窗口加上 `WS_CHILD`、去掉 `WS_POPUP`，
    /// 不需要手动 `SetWindowLong` 改样式（手动改反而容易漏掉配套位）。
    pub fn attach_to_parent(&self, parent: isize, x: i32, y: i32) {
        if parent == 0 {
            return;
        }
        // SAFETY: hwnd 有效；parent 由调用方保证是有效窗口句柄。
        unsafe {
            let _ = SetParent(self.hwnd, Some(HWND(parent as *mut std::ffi::c_void)));
            let _ = MoveWindow(self.hwnd, x, y, self.width as i32, self.height as i32, true);
            let _ = ShowWindow(self.hwnd, SW_SHOW);
        }
    }

    /// 把队列里**当前已排**的消息全部处理掉（非阻塞）
    ///
    /// # 为什么需要它
    ///
    /// `CreateSwapChainForHwnd` 在子窗口上会同步给窗口过程发消息并等待
    /// 处理完。如果调用点没有消息泵（比如建窗口的同一段代码里紧接着
    /// 建 swap chain），那个等待会**永久阻塞** —— 窗口过程跑在本线程，
    /// 而本线程正卡在 CreateSwapChainForHwnd 里。
    ///
    /// 在创建 swap chain 之前调一次，让子窗口完成首次布局。
    ///
    /// # 为什么用 PeekMessageW 而不是 GetMessageW
    ///
    /// `GetMessageW` 在没有消息时会阻塞，这里必须立刻返回 ——
    /// 调用方是在启动流程里，不是消息循环。
    pub fn pump_once(&self) {
        // SAFETY: 标准 PeekMessageW 循环；MSG 是栈上值，生命周期覆盖单次迭代。
        unsafe {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_QUIT {
                    continue;
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }

    /// 子窗口的挂载位置 `(父句柄, x, y)`；顶层窗口返回 `None`
    ///
    /// 给设备丢失后的重建用：要还原同样的挂载点，否则新窗口会变成
    /// 独立顶层窗口。
    pub fn child_placement(&self) -> Option<(isize, i32, i32)> {
        self.child
    }

    /// 窗口句柄
    pub fn hwnd(&self) -> HWND {
        self.hwnd
    }

    /// 用户是否已经关窗
    pub fn is_closed(&self) -> bool {
        self.quit.load(Ordering::Relaxed)
    }

    /// 客户区尺寸（窗口被拖动改变大小时会变）
    pub fn client_size(&self) -> (u32, u32) {
        let mut rc = RECT::default();
        let ok = unsafe { GetClientRect(self.hwnd, &mut rc) };
        if ok.is_ok() {
            let w = (rc.right - rc.left).max(0) as u32;
            let h = (rc.bottom - rc.top).max(0) as u32;
            if w > 0 && h > 0 {
                return (w, h);
            }
        }
        (self.width, self.height)
    }

    /// 抽一遍消息队列
    ///
    /// 返回 `false` 表示用户关窗（或收到 `WM_QUIT`）。
    /// 用 `PeekMessage` 而不是 `GetMessage`：不阻塞，每帧调一次不卡渲染节奏。
    pub fn pump_events(&self) -> bool {
        if self.quit.load(Ordering::Relaxed) {
            return false;
        }
        unsafe {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_QUIT {
                    self.quit.store(true, Ordering::Relaxed);
                    return false;
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        !self.quit.load(Ordering::Relaxed)
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        if !self.hwnd.is_invalid() {
            unsafe {
                // 先把 USERDATA 清掉，避免 DestroyWindow 期间窗口过程
                // 读到一个即将失效的指针
                SetWindowLongPtrW(self.hwnd, GWLP_USERDATA, 0);
                let _ = DestroyWindow(self.hwnd);
            }
            self.hwnd = HWND::default();
        }
        // 收回 into_raw 的那份引用。
        // 注意：只有在窗口过程不再可能被调用之后才能这么做 ——
        // 上面的 DestroyWindow 已经保证了这一点。
        unsafe {
            drop(Arc::from_raw(Arc::as_ptr(&self.quit)));
        }
    }
}

/// 窗口过程
///
/// 只处理三件事：关窗、吃掉重绘（避免闪烁）、其余交给 `DefWindowProcW`。
///
/// # Safety
///
/// 由 Win32 在窗口线程上调用，参数由系统保证有效。
unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_DESTROY => {
            // 从 GWLP_USERDATA 取回这个窗口自己的 quit 标志
            let raw = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) };
            if raw != 0 {
                // SAFETY: 这个指针是 Window::new 里 Arc::into_raw 给的，
                // 且在 Window::drop 里（DestroyWindow 之后）才收回。
                // 这里只读不写所有权，不增加也不减少引用计数。
                let flag = unsafe { &*(raw as *const AtomicBool) };
                flag.store(true, Ordering::Relaxed);
            }
            unsafe {
                PostQuitMessage(0);
            }
            LRESULT(0)
        }
        // 返回 1 = 「背景我已经擦了」，系统就不再刷一遍底色，消除闪烁
        WM_ERASEBKGND => LRESULT(1),
        // 画面由 present() 主动推，这里只把 PAINT 消费掉
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            unsafe {
                let hdc = BeginPaint(hwnd, &mut ps);
                if !hdc.is_invalid() {
                    let _ = EndPaint(hwnd, &ps);
                }
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}
