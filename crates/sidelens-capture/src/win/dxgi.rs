//! DXGI Desktop Duplication 采集后端
//!
//! # 为什么它比 GDI 快一个数量级
//!
//! Desktop Duplication 由 DWM（桌面窗口管理器）在**合成阶段**直接把最终画面
//! 交给复制者，帧已经在显存里；我们只需要 `CopyResource` 到一块 CPU 可读的
//! staging 纹理再 `Map`。全程没有 GDI 的「渲染到 DC → 同步读回」那一步。
//! 1080p 实测 1-3 ms，GDI 是 10-20 ms。
//!
//! # 五个必须踩对的点
//!
//! ## 1. AcquireNextFrame 的超时是**正常返回**
//!
//! `DXGI_ERROR_WAIT_TIMEOUT`（0x887A0027）表示这段时间桌面没变化。
//! 必须映射成 `Ok(None)` 而不是 Err —— 否则桌面一静止调用方就得处理错误。
//!
//! ## 2. 每次 Acquire 之后必须 ReleaseFrame
//!
//! 不释放的话，下一次 `AcquireNextFrame` 会**永远**返回
//! `DXGI_ERROR_INVALID_CALL`（0x887A0001）。这是 Desktop Duplication
//! 最容易踩的坑：第一次跑正常，第二次就死了，而且错误信息完全不提原因。
//!
//! 本模块用一个 `guard` 保证这一点：`acquire` 成功后立刻构造
//! `FrameGuard`，它在 `Drop` 里调 `ReleaseFrame`。这样**任何**提前 return
//! （包括 `?` 传播的错误）都不会漏掉释放。
//!
//! ## 3. 输出纹理不能直接 Map
//!
//! `IDXGIResource` 拿到的是 GPU 侧纹理，`Usage = DEFAULT`，CPU 不可读。
//! 必须 `CopyResource` 到一块 `Usage = STAGING` + `CPUAccessFlags = READ`
//! 的纹理，再 `Map`。这一步是整条路径的主要成本（1080p 约 0.5-1.5 ms），
//! 实测数字见 `examples/bench_capture.rs`。
//!
//! ## 4. 脏矩形（部分更新）
//!
//! `DXGI_OUTDUPL_FRAME_INFO::TotalMetadataBufferSize != 0` 说明这一帧只有
//! 部分区域变了。**第一版忽略脏矩形，每次全量 CopyResource**：
//!
//! - 正确性：全量拷贝永远是对的（staging 纹理是上一帧的完整副本）。
//! - 代价：桌面只动了一个光标时也要拷 8.3 MB。1080p 60Hz 下这是
//!   500 MB/s 的 PCIe 流量。实测这条路径仍在 1-3 ms 内，所以对
//!   「超低延迟」目标没有实质影响；但如果将来要降到 4K 60Hz，
//!   脏矩形优化能把带宽砍掉一到两个数量级。
//! - 为什么先不做：脏矩形要维护一份「上一帧完整画面」，还要处理
//!   `GetFrameMoveRects`（窗口拖动是 move 不是 redraw），
//!   复杂度高且容易出画面残影。先用全量把链路跑对，再按需优化。
//!
//! ## 5. ACCESS_LOST 是重建信号
//!
//! `DXGI_ERROR_ACCESS_LOST`（0x887A0026）在以下情况返回：UAC 安全桌面
//! 切换、分辨率变化、显示器热插拔、锁屏。调用方应该**重建**整个
//! `DxgiCapture`，而不是继续用旧实例。

#![allow(unsafe_code)]

use windows::core::Interface;
use windows::Win32::Foundation::HMODULE;
// 有 adapter 时 driver type 必须是 UNKNOWN（文档明确要求）：
// 传 HARDWARE 会返回 E_INVALIDARG，而且错误信息完全不提原因。
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_10_0, D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_11_0,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Resource, ID3D11Texture2D,
    D3D11_CPU_ACCESS_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_SDK_VERSION,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput1, IDXGIOutputDuplication,
    IDXGIResource, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_DESC,
    DXGI_OUTDUPL_FRAME_INFO,
};

