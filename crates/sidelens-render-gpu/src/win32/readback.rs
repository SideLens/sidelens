//! 离屏读回：把 GPU 转换后的 BGRA 拉回 CPU
//!
//! # 为什么需要它
//!
//! 窗口模式下 `present` 的成功只代表「命令提交成功」—— 画面到底对不对，
//! 肉眼才能验证。而 CI 和 `cargo test` 没有眼睛。所以离屏路径必须能把
//! 像素读回来，否则「GPU 真的做了 YUV→RGB 且矩阵/range/裁剪都对」这句话
//! 就没有任何证据。
//!
//! 测试用**已知的纯色 NV12**：`(Y=16, UV=128)` 是 limited range 的黑，
//! `(Y=235, UV=128)` 是白。读回的 BGRA 若在 0±5 / 255±5 内，就同时证明了：
//!
//! 1. 色彩矩阵选对了（选错 BT.601 会偏色）；
//! 2. range 处理对了（把 limited 当 full 解，16 会变成 16 而不是 0）；
//! 3. 裁剪生效了（1088 的填充行若上屏，底部会有非零像素）；
//! 4. 通道顺序对了（B8G8R8A8 的 B 和 R 若反了，纯灰测试看不出来，
//!    所以另有一组彩色断言）。
//!
//! # 为什么要 staging 中转
//!
//! 渲染目标是 `D3D11_USAGE_DEFAULT`（GPU 独占），**不能 Map**。
//! 必须 `CopyResource` 到一块 `STAGING + CPU_ACCESS_READ` 的纹理再 Map。
//! 这是一次额外的显存拷贝 —— 但只发生在测试/截图路径，正常呈现不付这个成本。

use windows::Win32::Graphics::Direct3D11::{
    ID3D11DeviceContext, ID3D11Texture2D, D3D11_CPU_ACCESS_READ, D3D11_MAPPED_SUBRESOURCE,
    D3D11_MAP_READ, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};

use super::device::GpuDevice;
use crate::error::{hresult, GpuError};

/// 读回的一块 BGRA 图像
///
/// `pixels` 是**紧凑**的（`stride == width * 4`）：staging 纹理的
/// `RowPitch` 通常大于这个值（对齐填充），所以读回时逐行拷紧 ——
/// 否则调用方按 `width*4` 算偏移会错位，表现为「画面斜切」。
#[derive(Debug, Clone)]
pub struct ReadbackImage {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}

impl ReadbackImage {
    /// 宽
    pub fn width(&self) -> u32 {
        self.width
    }

    /// 高
    pub fn height(&self) -> u32 {
        self.height
    }

    /// 紧凑 BGRA 像素
    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }

    /// 行距（紧凑布局下恒为 `width * 4`）
    pub fn stride(&self) -> usize {
        self.width as usize * 4
    }

    /// 取一个像素（越界返回 `None`）
    ///
    /// 先按 **width / height** 校验，再交给 `bgra_pixel` 做扁平缓冲校验 ——
    /// 只靠扁平长度是不够的：`(width, 0)` 的偏移正好等于第二行行首，
    /// 落在缓冲内，但它是越界的像素。只查长度会静默返回**下一行第一个像素**，
    /// 表现为「右边多出一列」。这个 bug 只有在 width 不是 4 的倍数时才明显，
    /// 所以必须在类型层面挡住。
    pub fn pixel(&self, x: u32, y: u32) -> Option<[u8; 4]> {
        if x >= self.width || y >= self.height {
            return None;
        }
        crate::layout::bgra_pixel(&self.pixels, self.stride(), x, y)
    }

    /// 全图的最大 / 最小通道值（诊断用：纯色帧这两个数应该几乎相等）
    pub fn channel_range(&self) -> Option<(u8, u8)> {
        let mut lo = 255u8;
        let mut hi = 0u8;
        // 跳过 alpha 通道：B8G8R8A8 的 A 可能是 0 也可能是 255，
        // 取决于驱动，拿它做断言没有意义
        for px in self.pixels.chunks_exact(4) {
            for &c in &px[..3] {
                lo = lo.min(c);
                hi = hi.max(c);
            }
        }
        if self.pixels.is_empty() {
            None
        } else {
            Some((lo, hi))
        }
    }
}

/// 把一块 GPU 纹理读回 CPU
pub fn read_texture(
    device: &GpuDevice,
    texture: &ID3D11Texture2D,
    width: u32,
    height: u32,
) -> Result<ReadbackImage, GpuError> {
    if width == 0 || height == 0 {
        return Err(GpuError::ZeroSize { width, height });
    }

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
    let mut staging: Option<ID3D11Texture2D> = None;
    unsafe {
        device
            .device()
            .CreateTexture2D(&desc, None, Some(&mut staging))
    }
    .map_err(|e| GpuError::Texture(hresult("CreateTexture2D(readback staging)", &e)))?;
    let staging = staging
        .ok_or_else(|| GpuError::Texture("CreateTexture2D(readback staging) 未返回纹理".into()))?;

    let context: &ID3D11DeviceContext = device.context();
    unsafe { context.CopyResource(&staging, texture) };

    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    unsafe { context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)) }
        .map_err(|e| GpuError::Texture(hresult("Map(readback staging)", &e)))?;

    let copy = {
        if mapped.pData.is_null() {
            Err(GpuError::Texture("Map(readback staging) 返回空指针".into()))
        } else {
            let pitch = mapped.RowPitch as usize;
            let row_bytes = width as usize * 4;
            let mut out = vec![0u8; row_bytes * height as usize];
            // SAFETY: D3D11 保证 pData 在 Unmap 前指向 RowPitch * height 字节
            // 的可读内存；只读前 row_bytes（<= RowPitch，因为 D3D11 的
            // RowPitch 至少等于宽度字节数），不越界。
            let src = unsafe {
                std::slice::from_raw_parts(
                    mapped.pData as *const u8,
                    pitch.saturating_mul(height as usize),
                )
            };
            for r in 0..height as usize {
                let s = r * pitch;
                let d = r * row_bytes;
                out[d..d + row_bytes].copy_from_slice(&src[s..s + row_bytes]);
            }
            Ok(ReadbackImage {
                width,
                height,
                pixels: out,
            })
        }
    };

    unsafe { context.Unmap(&staging, 0) };
    copy
}

#[cfg(test)]
mod tests {
    use super::*;

    fn img(w: u32, h: u32, fill: u8) -> ReadbackImage {
        ReadbackImage {
            width: w,
            height: h,
            pixels: vec![fill; (w * h * 4) as usize],
        }
    }

    #[test]
    fn stride_is_tight() {
        let i = img(4, 2, 7);
        assert_eq!(i.stride(), 16);
        assert_eq!(i.pixels().len(), 32);
    }

    #[test]
    fn pixel_access_is_bounds_checked() {
        let i = img(2, 2, 9);
        assert_eq!(i.pixel(0, 0), Some([9, 9, 9, 9]));
        assert_eq!(i.pixel(2, 0), None);
        assert_eq!(i.pixel(0, 2), None);
    }

    #[test]
    fn channel_range_ignores_alpha() {
        let mut i = img(2, 1, 100);
        // 只改 alpha，范围不应变化
        i.pixels[3] = 0;
        i.pixels[7] = 255;
        assert_eq!(i.channel_range(), Some((100, 100)));
    }

    #[test]
    fn channel_range_on_empty_is_none() {
        let i = ReadbackImage {
            width: 0,
            height: 0,
            pixels: Vec::new(),
        };
        assert_eq!(i.channel_range(), None);
    }
}
