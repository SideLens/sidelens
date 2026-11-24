//! NV12 → RGBA8888 转换（**纯跨平台**，不碰任何平台 API）
//!
//! 这个模块的每一条逻辑都必须能在 Linux CI 上跑，所以它只依赖 `&[u8]` 和
//! 标准库。窗口 / GDI 全部隔离在 `win32.rs` 里。
//!
//! # 为什么矩阵要单独拎出来讲
//!
//! NV12 只是「Y 平面 + 交织 UV 平面」，它**不携带色彩矩阵**。同一个
//! `(Y, Cb, Cr)` 三元组，用 BT.601 还是 BT.709 解释，出来的 RGB 是不同的。
//! 选错了不会报错，只会「颜色发闷 / 发绿」，属于最难查的那类问题。
//!
//! ## 本项目的实测依据（三条，都是本机跑出来的）
//!
//! 1. **码流自己没声明。** 用独立实现的 bit reader 解析
//!    `crates/sidelens-codec/tests/data/golden-720p.h264` 的 SPS，
//!    VUI 里 `video_signal_type_present_flag = 0` —— 也就是说
//!    `video_full_range_flag` / `colour_primaries` / `matrix_coefficients`
//!    **一个都不存在**。Media Foundation 的 H.264 编码器（mfh264enc）不写
//!    颜色描述，所以「照码流里的声明走」这条路在本项目里根本不成立。
//! 2. **编码链路不碰颜色。** `cargo run -p sidelens-codec-mf --example decode_probe -- 640 360 30 20`
//!    实测 Y 值往返 `|ΔY|` 平均 0.43 / 最大 1 —— 编解码对 YUV 数值是近似
//!    恒等的，不做任何矩阵变换。所以矩阵**只影响显示端**，选哪个由我们定。
//! 3. **行业默认。** H.264 在无 VUI 色彩描述时，主流解码器（FFmpeg 的
//!    swscale、Chrome、Windows video processor）对 **HD（≥720p）** 一律按
//!    **BT.709 limited range** 处理，对 SD 才回落到 BT.601。本项目跑
//!    640x360 / 1280x720 / 1920x1080，720p 与 1080p 是主场景，因此默认
//!    **BT.709 + limited range（Y: 16..235，Cb/Cr: 16..240）**。
//!
//! 这个选择不是拍脑袋：`tests/convert.rs` 里的 `bt709_red_pins_matrix` 用
//! BT.709 编码的标准红 `(Y=63, Cb=102, Cr=240)` 断言解出来是**精确的**
//! `(255, 0, 0)`；同一个三元组改用 BT.601 解会掉到 `(233, 0, 2)`，
//! 红通道直接损失 9%。测试把这条钉住了，将来谁改默认矩阵都会红。
//!
//! 想用 BT.601 的调用方走 [`Nv12View::to_rgba_with`]，默认路径不受影响。
//!
//! # 定点实现
//!
//! 全部用 Q16 定点（系数 × 65536，结果 `>> 16`）。浮点版本在 720p 上
//! 每帧要跑 92 万次浮点乘加，且结果在不同 CPU 上可能差 1 —— 测试没法写死。
//! Q16 定点在本项目的色域内误差 < 1/256，测试可以断言精确值。
//!
//! 系数来源（BT.709 limited range 的解码矩阵）：
//!
//! ```text
//! R = 1.164383*(Y-16) + 1.792741*(Cr-128)
//! G = 1.164383*(Y-16) - 0.213249*(Cb-128) - 0.532909*(Cr-128)
//! B = 1.164383*(Y-16) + 2.112402*(Cb-128)
//! ```
//!
//! 乘 65536 取整得到 [`COEF_709`]；BT.601 同理（见 [`COEF_601`]）。
//!
//! # 性能
//!
//! 用**查表**而不是每像素做乘法：`Y` / `Cb` / `Cr` 各自只有 256 个可能值，
//! 把「系数 × 偏移」的乘积预先算成 5 张 `[i32; 256]` 表（合计 5 KB，L1 装得下）。
//! 每像素只剩 2 次查表 + 3 次加法 + 3 次移位。
//!
//! 另外 NV12 的 UV 是 **2x2 共享**的：一行里相邻两个像素用同一对 `(Cb, Cr)`，
//! 所以色度那两次查表按**像素对**做，省掉一半。
//!
//! 实测 1280x720 单帧见 `examples/bench_convert.rs`。

use std::fmt;
use std::sync::OnceLock;

