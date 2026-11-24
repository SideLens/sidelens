//! NV12 上传 + `ID3D11VideoProcessor` 的 YUV→RGB
//!
//! # 为什么用视频处理器而不是自己写 pixel shader
//!
//! `ID3D11VideoDevice::CreateVideoProcessor` + `ID3D11VideoProcessorBlt`：
//!
//! - 自动处理 BT.709 / BT.601 色彩矩阵（`VideoProcessorSetStreamColorSpace`）
//! - 自动处理 limited / full range
//! - 自动处理 4:2:0 的色度上采样（这是最容易写错的部分）
//! - 硬件固定功能单元，几乎零开销
//!
//! 自己写 shader 也能做，但要手写矩阵、处理 range、处理色度上采样 ——
//! 每一处都能「跑起来但颜色不对」，而颜色错误在单端测试里看不出来
//! （编码和解码各自都「对」）。
//!
//! # NV12 上传为什么这么绕
//!
//! 两条看起来可行的路都是错的：
//!
//! | 尝试 | 结果 |
//! | --- | --- |
//! | 直接 `Map` NV12 纹理 | `D3D11_USAGE_DEFAULT` 不允许 Map |
//! | 用 `R8_UNORM`（高 1.5x）中转再 `CopyResource` | 格式不一致，一律 `E_INVALIDARG` |
//!
//! 正确做法：给 NV12 纹理建一个 **STAGING** 副本，`Map` 后**逐行写**
//! （驱动给的 `RowPitch` 可能大于宽度，多出来的是对齐填充，不能覆盖），
//! 再 `CopyResource` 到真正的 NV12 纹理。

use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11Texture2D, ID3D11VideoContext, ID3D11VideoDevice, ID3D11VideoProcessor,
    ID3D11VideoProcessorEnumerator, ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView,
    D3D11_BIND_DECODER, D3D11_BIND_SHADER_RESOURCE, D3D11_CPU_ACCESS_WRITE,
    D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_WRITE, D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING,
    D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_CONTENT_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
    D3D11_VPIV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};

use super::device::GpuDevice;
use crate::color::ColorSpace;
use crate::error::GpuError;
use crate::layout::Nv12Layout;

/// 把 `Rect` 转成 Win32 `RECT`
fn to_rect(r: crate::layout::Rect) -> windows::Win32::Foundation::RECT {
    windows::Win32::Foundation::RECT {
        left: r.left,
        top: r.top,
        right: r.right,
        bottom: r.bottom,
    }
}

/// 一块 NV12 纹理 + 它的 staging 副本
///
/// # 为什么要单独一个类型
///
/// 上传涉及「staging 纹理 → Map → 逐行写 → Unmap → CopyResource」四步，
/// 每一步都必须配对（漏 Unmap 会让驱动一直锁着这块内存）。
/// 把配对关系收在一个类型里，调用方只看到 `upload()`。
pub struct Nv12Texture {
    /// 真正的 NV12 纹理（视频处理器的输入）
    texture: ID3D11Texture2D,
    /// CPU 可写的 staging 副本
    staging: ID3D11Texture2D,
    layout: Nv12Layout,
}

impl std::fmt::Debug for Nv12Texture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Nv12Texture")
            .field("layout", &self.layout)
            .finish_non_exhaustive()
    }
}

