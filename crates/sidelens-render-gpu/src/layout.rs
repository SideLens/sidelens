//! NV12 帧的几何计算与 CPU 侧打包 —— **纯跨平台**，一行 D3D 都不碰
//!
//! # 为什么单独一个模块
//!
//! 「coded 尺寸 vs display 尺寸」「Y 平面 + 交织 UV 平面」「staging 纹理的行距」
//! 这三件事全是**算术**，但它们在 GPU 路径上算错就是黑屏 / 绿边 / 花屏，
//! 而且只有真机才能复现。把这部分抽成不依赖平台的纯函数之后，Linux CI 上
//! 就能把每一条边界条件钉死（`tests/layout.rs`），Windows 那边只剩「调 API」。
//!
//! # 为什么 CPU 必须碰一次数据
//!
//! `DXGI_FORMAT_NV12` 的纹理在 D3D11 里是**驱动私有布局**（tiled / 不透明），
//! 而 `D3D11_USAGE_DEFAULT` 的纹理**根本不允许 Map**（Map 只对 STAGING /
//! DYNAMIC 有效）。所以「把 CPU 里的 NV12 塞进 GPU 纹理」只有一条可移植的路：
//!
//! 1. 建一块 `D3D11_USAGE_STAGING` 的 **NV12** staging 纹理（CPU 可写）；
//! 2. Map 它 —— 拿到的 `pData` 里，Y 平面在偏移 0、UV 平面在
//!    `RowPitch * coded_height` 处，两个平面共用同一个 `RowPitch`；
//! 3. 把 Y / UV 两段逐行写进去（[`Nv12Layout::scatter_to_rows`]）；
//! 4. GPU 侧 `CopyResource` 到真正的 NV12 纹理（同格式同尺寸，合法）。
//!
//! [`Nv12Layout::scatter_to_rows`] 就是第 3 步，它是整条链路上**唯一**一次
//! CPU 全帧接触 —— 而且是纯 memcpy，不做任何逐像素运算（色彩矩阵、色度上采样
//! 全在 `ID3D11VideoProcessor` 里由硬件做）。
//!
//! > 试过但走不通的写法：把 NV12 当成一块高 `coded_h * 1.5` 的
//! > `DXGI_FORMAT_R8_UNORM` 纹理，再 `CopyResource` 到 NV12 纹理。
//! > `CopyResource` 要求源/目标**格式与尺寸完全一致**，跨格式一律
//! > `E_INVALIDARG`（实测见 `tests/gpu_win.rs` 的
//! > `r8_to_nv12_copy_resource_is_rejected`）。`CopySubresourceRegion`
//! > 同理。所以中转只能是「同格式的 staging」。

use std::fmt;

/// 矩形（左闭右开），单位是像素
///
/// 单独定义而不是直接用 `windows::Win32::Foundation::RECT`：本模块要在
/// Linux / macOS 上编译并测试，不能引 `windows` crate。Windows 侧再转成
/// `RECT`（内存布局完全相同，见 `win32::processor::to_rect`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    /// 左边界（含）
    pub left: i32,
    /// 上边界（含）
    pub top: i32,
    /// 右边界（不含）
    pub right: i32,
    /// 下边界（不含）
    pub bottom: i32,
}

impl Rect {
    /// 直接给出四个边界
    pub const fn new(left: i32, top: i32, right: i32, bottom: i32) -> Self {
        Rect {
            left,
            top,
            right,
            bottom,
        }
    }

    /// 从「宽高」造一个左上角在 (0,0) 的矩形
    pub const fn from_size(width: u32, height: u32) -> Self {
        Rect {
            left: 0,
            top: 0,
            right: width as i32,
            bottom: height as i32,
        }
    }

    /// 宽（像素）
    pub const fn width(&self) -> i32 {
        self.right - self.left
    }

    /// 高（像素）
    pub const fn height(&self) -> i32 {
        self.bottom - self.top
    }

    /// 宽或高不是正数时为 `true`
    pub const fn is_empty(&self) -> bool {
        self.width() <= 0 || self.height() <= 0
    }
}