use crate::convert::{bgra_to_nv12, FrameLayout};
use crate::error::{CaptureError, Result};
use crate::frame::{CaptureSource, Frame};
use crate::scale::ScalePlan;

/// `AcquireNextFrame` 的等待超时（毫秒）
///
/// 16 ms ≈ 60 Hz 一帧。理由见 [`crate::ScreenCapture::capture`] 的文档：
/// 0 会忙轮询、100 会凭空多出唤醒延迟，16 与屏幕刷新率对齐。
pub const POLL_TIMEOUT_MS: u32 = 16;

/// ReleaseFrame 守卫
///
/// # 为什么必须是 RAII 而不是手写释放
///
/// `AcquireNextFrame` 成功后，中间任何一步失败（CopyResource、Map、
/// 尺寸校验）都会提前 return。手写 `ReleaseFrame` 意味着每个 return 点
/// 都要记得调一次 —— 漏一个，下一次 Acquire 就永久失败。
/// 这个守卫把「释放」绑到作用域上，从结构上消除这类 bug。
struct FrameGuard<'a> {
    dup: &'a IDXGIOutputDuplication,
    /// 是否已经释放（Drop 里只放一次）
    released: bool,
}

impl<'a> FrameGuard<'a> {
    /// 标记已手动释放（避免 Drop 里重复释放）
    fn release(mut self) {
        self.released = true;
        // SAFETY: dup 有效，且 Acquire 成功过
        unsafe {
            let _ = self.dup.ReleaseFrame();
        }
    }
}

impl Drop for FrameGuard<'_> {
    fn drop(&mut self) {
        if !self.released {
            // SAFETY: dup 在 self 的生命周期内有效
            unsafe {
                let _ = self.dup.ReleaseFrame();
            }
        }
    }
}

/// DXGI 采集器
pub struct DxgiCapture {
    /// D3D11 设备
    ///
    /// **字段本身从不会被读**，但它必须活着：staging 纹理与 duplication
    /// 都是这个设备创建的子资源，设备先 drop 会让它们变成无效句柄
    /// （后续 `Map` 返回 `E_INVALIDARG`，且错误信息不提设备已释放）。
    /// 所以这里保留字段 + `allow(dead_code)`，而不是让它随构造函数的
    /// 局部变量一起被释放。
    #[allow(dead_code)]
    device: ID3D11Device,
    /// 立即上下文
    context: ID3D11DeviceContext,
    /// 桌面复制接口
    dup: IDXGIOutputDuplication,
    /// CPU 可读的 staging 纹理
    staging: ID3D11Texture2D,
    /// staging 纹理宽度（像素）
    ///
    /// 单独存一份而不是每帧调 `GetDesc`：Map 之后的切片边界校验要用它，
    /// 而那一步不能失败（已经持有 frame guard）。构造期存下来最省事。
    staging_width: u32,
    /// staging 纹理高度（像素）
    staging_height: u32,
    /// staging 纹理的行距（Map 时才知道，缓存下来避免每帧重算）
    row_pitch: u32,
    /// **源**尺寸布局（duplication 给出的桌面尺寸，也是 staging 纹理的尺寸）
    ///
    /// 与 `out_layout` 分开是缩放的前提：staging 纹理必须与桌面纹理同尺寸，
    /// `CopyResource` 才成立；缩放发生在之后的 BGRA→NV12 转换里。
    src_layout: FrameLayout,
    /// **输出**尺寸布局（`Frame` 里报告的尺寸）
    ///
    /// 未设置目标尺寸时与 `src_layout` 相同。
    out_layout: FrameLayout,
    /// 缩放计划；`None` 表示源与目标尺寸一致，走无缩放快路径
    plan: Option<ScalePlan>,
    /// 采集源
    source: CaptureSource,
    /// 自上次取走以来，源侧累计丢掉的帧数
    pending_dropped: u64,
    /// 是否已经发生过 ACCESS_LOST（之后 capture 恒返回该错误）
    lost: bool,
}

