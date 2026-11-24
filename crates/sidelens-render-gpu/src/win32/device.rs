//! D3D11 设备、DXGI 适配器与视频处理器工厂
//!
//! # 为什么把「设备」和「渲染」拆开
//!
//! [`GpuDevice`] 只负责「拿到一个能跑视频处理器的 D3D11 设备」，不知道窗口、
//! 不知道 swap chain。这样离屏（offscreen）和真窗口（window）两条路径共用同一份
//! 设备创建代码 —— 而设备创建恰恰是最容易在不同机器上失败的环节（无独显、
//! 远程桌面、驱动太老），必须只有一份实现、一处日志。
//!
//! # 为什么一定要 D3D11_CREATE_DEVICE_VIDEO_SUPPORT
//!
//! `ID3D11VideoDevice` 在 D3D11 里是**可选**的：不带这个 flag 创建的设备
//! `QueryInterface` 到 `ID3D11VideoDevice` 会返回 `E_NOINTERFACE`。
//! 而本 crate 的整条色彩转换链都依赖视频处理器，所以这个 flag 不是优化，
//! 是硬性前提。
//!
//! # 为什么硬件失败要退到 WARP
//!
//! 远程桌面、无独显的虚拟机、被策略禁用的驱动 —— 这些环境里
//! `D3D11CreateDevice(HARDWARE)` 会直接失败。WARP 是微软的软件光栅化器，
//! 它的视频处理器是纯 CPU 实现：慢一个数量级，但**语义完全一致**，
//! 所以颜色、裁剪、swap chain 这些逻辑照样能验证。用 WARP 跑通总比黑屏好，
//! 但必须通过 [`GpuDevice::is_software`] 如实告诉调用方，不能装作一切正常。

use windows::core::Interface;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE, D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL,
    D3D_FEATURE_LEVEL_10_0, D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11VideoDevice,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter, IDXGIFactory1};

use crate::error::GpuError;

/// 一次创建好的 D3D11 设备
///
/// 持有 device / immediate context / video device 三件套。`video` 是
/// `QueryInterface` 得到的 `ID3D11VideoDevice`，视频处理器相关的所有对象
/// （enumerator、processor、input/output view）都由它创建。
pub struct GpuDevice {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    video: ID3D11VideoDevice,
    adapter_name: String,
    /// 是否落到了 WARP（软件光栅化）
    ///
    /// 远程桌面 / 无独显的虚拟机里硬件设备会创建失败，WARP 能跑通视频处理器
    /// 但会慢一个数量级。调用方据此决定要不要降级到 GDI —— 所以这个标志
    /// 必须暴露出去，不能悄悄用 WARP 装作一切正常。
    software: bool,
}

impl std::fmt::Debug for GpuDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GpuDevice")
            .field("adapter", &self.adapter_name)
            .field("software", &self.software)
            .finish_non_exhaustive()
    }
}

impl GpuDevice {
    /// 创建硬件设备；失败时尝试 WARP
    ///
    /// 返回的 [`GpuDevice::is_software`] 为 `true` 说明落到了 WARP。
    pub fn new() -> Result<Self, GpuError> {
        match Self::create(D3D_DRIVER_TYPE_HARDWARE) {
            Ok(d) => Ok(d),
            Err(hw_err) => {
                // 硬件路径失败时退到 WARP。
                // 两条都失败时**报硬件那条**的错：那才是用户环境的问题所在，
                // WARP 失败通常是连软件光栅化都被策略禁用了。
                match Self::create(D3D_DRIVER_TYPE_WARP) {
                    Ok(d) => Ok(d),
                    Err(_) => Err(hw_err),
                }
            }
        }
    }