/// 布局校验失败
///
/// 全部是「输入不自洽」，没有环境问题 —— 所以纯单元测试能覆盖每一个分支。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayoutError {
    /// 宽或高为 0
    ZeroDimension {
        /// 哪个尺寸（"coded" / "display"）
        what: &'static str,
        /// 宽
        width: u32,
        /// 高
        height: u32,
    },
    /// NV12 的色度是 2x2 子采样，宽高必须是偶数
    OddDimension {
        /// 哪个尺寸
        what: &'static str,
        /// 宽
        width: u32,
        /// 高
        height: u32,
    },
    /// 显示尺寸不能超过编码尺寸（否则要凭空补像素）
    DisplayLargerThanCoded {
        /// 编码尺寸
        coded: (u32, u32),
        /// 显示尺寸
        display: (u32, u32),
    },
    /// 输入的 NV12 缓冲放不下一整帧
    BufferTooSmall {
        /// 至少需要多少字节
        need: usize,
        /// 实际给了多少
        have: usize,
    },
    /// staging 纹理的行距小于宽度，行与行会重叠
    RowPitchTooSmall {
        /// 实际行距
        pitch: usize,
        /// 编码宽
        width: u32,
    },
    /// 目标缓冲放不下整帧
    OutputTooSmall {
        /// 至少需要多少字节
        need: usize,
        /// 实际给了多少
        have: usize,
    },
}

impl fmt::Display for LayoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LayoutError::ZeroDimension {
                what,
                width,
                height,
            } => write!(f, "{what} 尺寸不能为 0（{width}x{height}）"),
            LayoutError::OddDimension {
                what,
                width,
                height,
            } => write!(
                f,
                "{what} 尺寸必须是偶数（NV12 的色度是 2x2 子采样），实际 {width}x{height}"
            ),
            LayoutError::DisplayLargerThanCoded { coded, display } => write!(
                f,
                "显示尺寸 {}x{} 大于编码尺寸 {}x{}",
                display.0, display.1, coded.0, coded.1
            ),
            LayoutError::BufferTooSmall { need, have } => {
                write!(f, "NV12 缓冲不足：需要 {need} 字节，只有 {have} 字节")
            }
            LayoutError::RowPitchTooSmall { pitch, width } => {
                write!(f, "staging 行距 {pitch} 小于编码宽 {width}，行会重叠")
            }
            LayoutError::OutputTooSmall { need, have } => {
                write!(f, "目标缓冲不足：需要 {need} 字节，只有 {have} 字节")
            }
        }
    }
}

impl std::error::Error for LayoutError {}

/// 一帧 NV12 的几何描述
///
/// 同时携带 **coded 尺寸**（解码器实际输出的、宏块对齐后的尺寸，1920x1080 →
/// 1920x**1088**）和 **display 尺寸**（SPS cropping 之后真正该显示的尺寸）。
///
/// # 为什么两个尺寸都要带
///
/// `sidelens-codec-mf` 的 `H264Decoder::output_size()` 给 coded，
/// `output_display_size()` 给 display。底部多出来的 8 行是编码器的宏块填充，
/// 内容是随机的 —— 直接按 coded 出图，画面底部会有一条绿边。
///
/// GPU 路径上的裁剪**不需要 CPU 参与**：把
/// `ID3D11VideoProcessorBlt` 的 `pStreamFrameSourceRect` 设成 display 尺寸，
/// 视频处理器就只采样左上角那块，右下角的填充一个像素都不会读。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Nv12Layout {
    coded: (u32, u32),
    display: (u32, u32),
}

impl Nv12Layout {
    /// 校验并构造
    ///
    /// 拒绝：0 尺寸、奇数尺寸（NV12 色度 2x2 子采样）、display > coded。
    pub fn new(coded: (u32, u32), display: (u32, u32)) -> Result<Self, LayoutError> {
        let (cw, ch) = coded;
        let (dw, dh) = display;

        if cw == 0 || ch == 0 {
            return Err(LayoutError::ZeroDimension {
                what: "coded",
                width: cw,
                height: ch,
            });
        }
        if dw == 0 || dh == 0 {
            return Err(LayoutError::ZeroDimension {
                what: "display",
                width: dw,
                height: dh,
            });
        }
        if cw % 2 != 0 || ch % 2 != 0 {
            return Err(LayoutError::OddDimension {
                what: "coded",
                width: cw,
                height: ch,
            });
        }
        if dw % 2 != 0 || dh % 2 != 0 {
            return Err(LayoutError::OddDimension {
                what: "display",
                width: dw,
                height: dh,
            });
        }
        if dw > cw || dh > ch {
            return Err(LayoutError::DisplayLargerThanCoded { coded, display });
        }

        Ok(Nv12Layout {
            coded: (cw, ch),
            display: (dw, dh),
        })
    }