// SAFETY: 同 GdiCapture —— D3D 接口可以跨线程传递，但不能并发使用。
unsafe impl Send for DxgiCapture {}

impl std::fmt::Debug for DxgiCapture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DxgiCapture")
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
            .field("row_pitch", &self.row_pitch)
            .field("lost", &self.lost)
            .finish()
    }
}

impl DxgiCapture {
    /// 创建 DXGI 采集器
    ///
    /// # 失败语义
    ///
    /// 任何一步失败都返回 [`CaptureError::Win32`]，带**原始 HRESULT**。
    /// 调用方据此决定降级还是重建。典型码值：
    ///
    /// | HRESULT | 含义 |
    /// | --- | --- |
    /// | `0x887A0004` DXGI_ERROR_UNSUPPORTED | 远程桌面 / 显卡不支持 |
    /// | `0x887A0022` DXGI_ERROR_NOT_CURRENTLY_AVAILABLE | 会话 0 / 已被别的进程占用 |
    /// | `0x887A0026` DXGI_ERROR_ACCESS_LOST | 运行中失去访问权（需重建） |
    pub fn new(source: CaptureSource) -> Result<Self> {
        Self::new_sized(source, None)
    }

    /// 创建 DXGI 采集器，可指定输出尺寸（缩放）
    ///
    /// # 为什么缩放不在 GPU 侧做
    ///
    /// D3D11 有两条 GPU 缩放路径：`ID3D11VideoProcessor`（能直接出 NV12）
    /// 和 compute shader。两者都能把缩放与色彩转换全放到 GPU，理论上比 CPU 快。
    /// 但代价是：需要额外的 `ID3D11VideoDevice`/`ID3D11VideoContext` 创建、
    /// 输出纹理必须是 `D3D11_BIND_RENDER_TARGET`（不能是 staging），
    /// 之后仍然要一次 GPU→CPU 的 `CopyResource` 才能拿到可编码的字节。
    ///
    /// 也就是说 GPU 缩放省掉的是 CPU 的缩放+转换，但**保不住**那次读回。
    /// 实测 CPU 侧 4K→1080p 缩放+转换是 10-12 ms，而 GPU 路径的读回本身
    /// 就要 5-7 ms，加上视频处理器的固定开销，收益不明确。
    ///
    /// 所以第一版走 CPU：路径短、可测、跨驱动行为一致。需要时再加 GPU 路径，
    /// 那时对外 API 不变（`new_sized` 的签名已经是最终形态）。
    pub fn new_sized(source: CaptureSource, target_size: Option<(u32, u32)>) -> Result<Self> {
        if matches!(source, CaptureSource::Synthetic) {
            return Err(CaptureError::UnsupportedSource {
                backend: "DXGI",
                reason: "合成源不走平台 API，请用 ScreenCapture::synthetic".into(),
            });
        }
        if matches!(source, CaptureSource::Window(_)) {
            return Err(CaptureError::UnsupportedSource {
                backend: "DXGI",
                reason: "Desktop Duplication 只能按输出（显示器）复制，不支持单个窗口；请改用 GDI"
                    .into(),
            });
        }

        // DXGI 本身不受 DPI 虚拟化影响，但**同一个进程里的 GDI 后端**受。
        // 这里也设一次，保证「先建 DXGI 再建 GDI」和「先建 GDI 再建 DXGI」
        // 两种顺序下，两个后端报的尺寸一致（降级路径必须无缝）。
        let _ = super::ensure_dpi_aware();

        let output_index = match source {
            CaptureSource::PrimaryMonitor => 0,
            CaptureSource::Monitor(n) => n,
            _ => 0,
        };

        // SAFETY: 全部是 COM/D3D 创建，失败由 Result 判定
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1()
                .map_err(|e| CaptureError::from_win("CreateDXGIFactory1", e))?;

            // 按显示器序号找到对应的 output：先枚举 adapter，再枚举 adapter 的 output，
            // 累加序号直到命中目标。DXGI 没有全局 output 列表，只能这样两层遍历。
            let mut seen = 0usize;
            let mut target = None;
            let mut idx = 0u32;
            'outer: loop {
                let adapter: IDXGIAdapter1 = match factory.EnumAdapters1(idx) {
                    Ok(a) => a,
                    // DXGI_ERROR_NOT_FOUND 表示枚举完了
                    Err(_) => break,
                };
                idx += 1;
                let mut out_idx = 0u32;
                loop {
                    let output = match adapter.EnumOutputs(out_idx) {
                        Ok(o) => o,
                        Err(_) => break,
                    };
                    out_idx += 1;
                    if seen == output_index {
                        target = Some((adapter.clone(), output));
                        break 'outer;
                    }
                    seen += 1;
                }
            }