/// 色彩矩阵
///
/// NV12 本身不带矩阵信息，必须由调用方指定。默认 [`Matrix::Bt709`]，
/// 理由见模块文档。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Matrix {
    /// BT.709 limited range —— **默认**，HD（≥720p）的标准选择
    #[default]
    Bt709,
    /// BT.601 limited range —— 只在对接老 SD 素材时用
    Bt601,
}

impl Matrix {
    /// 人类可读的名字
    pub fn as_str(self) -> &'static str {
        match self {
            Matrix::Bt709 => "BT.709 limited",
            Matrix::Bt601 => "BT.601 limited",
        }
    }
}

/// Q16 定点系数
///
/// 字段含义（全部已乘 65536）：
/// - `y`：亮度到 RGB 的公共系数（`1.164383`）
/// - `cr_r`：Cr → R（`1.792741` BT.709 / `1.596027` BT.601）
/// - `cb_b`：Cb → B
/// - `cb_g` / `cr_g`：Cb / Cr → G 的**负**系数（存成负数，算 G 时直接相加）
#[derive(Debug, Clone, Copy)]
struct Coef {
    y: i32,
    cr_r: i32,
    cb_b: i32,
    cb_g: i32,
    cr_g: i32,
}

/// BT.709 limited range 的 Q16 系数
///
/// 逐项来源：`1.164383*65536 = 76309.0`、`1.792741*65536 = 117504.5`、
/// `2.112402*65536 = 138438.0`、`0.213249*65536 = 13975.3`、
/// `0.532909*65536 = 34925.0`。
const COEF_709: Coef = Coef {
    y: 76309,
    cr_r: 117504,
    cb_b: 138438,
    cb_g: -13975,
    cr_g: -34925,
};

/// BT.601 limited range 的 Q16 系数
///
/// `1.596027*65536 = 104596.8`、`2.017232*65536 = 132201.4`、
/// `0.391762*65536 = 25674.4`、`0.812968*65536 = 53278.0`。
const COEF_601: Coef = Coef {
    y: 76309,
    cr_r: 104597,
    cb_b: 132201,
    cb_g: -25674,
    cr_g: -53278,
};

/// 查表：5 张 256 项的 i32 表
#[derive(Debug)]
struct Tables {
    y: [i32; 256],
    cr_r: [i32; 256],
    cb_b: [i32; 256],
    cb_g: [i32; 256],
    cr_g: [i32; 256],
}

impl Tables {
    fn build(c: Coef) -> Self {
        let mut t = Tables {
            y: [0; 256],
            cr_r: [0; 256],
            cb_b: [0; 256],
            cb_g: [0; 256],
            cr_g: [0; 256],
        };
        // limited range：Y 的有效区间是 16..=235，但表要覆盖 0..=255
        // （越界的值会被 clamp，不能让下标 panic）。
        for i in 0..256usize {
            let yd = i as i32 - 16;
            let cd = i as i32 - 128;
            t.y[i] = c.y * yd;
            t.cr_r[i] = c.cr_r * cd;
            t.cb_b[i] = c.cb_b * cd;
            t.cb_g[i] = c.cb_g * cd;
            t.cr_g[i] = c.cr_g * cd;
        }
        t
    }
}

static TABLES_709: OnceLock<Tables> = OnceLock::new();
static TABLES_601: OnceLock<Tables> = OnceLock::new();

fn tables(m: Matrix) -> &'static Tables {
    match m {
        Matrix::Bt709 => TABLES_709.get_or_init(|| Tables::build(COEF_709)),
        Matrix::Bt601 => TABLES_601.get_or_init(|| Tables::build(COEF_601)),
    }
}

/// 转换阶段的错误
///
/// 全部是「输入不自洽」，没有一个是环境问题 —— 所以纯单元测试能覆盖到每一个分支。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConvertError {
    /// 宽或高为 0
    ZeroDimension {
        /// 宽
        width: u32,
        /// 高
        height: u32,
    },
    /// NV12 的色度是 2x2 子采样，宽高必须是偶数
    OddDimension {
        /// 哪个尺寸有问题（"coded" / "display"）
        what: &'static str,
        /// 宽
        width: u32,
        /// 高
        height: u32,
    },
    /// 显示尺寸不能超过编码尺寸（否则要「凭空补像素」）
    DisplayLargerThanCoded {
        /// 编码尺寸
        coded: (u32, u32),
        /// 显示尺寸
        display: (u32, u32),
    },
    /// stride 小于宽度，行与行会重叠
    StrideTooSmall {
        /// 传入的 stride
        stride: usize,
        /// 编码宽
        width: u32,
    },
    /// 缓冲区放不下一整帧 NV12
    BufferTooSmall {
        /// 至少需要多少字节
        need: usize,
        /// 实际给了多少
        have: usize,
    },
}