impl Nv12Texture {
    /// 建 NV12 纹理 + staging 副本
    ///
    /// `BindFlags` 必须含 `D3D11_BIND_DECODER` —— 视频处理器的输入视图
    /// 要求纹理带这个标志，缺了 `CreateVideoProcessorInputView` 会返回
    /// `E_INVALIDARG` 且不提原因。
    pub fn new(device: &ID3D11Device, layout: Nv12Layout) -> Result<Self, GpuError> {
        let (cw, ch) = layout.coded();

        let desc = D3D11_TEXTURE2D_DESC {
            Width: cw,
            Height: ch,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_DECODER.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut texture: Option<ID3D11Texture2D> = None;
        // SAFETY: desc 是本函数栈上值；texture 是出参。
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }
            .map_err(|e| GpuError::Texture(format!("CreateTexture2D(NV12)：{}", e.message())))?;
        let texture = texture.ok_or_else(|| GpuError::Texture("CreateTexture2D 返回空".into()))?;

        // staging：同样的 NV12 格式，但 CPU 可写。
        //
        // # 高度必须是 `coded_h`，不是 `staging_rows()`
        //
        // 这是我重写时踩的真 bug：`CopyResource` 要求源/目标**尺寸与格式
        // 完全一致**，而 staging 用了 `coded_h * 1.5` 当高度，与 NV12 纹理的
        // `coded_h` 不符 —— `CopyResource` 静默失败（不返回错误！），
        // 于是视频处理器读到的是一块从未被写入的纹理。
        //
        // 实测症状：黑色 NV12（Y=16）读回来 G=77 而不是 0。
        // 那个 77 是「读了一块未初始化内存」的结果，不是色彩矩阵问题。
        //
        // 1.5 倍只体现在**内存布局**里：`Map` 出来的 `pData` 覆盖
        // `row_pitch * coded_h * 1.5` 字节，但纹理描述的高度仍是 `coded_h`。
        let staging_desc = D3D11_TEXTURE2D_DESC {
            Width: cw,
            Height: ch,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
            MiscFlags: 0,
        };
        let mut staging: Option<ID3D11Texture2D> = None;
        // SAFETY: 同上。
        unsafe { device.CreateTexture2D(&staging_desc, None, Some(&mut staging)) }
            .map_err(|e| GpuError::Texture(format!("CreateTexture2D(staging)：{}", e.message())))?;
        let staging = staging.ok_or_else(|| GpuError::Texture("staging 纹理为空".into()))?;

        Ok(Self {
            texture,
            staging,
            layout,
        })
    }

    /// 把紧凑 NV12 上传到纹理
    ///
    /// `nv12` 是解码器输出的紧凑布局（`stride == coded_width`）。
    pub fn upload(&mut self, device: &GpuDevice, nv12: &[u8]) -> Result<(), GpuError> {
        if nv12.len() < self.layout.frame_len() {
            return Err(GpuError::Texture(format!(
                "NV12 数据不足：需要 {} 字节，实际 {}",
                self.layout.frame_len(),
                nv12.len()
            )));
        }

        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: staging 有效；mapped 是出参；用完必须 Unmap（下面配对）。
        unsafe {
            device
                .context()
                .Map(&self.staging, 0, D3D11_MAP_WRITE, 0, Some(&mut mapped))
        }
        .map_err(|e| GpuError::Texture(format!("Map(staging)：{}", e.message())))?;

        // SAFETY: Map 成功时 pData 指向 row_pitch * staging_rows 的可写内存。
        let result = unsafe {
            let dst = std::slice::from_raw_parts_mut(
                mapped.pData as *mut u8,
                mapped.RowPitch as usize * self.layout.staging_rows(),
            );
            self.layout
                .scatter_to_rows(nv12, dst, mapped.RowPitch as usize)
        };

        // SAFETY: 与上面的 Map 配对。
        unsafe { device.context().Unmap(&self.staging, 0) };

        result.map_err(|e| GpuError::Texture(format!("逐行拷贝：{e}")))?;

        // SAFETY: 两块纹理格式与尺寸一致（都是 NV12 / coded size）。
        unsafe { device.context().CopyResource(&self.texture, &self.staging) };
        Ok(())
    }

    /// 底层 NV12 纹理（建输入视图用）
    pub fn texture(&self) -> &ID3D11Texture2D {
        &self.texture
    }

    /// 尺寸布局
    pub fn layout(&self) -> Nv12Layout {
        self.layout
    }
}

/// 视频处理器：YUV→RGB + 裁剪 + 缩放
pub struct VideoProcessor {
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    layout: Nv12Layout,
    color_space: ColorSpace,
}

impl std::fmt::Debug for VideoProcessor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VideoProcessor")
            .field("layout", &self.layout)
            .field("color_space", &self.color_space)
            .finish_non_exhaustive()
    }
}

impl VideoProcessor {
    /// 建 enumerator + processor，并一次性配好色彩空间与裁剪矩形
    ///
    /// # `output` 是真实输出纹理尺寸，不是 display 尺寸
    ///
    /// `D3D11_VIDEO_PROCESSOR_CONTENT_DESC` 的 `OutputWidth/OutputHeight`
    /// 告诉驱动「这个处理器会往多大的目标上画」。传小了（比如传 display
    /// 的 1280x720 而实际画到 1736x928）驱动会退回**最近邻**采样 ——
    /// 放大时表现为横向条纹。
    ///
    /// 实测：第五十四轮传 display 尺寸，1280x720 放大到 1736x928 的渐变
    /// 画面有明显条纹；传真实输出尺寸后平滑。
    pub fn new(
        device: &GpuDevice,
        layout: Nv12Layout,
        color_space: ColorSpace,
        output: (u32, u32),
    ) -> Result<Self, GpuError> {
        let (cw, ch) = layout.coded();
        let (ow, oh) = if output.0 == 0 || output.1 == 0 {
            layout.display()
        } else {
            output
        };

        let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: windows::Win32::Graphics::Dxgi::Common::DXGI_RATIONAL {
                Numerator: 60,
                Denominator: 1,
            },
            InputWidth: cw,
            InputHeight: ch,
            OutputFrameRate: windows::Win32::Graphics::Dxgi::Common::DXGI_RATIONAL {
                Numerator: 60,
                Denominator: 1,
            },
            OutputWidth: ow,
            OutputHeight: oh,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };

