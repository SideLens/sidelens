//! SideLens 屏幕采集层
//!
//! 三个后端，同一个对外接口：
//!
//! | 后端 | 构造 | 平台 | 1080p 单帧 |
//! | --- | --- | --- | --- |
//! | GDI BitBlt | [`ScreenCapture::new_gdi`] | Windows | 约 10-20 ms |
//! | DXGI Desktop Duplication | [`ScreenCapture::new_dxgi`] | Windows | 约 1-3 ms |
//! | 合成图案 | [`ScreenCapture::synthetic`] | 全部 | 约 0.5 ms |
//!
//! # 输出格式固定为 NV12 + 16 对齐
//!
//! 下游 `sidelens-codec-mf` 的 H.264 编码器只吃 NV12，且 coded size 必须
//! 16 对齐（1920x1080 → 1920x1088）。采集层直接产出这个格式，中间不做
//! 二次转换 —— 少一次全帧拷贝在 1080p 上就是 2 ms。
//!
//! 色彩约定（BT.709 limited range）见 [`convert`] 的模块文档。**这一条必须
//! 与接收端 `sidelens-render` 保持一致**，改一端不改另一端会导致颜色偏。
//!
//! # 为什么 `capture()` 返回 `Option`
//!
//! 桌面静止时 DXGI 的 `AcquireNextFrame` 会超时返回，这是**正常现象**而不是
//! 错误。如果把它当 Err 抛出去，调用方要么每帧重试要么把整个会话判死 ——
//! 两种都是错的。所以超时统一映射成 `Ok(None)`，语义是「这段时间没有新内容，
//! 保留上一帧即可」。
//!
//! # 跨平台
//!
//! 整个 crate 的**纯逻辑**（BGRA→NV12、对齐、合成源）不带任何 cfg，
//! 在 Linux / macOS 上照常编译、照常测试。只有 [`win`] 模块是 Windows 专属。
//! 非 Windows 上 [`ScreenCapture::new_gdi`] / [`ScreenCapture::new_dxgi`]
//! 返回 [`CaptureError::UnsupportedPlatform`]，调用方降级到
//! [`ScreenCapture::synthetic`]。

#![deny(unsafe_code)]
#![warn(missing_debug_implementations)]

pub mod convert;
pub mod error;
pub mod frame;
pub mod scale;
pub mod synthetic;

#[cfg(windows)]
pub mod win;

pub use convert::{align16, bgra_to_nv12, fill_black_nv12, FrameLayout, MAX_DIMENSION};
pub use error::{CaptureError, Result};
pub use frame::{Backend, CaptureClock, CaptureSource, CaptureStats, Frame, PlaneStats};
pub use scale::{bgra_to_nv12_scaled, bgra_to_nv12_with_plan, ScaleMode, ScalePlan};
pub use synthetic::{render_pattern, SyntheticSource, SYNTH_BAR_STEP};

use std::time::Instant;

/// 采集后端实例
///
/// 枚举而不是 trait object：后端只有三个且都是具体类型，用枚举可以让
/// `capture()` 走静态分发，也省掉 `Box<dyn>` 的一次指针跳转。
#[derive(Debug)]
enum Inner {
    /// 合成源（全平台）
    Synthetic(SyntheticSource),
    /// GDI BitBlt
    #[cfg(windows)]
    Gdi(win::gdi::GdiCapture),
    /// DXGI Desktop Duplication
    #[cfg(windows)]
    Dxgi(win::dxgi::DxgiCapture),
}

/// 屏幕采集器
///
/// 一个实例对应一个采集源。**非线程安全**：DXGI 的 duplication 接口与
/// GDI 的 HDC 都要求同一线程调用，跨线程使用需要调用方自己加锁或
/// 每线程一个实例。
pub struct ScreenCapture {
    inner: Inner,
    source: CaptureSource,
    backend: Backend,
    stats: CaptureStats,
    /// 创建时刻，用于 `captured_at_us` 的相对基准
    created: Instant,
    /// 目标输出尺寸；`None` = 按采集源原始分辨率输出
    ///
    /// 合成源也在用：`synthetic()` 之后再 `set_target_size()` 会改变
    /// 产出的 NV12 尺寸（合成图案按目标尺寸直接渲染，不做缩放 ——
    /// 它本来就是程序生成的，没必要先生成大的再缩）。
    target_size: Option<(u32, u32)>,
}

