//! DXGI flip-model swap chain
//!
//! # 为什么必须 flip-model，而不是 blt-model
//!
//! DXGI 有两代呈现模型：
//!
//! | | blt-model | flip-model |
//! | --- | --- | --- |
//! | 效果常量 | `DXGI_SWAP_EFFECT_DISCARD` | `DXGI_SWAP_EFFECT_FLIP_DISCARD` |
//! | 呈现路径 | 先 blt 到 DWM 的共享表面，DWM 再合成 | 直接把后缓冲**翻**给 DWM |
//! | 额外拷贝 | **有**，一整个屏幕大小 | 无 |
//! | 1080p 每帧成本 | 额外一次 ~8MB 的显存拷贝 | 只是换一个指针 |
//! | Win8+ 全屏优化 | 不参与 | 参与（可绕过 DWM 直接扫出） |
//!
//! 本项目的要求是「超低延迟 + 极致原生 UI 性能」，blt-model 那一趟额外拷贝
//! 在 1080p 上就是实打实的几毫秒，而且它发生在 DWM 线程里、不受我们控制。
//! 所以必须 flip。
//!
//! # 为什么 BufferCount 至少是 2
//!
//! flip-model 要求 `BufferCount >= 2`，而且**实际可用队列深度**由
//! `SetMaximumFrameLatency` 控制，不是由 BufferCount 控制。见下一条。
//!
//! # 为什么必须 SetMaximumFrameLatency(1)
//!
//! DXGI 默认的最大帧延迟是 **3**：Present 之后最多可以有 3 帧在队列里排队。
//! 对播放器来说这无所谓（画面早晚会出来），但对**实时共享**来说这就是
//! 「多出 2 帧的延迟」—— 60fps 下 33ms，远超整条链路的预算。
//!
//! 压到 1 之后，`Present` 会阻塞到上一帧真正被消费，形成背压：
//! 生产端（解码线程）自然被节流到显示器刷新率，既不会堆积也不会空转。
//!
//! # 为什么要 FRAME_LATENCY_WAITABLE_OBJECT
//!
//! 光设 `SetMaximumFrameLatency(1)` 还不够：`Present` 在队列满时是
//! **阻塞**调用，会占住渲染线程，期间连窗口消息都处理不了（拖动窗口就卡）。
//! 带上 `DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT` 之后可以拿到
//! 一个可等待句柄，先 `WaitForSingleObject(0)` 试探、没空位就跳过这一帧，
//! 渲染线程永远不会被 GPU 拖住。
//!
//! # 为什么允许 tearing
//!
//! `DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING` + `Present(0, ALLOW_TEARING)`
//! 可以绕开垂直同步，把「等下一个刷新周期」的平均 8ms 延迟省掉。代价是
//! 画面可能撕裂 —— 对「看自己手机屏幕」这种场景，**延迟比无撕裂重要**，
//! 所以默认开启，但用 `tearing` 标志如实报告给调用方（有些驱动/远程桌面
//! 不支持，会被静默忽略）。

use windows::core::Interface;
use windows::Win32::Foundation::{HWND, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_IGNORE, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    IDXGIFactory2, IDXGISwapChain2, IDXGISwapChain3, DXGI_PRESENT, DXGI_PRESENT_ALLOW_TEARING,
    DXGI_SCALING_STRETCH, DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING,
    DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT, DXGI_SWAP_EFFECT_FLIP_DISCARD,
    DXGI_USAGE_RENDER_TARGET_OUTPUT,
};
use windows::Win32::System::Threading::WaitForSingleObject;

use super::device::GpuDevice;
use crate::error::{hresult, GpuError};

/// swap chain 的呈现格式
///
/// 固定 `B8G8R8A8_UNORM`：DWM 的原生格式，视频处理器的输出格式，
/// 也是 `ID3D11VideoProcessorEnumerator::CheckVideoProcessorFormat`
/// 在所有驱动上都支持的格式。换成 RGBA 会多一次通道交换。
pub const SWAPCHAIN_FORMAT: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT =
    DXGI_FORMAT_B8G8R8A8_UNORM;