        let video_device: &ID3D11VideoDevice = device.video();

        // SAFETY: content 是栈上值，出参是枚举器。
        let enumerator =
            unsafe { video_device.CreateVideoProcessorEnumerator(&content) }.map_err(|e| {
                GpuError::VideoProcessor(format!("CreateVideoProcessorEnumerator：{}", e.message()))
            })?;

        // SAFETY: enumerator 有效，rate 0 = 默认帧率。
        let processor =
            unsafe { video_device.CreateVideoProcessor(&enumerator, 0) }.map_err(|e| {
                GpuError::VideoProcessor(format!("CreateVideoProcessor：{}", e.message()))
            })?;

        let video_context: ID3D11VideoContext = device.context().cast().map_err(|e| {
            GpuError::VideoProcessor(format!("cast ID3D11VideoContext：{}", e.message()))
        })?;

        let in_cs = windows::Win32::Graphics::Direct3D11::D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
            _bitfield: color_space.input_bitfield(),
        };
        let out_cs = windows::Win32::Graphics::Direct3D11::D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
            _bitfield: crate::color::output_bitfield(),
        };

        // SAFETY: 以下整段是配置视频处理器的标准流程，所有指针都指向本函数栈上值。
        unsafe {
            video_context.VideoProcessorSetStreamFrameFormat(
                &processor,
                0,
                D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            );
            video_context.VideoProcessorSetStreamColorSpace(&processor, 0, &in_cs);
            video_context.VideoProcessorSetOutputColorSpace(&processor, &out_cs);

            // 输入矩形用 display 尺寸（coded 坐标系）：右下角多出来的宏块填充不采样
            let src = to_rect(layout.source_rect());
            video_context.VideoProcessorSetStreamSourceRect(&processor, 0, true, Some(&src));

            // 输出矩形：默认 1:1。窗口模式下由 `set_dest_rect` 改成填满窗口。
            let dst = to_rect(layout.dest_rect());
            video_context.VideoProcessorSetStreamDestRect(&processor, 0, true, Some(&dst));

            video_context.VideoProcessorSetOutputTargetRect(&processor, false, None);
        }

        Ok(VideoProcessor {
            enumerator,
            processor,
            layout,
            color_space,
        })
    }

    /// 设置输出区域（`pStreamDestRect`）
    ///
    /// # 为什么可以随时改
    ///
    /// `VideoProcessorSetStreamDestRect` 是轻量调用，比重建整个视频处理器
    /// 便宜得多。窗口被拖动改变大小时调它即可。
    ///
    /// # 填满但不变形
    ///
    /// 传 `Some(surface)` 时按 [`Nv12Layout::dest_rect_fit`] 算「填满但保持
    /// 宽高比」的居中矩形（两侧或上下留黑边）。直接拉伸会让画面变形，
    /// 而变形是最容易被用户察觉的显示错误。
    ///
    /// 传 `None` 退回 1:1。
    pub fn set_dest_rect(
        &self,
        device: &GpuDevice,
        surface: Option<(u32, u32)>,
    ) -> Result<(), GpuError> {
        let video_context: ID3D11VideoContext = device.context().cast().map_err(|e| {
            GpuError::VideoProcessor(format!("cast ID3D11VideoContext：{}", e.message()))
        })?;

        let rect = match surface {
            Some(s) => self.layout.dest_rect_fit(s),
            None => self.layout.dest_rect(),
        };
        let dst = to_rect(rect);
        // SAFETY: processor 在 self 生命周期内有效。
        unsafe {
            video_context.VideoProcessorSetStreamDestRect(&self.processor, 0, true, Some(&dst));
        }
        Ok(())
    }

    /// 设置输出尺寸（会让视频处理器按新尺寸重新枚举）
    ///
    /// # 为什么缩放会有条纹（第五十五轮实测）
    ///
    /// `D3D11_VIDEO_PROCESSOR_CONTENT_DESC` 的 `OutputWidth/OutputHeight`
    /// 在创建时定死了视频处理器**预期的输出尺寸**。第五十四轮之前
    /// 我们传的是 `display` 尺寸（1280x720），而实际输出区域是窗口尺寸
    /// （1736x928）—— 两者不符时，驱动会退回**最近邻**采样。
    ///
    /// 1.36x 放大 + 最近邻 = 每 4 个源像素里有 1 个被重复 2 次，
    /// 渐变画面上表现为**横向条纹**（实测截图可见）。
    ///
    /// 修法：创建时 `OutputWidth/OutputHeight` 就传真实输出纹理尺寸。
    /// 这需要在建 swap chain 之后才能知道，所以把尺寸作为构造参数。
    pub fn output_size(&self) -> (u32, u32) {
        let _ = &self.enumerator;
        // 处理器内部记录的是创建时的输出尺寸；这里返回 display 尺寸
        // 作为「当前有效输出区域」的近似 —— 精确值由 `set_dest_rect` 决定。
        self.layout.display()
    }

    /// 建输入视图（绑定 NV12 纹理）
    pub fn create_input_view(
        &self,
        device: &GpuDevice,
        texture: &ID3D11Texture2D,
    ) -> Result<ID3D11VideoProcessorInputView, GpuError> {
        let desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV {
                    MipSlice: 0,
                    ArraySlice: 0,
                },
            },
        };
        let video_device: &ID3D11VideoDevice = device.video();
        // SAFETY: desc 与 texture 都有效。
        let mut view: Option<ID3D11VideoProcessorInputView> = None;
        // SAFETY: desc 与 texture 有效，view 是出参。
        unsafe {
            video_device.CreateVideoProcessorInputView(
                texture,
                &self.enumerator,
                &desc,
                Some(&mut view),
            )
        }
        .map_err(|e| {
            GpuError::VideoProcessor(format!("CreateVideoProcessorInputView：{}", e.message()))
        })?;
        view.ok_or_else(|| GpuError::VideoProcessor("输入视图创建返回空".into()))
    }

    /// 建输出视图（绑定 BGRA 输出纹理）
    pub fn create_output_view(
        &self,
        device: &GpuDevice,
        texture: &ID3D11Texture2D,
    ) -> Result<ID3D11VideoProcessorOutputView, GpuError> {
        let desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
            },
        };
        let video_device: ID3D11VideoDevice = device.device().cast().map_err(|e| {
            GpuError::VideoProcessor(format!("cast ID3D11VideoDevice：{}", e.message()))
        })?;
        // SAFETY: desc 与 texture 都有效。
        let mut view: Option<ID3D11VideoProcessorOutputView> = None;
        // SAFETY: desc 与 texture 有效，view 是出参。
        unsafe {
            video_device.CreateVideoProcessorOutputView(
                texture,
                &self.enumerator,
                &desc,
                Some(&mut view),
            )
        }
        .map_err(|e| {
            GpuError::VideoProcessor(format!("CreateVideoProcessorOutputView：{}", e.message()))
        })?;
        view.ok_or_else(|| GpuError::VideoProcessor("输出视图创建返回空".into()))
    }

    /// 执行 YUV→RGB 转换
    pub fn blt(
        &self,
        device: &GpuDevice,
        input: &ID3D11VideoProcessorInputView,
        output: &ID3D11VideoProcessorOutputView,
    ) -> Result<(), GpuError> {
        let video_context: ID3D11VideoContext = device.context().cast().map_err(|e| {
            GpuError::VideoProcessor(format!("cast ID3D11VideoContext：{}", e.message()))
        })?;

        let stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            OutputIndex: 0,
            InputFrameOrField: 0,
            PastFrames: 0,
            FutureFrames: 0,
            ppPastSurfaces: std::ptr::null_mut(),
            pInputSurface: std::mem::ManuallyDrop::new(Some(input.clone())),
            ppFutureSurfaces: std::ptr::null_mut(),
            ppPastSurfacesRight: std::ptr::null_mut(),
            pInputSurfaceRight: std::mem::ManuallyDrop::new(None),
            ppFutureSurfacesRight: std::ptr::null_mut(),
        };

        // SAFETY: stream 的 pInputSurface 持有 input 的一份引用，
        // VideoProcessorBlt 在返回前不会保留它。
        unsafe { video_context.VideoProcessorBlt(&self.processor, output, 0, &[stream]) }
            .map_err(|e| GpuError::VideoProcessor(format!("VideoProcessorBlt：{}", e.message())))
    }

    /// 尺寸布局
    pub fn layout(&self) -> Nv12Layout {
        self.layout
    }
}
