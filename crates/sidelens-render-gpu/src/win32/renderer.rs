//! GPU 渲染器门面（和 `sidelens-render::Renderer` 同构）
//!
//! # 三种模式，一套 API
//!
//! | 模式 | 窗口 | swap chain | 用途 |
//! | --- | --- | --- | --- |
//! | [`GpuMode::Window`] | 有 | flip-model | 真实显示 |
//! | [`GpuMode::Offscreen`] | 无 | 无（渲染到 RT 纹理） | 测试 / 截图 |
//! | [`GpuMode::Headless`] | 无 | 无 | 无 GPU 环境（CI） |
//!
//! 三条路径共用 [`GpuRenderer::present`] / [`GpuRenderer::pump_events`] /
//! 统计方法，调用方不用分叉 —— 这正是 `sidelens-render` 的设计，
//! 主控按同一个模式接线，两边可以互换。
//!
//! # 每帧的数据流（Window 模式）
//!
//! ~~~text
//! NV12 (CPU)                                     ← 解码器输出
//!   │ Map staging + 逐行 memcpy（唯一的 CPU 全帧接触）
//!   ▼
//! NV12 staging (STAGING)  ──CopyResource──▶  NV12 texture (DEFAULT)
//!                                                │
//!                             ID3D11VideoProcessorBlt（固定功能单元）
//!                                                ▼
//! swap chain back buffer (BGRA)  ──Present(flip)──▶  DWM ──▶ 屏幕
//! ~~~
//!
//! 关键点：**色彩转换在 GPU 上**，CPU 只做一次 memcpy 上传。
//! 对比 GDI 路径（NV12 → CPU 转 RGBA → CPU 转 BGRA → StretchDIBits），
//! 少两次全帧遍历，且转换本身是免费的。
//!
//! # 为什么 present 返回 Result 而不是静默吞错
//!
//! 设备丢失（`DXGI_ERROR_DEVICE_REMOVED`）必须让调用方知道 ——
//! 那是「显卡驱动崩了」或者「GPU 被切走」的信号，继续调用只会一直失败。
//! 调用方应当销毁本渲染器并降级到 GDI。

use std::time::Instant;

use windows::Win32::Graphics::Direct3D11::{
    ID3D11Texture2D, ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView,
    D3D11_BIND_RENDER_TARGET, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};

use super::device::GpuDevice;
use super::processor::{Nv12Texture, VideoProcessor};
use super::readback::{read_texture, ReadbackImage};
use super::swapchain::{SwapChain, SWAPCHAIN_FORMAT_NAME};
use super::timer::GpuTimer;
use super::window::Window;
use crate::color::ColorSpace;
use crate::error::{hresult, GpuError};
use crate::layout::Nv12Layout;
use crate::stats::FrameStats;

/// 渲染模式
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuMode {
    /// 真窗口 + flip-model swap chain
    Window,
    /// 离屏：渲染到 RT 纹理，可读回像素（测试用）
    Offscreen,
    /// 无 GPU：只计数，不碰任何 D3D 对象
    Headless,
}

impl GpuMode {
    /// 人类可读的名字
    pub fn as_str(self) -> &'static str {
        match self {
            GpuMode::Window => "window",
            GpuMode::Offscreen => "offscreen",
            GpuMode::Headless => "headless",
        }
    }
}

/// GPU 侧一条渲染管线的全部对象
///
/// 尺寸变化时整体重建 —— 视频处理器的 content desc 是创建时定死的，
/// 没法原地改尺寸。
struct Pipeline {
    layout: Nv12Layout,
    nv12: Nv12Texture,
    processor: VideoProcessor,
    input_view: ID3D11VideoProcessorInputView,
    /// 输出目标（window 模式 = 后缓冲，offscreen 模式 = RT 纹理）
    output_texture: ID3D11Texture2D,
    output_view: ID3D11VideoProcessorOutputView,
}