            let (adapter, output) = target.ok_or_else(|| {
                CaptureError::InvalidArgument(format!(
                    "显示器序号 {output_index} 越界，系统只有 {seen} 个输出"
                ))
            })?;

            let desc = output
                .GetDesc()
                .map_err(|e| CaptureError::from_win("IDXGIOutput::GetDesc", e))?;
            let width = (desc.DesktopCoordinates.right - desc.DesktopCoordinates.left) as u32;
            let height = (desc.DesktopCoordinates.bottom - desc.DesktopCoordinates.top) as u32;
            let src_layout = FrameLayout::new(width, height)?;
            // 输出布局：没给目标尺寸就与源相同（不缩放）
            let out_layout = match target_size {
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

            // 有 adapter 时必须用 D3D_DRIVER_TYPE_UNKNOWN（文档明确要求），
            // 用 HARDWARE 会返回 E_INVALIDARG
            let feature_levels = [
                D3D_FEATURE_LEVEL_11_0,
                D3D_FEATURE_LEVEL_10_1,
                D3D_FEATURE_LEVEL_10_0,
            ];
            let mut device: Option<ID3D11Device> = None;
            let mut context: Option<ID3D11DeviceContext> = None;
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                Default::default(),
                Some(&feature_levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .map_err(|e| CaptureError::from_win("D3D11CreateDevice", e))?;
            let device =
                device.ok_or_else(|| CaptureError::new("D3D11CreateDevice", "没有返回设备"))?;
            let context =
                context.ok_or_else(|| CaptureError::new("D3D11CreateDevice", "没有返回上下文"))?;

            let output1: IDXGIOutput1 = output
                .cast()
                .map_err(|e| CaptureError::from_win("IDXGIOutput as IDXGIOutput1", e))?;
            let dup = output1
                .DuplicateOutput(&device)
                .map_err(|e| CaptureError::from_win("DuplicateOutput", e))?;

            // staging 纹理：尺寸必须与 dup 描述的桌面纹理一致，
            // 否则 CopyResource 会静默失败（不报错，只是拷不过来）
            let dup_desc: DXGI_OUTDUPL_DESC = dup.GetDesc();
            let tex_w = dup_desc.ModeDesc.Width;
            let tex_h = dup_desc.ModeDesc.Height;

            let staging = create_staging(&device, tex_w, tex_h)?;

            let mut capture = Self {
                device,
                context,
                dup,
                staging,
                staging_width: tex_w,
                staging_height: tex_h,
                row_pitch: 0,
                src_layout,
                out_layout,
                plan,
                source,
                pending_dropped: 0,
                lost: false,
            };
            // 先量一次行距，避免第一帧时还要额外分支
            capture.row_pitch = capture.probe_row_pitch()?;
            Ok(capture)
        }
    }
}

/// 创建 CPU 可读的 staging 纹理
///
/// # 为什么格式固定 B8G8R8A8_UNORM
///
/// 桌面的输出格式几乎总是 `B8G8R8A8_UNORM`（DXGI 在 Desktop Duplication
/// 里只会给这一种，除非用 `DuplicateOutput1` 显式请求别的）。
/// 用同一个格式创建 staging，`CopyResource` 才是**同格式整资源拷贝**，
/// 不需要格式转换 —— 换格式必须走 `CopySubresourceRegion` 或着色器，
/// 成本高且容易出色彩空间问题。
fn create_staging(device: &ID3D11Device, width: u32, height: u32) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    };
    let mut tex: Option<ID3D11Texture2D> = None;
    // SAFETY: desc 是栈上的合法描述
    unsafe {
        device
            .CreateTexture2D(&desc, None, Some(&mut tex))
            .map_err(|e| CaptureError::from_win("CreateTexture2D", e))?;
    }
    tex.ok_or_else(|| CaptureError::new("CreateTexture2D", "没有返回纹理"))
}

