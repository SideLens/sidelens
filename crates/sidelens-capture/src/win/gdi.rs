//! GDI BitBlt 采集后端（保底方案）
//!
//! # 为什么 GDI 仍然是必需的
//!
//! DXGI 的 Desktop Duplication 在下列环境**拿不到**：远程桌面、会话 0、
//! 老显卡、输出已被别的进程复制。而 `BitBlt` 从 Win3.1 就有，
//! 任何交互式 Windows 会话都能跑。它是「至少能出图」的底线。
//!
//! # 实现要点
//!
//! 1. 目标设备用 `CreateCompatibleDC` + `CreateDIBSection`（32bpp 顶向下
//!    DIB），**不是** `CreateCompatibleBitmap` —— 后者拿不到像素指针，
//!    还得再 `GetDIBits` 拷一次，等于多一次全帧拷贝。
//! 2. DIB 用**顶向下**（`biHeight = -h`）：否则内存里是倒的，
//!    转换时每行都要算 `(h-1-y)`，多一次乘法和减法。
//! 3. `BitBlt` 的源 DC：显示器用 `GetDC(NULL)`（整个虚拟桌面），
//!    窗口用 `GetWindowDC(hwnd)` + **相对坐标 (0,0)**。
//!    窗口用屏幕坐标会让标题栏偏出去，这个坑很容易踩。
//!
//! # 性能
//!
//! 1080p 单帧的构成大致是：`BitBlt` 8-15 ms（大头，同步 GPU→CPU 读回）+
//! BGRA→NV12 3-6 ms。实测数字见 `examples/bench_capture.rs`。

#![allow(unsafe_code)]

use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC,
    SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ,
    SRCCOPY,
};

use crate::convert::{bgra_to_nv12, FrameLayout};
use crate::error::{CaptureError, Result};
use crate::frame::{CaptureSource, Frame};
use crate::scale::ScalePlan;

/// GDI 采集器
///
/// 持有一个内存 DC + 一块 DIB section。缓冲区在构造时按当时的尺寸分配一次，
/// 之后每帧复用 —— 1080p 一块 8.3 MB，每帧重分配会带来明显的分配器压力。
pub struct GdiCapture {
    /// 源 DC（屏幕或窗口）
    src_dc: HDC,
    /// 是否是自己创建的源 DC（窗口 DC 需要 ReleaseDC，屏幕 DC 也需要）
    src_hwnd: Option<HWND>,
    /// 内存 DC（选入了 dib）
    mem_dc: HDC,
    /// DIB section 的位图句柄
    dib: HBITMAP,
    /// 选入前的旧对象（DeleteDC 前要换回去）
    old_obj: HGDIOBJ,
    /// 像素指针（由 CreateDIBSection 给出，生命周期跟随 dib）
    bits: *mut u8,
    /// **源**尺寸布局（DIB 的尺寸，也就是 BitBlt 的目标区域）
    ///
    /// 与 `out_layout` 分开是缩放的前提：BitBlt 必须按源尺寸抓取
    /// （GDI 不做缩放），缩放发生在之后的 BGRA→NV12 转换里。
    src_layout: FrameLayout,
    /// **输出**尺寸布局（`Frame` 里报告的尺寸）
    ///
    /// 未设置目标尺寸时与 `src_layout` 相同。
    out_layout: FrameLayout,
    /// 缩放计划；`None` 表示源与目标尺寸一致，走无缩放路径
    plan: Option<ScalePlan>,
    /// 目标矩形的左上角（窗口用 0,0，显示器用虚拟桌面坐标）
    origin: (i32, i32),
    /// 源类型
    source: CaptureSource,
}

// SAFETY: GdiCapture 持有的都是 Win32 句柄，句柄本身可以跨线程传递；
// 但**不能并发使用**（BitBlt 到同一个 mem_dc 是数据竞争）。
// 这里手动实现 Send 是为了让调用方能把整个采集器 move 到工作线程 ——
// 这是发送端的基本需求。Sync 不实现，保证同一时刻只有一个线程在用。
unsafe impl Send for GdiCapture {}

impl std::fmt::Debug for GdiCapture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GdiCapture")
            .field(
                "src",
                &format_args!(
                    "{}x{}",
                    self.src_layout.display_width(),
                    self.src_layout.display_height()
                ),
            )
            .field(
                "out",
                &format_args!(
                    "{}x{}",
                    self.out_layout.display_width(),
                    self.out_layout.display_height()
                ),
            )
            .field(
                "coded",
                &format_args!(
                    "{}x{}",
                    self.out_layout.coded_width(),
                    self.out_layout.coded_height()
                ),
            )
            .field("scale", &self.plan.map(|p| p.mode().as_str()))
            .field("source", &self.source)
            .finish()
    }
}