/// GPU 渲染器
///
/// 三种模式共用一个类型（内部是 [`GpuMode`]），调用方代码不分叉。
pub struct GpuRenderer {
    mode: GpuMode,
    /// 只有 Window 模式有
    window: Option<Window>,
    /// 只有 Window 模式有
    swapchain: Option<SwapChain>,
    /// Headless 模式下恒为 `None`
    device: Option<GpuDevice>,
    /// Headless 模式下恒为 `None`
    pipeline: Option<Pipeline>,
    /// GPU 计时器（设备不支持时为 `None`，不影响渲染）
    timer: Option<GpuTimer>,
    /// 请求的显示尺寸
    width: u32,
    height: u32,
    /// 离屏输出纹理的**实际**尺寸
    ///
    /// 通常等于 width/height，但当 present 传入的 display 比构造尺寸大时会被
    /// 撑大 —— 否则视频处理器的输出矩形会超出纹理，Blt 直接 E_INVALIDARG
    /// （实测踩过：offscreen(64,64) 之后 present 128x96 就炸在这里）。
    /// 窗口模式用 swap chain 后缓冲尺寸，这个字段不用。
    surface: (u32, u32),
    /// 呈现耗时统计（CPU 侧，含提交与 Present 等待）
    present_stats: FrameStats,
    /// GPU 侧转换耗时统计（来自 timestamp query）
    gpu_stats: FrameStats,
    /// 上一次 upload 的 CPU 耗时（微秒）
    last_upload_us: f64,
}

impl std::fmt::Debug for GpuRenderer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GpuRenderer")
            .field("mode", &self.mode)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("frames", &self.present_stats.count())
            .finish_non_exhaustive()
    }
}

/// 判断一个 HRESULT 是否表示「设备丢失」（可恢复）
///
/// # 三个相关码
///
/// | HRESULT | 常量 | 含义 |
/// | --- | --- | --- |
/// | `0x887A0005` | `DXGI_ERROR_DEVICE_REMOVED` | 显卡被拔/驱动更新/切显卡 |
/// | `0x887A0006` | `DXGI_ERROR_DEVICE_HUNG` | TDR（GPU 超过 2 秒没响应被复位） |
/// | `0x887A0007` | `DXGI_ERROR_DEVICE_RESET` | 驱动内部复位 |
///
/// 三个都是「对象全失效、必须重建」，所以归成一类。
///
/// # 为什么不用 `windows` crate 的常量比较
///
/// `windows::Win32::Graphics::Dxgi::DXGI_ERROR_DEVICE_REMOVED` 是 `HRESULT`
/// 类型，而我们这里拿到的是 `GpuError` 里的格式化字符串（各模块自己拼的）。
/// 直接比 HRESULT 需要在每个调用点保留原始类型，改动面大。
///
/// 用数值判断简单可靠 —— 这三个值来自 DXGI 头文件，是 ABI 稳定的。
fn is_device_removed_hresult(code: u32) -> bool {
    (0x887A_0005..=0x887A_0007).contains(&code)
}

/// 从错误消息里提取 HRESULT（各模块格式化时都带 `0x` 前缀）
///
/// 这是「分类枚举 + 字符串带 HRESULT」设计的代价：要判断具体错误码得解析字符串。
/// 替代方案是给每个变体加一个 `Option<u32>` 字段，但那会让 6 个变体全部变形 ——
/// 而真正需要区分错误码的只有「设备丢失」这一处。
fn extract_hresult(msg: &str) -> Option<u32> {
    let idx = msg.find("0x")?;
    let hex: String = msg[idx + 2..]
        .chars()
        .take_while(|c| c.is_ascii_hexdigit())
        .collect();
    if hex.len() < 4 {
        return None;
    }
    u32::from_str_radix(&hex, 16).ok()
}
impl GpuRenderer {
    /// 创建真窗口 + flip-model swap chain
    ///
    /// 失败时调用方应降级到 `sidelens-render` 的 GDI 路径。
    ///
    /// 传入的宽高是**客户区**尺寸。离屏输出纹理也按这个尺寸建，
    /// 所以窗口被拖动改变大小时画面会按 1:1 留在左上角（不拉伸）——
    /// 拉伸会让「coded 没裁干净」看起来像「比例不对」，反而更难查。
    pub fn new(title: &str, width: u32, height: u32) -> Result<Self, GpuError> {
        if width == 0 || height == 0 {
            return Err(GpuError::ZeroSize { width, height });
        }

        let window = Window::new(title, width, height)?;
        Self::from_window(window, width, height)
    }

