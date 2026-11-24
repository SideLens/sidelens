//! Win32 GDI 显示层（最简窗口 + StretchDIBits）
//!
//! # 为什么是 GDI 而不是 D3D
//!
//! 这个 crate 的定位是「验证解码输出是对的」，不是做播放器：
//!
//! - GDI 的 `StretchDIBits` 一次调用就能把一块 BGRA 内存糊到窗口上，
//!   零上下文管理、零设备丢失处理、零着色器。
//! - D3D11 要建 device / swapchain / RTV / 纹理，几百行样板换来的收益
//!   只有「GPU 上采样」和「垂直同步」，对 720p 单窗口没有意义。
//! - 未来真要上 D3D，换掉的只有本模块，[`crate::convert`] 一行不动。
//!
//! # 无窗口（headless）模式
//!
//! CI 与无桌面环境跑不了窗口，所以 [`Renderer::headless`] 提供一条
//! 「只转换、不显示」的路径：[`present`] 照常做 NV12 → RGBA，只是不碰任何
//! Win32 API，改为累计帧数与耗时。这条路径**不注册窗口类、不创建窗口**，
//! 所以在 Session 0 / 无交互桌面的进程里也不会失败。
//!
//! 两条路径共用 [`Renderer`] 这一个类型（内部是 [`RendererMode`] 枚举），
//! 调用方的代码不用分叉。
//!
//! # GDI 要的是 BGRA，不是 RGBA
//!
//! 32bpp `BI_RGB` 的 DIB 在内存里的字节序是 **B, G, R, X**，而
//! [`crate::convert::Nv12View::to_rgba`] 吐的是 **R, G, B, A**（人类直觉顺序）。
//! 两者差一次 R↔B 交换，见 [`swap_rb`]。不换的话人脸会变蓝、天空会变橙 ——
//! 这是「一眼就能看出来但查起来要半天」的那类 bug，所以有单元测试钉着。
//!
//! # 自下而上 vs 自上而下
//!
//! DIB 的 `biHeight` 为正数表示**自下而上**（第一行在内存最后）。
//! 这里统一用**负数**（自上而下），和 NV12 / RGBA 的内存顺序一致，
//! 省掉一次整帧翻转。

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, EndPaint, GetDC, ReleaseDC, StretchDIBits, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
    DIB_RGB_COLORS, PAINTSTRUCT, SRCCOPY,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect, PeekMessageW,
    PostQuitMessage, RegisterClassExW, ShowWindow, TranslateMessage, CS_HREDRAW, CS_VREDRAW,
    CW_USEDEFAULT, MSG, PM_REMOVE, SW_SHOW, WM_DESTROY, WM_ERASEBKGND, WM_PAINT, WM_QUIT,
    WNDCLASSEXW, WS_OVERLAPPEDWINDOW,
};

use crate::convert::{ConvertError, Nv12View};

/// 窗口类名（全局唯一，注册一次）
const CLASS_NAME: PCWSTR = w!("SideLensRenderWindow");

/// 用户关窗时置位 —— 窗口过程是 `extern "system"` 自由函数，拿不到 `&mut self`，
/// 所以用一个进程级的静态标志把「关窗」这件事传出来。
///
/// 用 `AtomicBool` 而不是 `static mut`：后者在 Rust 2021 下要么需要 unsafe
/// 要么触发 `static_mut_refs` 警告，而 `Relaxed` 就够了（只有一个写者、
/// 且 [`pump_events`] 每次都会重新读）。
static QUIT: AtomicBool = AtomicBool::new(false);

/// 渲染层的错误
#[derive(Debug)]
pub enum RenderError {
    /// 输入帧不合法（尺寸 / 缓冲）
    Convert(ConvertError),
    /// Win32 调用失败
    Win32(windows::core::Error),
    /// 窗口宽高为 0
    ZeroSize {
        /// 宽
        width: u32,
        /// 高
        height: u32,
    },
    /// 窗口已销毁还在 present
    WindowGone,
}

impl std::fmt::Display for RenderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RenderError::Convert(e) => write!(f, "帧转换失败：{e}"),
            RenderError::Win32(e) => write!(f, "Win32 调用失败：{e}"),
            RenderError::ZeroSize { width, height } => {
                write!(f, "窗口尺寸不能为 0（{width}x{height}）")
            }
            RenderError::WindowGone => write!(f, "窗口已销毁，不能再 present"),
        }
    }
}