    /// 编码尺寸（宏块对齐后）
    pub const fn coded(&self) -> (u32, u32) {
        self.coded
    }

    /// 显示尺寸（裁剪后）
    pub const fn display(&self) -> (u32, u32) {
        self.display
    }

    /// Y 平面字节数
    pub const fn y_len(&self) -> usize {
        self.coded.0 as usize * self.coded.1 as usize
    }

    /// UV 交织平面字节数（Y 的一半）
    pub const fn uv_len(&self) -> usize {
        self.y_len() / 2
    }

    /// 一整帧 NV12 的字节数
    pub const fn frame_len(&self) -> usize {
        self.y_len() + self.uv_len()
    }

    /// staging 纹理按「行」算有多少行（Y 平面行数 + UV 平面行数）
    ///
    /// `DXGI_FORMAT_NV12` 的纹理高度是 `coded_h`，但 Map 出来的内存里
    /// UV 平面**紧跟在** Y 平面之后，所以按行算是 1.5 倍。
    pub const fn staging_rows(&self) -> usize {
        self.coded.1 as usize + self.coded.1 as usize / 2
    }

    /// 是否需要裁剪（coded != display）
    pub const fn needs_crop(&self) -> bool {
        self.coded.0 != self.display.0 || self.coded.1 != self.display.1
    }

    /// 视频处理器的**输入**矩形（`pStreamFrameSourceRect`，coded 坐标系）
    ///
    /// 就是左上角 `display_w x display_h` 那块 —— 右下角的宏块填充不采样。
    pub const fn source_rect(&self) -> Rect {
        Rect::from_size(self.display.0, self.display.1)
    }

    /// 视频处理器的**输出**矩形（`pStreamDestRect`，输出纹理坐标系）
    ///
    /// 默认与输入矩形同尺寸（1:1 不缩放）。想填满窗口就设一个
    /// [`SurfaceFit`]，见 [`Self::dest_rect_fit`]。
    pub const fn dest_rect(&self) -> Rect {
        Rect::from_size(self.display.0, self.display.1)
    }

    /// 按「填满但不变形」算输出矩形（letterbox / pillarbox）
    ///
    /// # 为什么要保持宽高比
    ///
    /// 视频区的宽高比几乎从不等于视频的宽高比（实测面板 1736x928 = 1.87，
    /// 视频 1280x720 = 1.78）。直接拉伸到填满会让画面**变形** ——
    /// 人脸会变胖或变瘦，这是最容易被用户察觉的显示错误。
    ///
    /// 所以按较小的缩放比居中放置，两侧或上下留黑边（letterbox / pillarbox）。
    /// 这也是所有播放器与视频会议的默认行为。
    ///
    /// # `surface` 是什么坐标系
    ///
    /// 输出纹理的尺寸（物理像素）。传窗口客户区尺寸即可 —— 输出纹理
    /// 与客户区同尺寸时，视频处理器的缩放就是「免费的」。
    ///
    /// # 边界
    ///
    /// - `surface` 任一维为 0 → 退回 1:1（调用方传错了，不要 panic）
    /// - `surface` 比视频还小 → 缩小到适配（不放大到溢出）
    pub fn dest_rect_fit(&self, surface: (u32, u32)) -> Rect {
        if surface.0 == 0 || surface.1 == 0 {
            return self.dest_rect();
        }
        let vw = self.display.0 as f64;
        let vh = self.display.1 as f64;
        let sw = surface.0 as f64;
        let sh = surface.1 as f64;

        // 取较小的缩放比：保证两个方向都不溢出
        let scale = (sw / vw).min(sh / vh);
        let w = (vw * scale).round().max(1.0) as i32;
        let h = (vh * scale).round().max(1.0) as i32;

        // 居中：左边距 = (surface_w - w) / 2
        let left = ((surface.0 as i32) - w) / 2;
        let top = ((surface.1 as i32) - h) / 2;
        Rect::new(left, top, left + w, top + h)
    }