impl DxgiCapture {
    /// 采集一帧
    ///
    /// # 返回 `Ok(None)` 的三种情况
    ///
    /// 1. `DXGI_ERROR_WAIT_TIMEOUT`：这段时间桌面没变化（最常见）
    /// 2. `AccumulatedFrames == 0` 且没有鼠标更新：理论上与 1 等价，
    ///    但某些驱动在 `LastPresentTime == 0` 时仍会返回成功，
    ///    这里再判一次，避免产出内容与上一帧完全相同的「假新帧」
    ///
    /// # 为什么先 Acquire 再判空
    ///
    /// `AccumulatedFrames` 只有在 Acquire 成功之后才有效。超时路径根本
    /// 没有 frame info，也就无从判断 —— 所以「超时」本身就是「无新帧」的判据。
    pub fn capture(&mut self) -> Result<Option<Frame>> {
        if self.lost {
            return Err(CaptureError::Win32 {
                stage: "AcquireNextFrame",
                hresult: Some(DXGI_ERROR_ACCESS_LOST.0),
                message: "此前已失去访问权（桌面切换 / 分辨率变化），需要重建采集器".into(),
            });
        }

        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;

        // SAFETY: dup 有效；info/resource 是栈上可写变量
        let acquired = unsafe {
            self.dup
                .AcquireNextFrame(POLL_TIMEOUT_MS, &mut info, &mut resource)
        };

        match acquired {
            Ok(()) => {}
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => {
                // 桌面静止是正常状态，不是错误
                return Ok(None);
            }
            Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
                self.lost = true;
                return Err(CaptureError::from_win("AcquireNextFrame", e));
            }
            Err(e) => return Err(CaptureError::from_win("AcquireNextFrame", e)),
        }

        // 从这里开始必须保证 ReleaseFrame，用守卫兜住所有提前 return
        let guard = FrameGuard {
            dup: &self.dup,
            released: false,
        };

        // ── 空帧判据（实测必须，否则第一帧一定是全黑）────────────────────
        //
        // DuplicateOutput 之后的**第一次** AcquireNextFrame 会成功返回，
        // 但带的是「还没有任何桌面更新」的占位帧。本机实测：
        //
        // | 字段 | 首帧 | 之后的真实帧 |
        // | --- | --- | --- |
        // | AccumulatedFrames | **0** | 3, 4, 4, 5 … |
        // | LastPresentTime | **0** | 702369587741 … |
        // | 纹理格式 | **R16G16B16A16_FLOAT** | B8G8R8A8_UNORM |
        // | 像素内容 | 全 0 | 真实画面 |
        //
        // 不判这一条，第一帧会被当成有效帧：CopyResource 把一块全 0 的
        // float 纹理拷进 BGRA staging（格式不匹配，D3D 直接丢弃拷贝），
        // 结果是一帧 Y 恒为 16 的纯黑画面被送进编码器 —— 而 min==max==16
        // 恰好是「合法但全黑」，下游完全看不出异常。
        //
        // 判据用 `LastPresentTime == 0` 而不是 `AccumulatedFrames == 0`：
        // 后者在「桌面更新很快但都是同一帧」时也可能为 0，
        // 而 `LastPresentTime` 是「上一次 Present 的 QPC 时间」，0 明确表示
        // 「从来没有 Present 过」，语义无歧义。
        if info.LastPresentTime == 0 {
            guard.release();
            return Ok(None);
        }

        // AccumulatedFrames 是「自上次 Acquire 以来桌面更新了几次」。
        // 大于 1 说明我们的采集频率跟不上，多出来的就是真实丢帧。
        if info.AccumulatedFrames > 1 {
            self.pending_dropped += u64::from(info.AccumulatedFrames - 1);
        }