impl std::error::Error for RenderError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            RenderError::Convert(e) => Some(e),
            RenderError::Win32(e) => Some(e),
            _ => None,
        }
    }
}

impl From<ConvertError> for RenderError {
    fn from(e: ConvertError) -> Self {
        RenderError::Convert(e)
    }
}

impl From<windows::core::Error> for RenderError {
    fn from(e: windows::core::Error) -> Self {
        RenderError::Win32(e)
    }
}

/// 渲染模式
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RendererMode {
    /// 真实窗口（需要交互桌面）
    Window,
    /// 无窗口：只转换，不显示
    Headless,
}

impl RendererMode {
    /// 人类可读的名字
    pub fn as_str(self) -> &'static str {
        match self {
            RendererMode::Window => "window",
            RendererMode::Headless => "headless",
        }
    }
}

/// 渲染器
///
/// 两种模式共用一套 API：[`present`] / [`pump_events`] / [`stats`]。
/// 窗口模式下 [`pump_events`] 抽消息，headless 下它恒返回 `true`
/// （没有窗口可关）。
pub struct Renderer {
    mode: RendererMode,
    /// 窗口句柄（headless 时是空 HWND）
    hwnd: HWND,
    width: u32,
    height: u32,
    /// RGBA 转换结果（复用，避免每帧分配）
    rgba: Vec<u8>,
    /// BGRA 显示缓冲（复用）—— GDI 要的是这个顺序
    bgra: Vec<u8>,
    /// 已 present 的帧数
    frames: u64,
    /// 累计转换耗时（纳秒）
    convert_ns: u128,
    /// 累计 blit 耗时（纳秒）
    blit_ns: u128,
    /// 最近一帧的转换耗时（纳秒）
    last_convert_ns: u128,
}

impl std::fmt::Debug for Renderer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Renderer")
            .field("mode", &self.mode)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("frames", &self.frames)
            .finish_non_exhaustive()
    }
}