    /// 把紧凑布局的 NV12 逐行写进 staging 纹理的映射内存
    ///
    /// - `src`：解码器输出的紧凑 NV12（stride == coded_width，`codec-mf` 的
    ///   `repack` 保证这一点），长度至少 [`Self::frame_len`]
    /// - `dst`：`Map` 出来的整块内存，长度至少 `row_pitch * staging_rows()`
    /// - `row_pitch`：`D3D11_MAPPED_SUBRESOURCE::RowPitch`（驱动给的，通常
    ///   大于 `coded_width`，多出来的是对齐填充，**不能覆盖**）
    ///
    /// 只拷 `coded` 区域：display 之外的填充行也照拷（GPU 侧靠 source rect
    /// 裁掉），这样本函数与「要不要裁剪」完全解耦，逻辑最简单。
    pub fn scatter_to_rows(
        &self,
        src: &[u8],
        dst: &mut [u8],
        row_pitch: usize,
    ) -> Result<(), LayoutError> {
        let cw = self.coded.0 as usize;
        let ch = self.coded.1 as usize;

        if row_pitch < cw {
            return Err(LayoutError::RowPitchTooSmall {
                pitch: row_pitch,
                width: self.coded.0,
            });
        }
        if src.len() < self.frame_len() {
            return Err(LayoutError::BufferTooSmall {
                need: self.frame_len(),
                have: src.len(),
            });
        }
        let need = row_pitch * self.staging_rows();
        if dst.len() < need {
            return Err(LayoutError::OutputTooSmall {
                need,
                have: dst.len(),
            });
        }

        // Y 平面：src 的第 r 行 → dst 的第 r 行
        for r in 0..ch {
            let s = r * cw;
            let d = r * row_pitch;
            dst[d..d + cw].copy_from_slice(&src[s..s + cw]);
        }
        // UV 交织平面：src 里紧跟在 Y 之后，dst 里紧跟在第 ch 行之后
        let uv_src = cw * ch;
        for r in 0..ch / 2 {
            let s = uv_src + r * cw;
            let d = (ch + r) * row_pitch;
            dst[d..d + cw].copy_from_slice(&src[s..s + cw]);
        }
        Ok(())
    }
}

/// 生成一帧纯色 NV12（`y` 填 Y 平面，`uv` 同时填 U 和 V）
///
/// 测试与示例用：`(y=16, uv=128)` 是 limited range 的**黑**，
/// `(y=235, uv=128)` 是**白**。
pub fn solid_nv12(layout: Nv12Layout, y: u8, uv: u8) -> Vec<u8> {
    let mut buf = vec![y; layout.y_len()];
    buf.resize(layout.frame_len(), uv);
    buf
}

/// 在一帧 NV12 上填一个矩形（`y` 填 Y，`uv` 同时填 U/V）
///
/// 矩形会被裁到 display 区域内。色度是 2x2 子采样，所以 UV 平面按
/// 「像素坐标 / 2」写入 —— 边缘坐标请传偶数，否则色度块会外扩一格
/// （视觉上就是一个半像素的色边，测试与示例都不在意）。
pub fn fill_rect_nv12(buf: &mut [u8], layout: Nv12Layout, rect: Rect, y: u8, uv: u8) {
    let (dw, dh) = layout.display();
    let cw = layout.coded.0 as usize;
    let ch = layout.coded.1 as usize;
    let y_len = layout.y_len();

    let left = rect.left.clamp(0, dw as i32) as usize;
    let right = rect.right.clamp(0, dw as i32) as usize;
    let top = rect.top.clamp(0, dh as i32) as usize;
    let bottom = rect.bottom.clamp(0, dh as i32) as usize;
    if left >= right || top >= bottom {
        return;
    }
    if buf.len() < layout.frame_len() {
        return;
    }

    for row in top..bottom {
        let base = row * cw;
        buf[base + left..base + right].fill(y);
    }

    let cx0 = left / 2;
    let cx1 = right.div_ceil(2);
    let cy0 = top / 2;
    let cy1 = bottom.div_ceil(2);
    for crow in cy0..cy1.min(ch / 2) {
        let base = y_len + crow * cw;
        // UV 交织：每两个字节是一组 (U, V)
        for cell in cx0..cx1.min(cw / 2) {
            buf[base + cell * 2] = uv;
            buf[base + cell * 2 + 1] = uv;
        }
    }
}