    /// 创建**子窗口**渲染器：嵌进 `parent` 的客户区 `(x, y)` 处
    ///
    /// # 为什么需要它（第五十四轮）
    ///
    /// 第五十三轮把原生 UI 接进 daemon 后暴露一个问题：
    /// [`GpuRenderer::new`] 建的是 `WS_OVERLAPPEDWINDOW` 顶层窗口，
    /// 会**盖住**状态面板的标签和按钮。
    ///
    /// 面板已经算好了视频区矩形（实测 200% DPI 下 `(32, 192, 1736, 928)`），
    /// 但渲染层只能开独立窗口 —— 用户看到的是「一个空白面板 + 另一个窗口里
    /// 有视频」，而不是「面板里嵌着视频」。
    ///
    /// # 参数口径
    ///
    /// `x` / `y` / `width` / `height` 全是**父窗口客户区的物理像素**。
    /// `sidelens-ui` 的 `MainWindow::video_rect()` 正好给这个口径。
    ///
    /// # 与 `new` 的唯一区别
    ///
    /// 窗口样式（`WS_CHILD` 而不是 `WS_OVERLAPPEDWINDOW`）和父句柄。
    /// swap chain、设备、视频处理器全部共用同一套构建代码 ——
    /// 分成两份实现会让「coded/display 裁剪」这类关键逻辑只在一处正确。
    pub fn new_child(
        parent: isize,
        title: &str,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
    ) -> Result<Self, GpuError> {
        if width == 0 || height == 0 {
            return Err(GpuError::ZeroSize { width, height });
        }

        let window = Window::new_child(parent, title, x, y, width, height)?;
        // 先建 swap chain（此时窗口还是无边框顶层窗口，不会死锁），
        // 再挂到父窗口上。顺序不能反 —— 见 `Window::attach_to_parent` 的文档。
        let r = Self::from_window(window, width, height)?;
        if let Some(w) = r.window.as_ref() {
            w.attach_to_parent(parent, x, y);
        }
        Ok(r)
    }

    /// 从已建好的窗口构建渲染器（顶层 / 子窗口共用）
    ///
    /// # 为什么建 swap chain 前要先泵一次消息
    ///
    /// `CreateSwapChainForHwnd` 在**子窗口**上会同步发送 `WM_SIZE` /
    /// `WM_WINDOWPOSCHANGED` 等消息给窗口过程，并**等待它们处理完**。
    /// 而窗口过程运行在创建它的线程上 —— 也就是当前线程。
    ///
    /// 如果此刻没有消息泵在跑（我们正是在建窗口的同一段代码里），
    /// 那些消息永远发不出去，`CreateSwapChainForHwnd` **永久阻塞**。
    ///
    /// 实测症状：daemon 打到「GPU 渲染器嵌入面板视频区」这行日志后
    /// 就再没有任何输出，进程活着但不干活（不是崩溃，是死锁）。
    ///
    /// 修法：建 swap chain 之前把队列里已排的消息处理掉，让子窗口
    /// 完成它的第一次布局。之后 swap chain 创建时窗口已经稳定，
    /// 不再需要同步等待。
    fn from_window(window: Window, width: u32, height: u32) -> Result<Self, GpuError> {
        window.pump_once();
        let device = GpuDevice::new()?;
        let swapchain = SwapChain::for_window(&device, window.hwnd(), width, height)?;

        let timer = match GpuTimer::new(&device) {
            Ok(t) => Some(t),
            Err(e) => {
                eprintln!("[sidelens-render-gpu] GPU 计时不可用（不影响渲染）：{e}");
                None
            }
        };

        Ok(GpuRenderer {
            mode: GpuMode::Window,
            window: Some(window),
            swapchain: Some(swapchain),
            device: Some(device),
            pipeline: None,
            timer,
            width,
            height,
            surface: (width, height),
            present_stats: FrameStats::new(),
            gpu_stats: FrameStats::new(),
            last_upload_us: 0.0,
        })
    }