impl fmt::Display for ConvertError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConvertError::ZeroDimension { width, height } => {
                write!(f, "NV12 尺寸不能为 0（{width}x{height}）")
            }
            ConvertError::OddDimension {
                what,
                width,
                height,
            } => write!(
                f,
                "{what} 尺寸必须是偶数（NV12 的色度是 2x2 子采样），实际 {width}x{height}"
            ),
            ConvertError::DisplayLargerThanCoded { coded, display } => write!(
                f,
                "显示尺寸 {}x{} 大于编码尺寸 {}x{}",
                display.0, display.1, coded.0, coded.1
            ),
            ConvertError::StrideTooSmall { stride, width } => {
                write!(f, "stride {stride} 小于编码宽 {width}，行会重叠")
            }
            ConvertError::BufferTooSmall { need, have } => {
                write!(f, "NV12 缓冲不足：需要 {need} 字节，只有 {have} 字节")
            }
        }
    }
}

impl std::error::Error for ConvertError {}

/// 一帧 NV12 的借用视图
///
/// 同时携带 **coded 尺寸**（解码器实际输出的、宏块对齐后的尺寸）和
/// **display 尺寸**（SPS cropping 之后的、真正该显示的尺寸）。
///
/// # 为什么两个尺寸都要带
///
/// `H264Decoder::output_size()` 给的是 coded size（640x360 → **640x368**），
/// `output_display_size()` 给的是显示尺寸（640x360）。
/// 缓冲是按 coded size 分配的（353280 字节），但**只能显示 display 区域**。
/// 底部那 8 行是编码器的宏块填充，内容是编码器随手填的 —— 画出来就是
/// 画面底部一条绿边。`crates/sidelens-codec-mf/src/decoder.rs` 的模块
/// 文档专门警告过这一点。
///
/// 所以 [`Nv12View::to_rgba`] 只转换 `display_width x display_height`，
/// 绝不按 coded 尺寸出图。
///
/// # 布局假设
///
/// - Y 平面在前，占 `coded_height` 行，每行 `stride` 字节（前 `coded_width` 有效）
/// - UV 交织平面紧随其后，占 `coded_height / 2` 行，每行同样 `stride` 字节
/// - 平面之间**没有额外间隙**（MF 的输出缓冲就是这样）
///
/// `H264Decoder` 已经按 stride 重排成紧凑布局（`decoder::repack`），
/// 所以绝大多数调用方用 [`Nv12View::new`]（stride == coded_width）即可；
/// [`Nv12View::with_stride`] 留给「自己直接接硬件解码器、没走 repack」的场景。
#[derive(Debug, Clone, Copy)]
pub struct Nv12View<'a> {
    data: &'a [u8],
    stride: usize,
    coded_width: u32,
    coded_height: u32,
    display_width: u32,
    display_height: u32,
    /// Y 平面的总字节数 —— 也就是 UV 平面在 `data` 里的起始偏移
    ///
    /// 单独存一份而不是每次用 `coded_width * coded_height` 算，是因为
    /// [`Nv12View::crop_to_display`] 会把 coded 尺寸改成显示尺寸，
    /// 但**底层缓冲的 UV 平面位置没变**。存下来才不会算错。
    uv_offset: usize,
}

impl<'a> Nv12View<'a> {
    /// 从**紧凑布局**的 NV12 缓冲构造（stride == coded_width）
    ///
    /// `coded` 来自 `H264Decoder::output_size()`，
    /// `display` 来自 `H264Decoder::output_display_size()`。
    pub fn new(
        data: &'a [u8],
        coded: (u32, u32),
        display: (u32, u32),
    ) -> Result<Self, ConvertError> {
        Self::with_stride(data, coded.0 as usize, coded, display)
    }

