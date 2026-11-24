//! Windows 平台后端（GDI / DXGI）
//!
//! 这个模块里的每一条代码都是 Windows 专属，全部用 `#[cfg(windows)]` 门控在
//! [`crate::lib`] 的 `pub mod win;` 上 —— 非 Windows 平台上整个模块不存在，
//! 纯逻辑部分（[`crate::convert`] / [`crate::synthetic`]）照常编译。
//!
//! # 为什么 `unsafe` 集中在这里
//!
//! crate 根上写了 `#![deny(unsafe_code)]`，本模块用 `#![allow(unsafe_code)]`
//! 局部放开。这样做的好处是：任何一次 `unsafe` 出现在 `win` 之外都会**编译失败**，
//! 而 `win` 里的 unsafe 都是对 Win32 的直接调用，评审时只需要盯这一个目录。
//!
//! # 句柄所有权约定
//!
//! 每个后端结构体持有自己创建的 HDC / HBITMAP / COM 接口，全部在 `Drop` 里
//! 按**创建的反序**释放。DXGI 的 `IDXGIOutputDuplication` 必须显式 `Drop`
//! 掉才能让**别的进程**重新复制同一个输出 —— 这不是可选清理，是资源独占。

#![allow(unsafe_code)]

pub mod dxgi;
pub mod gdi;

use windows::Win32::Foundation::{LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO,
};

use crate::error::{CaptureError, Result};

/// 把当前进程设为 per-monitor DPI aware
///
/// # 为什么这一步是必须的，不是可选优化
///
/// Windows 对 **DPI-unaware** 进程做坐标虚拟化：一个 3840x2160 的屏、
/// 缩放 200% 时，`EnumDisplayMonitors` / `GetSystemMetrics` /
/// `BitBlt` 全部报告 1920x1080，实际绘制时系统再帮你拉伸。
///
/// 而 **DXGI 的 Desktop Duplication 不受虚拟化影响**，它直接给
/// `ModeDesc.Width/Height = 3840x2160`。于是同一个「主显示器」：
///
/// | 后端 | 报告的尺寸 | 实际拿到 |
/// | --- | --- | --- |
/// | GDI | 1920x1080 | 左上角 1/4 画面，被系统缩放 |
/// | DXGI | 3840x2160 | 完整画面 |
///
/// 本机实测就是这个现象（AOC 2K 屏 + 200% 缩放）。两个后端给出不同分辨率
/// 会让「DXGI 失败降级到 GDI」这条路径直接坏掉：编码器按 DXGI 的尺寸配好，
/// 降级后帧尺寸突然变小，接收端要么花屏要么重建。
///
/// 设成 per-monitor aware 之后两边都报真实像素，降级路径才是无缝的。
///
/// # 为什么失败不算错误
///
/// `SetProcessDpiAwarenessContext` 在**进程已经设置过** DPI 感知时返回
/// `ERROR_ACCESS_DENIED`。宿主进程（比如 sidelenssend）可能已经设过，
/// 而且设的是比我们想要的更严格的级别 —— 那是好事，不该报错。
/// 另外 Win7 没有这个 API，失败也只能降级运行（那时没有 per-monitor 缩放）。
///
/// 所以：**尽力而为，返回值只用于日志**。
pub fn ensure_dpi_aware() -> Result<()> {
    use std::sync::OnceLock;
    // 全局只尝试一次。每次采集都调会让 SetProcessDpiAwarenessContext 反复
    // 返回 ACCESS_DENIED，白白多一次系统调用。
    static DONE: OnceLock<Result<()>> = OnceLock::new();
    DONE.get_or_init(|| {
        // SAFETY: 只是设置进程级标志，无内存安全影响
        unsafe {
            windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
                windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
            )
        }
        .map_err(|e| CaptureError::from_win("SetProcessDpiAwarenessContext", e))
    })
    .clone()
}

/// 当前进程是否已经 per-monitor DPI aware（用于诊断日志）
///
/// # 为什么用「主屏 DPI 是否等于 96」间接判断
///
/// `GetAwarenessFromDpiAwarenessContext` 需要先拿到当前 context，
/// 而拿 context 的 API（`GetThreadDpiAwarenessContext`）同样在
/// DPI-unaware 进程里返回的是虚拟化后的值，判不出真实状态。
/// 用 `GetDpiForSystem` 更直接：它返回**系统 DPI**，虚拟化进程里恒为 96。
pub fn system_dpi() -> u32 {
    // SAFETY: 纯查询
    unsafe { windows::Win32::UI::HiDpi::GetDpiForSystem() }
}

/// 一个显示器的信息
///
/// 不 derive `Eq`：内部的 `RECT` 只实现了 `PartialEq`（它是 Win32 的
/// POD 结构，windows crate 没给它 `Eq`）。强行实现会让整个类型不能用于
/// `HashMap` 的 key —— 但这里本来也不需要。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MonitorDesc {
    /// 显示器在**虚拟桌面坐标系**里的矩形
    ///
    /// 多显示器时 left/top 可能是负数（主屏左边的副屏），
    /// 所以这里是 `i32` 而不是 `u32` —— 转 `u32` 会让副屏采集出空帧。
    pub rect: RECT,
    /// 是否是主显示器
    pub primary: bool,
}