impl std::fmt::Debug for ScreenCapture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScreenCapture")
            .field("source", &self.source)
            .field("backend", &self.backend)
            .field("frames", &self.stats.frames)
            .finish()
    }
}

impl ScreenCapture {
    /// GDI BitBlt 基线：兼容性最好，但慢（1080p 约 10-20 ms）
    ///
    /// # 什么时候该用它
    ///
    /// - DXGI 打不开（远程桌面、会话 0、旧显卡、被别的进程占用）
    /// - 需要采集**单个窗口**（DXGI 只能按显示器复制）
    ///
    /// # 为什么它是保底方案
    ///
    /// `BitBlt` 从 Win3.1 就有了，任何 Windows 会话都能跑，不需要 D3D 设备。
    /// 代价是数据要经过 CPU 中转，1080p 单帧 10-20 ms，30fps 下已经吃掉
    /// 三分之一的时间预算。
    pub fn new_gdi(source: CaptureSource) -> Result<Self> {
        Self::new_gdi_sized(source, None)
    }

    /// GDI + 指定输出尺寸（缩放）
    ///
    /// `None` 与 [`Self::new_gdi`] 等价。缩放发生在 BGRA→NV12 那一步
    /// （见 [`win::gdi::GdiCapture::new_sized`] 的说明：不交给 `StretchBlt`）。
    pub fn new_gdi_sized(source: CaptureSource, target: Option<(u32, u32)>) -> Result<Self> {
        #[cfg(windows)]
        {
            let cap = win::gdi::GdiCapture::new_sized(source, target)?;
            let mut s = Self::with_inner(Inner::Gdi(cap), source, Backend::Gdi);
            s.target_size = target;
            Ok(s)
        }
        #[cfg(not(windows))]
        {
            let _ = (source, target);
            Err(CaptureError::UnsupportedPlatform { backend: "GDI" })
        }
    }

    /// DXGI Desktop Duplication：GPU 侧拷贝，1080p 约 1-3 ms
    ///
    /// # 失败是常态
    ///
    /// 这个方法在以下环境**一定**失败，而且都是正常现象：
    ///
    /// - 远程桌面（RDP）会话 —— `DuplicateOutput` 返回 `DXGI_ERROR_UNSUPPORTED`
    /// - Windows 会话 0（服务进程）—— `DXGI_ERROR_NOT_CURRENTLY_AVAILABLE`
    /// - 同一输出已经被别的进程复制 —— `DXGI_ERROR_NOT_CURRENTLY_AVAILABLE`
    ///   （Desktop Duplication 每个输出同时只允许一个复制者）
    /// - 显卡驱动不支持 —— `DXGI_ERROR_UNSUPPORTED`
    ///
    /// 调用方必须准备好降级到 [`Self::new_gdi`]。
    pub fn new_dxgi(source: CaptureSource) -> Result<Self> {
        Self::new_dxgi_sized(source, None)
    }

    /// DXGI + 指定输出尺寸（缩放）
    ///
    /// `None` 与 [`Self::new_dxgi`] 等价。这是**推荐的生产用法**：
    /// 4K 屏 + 720p 编码器时，直接在这里传 `Some((1280, 720))`，
    /// 采集层产出编码器要的尺寸，发送端不需要任何兜底逻辑。
    pub fn new_dxgi_sized(source: CaptureSource, target: Option<(u32, u32)>) -> Result<Self> {
        #[cfg(windows)]
        {
            let cap = win::dxgi::DxgiCapture::new_sized(source, target)?;
            let mut s = Self::with_inner(Inner::Dxgi(cap), source, Backend::Dxgi);
            s.target_size = target;
            Ok(s)
        }
        #[cfg(not(windows))]
        {
            let _ = (source, target);
            Err(CaptureError::UnsupportedPlatform { backend: "DXGI" })
        }
    }