/// 呈现格式的人类可读名字（给 `GpuRenderer::swapchain_format` 用）
pub const SWAPCHAIN_FORMAT_NAME: &str = "B8G8R8A8_UNORM";

/// flip-model swap chain 的封装
pub struct SwapChain {
    chain: IDXGISwapChain3,
    /// 帧延迟等待句柄；不支持 waitable 时为 0
    waitable: isize,
    width: u32,
    height: u32,
    /// 是否真的带上了 tearing 标志（驱动可能拒绝）
    tearing: bool,
}

impl std::fmt::Debug for SwapChain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SwapChain")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("tearing", &self.tearing)
            .finish_non_exhaustive()
    }
}

impl SwapChain {
    /// 为窗口创建 flip-model swap chain
    pub fn for_window(
        device: &GpuDevice,
        hwnd: HWND,
        width: u32,
        height: u32,
    ) -> Result<Self, GpuError> {
        if width == 0 || height == 0 {
            return Err(GpuError::ZeroSize { width, height });
        }

        let factory: IDXGIFactory2 = super::dxgi_factory()
            .map_err(|e| GpuError::SwapChain(format!("拿 IDXGIFactory2 失败：{e}")))?;

        let tearing = super::tearing_supported(&factory);
        let mut flags = DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT.0 as u32;
        if tearing {
            flags |= DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING.0 as u32;
        }

        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: width,
            Height: height,
            Format: SWAPCHAIN_FORMAT,
            Stereo: false.into(),
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            // flip-model 最少 2 个缓冲。用 2 而不是 3：缓冲越少，
            // 队列里能堆积的帧越少，延迟越低。
            BufferCount: 2,
            // STRETCH：窗口尺寸和后缓冲不一致时由 DWM 拉伸，不报错。
            // 画面内容我们自己按 1:1 画，所以实际不会触发缩放。
            Scaling: DXGI_SCALING_STRETCH,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
            AlphaMode: DXGI_ALPHA_MODE_IGNORE,
            Flags: flags,
        };

        let chain1 =
            unsafe { factory.CreateSwapChainForHwnd(device.device(), hwnd, &desc, None, None) }
                .map_err(|e| GpuError::SwapChain(hresult("CreateSwapChainForHwnd(flip)", &e)))?;

        let chain: IDXGISwapChain3 = chain1
            .cast()
            .map_err(|e| GpuError::SwapChain(hresult("cast IDXGISwapChain3", &e)))?;

        // 压队列深度。失败不致命（有些驱动会拒绝），但必须记下来 ——
        // 这是「延迟为什么比预期高」的第一嫌疑人。
        let chain2: IDXGISwapChain2 = chain
            .cast()
            .map_err(|e| GpuError::SwapChain(hresult("cast IDXGISwapChain2", &e)))?;
        if let Err(e) = unsafe { chain2.SetMaximumFrameLatency(1) } {
            // 不 return：默认值 3 也能跑，只是多两帧延迟
            eprintln!(
                "[sidelens-render-gpu] SetMaximumFrameLatency(1) 失败：{}",
                e.message()
            );
        }
        // 句柄 0 表示驱动不支持可等待对象，wait_for_frame 会退化成「恒 true」
        let waitable = unsafe { chain2.GetFrameLatencyWaitableObject() }.0 as isize;