impl GdiCapture {
    /// 创建 GDI 采集器（按显示器原始分辨率输出）
    pub fn new(source: CaptureSource) -> Result<Self> {
        Self::new_sized(source, None)
    }

    /// 创建 GDI 采集器，可指定输出尺寸（缩放）
    ///
    /// # 为什么缩放不交给 GDI 的 `StretchBlt`
    ///
    /// `StretchBlt` 可以做缩放，但它是**在 DC 里**做的：要么先 BitBlt 到
    /// 源尺寸再 StretchBlt 到目标尺寸（多一次全帧拷贝 + 一次 DC 往返），
    /// 要么让 StretchBlt 直接从屏幕 DC 抓（它内部还是走同一条路径）。
    /// 两种都比「BitBlt 原尺寸 → 在 CPU 上缩放并转 NV12」慢，
    /// 而且 `StretchBlt` 的默认滤波（`COLORONCOLOR`）会丢像素。
    ///
    /// 所以这里保持 `src_layout` 是**源**尺寸（DIB 也按源尺寸分配），
    /// 缩放发生在 [`capture`] 里的 BGRA→NV12 那一步 —— 那一步本来就要
    /// 逐像素读源数据，缩放是「顺路」的，不额外多读一遍。
    pub fn new_sized(source: CaptureSource, target: Option<(u32, u32)>) -> Result<Self> {
        if matches!(source, CaptureSource::Synthetic) {
            return Err(CaptureError::UnsupportedSource {
                backend: "GDI",
                reason: "合成源不走平台 API，请用 ScreenCapture::synthetic".into(),
            });
        }
        // 必须在查任何尺寸之前设 DPI 感知：否则 GetWindowRect / 显示器矩形
        // 拿到的都是被系统虚拟化缩放过的值（见 win::ensure_dpi_aware 的说明）。
        let _ = super::ensure_dpi_aware();
        let (rect, is_window) = super::resolve_target(source)?;
        let src_layout = FrameLayout::new(
            (rect.right - rect.left) as u32,
            (rect.bottom - rect.top) as u32,
        )?;
        // 输出布局：没给目标尺寸就与源相同
        let out_layout = match target {
            Some((w, h)) => FrameLayout::new(w, h)?,
            None => src_layout,
        };
        // 只有真的需要缩放时才建 plan（None 走无缩放快路径）
        let plan = if out_layout == src_layout {
            None
        } else {
            Some(ScalePlan::new(
                src_layout.display_width(),
                src_layout.display_height(),
                out_layout.display_width(),
                out_layout.display_height(),
            )?)
        };

        let hwnd = match source {
            CaptureSource::Window(h) => Some(HWND(h as *mut std::ffi::c_void)),
            _ => None,
        };

        // SAFETY: 全部是句柄创建，失败由返回值判定
        unsafe {
            // 窗口 DC 拿的是**窗口客户区+边框**，坐标原点是窗口左上角；
            // 屏幕 DC 的坐标原点是虚拟桌面左上角。
            let src_dc = match hwnd {
                Some(h) => windows::Win32::Graphics::Gdi::GetWindowDC(Some(h)),
                None => GetDC(None),
            };
            if src_dc.is_invalid() {
                return Err(CaptureError::new("GetDC", "拿不到源设备上下文"));
            }

            let mem_dc = CreateCompatibleDC(Some(src_dc));
            if mem_dc.is_invalid() {
                ReleaseDC(hwnd, src_dc);
                return Err(CaptureError::new("CreateCompatibleDC", "创建内存 DC 失败"));
            }

            // 直接用结构体字面量构造，不先 `default()` 再逐字段赋值 ——
            // clippy 的 `field_reassign_with_default` 会拦后者，而且前者
            // 一眼能看出「哪些字段被显式设了」，后者要对比两处。
            let bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: src_layout.display_width() as i32,
                    // 负数 = 顶向下，内存顺序与屏幕一致
                    biHeight: -(src_layout.display_height() as i32),
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    biSizeImage: 0,
                    biXPelsPerMeter: 0,
                    biYPelsPerMeter: 0,
                    biClrUsed: 0,
                    biClrImportant: 0,
                },
                bmiColors: [windows::Win32::Graphics::Gdi::RGBQUAD::default()],
            };