    /// 离屏渲染器：不创建窗口，渲染到 `B8G8R8A8_UNORM` 纹理，可读回
    ///
    /// 用于测试与截图。[`GpuRenderer::read_pixels`] 能拿到 GPU 转换后的真实像素。
    pub fn offscreen(width: u32, height: u32) -> Result<Self, GpuError> {
        if width == 0 || height == 0 {
            return Err(GpuError::ZeroSize { width, height });
        }

        let device = GpuDevice::new()?;
        let timer = match GpuTimer::new(&device) {
            Ok(t) => Some(t),
            Err(e) => {
                eprintln!("[sidelens-render-gpu] GPU 计时不可用（不影响渲染）：{e}");
                None
            }
        };

        Ok(GpuRenderer {
            mode: GpuMode::Offscreen,
            window: None,
            swapchain: None,
            device: Some(device),
            pipeline: None,
            timer,
            width,
            height,
            surface: (width, height),
            present_stats: FrameStats::new(),
            gpu_stats: FrameStats::new(),
            last_upload_us: 0.0,
        })
    }

    /// 无 GPU 渲染器：只计数、只校验尺寸，不碰任何 D3D 对象
    ///
    /// CI / 无显卡环境用。[`GpuRenderer::present`] 会做完整的尺寸校验
    /// （这样输入校验的测试能在任何平台跑），但不做转换、不分配大缓冲。
    pub fn headless(width: u32, height: u32) -> Self {
        GpuRenderer {
            mode: GpuMode::Headless,
            window: None,
            swapchain: None,
            device: None,
            pipeline: None,
            timer: None,
            width,
            height,
            surface: (width, height),
            present_stats: FrameStats::new(),
            gpu_stats: FrameStats::new(),
            last_upload_us: 0.0,
        }
    }

    /// 当前模式
    pub fn mode(&self) -> GpuMode {
        self.mode
    }

    /// 请求的尺寸
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// 离屏输出纹理的实际尺寸
    ///
    /// 可能比 [`Self::size`] 大（present 传了更大的 display 时会被撑开）。
    /// 窗口模式下恒等于请求尺寸。
    pub fn surface_size(&self) -> (u32, u32) {
        self.surface
    }