        Ok(SwapChain {
            chain,
            waitable,
            width,
            height,
            tearing,
        })
    }

    /// 为离屏渲染创建 swap chain —— **不存在**这种对象
    ///
    /// 离屏路径不用 swap chain（没有窗口可翻），而是渲染到一块普通的
    /// `B8G8R8A8_UNORM` 纹理再拷贝到 staging。这个函数存在的唯一理由是
    /// 让调用方在文档里一眼看到「离屏没有 swap chain」，而不是去猜。
    pub fn offscreen_has_no_swapchain() -> &'static str {
        "offscreen 模式不创建 swap chain，直接渲染到 RT 纹理后读回"
    }

    /// 后缓冲索引
    pub fn current_index(&self) -> u32 {
        unsafe { self.chain.GetCurrentBackBufferIndex() }
    }

    /// 取后缓冲纹理（每帧取，不缓存 —— 索引会变）
    pub fn back_buffer(&self) -> Result<ID3D11Texture2D, GpuError> {
        unsafe { self.chain.GetBuffer(0) }
            .map_err(|e| GpuError::SwapChain(hresult("GetBuffer(0)", &e)))
    }

    /// 等到可以再交一帧
    ///
    /// - 有 waitable 句柄：非阻塞探测，队列满返回 `false`（调用方跳过这帧）
    /// - 没有：恒 `true`（只能让 `Present` 自己阻塞）
    ///
    /// # 为什么要非阻塞
    ///
    /// 解码线程不应该被 GPU 队列拖住：跳过一帧远好过卡住整条管线。
    /// 这也是「超低延迟」的必要条件 —— 队列里堆积的帧就是延迟。
    pub fn wait_for_frame(&self) -> bool {
        if self.waitable == 0 {
            return true;
        }
        let handle = windows::Win32::Foundation::HANDLE(self.waitable as *mut std::ffi::c_void);
        let r = unsafe { WaitForSingleObject(handle, 0) };
        r == WAIT_OBJECT_0
    }

    /// 呈现
    ///
    /// `sync_interval` 恒为 0：垂直同步由 waitable 句柄控制，
    /// 再用 sync interval 就是双重等待。
    pub fn present(&self) -> Result<(), GpuError> {
        let flags = if self.tearing {
            DXGI_PRESENT_ALLOW_TEARING
        } else {
            DXGI_PRESENT(0)
        };
        let hr = unsafe { self.chain.Present(0, flags) };
        // Present 返回 HRESULT 而不是 Result：S_OK 之外的都要当失败，
        // 但 DXGI_STATUS_OCCLUDED 是「窗口被遮住了」，不是错误。
        if hr.is_err() {
            let e = windows::core::Error::from_hresult(hr);
            return Err(GpuError::SwapChain(hresult("Present", &e)));
        }
        Ok(())
    }

    /// 窗口尺寸变化后重建后缓冲
    pub fn resize(&mut self, width: u32, height: u32) -> Result<(), GpuError> {
        if width == 0 || height == 0 {
            return Err(GpuError::ZeroSize { width, height });
        }
        if width == self.width && height == self.height {
            return Ok(());
        }
        // ResizeBuffers 要求调用前释放所有对后缓冲的引用（view / texture），
        // 所以调用方必须先 drop 掉 output view 和 back buffer。
        unsafe {
            self.chain
                .ResizeBuffers(0, width, height, SWAPCHAIN_FORMAT, self.flags())
        }
        .map_err(|e| GpuError::SwapChain(hresult("ResizeBuffers", &e)))?;
        self.width = width;
        self.height = height;
        Ok(())
    }

    fn flags(&self) -> windows::Win32::Graphics::Dxgi::DXGI_SWAP_CHAIN_FLAG {
        let mut f = DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT;
        if self.tearing {
            f |= DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING;
        }
        f
    }

    /// 后缓冲尺寸
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// 是否带 tearing 标志
    pub fn tearing(&self) -> bool {
        self.tearing
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_name_matches_constant() {
        assert_eq!(SWAPCHAIN_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM);
        assert_eq!(SWAPCHAIN_FORMAT_NAME, "B8G8R8A8_UNORM");
    }

    #[test]
    fn flip_discard_is_not_blt_model() {
        // 钉住「必须是 flip」这个决策：DISCARD(1) / SEQUENTIAL(3) 是 blt-model，
        // FLIP_DISCARD(4) / FLIP_SEQUENTIAL(3) 才是 flip-model。
        assert_eq!(DXGI_SWAP_EFFECT_FLIP_DISCARD.0, 4);
    }

    #[test]
    fn tearing_flag_value_is_documented() {
        assert_eq!(DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING.0, 0x800);
        assert_eq!(DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT.0, 0x40);
    }

    #[test]
    fn offscreen_has_no_swapchain_message_is_stable() {
        assert!(SwapChain::offscreen_has_no_swapchain().contains("swap chain"));
    }
}