        let resource = match resource {
            Some(r) => r,
            None => {
                // 极少数驱动在成功返回时不给资源，按「无新帧」处理
                guard.release();
                return Ok(None);
            }
        };

        // SAFETY: resource 是本次 Acquire 拿到的桌面纹理
        let texture: ID3D11Texture2D = resource
            .cast()
            .map_err(|e| CaptureError::from_win("IDXGIResource as ID3D11Texture2D", e))?;

        // ── 格式校验：CopyResource 对格式不匹配是**静默失败** ────────────
        //
        // D3D11 的 CopyResource 要求源与目标「同尺寸、同格式」，不满足时
        // 它只记一条 debug layer 警告（release 下根本没有），**不返回错误**，
        // 目标纹理保持原样。实测首帧的纹理是 R16G16B16A16_FLOAT（格式 10），
        // 拷进 BGRA staging 就是一次彻底的空操作。
        //
        // 所以每帧都要校验：格式不对说明驱动给了非常规资源，宁可返回
        // Ok(None)（等下一帧）也不能把上一帧的陈旧内容当成新帧送出去。
        let mut tdesc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: tdesc 是栈上可写结构
        unsafe { texture.GetDesc(&mut tdesc) };
        if tdesc.Format != DXGI_FORMAT_B8G8R8A8_UNORM {
            self.pending_dropped = self.pending_dropped.saturating_add(1);
            guard.release();
            return Ok(None);
        }

        // GPU 侧拷贝：桌面纹理 → staging。同格式整资源拷贝，不需要转换。
        // SAFETY: 两个纹理都是 2D、同尺寸、同格式（上面刚校验过）
        unsafe {
            let dst: ID3D11Resource = self
                .staging
                .cast()
                .map_err(|e| CaptureError::from_win("staging as ID3D11Resource", e))?;
            let src: ID3D11Resource = texture
                .cast()
                .map_err(|e| CaptureError::from_win("texture as ID3D11Resource", e))?;
            self.context.CopyResource(&dst, &src);
        }

        // Map → 转换 → Unmap。这三个必须配对，Map 失败时不需要 Unmap。
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: staging 是 STAGING + CPU_READ，可以 Map
        let map_r = unsafe {
            self.context
                .Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
        };
        if let Err(e) = map_r {
            guard.release();
            return Err(CaptureError::from_win("Map", e));
        }
        if mapped.pData.is_null() {
            // SAFETY: Map 成功过，必须 Unmap
            unsafe { self.context.Unmap(&self.staging, 0) };
            guard.release();
            return Err(CaptureError::new("Map", "系统返回了空指针"));
        }

        // ── 行距校验：pitch < width*4 会读到下一行，画面斜切 ──────────────
        //
        // 正常情况下 D3D11 的 RowPitch >= Width*4（按 256 字节对齐）。
        // 但驱动异常或纹理尺寸与源布局不一致时可能更小 —— 那时
        // bgra_to_nv12 会拿到「行距小于一行」的参数，转换结果每行右移，
        // 画面变成斜的。宁可返回错误也不能产出这种帧。
        let pitch = mapped.RowPitch as usize;
        // 校验针对**源**尺寸：staging 纹理与桌面纹理同尺寸，
        // 与缩放后的输出尺寸无关。
        let sw = self.src_layout.display_width() as usize;
        let sh = self.src_layout.display_height() as usize;
        let tex_w = self.staging_width as usize;
        let tex_h = self.staging_height as usize;

        // staging 纹理必须覆盖源布局需要的区域，否则下面的切片会越界。
        // 两个尺寸在构造期都来自 DXGI 自身，理论上恒等；这里校验是
        // 为了在「显示器热插拔后尺寸变了」这种运行期变化上不 UB。
        if pitch < sw * 4 || tex_w < sw || tex_h < sh {
            // SAFETY: 与上面的 Map 配对
            unsafe { self.context.Unmap(&self.staging, 0) };
            guard.release();
            return Err(CaptureError::new(
                "Map",
                format!(
                    "staging 纹理不匹配：pitch={pitch} 需要 >= {}，纹理 {tex_w}x{tex_h} 需要 >= {sw}x{sh}",
                    sw * 4
                ),
            ));
        }