    /// 窗口尺寸变化时重建 swap chain 与输出视图
    ///
    /// # 为什么需要显式调用（而不是自动检测）
    ///
    /// `WM_SIZE` 到达时，swap chain 的 back buffer 还是旧尺寸。
    /// 此时如果继续 `Present`，DWM 会把旧尺寸的画面拉伸到新窗口 ——
    /// 表现为「拖动窗口时画面变形」。
    ///
    /// 而 GPU 渲染器**收不到** `WM_SIZE`（窗口过程在 `window.rs` 里，
    /// 只处理退出标志）。所以必须由持有窗口的一方（UI 层 / daemon）
    /// 在收到 resize 后调用本方法。
    ///
    /// # 调用时机
    ///
    /// 收到 `WM_SIZE` / `UiEvent::Resized` 之后、下一次 `present` 之前。
    /// 重复调用同尺寸是幂等的（内部比较，相同则直接返回）。
    ///
    /// # 会做什么
    ///
    /// 1. `ResizeBuffers` 重建 back buffer
    /// 2. 丢弃旧管线（输出视图绑在旧 back buffer 上，必须重建）
    /// 3. 下一次 `present` 时按新尺寸重建
    pub fn resize(&mut self, width: u32, height: u32) -> Result<(), GpuError> {
        if width == 0 || height == 0 {
            return Err(GpuError::ZeroSize { width, height });
        }
        // 幂等：尺寸没变就什么都不做（WM_SIZE 会重复发）
        if (width, height) == (self.width, self.height) {
            return Ok(());
        }
        if self.mode == GpuMode::Headless {
            self.width = width;
            self.height = height;
            self.surface = (width, height);
            return Ok(());
        }

        self.width = width;
        self.height = height;
        self.surface = (width, height);

        if let Some(sc) = self.swapchain.as_mut() {
            sc.resize(width, height)?;
        }
        // 输出视图绑在旧 back buffer 上 —— 必须整条管线重建。
        // 下一次 present 会看到 `pipeline == None` 并调 `rebuild`。
        self.pipeline = None;
        Ok(())
    }

    /// 设备丢失后尝试重建整个渲染器
    ///
    /// # 什么时候会设备丢失
    ///
    /// - 显卡驱动更新 / 崩溃（TDR）
    /// - 从独立显卡切到集成显卡（笔记本拔电源）
    /// - 远程桌面连接 / 断开
    /// - GPU 过热降频导致复位
    ///
    /// 这些情况下所有 D3D11 对象都失效，`Present` 返回
    /// `DXGI_ERROR_DEVICE_REMOVED`。**唯一恢复办法是重建一切**。
    ///
    /// # 为什么需要调用方参与
    ///
    /// 重建要重新创建窗口（`SwapChain` 绑在 HWND 上）。窗口句柄变了，
    /// 调用方（UI 层）持有的 `hwnd` 也失效 —— 所以不能完全自动化。
    ///
    /// 本方法只负责「重建渲染器内部」，返回新的 `hwnd` 让调用方更新。
    ///
    /// # 返回值
    ///
    /// `Ok(None)` = headless 模式（无需重建）。
    /// `Ok(Some(hwnd))` = 重建成功，新窗口句柄。
    /// `Err(_)` = 重建失败（调用方应降级到 GDI 或 headless）。
    pub fn recover(&mut self) -> Result<Option<isize>, GpuError> {
        match self.mode {
            GpuMode::Headless => Ok(None),
            GpuMode::Offscreen => {
                // 离屏：丢掉旧设备重建即可，没有窗口依赖
                let (w, h) = (self.width, self.height);
                *self = Self::offscreen(w, h)?;
                Ok(None)
            }
            GpuMode::Window => {
                let (w, h) = (self.width, self.height);
                // 先取出窗口标题（重建后要复用）
                let title = "SideLens".to_string();
                // 子窗口模式要保留父句柄与位置
                let child = self.window.as_ref().and_then(|wd| wd.child_placement());
                *self = match child {
                    Some((parent, x, y)) => Self::new_child(parent, &title, x, y, w, h)?,
                    None => Self::new(title.as_str(), w, h)?,
                };
                Ok(Some(
                    self.window
                        .as_ref()
                        .map(|wd| wd.hwnd().0 as isize)
                        .unwrap_or(0),
                ))
            }
        }
    }

    /// 已 present 的帧数
    pub fn frames(&self) -> u64 {
        self.present_stats.count()
    }

    /// 平均呈现耗时（微秒）
    ///
    /// 含「上传 + Blt 提交 + Present（含帧延迟等待）」，也就是调用方
    /// 实际等待的时间。headless 下是纯校验耗时。
    pub fn avg_present_us(&self) -> f64 {
        self.present_stats.avg_us()
    }