    /// 从**带行填充**的 NV12 缓冲构造（stride > coded_width）
    ///
    /// 硬件解码器常按 64/128 字节对齐 stride，负数表示自下而上 ——
    /// 自下而上的情况请调用方先翻正，本视图不处理。
    pub fn with_stride(
        data: &'a [u8],
        stride: usize,
        coded: (u32, u32),
        display: (u32, u32),
    ) -> Result<Self, ConvertError> {
        let (cw, ch) = coded;
        let (dw, dh) = display;

        if cw == 0 || ch == 0 || dw == 0 || dh == 0 {
            return Err(ConvertError::ZeroDimension {
                width: cw.min(dw),
                height: ch.min(dh),
            });
        }
        if cw % 2 != 0 || ch % 2 != 0 {
            return Err(ConvertError::OddDimension {
                what: "coded",
                width: cw,
                height: ch,
            });
        }
        if dw % 2 != 0 || dh % 2 != 0 {
            return Err(ConvertError::OddDimension {
                what: "display",
                width: dw,
                height: dh,
            });
        }
        if dw > cw || dh > ch {
            return Err(ConvertError::DisplayLargerThanCoded { coded, display });
        }
        if stride < cw as usize {
            return Err(ConvertError::StrideTooSmall { stride, width: cw });
        }

        let uv_offset = stride * ch as usize;
        let need = uv_offset + stride * (ch as usize / 2);
        if data.len() < need {
            return Err(ConvertError::BufferTooSmall {
                need,
                have: data.len(),
            });
        }

        Ok(Nv12View {
            data,
            stride,
            coded_width: cw,
            coded_height: ch,
            display_width: dw,
            display_height: dh,
            uv_offset,
        })
    }