    /// 合成图案，不碰任何平台 API
    ///
    /// 尺寸为 0 时按 1x1 处理（再对齐到 16x16），不返回错误 ——
    /// 这个构造函数**不会失败**，测试与 CI 里不需要处理 Result。
    pub fn synthetic(width: u32, height: u32) -> Self {
        let src = SyntheticSource::new(width, height);
        let mut s = Self::with_inner(
            Inner::Synthetic(src),
            CaptureSource::Synthetic,
            Backend::Synthetic,
        );
        s.target_size = Some((width, height));
        s
    }

    /// 按优先级挑后端：先 DXGI，失败自动降级到 GDI
    ///
    /// # 为什么要这个构造函数
    ///
    /// 生产路径上绝大多数调用方想要的就是「给我最快的那个」。让每个调用方
    /// 自己写 `match` 加 `eprintln!` 会重复十几次，而且日志格式不统一。
    /// 这里把「尝试 → 降级 → 记录原因」收成一处。
    ///
    /// 返回 `(capture, Option<降级原因>)`：第二个值非 None 说明 DXGI 没成功。
    /// 两个后端都失败时返回合成源 —— 让上层链路不至于直接崩，
    /// 但错误必须挂在返回值里，不能静默。
    pub fn auto(source: CaptureSource) -> (Self, Option<CaptureError>) {
        Self::auto_sized(source, None)
    }

    /// 按优先级挑后端，并指定输出尺寸
    ///
    /// 这是生产路径的**唯一推荐入口**：4K 屏 + 720p 编码器时传
    /// `Some((1280, 720))`，DXGI 打不开就降级 GDI，两个都打不开才退回
    /// 合成源 —— 而且**每一级都按同一个目标尺寸产出**，编码器不需要重建。
    ///
    /// # 为什么降级不会造成尺寸突变
    ///
    /// 目标尺寸是「输出契约」，与后端无关：DXGI 和 GDI 都从各自的源尺寸
    /// 缩放到同一个目标。这正是主控在发送端踩到的坑 ——
    /// 之前两个后端各自报自己的分辨率，降级时帧尺寸会突变。
    pub fn auto_sized(
        source: CaptureSource,
        target: Option<(u32, u32)>,
    ) -> (Self, Option<CaptureError>) {
        match Self::new_dxgi_sized(source, target) {
            Ok(cap) => (cap, None),
            Err(dxgi_err) => match Self::new_gdi_sized(source, target) {
                Ok(cap) => (cap, Some(dxgi_err)),
                Err(gdi_err) => {
                    // 两个真后端都失败：给合成源，但**保持目标尺寸**，
                    // 免得编码器因为尺寸突变而重建
                    let (w, h) = target.unwrap_or((640, 360));
                    (Self::synthetic(w, h), Some(gdi_err))
                }
            },
        }
    }

    /// 内部构造：统一初始化计时基准与统计
    fn with_inner(inner: Inner, source: CaptureSource, backend: Backend) -> Self {
        Self {
            inner,
            source,
            backend,
            stats: CaptureStats::default(),
            created: Instant::now(),
            target_size: None,
        }
    }