    /// 平均 GPU 侧转换耗时（微秒）
    ///
    /// 来自 D3D11 timestamp query，只覆盖 GPU 上真正执行的部分，
    /// **不含** CPU 上传和 Present 等待。headless / 不支持计时的设备返回 0。
    pub fn avg_gpu_us(&self) -> f64 {
        self.gpu_stats.avg_us()
    }

    /// 呈现耗时的 p50 / p95 / max（微秒）
    pub fn present_percentiles(&self) -> (f64, f64, f64) {
        (
            self.present_stats.p50_us(),
            self.present_stats.p95_us(),
            self.present_stats.max_us(),
        )
    }

    /// GPU 耗时的 p50 / p95 / max（微秒）
    pub fn gpu_percentiles(&self) -> (f64, f64, f64) {
        (
            self.gpu_stats.p50_us(),
            self.gpu_stats.p95_us(),
            self.gpu_stats.max_us(),
        )
    }

    /// 最近一帧的上传耗时（微秒）—— 纯 CPU 的 Map + memcpy
    pub fn last_upload_us(&self) -> f64 {
        self.last_upload_us
    }

    /// 适配器名字
    pub fn adapter_name(&self) -> &str {
        match &self.device {
            Some(d) => d.adapter_name(),
            None => "headless（无 GPU）",
        }
    }

    /// 是否落到了 WARP（软件光栅化）
    pub fn is_software(&self) -> bool {
        self.device.as_ref().is_some_and(|d| d.is_software())
    }