            let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
            let dib = match CreateDIBSection(Some(mem_dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0)
            {
                Ok(b) => b,
                Err(e) => {
                    // SAFETY: 两个句柄都在上面创建成功，这里只是回收
                    let _ = DeleteDC(mem_dc);
                    let _ = ReleaseDC(hwnd, src_dc);
                    return Err(CaptureError::from_win("CreateDIBSection", e));
                }
            };
            if bits.is_null() {
                let _ = DeleteObject(HGDIOBJ(dib.0));
                let _ = DeleteDC(mem_dc);
                let _ = ReleaseDC(hwnd, src_dc);
                return Err(CaptureError::new(
                    "CreateDIBSection",
                    "系统返回了空像素指针",
                ));
            }

            let old_obj = SelectObject(mem_dc, HGDIOBJ(dib.0));

            Ok(Self {
                src_dc,
                src_hwnd: hwnd,
                mem_dc,
                dib,
                old_obj,
                bits: bits as *mut u8,
                src_layout,
                out_layout,
                plan,
                origin: if is_window {
                    // 窗口 DC 的原点就是窗口左上角，源坐标用 (0,0)。
                    // 用屏幕坐标会让标题栏偏出画面 —— 这是最容易踩的坑。
                    (0, 0)
                } else {
                    (rect.left, rect.top)
                },
                source,
            })
        }
    }

    /// 采集一帧
    ///
    /// GDI 是拉取式的，**永远**有帧（哪怕桌面完全静止），所以不返回 Option。
    pub fn capture(&mut self) -> Result<Frame> {
        // BitBlt 永远按**源**尺寸抓取：GDI 不做缩放，也不该做（见 new_sized 的说明）
        let w = self.src_layout.display_width() as i32;
        let h = self.src_layout.display_height() as i32;
        let (ox, oy) = self.origin;

        // SAFETY: 两个 DC 都有效且已配对，尺寸来自构造期
        unsafe {
            BitBlt(self.mem_dc, 0, 0, w, h, Some(self.src_dc), ox, oy, SRCCOPY)
                .map_err(|e| CaptureError::from_win("BitBlt", e))?;
        }

        let stride = self.src_layout.display_width() as usize * 4;
        let len = stride * self.src_layout.display_height() as usize;
        // SAFETY: bits 由 CreateDIBSection 分配，大小 = stride * height，
        // 生命周期跟随 self.dib（在 Drop 里才释放）。
        let bgra = unsafe { std::slice::from_raw_parts(self.bits, len) };

        let mut data = vec![0u8; self.out_layout.nv12_len()];
        match self.plan {
            // 有缩放：一次遍历同时降采样和转 NV12
            Some(plan) => crate::scale::bgra_to_nv12_with_plan(
                bgra,
                stride,
                plan,
                self.out_layout,
                &mut data,
            )?,
            // 无缩放：源尺寸即目标尺寸
            None => bgra_to_nv12(bgra, stride, self.out_layout, &mut data)?,
        }

        Ok(Frame {
            data,
            coded_width: self.out_layout.coded_width(),
            coded_height: self.out_layout.coded_height(),
            display_width: self.out_layout.display_width(),
            display_height: self.out_layout.display_height(),
            captured_at_us: 0,
        })
    }

    /// 输出尺寸（缩放后的；未设目标尺寸时等于显示器分辨率）
    pub fn display_size(&self) -> Option<(u32, u32)> {
        Some((
            self.out_layout.display_width(),
            self.out_layout.display_height(),
        ))
    }

    /// **源**尺寸（BitBlt 抓取的实际分辨率）
    ///
    /// 与 [`Self::display_size`] 不同：设了目标尺寸时这个是显示器原始分辨率。
    /// 发送端要用它做日志（「4K 屏 → 编码 720p」）。
    pub fn source_size(&self) -> (u32, u32) {
        (
            self.src_layout.display_width(),
            self.src_layout.display_height(),
        )
    }

    /// 缩放计划；`None` 表示未缩放
    pub fn scale_plan(&self) -> Option<ScalePlan> {
        self.plan
    }

    /// 采集源
    pub fn source(&self) -> &CaptureSource {
        &self.source
    }

    /// 源矩形（虚拟桌面坐标，按**源**尺寸算）
    pub fn source_rect(&self) -> RECT {
        let (ox, oy) = self.origin;
        RECT {
            left: ox,
            top: oy,
            right: ox + self.src_layout.display_width() as i32,
            bottom: oy + self.src_layout.display_height() as i32,
        }
    }
}

impl Drop for GdiCapture {
    fn drop(&mut self) {
        // SAFETY: 全部是配对释放，句柄在构造时已校验有效。
        // 顺序与创建相反：先把 DIB 换下来，再删 DC。
        unsafe {
            if !self.mem_dc.is_invalid() {
                SelectObject(self.mem_dc, self.old_obj);
                let _ = DeleteDC(self.mem_dc);
            }
            if !self.dib.is_invalid() {
                let _ = DeleteObject(HGDIOBJ(self.dib.0));
            }
            if !self.src_dc.is_invalid() {
                let _ = ReleaseDC(self.src_hwnd, self.src_dc);
            }
        }
    }
}