    fn create(driver: D3D_DRIVER_TYPE) -> Result<Self, GpuError> {
        let flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT;
        // 特性级别从高到低试。视频处理器要求至少 10_0；11_1 在 Win7 上不存在，
        // 所以列表里必须有低档兜底，否则老系统直接 E_INVALIDARG。
        let levels = [
            D3D_FEATURE_LEVEL_11_1,
            D3D_FEATURE_LEVEL_11_0,
            D3D_FEATURE_LEVEL_10_1,
            D3D_FEATURE_LEVEL_10_0,
        ];

        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        let mut level = D3D_FEATURE_LEVEL::default();

        // 传 None 作为 adapter = 用默认适配器（系统选的那块）
        let hr = unsafe {
            D3D11CreateDevice(
                None,
                driver,
                HMODULE::default(),
                flags,
                Some(&levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                Some(&mut level),
                Some(&mut context),
            )
        };
        hr.map_err(|e| {
            let which = if driver == D3D_DRIVER_TYPE_WARP {
                "WARP"
            } else {
                "HARDWARE"
            };
            GpuError::Device(format!(
                "D3D11CreateDevice(driver={which}) 失败：{} (0x{:08X})",
                e.message(),
                e.code().0
            ))
        })?;

        let device =
            device.ok_or_else(|| GpuError::Device("D3D11CreateDevice 未返回设备".into()))?;
        let context =
            context.ok_or_else(|| GpuError::Device("D3D11CreateDevice 未返回上下文".into()))?;

        // 没有 ID3D11VideoDevice 就没有视频处理器 —— 整条链路的前提
        let video: ID3D11VideoDevice = device.cast().map_err(|e| {
            GpuError::Device(format!(
                "设备不支持 ID3D11VideoDevice（{}）：缺少 D3D11_CREATE_DEVICE_VIDEO_SUPPORT \
                 或驱动版本过低",
                e.message()
            ))
        })?;

        let adapter_name = Self::query_adapter_name(&device)
            .unwrap_or_else(|| format!("未知适配器（feature level 0x{:x}）", level.0));

        Ok(GpuDevice {
            device,
            context,
            video,
            adapter_name,
            software: driver == D3D_DRIVER_TYPE_WARP,
        })
    }

    /// 通过 device → IDXGIDevice → adapter → GetDesc 拿显卡名
    ///
    /// 失败返回 `None` 而不是错误：适配器名字只是给人看的，
    /// 拿不到不该阻止渲染。
    fn query_adapter_name(device: &ID3D11Device) -> Option<String> {
        let dxgi_device: windows::Win32::Graphics::Dxgi::IDXGIDevice = device.cast().ok()?;
        let adapter: IDXGIAdapter = unsafe { dxgi_device.GetAdapter() }.ok()?;
        let desc = unsafe { adapter.GetDesc() }.ok()?;
        Some(utf16_to_string(&desc.Description))
    }

    /// 枚举所有适配器名字（诊断用）
    ///
    /// 示例在 GPU 创建失败时会打印这份列表，让用户一眼看到
    /// 「机器上到底有没有可用显卡」。
    pub fn list_adapters() -> Vec<String> {
        let mut out = Vec::new();
        let factory: IDXGIFactory1 = match unsafe { CreateDXGIFactory1() } {
            Ok(f) => f,
            Err(_) => return out,
        };
        let mut i = 0u32;
        loop {
            let adapter: IDXGIAdapter = match unsafe { factory.EnumAdapters(i) } {
                Ok(a) => a,
                Err(_) => break,
            };
            if let Ok(desc) = unsafe { adapter.GetDesc() } {
                out.push(utf16_to_string(&desc.Description));
            }
            i += 1;
            if i > 16 {
                break;
            }
        }
        out
    }

    /// D3D11 设备
    pub fn device(&self) -> &ID3D11Device {
        &self.device
    }

    /// 立即上下文
    pub fn context(&self) -> &ID3D11DeviceContext {
        &self.context
    }

    /// 视频设备（创建 processor / view 的入口）
    pub fn video(&self) -> &ID3D11VideoDevice {
        &self.video
    }

    /// 适配器名字
    pub fn adapter_name(&self) -> &str {
        &self.adapter_name
    }

    /// 是否落到了 WARP（软件光栅化）
    pub fn is_software(&self) -> bool {
        self.software
    }
}

/// UTF-16（NUL 结尾）→ String
///
/// 适配器描述是定长 `[u16; 128]`，尾部可能全是 0，也可能刚好填满没有 0 ——
/// 所以先截到第一个 0，再解码。
fn utf16_to_string(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_stops_at_nul() {
        let buf = [0x41u16, 0x42, 0, 0x43];
        assert_eq!(utf16_to_string(&buf), "AB");
    }

    #[test]
    fn utf16_handles_unterminated_buffer() {
        let buf = [0x4Eu16, 0x56];
        assert_eq!(utf16_to_string(&buf), "NV");
    }
}