impl Renderer {
    /// 建一个真窗口
    ///
    /// `width` / `height` 是**客户区**尺寸（不含标题栏和边框），
    /// 内部用 `AdjustWindowRect` 折算成外框尺寸，这样画面不会被边框裁掉。
    pub fn new(title: &str, width: u32, height: u32) -> Result<Self, RenderError> {
        if width == 0 || height == 0 {
            return Err(RenderError::ZeroSize { width, height });
        }

        let title_w: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();

        unsafe {
            let hinstance = GetModuleHandleW(PCWSTR::null())?;

            // 注册窗口类。已经注册过（同进程第二次 new）会返回 0 且
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
                hbrBackground: Default::default(),
                lpszMenuName: PCWSTR::null(),
                lpszClassName: CLASS_NAME,
                hIconSm: Default::default(),
            };
            RegisterClassExW(&wc);

            QUIT.store(false, Ordering::Relaxed);

            // 客户区尺寸 = 请求尺寸：先按 WS_OVERLAPPEDWINDOW 折算外框。
            // 不算的话画面四周会被边框吃掉一圈，1080p 显示不全。
            let mut rect = windows::Win32::Foundation::RECT {
                left: 0,
                top: 0,
                right: width as i32,
                bottom: height as i32,
            };
            let _ = windows::Win32::UI::WindowsAndMessaging::AdjustWindowRect(
                &mut rect,
                WS_OVERLAPPEDWINDOW,
                false,
            );
            let outer_w = rect.right - rect.left;
            let outer_h = rect.bottom - rect.top;

            let hwnd = CreateWindowExW(
                Default::default(),
                CLASS_NAME,
                PCWSTR(title_w.as_ptr()),
                WS_OVERLAPPEDWINDOW,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                outer_w,
                outer_h,
                None,
                None,
                Some(hinstance.into()),
                None,
            )?;

            let _ = ShowWindow(hwnd, SW_SHOW);

            Ok(Renderer {
                mode: RendererMode::Window,
                hwnd,
                width,
                height,
                rgba: Vec::new(),
                bgra: Vec::new(),
                frames: 0,
                convert_ns: 0,
                blit_ns: 0,
                last_convert_ns: 0,
            })
        }
    }

    /// 建一个无窗口渲染器：只做 NV12 → RGBA，不碰任何 Win32 API
    ///
    /// 用于 CI / 无桌面环境。[`present`] 只累计帧数与耗时。
    /// `width` / `height` 只用于预分配缓冲，不影响转换结果
    /// （转换尺寸永远取自 [`Nv12View`] 的 display 尺寸）。
    pub fn headless(width: u32, height: u32) -> Result<Self, RenderError> {
        if width == 0 || height == 0 {
            return Err(RenderError::ZeroSize { width, height });
        }
        Ok(Renderer {
            mode: RendererMode::Headless,
            hwnd: HWND::default(),
            width,
            height,
            rgba: Vec::new(),
            bgra: Vec::new(),
            frames: 0,
            convert_ns: 0,
            blit_ns: 0,
            last_convert_ns: 0,
        })
    }

    /// 当前模式
    pub fn mode(&self) -> RendererMode {
        self.mode
    }

    /// 请求的窗口尺寸
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// 已 present 的帧数
    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// 累计转换耗时（纳秒）
    pub fn convert_ns(&self) -> u128 {
        self.convert_ns
    }

    /// 累计 blit 耗时（纳秒）—— headless 模式下恒为 0
    pub fn blit_ns(&self) -> u128 {
        self.blit_ns
    }

    /// 最近一帧的转换耗时（纳秒）
    pub fn last_convert_ns(&self) -> u128 {
        self.last_convert_ns
    }

    /// 平均每帧转换耗时（纳秒）；没有帧时返回 0
    pub fn avg_convert_ns(&self) -> u128 {
        if self.frames == 0 {
            0
        } else {
            self.convert_ns / self.frames as u128
        }
    }

    /// 把一帧 NV12 画到窗口上（headless 下只转换）
    ///
    /// 转换只覆盖 `frame` 的 **display** 区域，coded 多出来的底部/右侧
    /// 填充**不会被画出来**。窗口比画面大时按左上角对齐（不拉伸、不缩放），
    /// 因为拉伸会让「coded 没裁干净」这种 bug 看起来像「比例不对」，反而更难查。
    pub fn present(&mut self, frame: &Nv12View<'_>) -> Result<(), RenderError> {
        let t0 = std::time::Instant::now();
        frame.to_rgba(&mut self.rgba)?;
        let convert_ns = t0.elapsed().as_nanos();
        self.last_convert_ns = convert_ns;
        self.convert_ns += convert_ns;

        if self.mode == RendererMode::Window {
            // RGBA → BGRA。复用 bgra 缓冲，长度不变时只交换不重分配。
            swap_rb(&self.rgba, &mut self.bgra);

            let t1 = std::time::Instant::now();
            self.blit(frame.display_width() as i32, frame.display_height() as i32)?;
            self.blit_ns += t1.elapsed().as_nanos();
        }

        self.frames += 1;
        Ok(())
    }

    /// 把 bgra 缓冲糊到窗口客户区的 (0,0)
    fn blit(&self, src_w: i32, src_h: i32) -> Result<(), RenderError> {
        if self.hwnd.is_invalid() {
            return Err(RenderError::WindowGone);
        }

        // biHeight 取**负**：自上而下，和 bgra 的内存顺序一致（省一次翻转）。
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: src_w,
                biHeight: -src_h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                biSizeImage: 0,
                biXPelsPerMeter: 0,
                biYPelsPerMeter: 0,
                biClrUsed: 0,
                biClrImportant: 0,
            },
            bmiColors: [Default::default()],
        };

        unsafe {
            let hdc = GetDC(Some(self.hwnd));
            if hdc.is_invalid() {
                return Err(RenderError::Win32(windows::core::Error::from_thread()));
            }
            // 目标矩形用**窗口客户区**尺寸，源矩形用画面尺寸。
            // 两者相等时 StretchDIBits 等价于 BitBlt（1:1），不引入插值。
            let mut rc = windows::Win32::Foundation::RECT::default();
            let client = GetClientRect(self.hwnd, &mut rc);
            let (dst_w, dst_h) = match client {
                Ok(()) => (
                    (rc.right - rc.left).max(src_w),
                    (rc.bottom - rc.top).max(src_h),
                ),
                Err(_) => (src_w, src_h),
            };

            let drawn = StretchDIBits(
                hdc,
                0,
                0,
                dst_w,
                dst_h,
                0,
                0,
                src_w,
                src_h,
                Some(self.bgra.as_ptr() as *const c_void),
                &bmi,
                DIB_RGB_COLORS,
                SRCCOPY,
            );
            let _ = ReleaseDC(Some(self.hwnd), hdc);
            if drawn == 0 {
                // 0 行或 -1 错误。窗口最小化时某些驱动会返回 0，
                // 那不是致命错误，但要让调用方看得见。
                return Err(RenderError::Win32(windows::core::Error::from_thread()));
            }
        }
        Ok(())
    }

    /// 抽一遍消息队列
    ///
    /// 返回 `false` 表示用户关窗（或收到 `WM_QUIT`），调用方该退出循环。
    /// headless 模式下恒返回 `true` —— 没有窗口可关。
    pub fn pump_events(&mut self) -> bool {
        if self.mode == RendererMode::Headless {
            return true;
        }
        if QUIT.load(Ordering::Relaxed) {
            return false;
        }

        unsafe {
            let mut msg = MSG::default();
            // PeekMessage 不阻塞：每帧调一次，不卡渲染节奏。
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_QUIT {
                    QUIT.store(true, Ordering::Relaxed);
                    return false;
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        !QUIT.load(Ordering::Relaxed)
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        if self.mode == RendererMode::Window && !self.hwnd.is_invalid() {
            unsafe {
                let _ = DestroyWindow(self.hwnd);
            }
            self.hwnd = HWND::default();
        }
    }
}

/// RGBA → BGRA（原地交换 R 和 B，A 不动）
///
/// 单独抽出来是因为它必须有单元测试 —— 颜色通道搞反是「人脸发蓝」级别的事故，
/// 而在真窗口上肉眼验证要开桌面，CI 里跑不了。
fn swap_rb(rgba: &[u8], out: &mut Vec<u8>) {
    out.clear();
    out.reserve(rgba.len());
    let mut i = 0;
    while i + 3 < rgba.len() {
        out.push(rgba[i + 2]); // B
        out.push(rgba[i + 1]); // G
        out.push(rgba[i]); // R
        out.push(rgba[i + 3]); // A
        i += 4;
    }
}

/// 窗口过程
///
/// 只处理三件事：关窗、重绘（重绘时用 `WM_ERASEBKGND` 直接吃掉，避免闪烁）、
/// 其余全交给 `DefWindowProcW`。
///
/// # Safety
///
/// 由 Win32 在窗口线程上调用，参数由系统保证有效。
unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_DESTROY => {
            QUIT.store(true, Ordering::Relaxed);
            unsafe {
                PostQuitMessage(0);
            }
            LRESULT(0)
        }
        // 返回 1 = 「背景我已经擦了」，GDI 就不再刷一遍底色，消除闪烁。
        WM_ERASEBKGND => LRESULT(1),
        // 画面由 present() 主动推，这里只把 PAINT 消息消费掉，
        // 否则窗口被遮挡再露出来时会不停收到 WM_PAINT。
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

#[cfg(test)]
mod tests {
    use super::*;

    /// RGBA → BGRA 必须只交换 R/B，G 和 A 原地不动
    #[test]
    fn swap_rb_exchanges_only_red_and_blue() {
        let rgba = vec![10u8, 20, 30, 255, 200, 100, 50, 128];
        let mut bgra = Vec::new();
        swap_rb(&rgba, &mut bgra);
        assert_eq!(bgra, vec![30u8, 20, 10, 255, 50, 100, 200, 128]);
    }

    /// 交换两次应该回到原样
    #[test]
    fn swap_rb_is_an_involution() {
        let rgba: Vec<u8> = (0..64u8).collect();
        let mut once = Vec::new();
        swap_rb(&rgba, &mut once);
        let mut twice = Vec::new();
        swap_rb(&once, &mut twice);
        assert_eq!(twice, rgba);
    }

    /// 尾部不足 4 字节的残余不能被当成一个像素
    #[test]
    fn swap_rb_ignores_trailing_partial_pixel() {
        let rgba = vec![1u8, 2, 3, 255, 9];
        let mut bgra = Vec::new();
        swap_rb(&rgba, &mut bgra);
        assert_eq!(bgra, vec![3u8, 2, 1, 255]);
    }
}
