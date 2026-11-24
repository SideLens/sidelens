//! Windows GPU 渲染后端（D3D11 + DXGI flip-model）
//!
//! 这个模块是 `cfg(windows)` 门控的，非 Windows 上整个不存在 ——
//! 跨平台的几何/统计/位域逻辑全在 crate 根下的 `layout` / `stats` / `color`。
//!
//! # 子模块分工
//!
//! | 模块 | 职责 |
//! | --- | --- |
//! | `device` | D3D11 设备 + `ID3D11VideoDevice`（硬件失败退 WARP） |
//! | `processor` | NV12 上传 + `ID3D11VideoProcessor` 的 YUV→RGB |
//! | `swapchain` | DXGI flip-model swap chain + 帧延迟控制 |
//! | `window` | 最简 Win32 窗口 + 消息泵 |
//! | `timer` | D3D11 timestamp query 测 GPU 耗时 |
//! | `readback` | 离屏读回（BGRA 纹理 → staging → CPU） |
//!
//! # 为什么 `dxgi_factory` 在这里而不是 device 里
//!
//! 它是 `swapchain` 和 `device` 都要用的公共设施（一个要工厂造 swap chain、
//! 一个要工厂枚举适配器），放在任一方都会造成循环依赖。

pub mod device;
pub mod processor;
pub mod readback;
pub mod renderer;
pub mod swapchain;
pub mod timer;
pub mod window;

use windows::core::Interface;
use windows::Win32::Graphics::Dxgi::{
    IDXGIFactory2, IDXGIFactory5, DXGI_CREATE_FACTORY_FLAGS, DXGI_FEATURE_PRESENT_ALLOW_TEARING,
};

use crate::error::GpuError;

/// 拿一个 `IDXGIFactory2`（造 flip-model swap chain 的最低要求）
pub fn dxgi_factory() -> Result<IDXGIFactory2, GpuError> {
    // CreateDXGIFactory2 比 CreateDXGIFactory1 多一个 flags 参数，
    // 传 0 = 不开调试层（生产路径不该有调试层开销）。
    let factory: IDXGIFactory2 =
        unsafe { windows::Win32::Graphics::Dxgi::CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)) }
            .map_err(|e| GpuError::SwapChain(format!("CreateDXGIFactory2：{}", e.message())))?;
    Ok(factory)
}

/// 查询驱动是否支持 `ALLOW_TEARING`
///
/// 不支持时必须**不要**带那个标志去 `CreateSwapChainForHwnd` ——
/// 带了会直接 `DXGI_ERROR_INVALID_CALL`，整条 GPU 路径都建不起来。
/// 远程桌面、部分虚拟显卡、以及 Win7 都返回不支持。
pub fn tearing_supported(factory: &IDXGIFactory2) -> bool {
    let f5: IDXGIFactory5 = match factory.cast() {
        Ok(f) => f,
        Err(_) => return false,
    };
    let mut supported: windows::core::BOOL = false.into();
    let hr = unsafe {
        f5.CheckFeatureSupport(
            DXGI_FEATURE_PRESENT_ALLOW_TEARING,
            &mut supported as *mut _ as *mut std::ffi::c_void,
            std::mem::size_of::<windows::core::BOOL>() as u32,
        )
    };
    hr.is_ok() && supported.as_bool()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn factory_creation_reports_error_not_panic() {
        // 这个测试在无 DXGI 的环境里应该拿到 Err 而不是崩 —— 主要是钉住
        // 「创建失败要变成错误值」这条约定。正常桌面环境会成功。
        match dxgi_factory() {
            Ok(_) => {}
            Err(e) => {
                let s = e.to_string();
                assert!(
                    s.contains("CreateDXGIFactory2"),
                    "错误信息要能定位到 API：{s}"
                );
            }
        }
    }
}