    /// 设置目标输出尺寸（缩放）；`None` = 按采集源原始分辨率输出
    ///
    /// # 缩放发生在哪里
    ///
    /// 在 BGRA → NV12 的转换里，**一次遍历同时做降采样和色彩转换** ——
    /// 不是「先转 NV12 再缩放」。后者要多一次全帧遍历和一块临时缓冲，
    /// 而且 NV12 的 UV 已经是 2x2 下采样过的，再缩一次会让色度总共降 4 倍。
    /// 详见 [`crate::scale`] 的模块文档。
    ///
    /// # 后端差异
    ///
    /// - **DXGI / GDI**：重建后端实例（缩放计划与源尺寸绑定，
    ///   而且 GDI 的 DIB、DXGI 的 staging 纹理尺寸也要跟着变）。
    ///   重建的代价是 DXGI 要重新 `DuplicateOutput`（约 10-30 ms），
    ///   所以**应该在构造时就决定尺寸**，不要在每帧循环里调它。
    /// - **合成源**：直接换渲染尺寸，不重建 —— 它没有平台资源。
    ///
    /// # 为什么不是「运行中改尺寸」
    ///
    /// 显示器分辨率本身变了（改显示设置 / 插拔）时，调用方应该**重建整个
    /// ScreenCapture**，而不是只调这个方法：DXGI 的 duplication 在分辨率
    /// 变化后会给 `ACCESS_LOST`，那时旧实例已经不可用了。
    pub fn set_target_size(&mut self, size: Option<(u32, u32)>) -> Result<()> {
        // 尺寸没变就不用重建 —— 这个检查很重要，发送端可能在每帧循环里
        // 无脑调一次（虽然不该那么写），重复重建会不停丢掉 duplication。
        if self.target_size == size {
            return Ok(());
        }
        // 提前校验，避免重建后端之后才发现尺寸非法（那时旧实例已经没了）
        if let Some((w, h)) = size {
            crate::convert::FrameLayout::new(w, h)?;
        }

        let new_inner = match &self.inner {
            Inner::Synthetic(_) => {
                // 合成源没有平台资源，直接换尺寸。
                // None 时保持当前尺寸不变（合成源没有「原始分辨率」的概念）。
                let (w, h) = match size {
                    Some(s) => s,
                    // None 时保持当前尺寸不变：合成源没有「原始分辨率」
                    // 的概念，无从「恢复」
                    None => self.display_size().unwrap_or((640, 360)),
                };
                Inner::Synthetic(SyntheticSource::new(w, h))
            }
            #[cfg(windows)]
            Inner::Gdi(_) => Inner::Gdi(win::gdi::GdiCapture::new_sized(self.source, size)?),
            #[cfg(windows)]
            Inner::Dxgi(_) => Inner::Dxgi(win::dxgi::DxgiCapture::new_sized(self.source, size)?),
        };
        self.inner = new_inner;
        self.target_size = size;
        Ok(())
    }

    /// 当前目标输出尺寸；`None` = 未缩放
    pub fn target_size(&self) -> Option<(u32, u32)> {
        self.target_size
    }

    /// 采集源的**原始**分辨率（缩放前）
    ///
    /// 与 [`Self::display_size`] 不同：设了目标尺寸时这个是屏幕真实分辨率，
    /// 用来打「4K 屏 → 编码 720p」这类日志。
    pub fn source_size(&self) -> Option<(u32, u32)> {
        match &self.inner {
            Inner::Synthetic(src) => Some((src.width(), src.height())),
            #[cfg(windows)]
            Inner::Gdi(cap) => Some(cap.source_size()),
            #[cfg(windows)]
            Inner::Dxgi(cap) => Some(cap.source_size()),
        }
    }

    /// 当前的缩放方式；未缩放时为 `None`
    ///
    /// 调用方可以据此打日志或断言（比如「期望 2:1 面积平均，实际是最近邻」
    /// 就说明目标尺寸选得不合适，画面会有锯齿）。
    pub fn scale_mode(&self) -> Option<ScaleMode> {
        match &self.inner {
            Inner::Synthetic(_) => None,
            #[cfg(windows)]
            Inner::Gdi(cap) => cap.scale_plan().map(|p| p.mode()),
            #[cfg(windows)]
            Inner::Dxgi(cap) => cap.scale_plan().map(|p| p.mode()),
        }
    }
}