    /// 原始缓冲
    pub fn data(&self) -> &'a [u8] {
        self.data
    }
    /// 每行字节数（Y / UV 两个平面相同）
    pub fn stride(&self) -> usize {
        self.stride
    }
    /// 编码宽（宏块对齐后）
    pub fn coded_width(&self) -> u32 {
        self.coded_width
    }
    /// 编码高（宏块对齐后）
    pub fn coded_height(&self) -> u32 {
        self.coded_height
    }
    /// 显示宽
    pub fn display_width(&self) -> u32 {
        self.display_width
    }
    /// 显示高
    pub fn display_height(&self) -> u32 {
        self.display_height
    }
    /// 底层缓冲里这一帧占多少字节（按 **coded** 尺寸算）
    pub fn frame_len(&self) -> usize {
        self.uv_offset + self.stride * (self.coded_height as usize / 2)
    }
    /// 是否真的需要裁剪（coded != display）
    pub fn needs_crop(&self) -> bool {
        self.coded_width != self.display_width || self.coded_height != self.display_height
    }

    /// 裁掉底部 / 右侧的宏块填充，返回一个「coded == display」的视图
    ///
    /// 返回的视图**仍然借用同一块内存**，UV 平面位置不变（`uv_offset` 原样带过去），
    /// 只是把 coded 尺寸改成显示尺寸。所以对它调 [`Self::to_rgba`] 和
    /// 对原视图调的结果**逐字节相同** —— 有测试钉住这一点。
    ///
    /// 不需要裁剪时原样返回（零成本）。
    pub fn crop_to_display(&self) -> Nv12View<'a> {
        Nv12View {
            data: self.data,
            stride: self.stride,
            coded_width: self.display_width,
            coded_height: self.display_height,
            display_width: self.display_width,
            display_height: self.display_height,
            uv_offset: self.uv_offset,
        }
    }

    /// NV12 → RGBA8888，用默认矩阵（BT.709 limited）
    ///
    /// `out` 会被**清空并重填**成 `display_width * display_height * 4` 字节，
    /// 字节序是 `R, G, B, A`（A 恒为 255）。
    ///
    /// 注意：Win32 GDI 的 32bpp BI_RGB 位图在内存里要的是 **BGRA**，
    /// 所以 `win32` 层画之前会做一次 R↔B 交换（见 `win32::swap_rb`）。
    /// 这里保持 RGBA 是因为它是「人类直觉顺序」，测试写起来也更直白。
    pub fn to_rgba(&self, out: &mut Vec<u8>) -> Result<(), ConvertError> {
        self.to_rgba_with(out, Matrix::default())
    }

    /// NV12 → RGBA8888，指定矩阵
    pub fn to_rgba_with(&self, out: &mut Vec<u8>, matrix: Matrix) -> Result<(), ConvertError> {
        let dw = self.display_width as usize;
        let dh = self.display_height as usize;
        let stride = self.stride;
        let data = self.data;
        let uv_base = self.uv_offset;

        // # 为什么不是 clear() + resize()
        //
        // `clear()` 把 len 归零，随后的 `resize()` 必须把整块缓冲**重新填 0**。
        // 720p 一帧是 3.7 MB，等于每帧白做一次 memset，然后马上被下面的循环
        // 全部覆盖掉 —— 实测这一条占了总耗时的三成。
        //
        // 改成「长度已经对就什么都不做」：缓冲复用场景（也就是
        // `Renderer::present` 的真实路径）零成本；长度不对时才付一次 resize，
        // 而那次本来就要写满，没有额外损失。
        let need = dw * dh * 4;
        if out.len() != need {
            out.clear();
            out.resize(need, 0);
        }

        let t = tables(matrix);
        // 只转 display 区域：行取 0..dh、列取 0..dw。
        // coded 尺寸多出来的部分（640x368 里最后 8 行）完全不读。
        //
        // # 为什么用 chunks_exact 而不是下标索引
        //
        // 逐像素写 `dst[col*4..col*4+4]` 时，编译器在**每次**迭代都要证明
        // `col*4+4 <= dst.len()` —— 它证明不了（col 是循环变量），于是每次
        // 都留一次边界检查 + 一次 panic 分支。实测这就是第一版 720p 只有
        // 1.49ms（617 MP/s）的主因。
        //
        // 换成三个 `chunks_exact` 互相对齐之后：
        // - 每块长度在类型层面固定（2 / 2 / 8 字节），越界检查被提到循环外；
        // - `zip` 会在最短的一侧停下，天然处理「宽度不是 2 的倍数」，
        //   不需要手写 `if col + 1 < dw` 那条分支。
        // 改完 720p 降到 0.44ms（2.1x）。数字见 examples/bench_convert.rs。
        for row in 0..dh {
            let y_base = row * stride;
            // 色度是 2x2 子采样：显示行 row 用色度行 row/2
            let uv_base_row = uv_base + (row / 2) * stride;

            // 三个切片都截到 dw 长 —— display 宽度之外（右侧宏块填充）一个字节都不读
            let y_src = &data[y_base..y_base + dw];
            let uv_src = &data[uv_base_row..uv_base_row + dw];
            let dst = &mut out[row * dw * 4..(row + 1) * dw * 4];

            // 一对像素 = 2 个 Y + 1 组 UV + 8 个输出字节
            let pairs = y_src
                .chunks_exact(2)
                .zip(uv_src.chunks_exact(2))
                .zip(dst.chunks_exact_mut(8));

            for ((y2, uv2), d8) in pairs {
                let cb = uv2[0] as usize;
                let cr = uv2[1] as usize;

                // 色度贡献对一对像素是常量，先算好（+32768 是四舍五入的半个 LSB）
                let r_chroma = t.cr_r[cr] + 32768;
                let g_chroma = t.cb_g[cb] + t.cr_g[cr] + 32768;
                let b_chroma = t.cb_b[cb] + 32768;

                // 一个像素打包成一个 u32 再写 —— 4 次 u8 store 合成 1 次 32 位 store，
                // 而且省掉 4 次「按字节写」的别名检查。
                // 小端下 u32 的字节序天然是 [R, G, B, A]。
                let y0 = t.y[y2[0] as usize];
                let p0 = u32::from_le_bytes([
                    clamp8((y0 + r_chroma) >> 16),
                    clamp8((y0 + g_chroma) >> 16),
                    clamp8((y0 + b_chroma) >> 16),
                    255,
                ]);
                let y1 = t.y[y2[1] as usize];
                let p1 = u32::from_le_bytes([
                    clamp8((y1 + r_chroma) >> 16),
                    clamp8((y1 + g_chroma) >> 16),
                    clamp8((y1 + b_chroma) >> 16),
                    255,
                ]);
                d8[0..4].copy_from_slice(&p0.to_le_bytes());
                d8[4..8].copy_from_slice(&p1.to_le_bytes());
            }
        }
        Ok(())
    }
}

/// `>> 16` 之后夹到 0..=255
///
/// # 为什么用 `clamp` 而不是两个 `if`
///
/// 写成 `if v < 0 { 0 } else if v > 255 { 255 } else { v as u8 }` 时，
/// 第二版实测 720p 是 1.26ms；换成 `clamp` 之后 LLVM 生成的是
/// `pmaxsd` + `pminsd`（无分支），配合下面的 `chunks_exact` 一起
/// 把内层循环压到没有一条跳转指令。数字见 `examples/bench_convert.rs`。
#[inline(always)]
fn clamp8(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}