    /// swap chain 的呈现格式名
    pub fn swapchain_format(&self) -> &'static str {
        match self.mode {
            GpuMode::Window | GpuMode::Offscreen => SWAPCHAIN_FORMAT_NAME,
            GpuMode::Headless => "none",
        }
    }

    /// 抽消息。返回 `false` 表示用户关窗
    ///
    /// headless / offscreen 恒返回 `true`（没有窗口可关）。
    pub fn pump_events(&mut self) -> bool {
        match &self.window {
            Some(w) => w.pump_events(),
            None => true,
        }
    }

    /// 呈现一帧 NV12
    ///
    /// `nv12` 是**紧凑布局**的解码器输出（stride == coded_width）；
    /// `coded` 是 16 对齐尺寸（1920x1080 → 1920x1088），
    /// `display` 是真实尺寸。
    ///
    /// coded 多出来的底部/右侧填充**不会被画出来**：视频处理器的输入矩形
    /// 用 display 尺寸，那些行根本不采样。
    pub fn present(
        &mut self,
        nv12: &[u8],
        coded: (u32, u32),
        display: (u32, u32),
    ) -> Result<(), GpuError> {
        // # 统一把「设备丢失」翻成 `DeviceRemoved`
        //
        // 底层各模块（swapchain / processor / texture）在失败时都拼了
        // 带 HRESULT 的字符串。这里统一过一遍：如果 HRESULT 是三个
        // 设备丢失码之一，换成专门的变体，让调用方能 match 到并重建。
        //
        // 不这么做的话，调用方只能靠字符串匹配判断 —— 错误信息一改就失效。
        self.present_inner(nv12, coded, display).map_err(|e| {
            if let Some(hr) = extract_hresult(&e.to_string()) {
                if is_device_removed_hresult(hr) {
                    return GpuError::DeviceRemoved { hresult: hr };
                }
            }
            e
        })
    }

    fn present_inner(
        &mut self,
        nv12: &[u8],
        coded: (u32, u32),
        display: (u32, u32),
    ) -> Result<(), GpuError> {
        let layout = Nv12Layout::new(coded, display)?;

        if layout.frame_len() > nv12.len() {
            return Err(GpuError::Layout(
                crate::layout::LayoutError::BufferTooSmall {
                    need: layout.frame_len(),
                    have: nv12.len(),
                },
            ));
        }

        if self.mode == GpuMode::Headless {
            // 无 GPU 路径：校验做完就算「呈现」了。不分配、不转换。
            self.present_stats.record(0.0);
            return Ok(());
        }

        let t0 = Instant::now();

        // 尺寸或布局变化 → 重建整条管线
        let need_rebuild = match &self.pipeline {
            Some(p) => p.layout != layout,
            None => true,
        };
        if need_rebuild {
            self.rebuild(layout)?;
        }

        let device = self
            .device
            .as_ref()
            .ok_or_else(|| GpuError::Device("设备已释放".into()))?;

        // 1) 上传（唯一的 CPU 全帧接触）
        let up0 = Instant::now();
        {
            let pipeline = self
                .pipeline
                .as_mut()
                .ok_or_else(|| GpuError::Texture("管线未初始化".into()))?;
            pipeline.nv12.upload(device, nv12)?;
        }
        self.last_upload_us = up0.elapsed().as_secs_f64() * 1e6;

        // 2) 读上一次的 GPU 计时（延迟一帧，不阻塞）
        //
        // 顺序很重要：先把**上一帧**的结果读掉，再为**本帧** Begin。
        // 反过来的话 Begin 会覆盖掉还没读的 end query。
        if let Some(t) = self.timer.as_mut() {
            if let Some(us) = t.try_read(device) {
                self.gpu_stats.record(us);
            }
        }
        if let Some(t) = self.timer.as_ref() {
            t.begin(device);
        }

        // 3) YUV → RGB（固定功能单元，一条命令）
        {
            let pipeline = self
                .pipeline
                .as_ref()
                .ok_or_else(|| GpuError::Texture("管线未初始化".into()))?;
            pipeline
                .processor
                .blt(device, &pipeline.input_view, &pipeline.output_view)?;
        }

        if let Some(t) = self.timer.as_mut() {
            t.end(device);
        }

        // 4) 呈现
        match &self.swapchain {
            Some(sc) => {
                // 队列满就跳过这一帧的呈现：解码线程不该被 GPU 拖住，
                // 堆积的帧就是延迟。像素已经算完了，下一帧会覆盖。
                if sc.wait_for_frame() {
                    sc.present()?;
                }
            }
            None => {
                // 离屏模式没有 Present 帮我们推命令队列。不 Flush 的话
                // 驱动可以一直攒着不执行 —— timestamp query 读不到结果
                // （实测 avg_gpu_us 恒为 0），read_pixels 也可能读到空纹理。
                unsafe { device.context().Flush() };
            }
        }

        self.present_stats.record(t0.elapsed().as_secs_f64() * 1e6);
        Ok(())
    }

    /// 重建整条管线（首次 present 或尺寸变化时）
    fn rebuild(&mut self, layout: Nv12Layout) -> Result<(), GpuError> {
        let device = self
            .device
            .as_ref()
            .ok_or_else(|| GpuError::Device("设备已释放".into()))?;

        let (dw, dh) = layout.display();

        // 先算出真实输出纹理尺寸，再建处理器 ——
        // 传错会让驱动退回最近邻采样（放大时有横向条纹）。
        let output_size = match self.mode {
            GpuMode::Window => self
                .swapchain
                .as_ref()
                .map(|s| s.size())
                .unwrap_or((dw, dh)),
            GpuMode::Offscreen => (self.width.max(dw), self.height.max(dh)),
            GpuMode::Headless => (dw, dh),
        };

        let nv12 = Nv12Texture::new(device.device(), layout)?;
        let processor = VideoProcessor::new(device, layout, ColorSpace::default(), output_size)?;
        let input_view = processor.create_input_view(device, nv12.texture())?;

        let (output_texture, output_view) = match self.mode {
            GpuMode::Window => {
                let sc = self
                    .swapchain
                    .as_ref()
                    .ok_or_else(|| GpuError::SwapChain("窗口模式缺少 swap chain".into()))?;
                // 后缓冲尺寸必须 >= 输出矩形，否则 Blt 会失败。
                // 窗口比画面小时把画面裁掉（不是缩放到窗口）——
                // 与 GDI 路径「左上角对齐、不缩放」的行为保持一致。
                let (bw, bh) = sc.size();
                let tex: ID3D11Texture2D = sc.back_buffer()?;
                let view = processor.create_output_view(device, &tex)?;
                // 填满窗口但**保持宽高比**（letterbox / pillarbox）。
                //
                // 第五十四轮之前是 1:1 不缩放，所以 1280x720 的画面在
                // 1736x928 的视频区里只占中间一块、四周黑边。用户看到的是
                // 「视频没铺满」，而不是「视频尺寸不对」。
                //
                // 直接拉伸到填满会让画面**变形**（人脸变胖/变瘦），
                // 这是最容易被察觉的显示错误。所以按较小的缩放比居中，
                // 留黑边是刻意的。
                processor.set_dest_rect(device, Some((bw, bh)))?;
                (tex, view)
            }
            GpuMode::Offscreen => {
                // 纹理必须 >= 输出矩形，否则 Blt 会 E_INVALIDARG。
                // 取「构造尺寸」和「本次 display 尺寸」的较大者：
                // 小的帧不该缩小纹理（否则下一帧又要重建），大的帧必须撑开。
                let sw = self.width.max(dw);
                let sh = self.height.max(dh);
                let desc = D3D11_TEXTURE2D_DESC {
                    Width: sw,
                    Height: sh,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
                    CPUAccessFlags: 0,
                    MiscFlags: 0,
                };
                let mut tex: Option<ID3D11Texture2D> = None;
                unsafe { device.device().CreateTexture2D(&desc, None, Some(&mut tex)) }
                    .map_err(|e| GpuError::Texture(hresult("CreateTexture2D(offscreen RT)", &e)))?;
                let tex = tex.ok_or_else(|| {
                    GpuError::Texture("CreateTexture2D(offscreen RT) 未返回纹理".into())
                })?;
                let view = processor.create_output_view(device, &tex)?;
                self.surface = (sw, sh);
                (tex, view)
            }
            GpuMode::Headless => {
                return Err(GpuError::Unsupported {
                    mode: "headless",
                    what: "GPU 管线",
                })
            }
        };

        self.pipeline = Some(Pipeline {
            layout,
            nv12,
            processor,
            input_view,
            output_texture,
            output_view,
        });
        // 布局变了，之前的耗时分布不再可比
        self.present_stats.reset();
        self.gpu_stats.reset();
        Ok(())
    }

    /// 离屏模式下把渲染结果读回 CPU
    ///
    /// 非离屏模式返回 [`GpuError::Unsupported`]。
    pub fn read_pixels(&self) -> Result<ReadbackImage, GpuError> {
        if self.mode != GpuMode::Offscreen {
            return Err(GpuError::Unsupported {
                mode: self.mode.as_str(),
                what: "read_pixels",
            });
        }
        let device = self
            .device
            .as_ref()
            .ok_or_else(|| GpuError::Device("设备已释放".into()))?;
        let pipeline = self
            .pipeline
            .as_ref()
            .ok_or_else(|| GpuError::Texture("还没有 present 过，无内容可读".into()))?;

        // 等 GPU 把 Blt 做完再拷贝，否则读到的可能是上一帧
        unsafe {
            device.context().Flush();
        }

        let (sw, sh) = self.surface;
        read_texture(device, &pipeline.output_texture, sw, sh)
    }
}

impl Drop for GpuRenderer {
    fn drop(&mut self) {
        // 先放管线（持有 input/output view，view 又引用 swap chain 后缓冲），
        // 再放 swap chain —— 顺序反了 ResizeBuffers 会失败。
        self.pipeline = None;
        self.swapchain = None;
        self.device = None;
        self.window = None;
    }
}