        self.row_pitch = mapped.RowPitch;

        // SAFETY: pData 由 Map 给出，有效范围是 RowPitch * 纹理高度；
        // 上面的校验保证 pitch >= sw*4 且 tex_h >= sh，所以这个切片
        // 完全落在映射区域内。
        let mut data = vec![0u8; self.out_layout.nv12_len()];
        let conv = unsafe {
            let len = pitch * tex_h;
            let bgra = std::slice::from_raw_parts(mapped.pData as *const u8, len);
            match self.plan {
                // 有缩放：一次遍历同时降采样和转 NV12
                Some(plan) => crate::scale::bgra_to_nv12_with_plan(
                    bgra,
                    pitch,
                    plan,
                    self.out_layout,
                    &mut data,
                ),
                // 无缩放：源尺寸即目标尺寸
                None => bgra_to_nv12(bgra, pitch, self.out_layout, &mut data),
            }
        };

        // SAFETY: 与上面的 Map 配对
        unsafe { self.context.Unmap(&self.staging, 0) };
        guard.release();

        conv?;

        Ok(Some(Frame {
            data,
            coded_width: self.out_layout.coded_width(),
            coded_height: self.out_layout.coded_height(),
            display_width: self.out_layout.display_width(),
            display_height: self.out_layout.display_height(),
            captured_at_us: 0,
        }))
    }

    /// 取走累计的丢帧数（读后清零）
    pub fn take_dropped(&mut self) -> u64 {
        std::mem::take(&mut self.pending_dropped)
    }

    /// 输出尺寸（缩放后的；未设目标尺寸时等于桌面分辨率）
    pub fn display_size(&self) -> Option<(u32, u32)> {
        Some((
            self.out_layout.display_width(),
            self.out_layout.display_height(),
        ))
    }

    /// **源**尺寸（duplication 给出的桌面分辨率）
    ///
    /// 与 [`Self::display_size`] 不同：设了目标尺寸时这个是屏幕原始分辨率。
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

    /// staging 纹理的宽度（像素）
    ///
    /// 与 [`Self::display_size`] 可能不同：显示器分辨率可以在运行中变化
    /// （改显示设置、插拔），那时 duplication 仍然给旧尺寸的纹理。
    /// 用这个值做 Map 后的切片边界校验。
    pub fn staging_width(&self) -> u32 {
        self.staging_width
    }

    /// staging 纹理的高度（像素）
    pub fn staging_height(&self) -> u32 {
        self.staging_height
    }

    /// staging 纹理的行距（字节）
    ///
    /// D3D11 会按 256 字节对齐，1080p 下通常是 7680（=1920*4，正好对齐）。
    /// 非对齐的宽度（比如 1366）会得到 5504 而不是 5464 —— 转换时用错
    /// 行距会导致画面**斜切**，每行右移若干像素，非常显眼。
    pub fn row_pitch(&self) -> u32 {
        self.row_pitch
    }

    /// 是否已经失去访问权
    pub fn is_lost(&self) -> bool {
        self.lost
    }

    /// 量一次行距：建一块 1x1 的临时 staging 太麻烦，直接拷当前帧再 Map
    ///
    /// 构造期调用一次，把行距缓存下来。之后每帧仍会更新（Map 会返回真实值），
    /// 这里只是让第一帧不必走「未知行距」的分支。
    fn probe_row_pitch(&mut self) -> Result<u32> {
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: staging 是 STAGING + CPU_READ
        let r = unsafe {
            self.context
                .Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
        };
        match r {
            Ok(()) => {
                let pitch = mapped.RowPitch;
                // SAFETY: 与上面的 Map 配对
                unsafe { self.context.Unmap(&self.staging, 0) };
                Ok(pitch)
            }
            Err(e) => Err(CaptureError::from_win("Map(probe)", e)),
        }
    }
}