/// 从紧凑 BGRA 缓冲里取一个像素
///
/// 读回（readback）测试要按像素断言，单独抽出来是为了让越界返回 `None`
/// 而不是 panic —— 测试里越界应该是一条断言失败，不是整个测试进程崩掉。
pub fn bgra_pixel(buf: &[u8], stride: usize, x: u32, y: u32) -> Option<[u8; 4]> {
    let off = y as usize * stride + x as usize * 4;
    let s = buf.get(off..off + 4)?;
    Some([s[0], s[1], s[2], s[3]])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout_1080() -> Nv12Layout {
        Nv12Layout::new((1920, 1088), (1920, 1080)).expect("1920x1088/1920x1080 必须合法")
    }

    #[test]
    fn rejects_zero_dimensions() {
        assert_eq!(
            Nv12Layout::new((0, 1080), (0, 1080)),
            Err(LayoutError::ZeroDimension {
                what: "coded",
                width: 0,
                height: 1080
            })
        );
        assert_eq!(
            Nv12Layout::new((1920, 1088), (0, 1080)),
            Err(LayoutError::ZeroDimension {
                what: "display",
                width: 0,
                height: 1080
            })
        );
    }

    #[test]
    fn rejects_odd_dimensions() {
        assert!(matches!(
            Nv12Layout::new((1921, 1088), (1920, 1080)),
            Err(LayoutError::OddDimension { what: "coded", .. })
        ));
        assert!(matches!(
            Nv12Layout::new((1920, 1088), (1919, 1080)),
            Err(LayoutError::OddDimension {
                what: "display",
                ..
            })
        ));
    }

    #[test]
    fn rejects_display_larger_than_coded() {
        assert_eq!(
            Nv12Layout::new((640, 368), (640, 400)),
            Err(LayoutError::DisplayLargerThanCoded {
                coded: (640, 368),
                display: (640, 400)
            })
        );
    }

    #[test]
    fn frame_len_matches_nv12_math() {
        let l = layout_1080();
        assert_eq!(l.y_len(), 1920 * 1088);
        assert_eq!(l.uv_len(), 1920 * 1088 / 2);
        assert_eq!(l.frame_len(), 1920 * 1088 * 3 / 2);
        assert_eq!(l.staging_rows(), 1088 + 544);
    }

    #[test]
    fn crop_rects_use_display_size() {
        let l = layout_1080();
        assert!(l.needs_crop());
        assert_eq!(l.source_rect(), Rect::new(0, 0, 1920, 1080));
        assert_eq!(l.dest_rect(), Rect::new(0, 0, 1920, 1080));
    }

    #[test]
    fn no_crop_when_sizes_match() {
        let l = Nv12Layout::new((1280, 720), (1280, 720)).expect("合法");
        assert!(!l.needs_crop());
    }

    #[test]
    fn scatter_puts_y_then_uv_with_padding() {
        let l = Nv12Layout::new((4, 4), (4, 4)).expect("合法");
        // Y 平面 4x4 = 0..16，UV 平面 4x2 = 16..24
        let mut src: Vec<u8> = (0..16u8).collect();
        src.extend_from_slice(&[200u8; 8]);
        // 行距 8 > 宽 4：每行后 4 字节是填充，必须原样保留
        let mut dst = vec![0xAAu8; 8 * l.staging_rows()];
        l.scatter_to_rows(&src, &mut dst, 8).expect("必须成功");

        assert_eq!(&dst[0..4], &[0, 1, 2, 3]);
        assert_eq!(&dst[4..8], &[0xAA; 4], "填充不能被覆盖");
        assert_eq!(&dst[8..12], &[4, 5, 6, 7]);
        // UV 从第 4 行开始
        assert_eq!(&dst[32..36], &[200; 4]);
        assert_eq!(&dst[36..40], &[0xAA; 4]);
        // UV 第二行（第 5 行）在 40..44，之后是填充
        assert_eq!(&dst[40..44], &[200; 4]);
        assert_eq!(&dst[44..48], &[0xAA; 4]);
    }

    #[test]
    fn scatter_rejects_small_row_pitch() {
        let l = Nv12Layout::new((8, 8), (8, 8)).expect("合法");
        let src = vec![0u8; l.frame_len()];
        let mut dst = vec![0u8; 8 * 16];
        assert_eq!(
            l.scatter_to_rows(&src, &mut dst, 7),
            Err(LayoutError::RowPitchTooSmall { pitch: 7, width: 8 })
        );
    }

    #[test]
    fn scatter_rejects_short_source() {
        let l = Nv12Layout::new((8, 8), (8, 8)).expect("合法");
        let src = vec![0u8; l.frame_len() - 1];
        let mut dst = vec![0u8; 16 * 12];
        assert!(matches!(
            l.scatter_to_rows(&src, &mut dst, 16),
            Err(LayoutError::BufferTooSmall { .. })
        ));
    }

    #[test]
    fn scatter_rejects_short_destination() {
        let l = Nv12Layout::new((8, 8), (8, 8)).expect("合法");
        let src = vec![0u8; l.frame_len()];
        let mut dst = vec![0u8; 16 * 12 - 1];
        assert!(matches!(
            l.scatter_to_rows(&src, &mut dst, 16),
            Err(LayoutError::OutputTooSmall { .. })
        ));
    }

    #[test]
    fn solid_frame_has_expected_planes() {
        let l = Nv12Layout::new((6, 4), (6, 4)).expect("合法");
        let f = solid_nv12(l, 16, 128);
        assert_eq!(f.len(), l.frame_len());
        assert!(f[..l.y_len()].iter().all(|&b| b == 16));
        assert!(f[l.y_len()..].iter().all(|&b| b == 128));
    }

    #[test]
    fn fill_rect_writes_y_and_chroma() {
        let l = Nv12Layout::new((8, 8), (8, 8)).expect("合法");
        let mut f = solid_nv12(l, 16, 128);
        fill_rect_nv12(&mut f, l, Rect::new(2, 2, 6, 6), 235, 128);
        // (3,3) 在矩形内
        assert_eq!(f[3 * 8 + 3], 235);
        // (0,0) 在矩形外
        assert_eq!(f[0], 16);
        // 色度 cell(1,1) 覆盖像素 (2..4, 2..4)，在矩形内
        let uv_base = l.y_len();
        assert_eq!(f[uv_base + 8 + 2], 128);
        // 矩形外的色度 cell 保持原值
        assert_eq!(f[uv_base], 128);
    }

    #[test]
    fn fill_rect_clips_to_display() {
        let l = Nv12Layout::new((8, 8), (8, 6)).expect("合法");
        let mut f = solid_nv12(l, 16, 128);
        // 越界矩形不能 panic，也不能写到 coded 填充区
        fill_rect_nv12(&mut f, l, Rect::new(-10, -10, 100, 100), 235, 128);
        assert_eq!(f[0], 235);
        assert_eq!(f[5 * 8 + 7], 235, "display 最后一行要填");
        assert_eq!(f[6 * 8], 16, "coded 填充行不能碰");
    }

    #[test]
    fn bgra_pixel_reads_and_bounds_checks() {
        let buf = vec![1u8, 2, 3, 4, 5, 6, 7, 8];
        assert_eq!(bgra_pixel(&buf, 8, 0, 0), Some([1, 2, 3, 4]));
        assert_eq!(bgra_pixel(&buf, 8, 1, 0), Some([5, 6, 7, 8]));
        assert_eq!(bgra_pixel(&buf, 8, 2, 0), None);
        assert_eq!(bgra_pixel(&buf, 8, 0, 1), None);
    }

    #[test]
    fn rect_helpers() {
        let r = Rect::new(0, 0, 1920, 1080);
        assert_eq!(r.width(), 1920);
        assert_eq!(r.height(), 1080);
        assert!(!r.is_empty());
        assert!(Rect::from_size(0, 10).is_empty());
    }
}