impl MonitorDesc {
    /// 宽度（像素）
    pub fn width(&self) -> i32 {
        self.rect.right - self.rect.left
    }

    /// 高度（像素）
    pub fn height(&self) -> i32 {
        self.rect.bottom - self.rect.top
    }
}

/// 枚举所有显示器
///
/// # 为什么用 `EnumDisplayMonitors` 而不是 `EnumDisplayDevices`
///
/// 前者直接给**虚拟桌面坐标**（采集时 BitBlt 要的就是这个坐标系），
/// 后者给的是设备名，还要再查一次 `EnumDisplaySettings` 才拿到分辨率，
/// 且多屏拼接的坐标关系对不上。
pub fn enumerate_monitors() -> Result<Vec<MonitorDesc>> {
    // 回调里通过 LPARAM 传 &mut Vec 的裸指针。这是 Win32 枚举回调的
    // 标准写法：回调是同步调用，Vec 在调用期间一直存活，指针不会悬垂。
    unsafe extern "system" fn cb(
        monitor: HMONITOR,
        _hdc: HDC,
        _rect: *mut RECT,
        data: LPARAM,
    ) -> windows::core::BOOL {
        // SAFETY: data 是 enumerate_monitors 里传进来的 &mut Vec，生命周期覆盖整个
        // EnumDisplayMonitors 调用；回调由系统同步调用，不会跨线程。
        let list = unsafe { &mut *(data.0 as *mut Vec<MonitorDesc>) };
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        // SAFETY: monitor 由系统提供，info 已填 cbSize
        if unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
            list.push(MonitorDesc {
                rect: info.rcMonitor,
                // MONITORINFOF_PRIMARY = 1
                primary: info.dwFlags & 1 != 0,
            });
        }
        windows::core::BOOL(1)
    }

    let mut list: Vec<MonitorDesc> = Vec::new();
    let ptr = &mut list as *mut Vec<MonitorDesc>;
    // SAFETY: ptr 指向栈上的 list，调用期间一直有效
    let ok = unsafe { EnumDisplayMonitors(None, None, Some(cb), LPARAM(ptr as isize)) };
    if !ok.as_bool() {
        return Err(CaptureError::new(
            "EnumDisplayMonitors",
            "枚举显示器失败（可能没有交互式桌面会话）",
        ));
    }
    if list.is_empty() {
        return Err(CaptureError::new(
            "EnumDisplayMonitors",
            "系统没有返回任何显示器",
        ));
    }
    // 系统枚举顺序不保证主屏在前，这里按 primary 排序，
    // 让 Monitor(0) 与 PrimaryMonitor 指向同一块屏（否则两者行为不一致）
    list.sort_by_key(|m| !m.primary);
    Ok(list)
}

/// 按 [`crate::CaptureSource`] 解析出要采集的矩形
///
/// 返回 `(rect, 是否窗口)`。窗口用 `GetWindowDC` + 相对坐标，
/// 显示器用屏幕 DC + 虚拟桌面坐标。
pub fn resolve_target(source: crate::CaptureSource) -> Result<(RECT, bool)> {
    use crate::CaptureSource;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{GetWindowRect, IsWindow};

    match source {
        CaptureSource::Synthetic => Err(CaptureError::UnsupportedSource {
            backend: "Win32",
            reason: "合成源不走平台 API，应该用 ScreenCapture::synthetic".into(),
        }),
        CaptureSource::PrimaryMonitor => {
            let monitors = enumerate_monitors()?;
            let m = monitors
                .iter()
                .find(|m| m.primary)
                .ok_or_else(|| CaptureError::new("PrimaryMonitor", "找不到主显示器"))?;
            Ok((m.rect, false))
        }
        CaptureSource::Monitor(idx) => {
            let monitors = enumerate_monitors()?;
            let m = monitors.get(idx).ok_or_else(|| {
                CaptureError::InvalidArgument(format!(
                    "显示器序号 {idx} 越界，系统只有 {} 块屏",
                    monitors.len()
                ))
            })?;
            Ok((m.rect, false))
        }
        CaptureSource::Window(handle) => {
            let hwnd = HWND(handle as *mut std::ffi::c_void);
            // SAFETY: hwnd 只是被查询，不解引用
            if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
                return Err(CaptureError::InvalidArgument(format!(
                    "窗口句柄 0x{handle:X} 无效（窗口可能已销毁）"
                )));
            }
            let mut rect = RECT::default();
            // SAFETY: rect 是栈上的可写 RECT
            unsafe { GetWindowRect(hwnd, &mut rect) }
                .map_err(|e| CaptureError::from_win("GetWindowRect", e))?;
            if rect.right <= rect.left || rect.bottom <= rect.top {
                return Err(CaptureError::InvalidArgument(format!(
                    "窗口矩形为空（可能已最小化）：({}, {})-({}, {})",
                    rect.left, rect.top, rect.right, rect.bottom
                )));
            }
            Ok((rect, true))
        }
    }
}