impl ScreenCapture {
    /// 采集一帧
    ///
    /// 返回 `Ok(None)` 表示**这段时间桌面没有变化**，不是错误。
    ///
    /// # 各后端的语义
    ///
    /// - **DXGI**：`AcquireNextFrame` 带超时。`DXGI_ERROR_WAIT_TIMEOUT` →
    ///   `Ok(None)`；`DXGI_ERROR_ACCESS_LOST` → Err（桌面切换 / 分辨率变化，
    ///   调用方应该重建）。每次 Acquire 之后**必须** ReleaseFrame，
    ///   否则下一次 Acquire 永远失败。
    /// - **GDI**：永远有帧（BitBlt 是拉取式的，不依赖桌面刷新），
    ///   所以永不返回 `Ok(None)`。
    /// - **合成源**：每调一次产一帧，永不返回 `Ok(None)`。
    ///
    /// # 超时值
    ///
    /// DXGI 的等待超时取 `win::dxgi::POLL_TIMEOUT_MS`（16 ms，约 60 Hz 一帧）。
    /// 选 16 而不是 0 或 100：
    ///
    /// - **0（立即返回）** 会让调用方进入忙轮询，一个核被吃满；
    /// - **100** 在桌面静止时凭空多出最多 100 ms 的唤醒延迟，屏幕一动
    ///   就要等下一次轮询才发现；
    /// - **16** 与 60 Hz 对齐：屏幕一动，最坏 16 ms 内被拿到，
    ///   而桌面静止时每秒只唤醒 60 次，CPU 占用可以忽略。
    pub fn capture(&mut self) -> Result<Option<Frame>> {
        let started = Instant::now();
        let created = self.created;

        let out = match &mut self.inner {
            Inner::Synthetic(src) => {
                let mut f = src.next_frame();
                f.captured_at_us = elapsed_us_since(created);
                Ok(Some(f))
            }
            #[cfg(windows)]
            Inner::Gdi(cap) => {
                let mut f = cap.capture()?;
                f.captured_at_us = elapsed_us_since(created);
                Ok(Some(f))
            }
            #[cfg(windows)]
            Inner::Dxgi(cap) => {
                let r = cap.capture();
                // 即使这次超时，源侧也可能已经累积了丢帧，必须无条件取走
                self.stats.frames_dropped += cap.take_dropped();
                match r? {
                    Some(mut f) => {
                        f.captured_at_us = elapsed_us_since(created);
                        Ok(Some(f))
                    }
                    None => {
                        self.stats.empty_polls += 1;
                        Ok(None)
                    }
                }
            }
        };

        let us = started.elapsed();
        self.stats.total_us += us.as_micros();
        if matches!(out, Ok(Some(_))) {
            self.stats.frames += 1;
        }
        out
    }

    /// 采集源
    pub fn source(&self) -> &CaptureSource {
        &self.source
    }

    /// 实际使用的后端
    pub fn backend(&self) -> Backend {
        self.backend
    }

    /// 已产出的帧数
    pub fn frames(&self) -> u64 {
        self.stats.frames
    }

    /// 源侧累计丢掉的帧数
    ///
    /// 语义见 [`CaptureStats`]：只统计源自己报的丢帧，不含调用方来不及取的。
    pub fn frames_dropped(&self) -> u64 {
        self.stats.frames_dropped
    }

    /// 采集返回 `Ok(None)` 的次数
    pub fn empty_polls(&self) -> u64 {
        self.stats.empty_polls
    }

    /// 单帧平均采集耗时（微秒）
    ///
    /// 只统计 `capture()` 的墙钟时间，**不含**调用方处理帧的时间。
    pub fn avg_capture_us(&self) -> f64 {
        self.stats.avg_us()
    }

    /// 完整统计快照
    pub fn stats(&self) -> CaptureStats {
        self.stats
    }

    /// 当前显示尺寸
    ///
    /// 分辨率可能在运行中变化（改显示设置、插拔显示器、窗口改大小）。
    /// 调用方应该在拿到 `Ok(Some(frame))` 后对比 `frame.coded_width` 与
    /// 编码器配置，不一致就重建编码器 —— 本方法只是提前查询，
    /// **不能**代替逐帧比对。
    pub fn display_size(&self) -> Option<(u32, u32)> {
        match &self.inner {
            Inner::Synthetic(src) => Some((src.width(), src.height())),
            #[cfg(windows)]
            Inner::Gdi(cap) => cap.display_size(),
            #[cfg(windows)]
            Inner::Dxgi(cap) => cap.display_size(),
        }
    }
}

/// 从 `created` 到现在的微秒数
///
/// 抽成自由函数而不是方法：`capture()` 里 `self.inner` 已被可变借用，
/// 再调 `&self` 方法会触发借用冲突（E0502）。传 `Instant` 值进去绕开。
fn elapsed_us_since(created: Instant) -> u64 {
    let d = created.elapsed();
    d.as_secs() * 1_000_000 + u64::from(d.subsec_micros())
}
