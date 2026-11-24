//! 采集层的缩放（**纯跨平台**，不碰任何平台 API）
//!
//! # 为什么缩放必须在 BGRA 域做，而不是先转 NV12 再缩
//!
//! NV12 的 UV 已经是 2x2 下采样过的。如果先转 NV12 再缩到一半，色度会被
//! **再**下采样一次 —— 4K→1080p 时色度总共降了 4 倍，颜色糊成一片。
//!
//! 正确顺序是：**BGRA 缩放到目标尺寸 → 再做一次标准的 2x2 UV 下采样**。
//!
//! # 为什么不需要中间缓冲
//!
//! 「先缩放再 2x2 下采样」看起来要物化一整帧缩放后的 BGRA（4K→1080p 是
//! 8.3 MB）。但缩放后的一个 UV 块 = 4 个缩放像素 = 源图上一个
//! 2fx × 2fy 的矩形，所以 UV 可以直接从源数据算出来，**逐字节等于**
//! 「先物化缩放结果、再跑标准转换」。
//!
//! 测试 `matches_materialized_scale` 用「真的物化一遍再跑
//! [`crate::convert::bgra_to_nv12`]」做对照，把这个等价关系钉住 ——
//! 省掉的那块临时缓冲不是靠推理保证的，是靠断言保证的。
//!
//! # 缩放算法
//!
//! | 源 → 目标 | 倍率 | 算法 |
//! | --- | --- | --- |
//! | 3840x2160 → 1920x1080 | 2:1 | **面积平均** |
//! | 3840x2160 → 1280x720 | 3:1 | **面积平均** |
//! | 1920x1080 → 1280x720 | 1.5:1 | 最近邻（**有锯齿**） |
//! | 任意 → 更大 | 放大 | 最近邻（无信息增益） |
//!
//! ## 整数倍为什么用面积平均
//!
//! 每个目标像素恰好对应 fx×fy 个源像素，取算术平均就是**数学上正确的降采样**
//! （box filter）：等价于理想低通后再抽取，不产生锯齿，也不会像双线性那样
//! 「只采 4 个角点」而丢掉其余样本。4K→1080p 是精确 2:1，源图每个 2x2 块
//! 正好贡献一个目标像素。
//!
//! ## 非整数倍为什么这一版不做双线性
//!
//! 面积平均对非整数倍**也**是正确的，但需要为每个目标像素算
//! 「源区间 → 权重」的分数表（区间可能跨 2 或 3 个源像素、权重各不相同），
//! 实现量和测试面都翻倍。而实际场景里编码器尺寸基本都是 16 的倍数
//! （640x360 / 1280x720 / 1920x1080），与常见屏幕分辨率（1920x1080 /
//! 2560x1440 / 3840x2160）之间**大多**是整数倍关系。
//!
//! 所以第一版：非整数倍走最近邻，**已知有锯齿**（细文字和高对比边缘最明显），
//! 但尺寸正确、不 panic、不偏色。需要更好质量时再补分数权重表 ——
//! 那时 [`ScaleMode`] 加一个 `AreaFractional` 变体即可，调用方不用改。
//!
//! ## 为什么即使做非整数倍也不优先选双线性
//!
//! 双线性是**插值**：2:1 降采样时它只取 4 个角点、跳过中间的像素，
//! 相当于先抽取再插值，会产生摩尔纹。面积平均是**积分**：读遍所有源像素，
//! 信息不丢。降采样场景下面积平均在质量和「读全量数据」上都不吃亏。

use crate::convert::{
    clamp_u8, FrameLayout, C_MAX, C_MIN, C_NEUTRAL, ROUND, TAB_Y_B, TAB_Y_G, TAB_Y_R, Y_BLACK,
    Y_WHITE,
};
use crate::error::{CaptureError, Result};

/// 缩放方式
///
/// 由 [`ScalePlan::new`] 按倍率自动选择 —— 调用方不该手动指定：
/// 选错会直接造成画质问题（2:1 降采样用最近邻会出锯齿），
/// 而这个判断是完全确定的。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScaleMode {
    /// 不缩放：源尺寸 == 目标尺寸
    None,
    /// 整数倍降采样，面积平均
    AreaAverage,
    /// 最近邻（非整数倍降采样，或任何放大）
    Nearest,
}

impl ScaleMode {
    /// 人类可读的名字（日志用）
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "无缩放",
            Self::AreaAverage => "面积平均",
            Self::Nearest => "最近邻",
        }
    }
}

/// 一次缩放的完整计划
///
/// # 为什么把四个尺寸和算法打包
///
/// 这些值必须彼此一致：因子算错会让面积平均的采样窗口跨到相邻行
/// （画面斜切），算法选错会让 2:1 降采样退化成有锯齿的最近邻。
/// 分散成独立参数时编译器抓不住，打包成一个 Copy 类型后
/// 只有 [`Self::new`] 一条构造路径。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScalePlan {
    src_width: u32,
    src_height: u32,
    dst_width: u32,
    dst_height: u32,
    fx: u32,
    fy: u32,
    mode: ScaleMode,
}

impl ScalePlan {
    /// 由源尺寸与目标尺寸推导缩放计划
    ///
    /// # 判定顺序
    ///
    /// 1. 任一维度要放大 → [`ScaleMode::Nearest`]（放大无信息增益）
    /// 2. 尺寸完全相同 → [`ScaleMode::None`]
    /// 3. 宽高都能整除（因子各自计算，允许 fx≠fy）→ [`ScaleMode::AreaAverage`]
    /// 4. 其余 → [`ScaleMode::Nearest`]
    ///
    /// 第 3 条允许 fx≠fy：3840x2160 → 1280x1080 是「横向 3:1、纵向 2:1」，
    /// 仍然可以面积平均，只是采样窗口是 3x2 而不是正方形。
    pub fn new(src_width: u32, src_height: u32, dst_width: u32, dst_height: u32) -> Result<Self> {
        if src_width == 0 || src_height == 0 {
            return Err(CaptureError::InvalidArgument(format!(
                "源尺寸不能为 0，当前 {src_width}x{src_height}"
            )));
        }
        if dst_width == 0 || dst_height == 0 {
            return Err(CaptureError::InvalidArgument(format!(
                "目标尺寸不能为 0，当前 {dst_width}x{dst_height}"
            )));
        }
        // 放大：仍然要能工作（用户可能把 720p 屏配成 1080p），但只能最近邻
        if dst_width > src_width || dst_height > src_height {
            return Ok(Self {
                src_width,
                src_height,
                dst_width,
                dst_height,
                fx: 1,
                fy: 1,
                mode: ScaleMode::Nearest,
            });
        }
        if src_width == dst_width && src_height == dst_height {
            return Ok(Self {
                src_width,
                src_height,
                dst_width,
                dst_height,
                fx: 1,
                fy: 1,
                mode: ScaleMode::None,
            });
        }
        let fx = src_width / dst_width;
        let fy = src_height / dst_height;
        let exact = src_width % dst_width == 0 && src_height % dst_height == 0;
        Ok(Self {
            src_width,
            src_height,
            dst_width,
            dst_height,
            fx,
            fy,
            mode: if exact && fx >= 1 && fy >= 1 {
                ScaleMode::AreaAverage
            } else {
                ScaleMode::Nearest
            },
        })
    }

    /// 不缩放的计划（源尺寸即目标尺寸）
    pub fn identity(size: (u32, u32)) -> Result<Self> {
        Self::new(size.0, size.1, size.0, size.1)
    }

    /// 源宽度
    pub const fn src_width(&self) -> u32 {
        self.src_width
    }

    /// 源高度
    pub const fn src_height(&self) -> u32 {
        self.src_height
    }

    /// 目标宽度
    pub const fn dst_width(&self) -> u32 {
        self.dst_width
    }

    /// 目标高度
    pub const fn dst_height(&self) -> u32 {
        self.dst_height
    }

    /// 横向整数倍因子（仅 [`ScaleMode::AreaAverage`] 有意义）
    pub const fn fx(&self) -> u32 {
        self.fx
    }

    /// 纵向整数倍因子（仅 [`ScaleMode::AreaAverage`] 有意义）
    pub const fn fy(&self) -> u32 {
        self.fy
    }

    /// 缩放方式
    pub const fn mode(&self) -> ScaleMode {
        self.mode
    }

    /// 是否实际做了缩放
    pub const fn is_scaling(&self) -> bool {
        !matches!(self.mode, ScaleMode::None)
    }

    /// 每个目标像素平均的源像素数（`AreaAverage` 时为 fx*fy，否则 1）
    pub const fn samples_per_pixel(&self) -> u32 {
        match self.mode {
            ScaleMode::AreaAverage => self.fx * self.fy,
            _ => 1,
        }
    }

    /// 源尺寸
    pub const fn src_size(&self) -> (u32, u32) {
        (self.src_width, self.src_height)
    }

    /// 目标尺寸
    pub const fn dst_size(&self) -> (u32, u32) {
        (self.dst_width, self.dst_height)
    }
}

/// 缩放并转成 NV12（面积平均 / 最近邻，见模块文档）
///
/// # 参数
///
/// - `bgra`：源像素，行距 `stride` 字节
/// - `stride`：源行距（DXGI 的 staging 纹理常按 256 字节对齐，不等于 width*4）
/// - `src`：源尺寸（显示尺寸）
/// - `layout`：**目标**尺寸布局（`display_width/height` 是缩放后的尺寸，
///   `coded_width/height` 是它 16 对齐后的值）
/// - `dst`：目标缓冲，长度必须够一帧 NV12
///
/// # 与 [`crate::convert::bgra_to_nv12`] 的关系
///
/// 那个函数的签名被发送端接线依赖，**不能改**。这个函数是它的超集：
/// 源尺寸与目标尺寸相同时内部直接走同一条无缩放路径。
///
/// # 为什么一次遍历同时做缩放和色彩转换
///
/// 分两步（先缩放成一张目标尺寸的 BGRA，再跑转换）要多一次全帧读写，
/// 4K→1080p 是「写 8.3 MB + 读 8.3 MB」，实测约 3 ms。融合之后这部分消失。
///
/// # UV 的处理
///
/// UV 的每个块对应目标图上 2x2 个像素，也就是源图上 2fx × 2fy 的矩形。
/// 这里直接从源数据取那个矩形的平均 —— 与「先缩放再 2x2 下采样」逐字节相同
/// （见模块文档，测试 `matches_materialized_scale` 钉住了）。
pub fn bgra_to_nv12_scaled(
    bgra: &[u8],
    stride: usize,
    src: (u32, u32),
    layout: FrameLayout,
    dst: &mut [u8],
) -> Result<()> {
    let plan = ScalePlan::new(
        src.0,
        src.1,
        layout.display_width(),
        layout.display_height(),
    )?;
    bgra_to_nv12_with_plan(bgra, stride, plan, layout, dst)
}

/// 用现成的 [`ScalePlan`] 缩放并转换
///
/// 采集层每帧复用同一个 plan（构造时算好），避免每帧重新推导 ——
/// 推导本身很便宜，但把它放在热路径上会让 [`ScalePlan::new`] 的
/// 参数校验（两个 `format!` 分支）每帧都跑一遍。
pub fn bgra_to_nv12_with_plan(
    bgra: &[u8],
    stride: usize,
    plan: ScalePlan,
    layout: FrameLayout,
    dst: &mut [u8],
) -> Result<()> {
    let sw = plan.src_width as usize;
    let sh = plan.src_height as usize;
    let cw = layout.coded_width() as usize;
    let ch = layout.coded_height() as usize;

    if layout.display_width() != plan.dst_width || layout.display_height() != plan.dst_height {
        return Err(CaptureError::InvalidArgument(format!(
            "layout 的显示尺寸 {}x{} 与 ScalePlan 的目标尺寸 {}x{} 不一致",
            layout.display_width(),
            layout.display_height(),
            plan.dst_width,
            plan.dst_height
        )));
    }
    if stride < sw * 4 {
        return Err(CaptureError::InvalidArgument(format!(
            "行距 {stride} 小于源宽*4 = {}",
            sw * 4
        )));
    }
    let need = (sh - 1) * stride + sw * 4;
    if bgra.len() < need {
        return Err(CaptureError::InvalidArgument(format!(
            "BGRA 缓冲不足：需要 {need} 字节，实际 {}",
            bgra.len()
        )));
    }
    if dst.len() < layout.nv12_len() {
        return Err(CaptureError::InvalidArgument(format!(
            "NV12 缓冲不足：需要 {} 字节，实际 {}",
            layout.nv12_len(),
            dst.len()
        )));
    }

    // 无缩放时直接复用现成实现，避免两套代码路径的数值分歧
    if !plan.is_scaling() {
        return crate::convert::bgra_to_nv12(bgra, stride, layout, dst);
    }

    match plan.mode {
        // 面积平均走融合路径：一次遍历同时产出 Y 和 UV，
        // 源数据只读一遍（分两遍会慢 45%，见 scale_area_fused 的说明）
        ScaleMode::AreaAverage => scale_area_fused(bgra, stride, &plan, cw, ch, dst),
        // None 已在上面提前返回，这里只剩 Nearest
        ScaleMode::Nearest | ScaleMode::None => {
            scale_nearest_y(bgra, stride, &plan, cw, ch, dst);
            scale_nearest_uv(bgra, stride, &plan, cw, ch, dst);
        }
    }
    Ok(())
}

/// 求源图一个矩形区域的平均 RGB（逐通道四舍五入）
///
/// # 为什么是「先平均 RGB 再转换」而不是「先转 Y 再平均」
///
/// 模块文档定义的语义是：**BGRA 缩放到目标尺寸 → 再跑标准转换**。
/// 「缩放」在 RGB 域就是逐通道平均，所以目标像素的颜色是
/// `(round(R̄), round(Ḡ), round(B̄))`，然后才转成 Y/Cb/Cr。
///
/// 另一种写法「先逐像素转 Y 再对 Y 求平均」看起来更省一次表查找，但那是
/// **另一套语义**，结果会差 1。测试 `matches_materialized_scale` 直接
/// 物化一份缩放后的 BGRA 再跑 [`crate::convert::bgra_to_nv12`] 做对照 ——
/// 那条断言要求实现严格遵循「先缩放后转换」，不能为了省事换语义。
#[inline]
fn avg_rgb(
    bgra: &[u8],
    stride: usize,
    sx0: usize,
    sy0: usize,
    fx: usize,
    fy: usize,
) -> (u8, u8, u8) {
    let n = (fx * fy) as u32;
    let half = n / 2;
    let (mut sr, mut sg, mut sb) = (0u32, 0u32, 0u32);
    for ry in 0..fy {
        let base = (sy0 + ry) * stride + sx0 * 4;
        let row = &bgra[base..base + fx * 4];
        for px in row.chunks_exact(4) {
            sb += u32::from(px[0]);
            sg += u32::from(px[1]);
            sr += u32::from(px[2]);
        }
    }
    (
        ((sr + half) / n) as u8,
        ((sg + half) / n) as u8,
        ((sb + half) / n) as u8,
    )
}

/// Y 平面：整数倍面积平均
///
/// 目标像素的 RGB 是源 fx×fy 块的平均，再按 BT.709 转 Y —— 与
/// 「先缩放 BGRA 再转换」逐字节相同。右侧/底部填充用边缘复制。
/// 面积平均：**一次遍历同时产出 Y 和 UV**
///
/// # 为什么必须融合
///
/// 分成「先算 Y 平面、再算 UV 平面」两遍时，UV 那一遍要把源图的每个像素
/// **再读一次**（因为每个 UV 块要 4 个目标像素，也就是 2fx × 2fy 个源像素，
/// 与 Y 完全重叠）。4K 源是 33 MB，多读一遍实测多花 7 ms ——
/// 比不缩放还慢（22.3 ms vs 15.5 ms），完全违背直觉。
///
/// 融合之后：源数据只读一遍，Y 和 UV 一起算。UV 需要「4 个目标像素的
/// 平均 RGB」，而这 4 个值正好是 2x2 个目标像素 —— 所以按**目标 2x2 块**
/// 为单位遍历，一次拿到 4 个 Y 和 1 对 UV。
///
/// # 为什么按 2x2 目标块遍历而不是按目标行
///
/// 按行遍历时，Y 和 UV 的循环边界不同（UV 行跨两个 Y 行），
/// 融合会变成「一行算 Y、隔行算 UV」的怪结构。按 2x2 块遍历天然对齐：
/// 每个块固定产出 4 个 Y + 1 个 U + 1 个 V。
///
/// 代价是**写 Y 时不能顺序写**（要跳两行），但这是 2 次跨行写 vs
/// 33 MB 多读一遍，前者便宜得多。
fn scale_area_fused(
    bgra: &[u8],
    stride: usize,
    plan: &ScalePlan,
    cw: usize,
    ch: usize,
    dst: &mut [u8],
) {
    let dw = plan.dst_width as usize;
    let dh = plan.dst_height as usize;
    let fx = plan.fx as usize;
    let fy = plan.fy as usize;
    let uv_base = cw * ch;
    let uvw = cw / 2;
    let uvh = ch / 2;

    // 目标行 -> 源行区间的起点，避免每个块重算
    let ty0 = |ty: usize| ty.min(dh - 1) * fy;
    // 目标列 -> 源列区间的起点
    let tx0 = |tx: usize| tx.min(dw - 1) * fx;

    for vy in 0..uvh {
        // 这一 UV 行覆盖的两个目标行（钳位公式与 convert 逐字一致）
        let dy0 = (vy * 2).min(dh - 1);
        let dy1 = (vy * 2 + 1).min(dh - 1);
        let sy0 = ty0(dy0);
        let sy1 = ty0(dy1);

        // Y 平面的两行（可能是同一行 —— 底部填充时 dy0 == dy1）
        let (y_row0, y_row1) = if dy0 == dy1 {
            // 填充行：复用同一个源行区间，但要写到**两个**不同的目标行
            let r = dy0.min(ch - 1);
            (r, r)
        } else {
            (dy0.min(ch - 1), dy1.min(ch - 1))
        };
        let y_off0 = y_row0 * cw;
        let y_off1 = y_row1 * cw;

        let uv_off = uv_base + vy * cw;

        for vx in 0..uvw {
            let dx0 = (vx * 2).min(dw - 1);
            let dx1 = (vx * 2 + 1).min(dw - 1);
            let sx0 = tx0(dx0);
            let sx1 = tx0(dx1);

            // 4 个目标像素的平均 RGB —— 每个都只读源一次
            let (r00, g00, b00) = avg_rgb(bgra, stride, sx0, sy0, fx, fy);
            let (r01, g01, b01) = avg_rgb(bgra, stride, sx1, sy0, fx, fy);
            let (r10, g10, b10) = avg_rgb(bgra, stride, sx0, sy1, fx, fy);
            let (r11, g11, b11) = avg_rgb(bgra, stride, sx1, sy1, fx, fy);

            // 写 4 个 Y（两个目标行各两个）
            let cy0 = if dx0 < cw { dx0 } else { cw - 1 };
            let cy1 = if dx1 < cw { dx1 } else { cw - 1 };
            dst[y_off0 + cy0] = luma(r00, g00, b00);
            dst[y_off0 + cy1] = luma(r01, g01, b01);
            dst[y_off1 + cy0] = luma(r10, g10, b10);
            dst[y_off1 + cy1] = luma(r11, g11, b11);

            // 写 1 对 UV
            dst[uv_off + vx * 2] =
                avg4_cb(r00, g00, b00, r01, g01, b01, r10, g10, b10, r11, g11, b11);
            dst[uv_off + vx * 2 + 1] =
                avg4_cr(r00, g00, b00, r01, g01, b01, r10, g10, b10, r11, g11, b11);
        }
    }

    // ---- 收尾：Y 平面的右侧与底部填充 ----
    //
    // 融合循环只写了「目标 2x2 块」覆盖到的位置。以下三种位置需要单独补：
    //
    // 1. **右侧**：dw 是奇数时最后一个 2x2 块只有 1 列，第 dw..cw 列没写
    // 2. **底部**：dh 是奇数时最后一行只被 dy0 覆盖，dy1 落到了填充行
    // 3. **coded 填充**：dh..ch 这一段（16 对齐多出来的行）完全没被写过
    //
    // 统一用边缘复制补齐 —— 与无缩放路径的策略一致。
    if cw > dw {
        for y in 0..ch {
            let last = dst[y * cw + dw - 1];
            for x in dw..cw {
                dst[y * cw + x] = last;
            }
        }
    }
    // 底部：从最后一个有效行往下一路复制
    if ch > dh {
        let src_row = (dh - 1) * cw;
        let mut prev = src_row;
        for y in dh..ch {
            let cur = y * cw;
            // 逐行复制上一行（不能一次性 memcpy 同一块，会别名）
            for x in 0..cw {
                dst[cur + x] = dst[prev + x];
            }
            prev = cur;
        }
    }
    // UV 平面的填充：uvh 已经覆盖到 ch/2，但右侧多出的列需要补
    if cw > dw {
        for vy in 0..uvh {
            let base = uv_base + vy * cw;
            // 有效 UV 列数 = ceil(dw/2)，其余复制最后一个有效 U/V 对
            let valid = dw.div_ceil(2) * 2;
            if valid < cw {
                let last_u = dst[base + valid - 2];
                let last_v = dst[base + valid - 1];
                for k in (valid..cw).step_by(2) {
                    dst[base + k] = last_u;
                    if k + 1 < cw {
                        dst[base + k + 1] = last_v;
                    }
                }
            }
        }
    }
}

/// RGB → Y（BT.709 limited range），与 [`crate::convert::rgb_to_y`] 同构
#[inline]
fn luma(r: u8, g: u8, b: u8) -> u8 {
    let v =
        (TAB_Y_R[usize::from(r)] + TAB_Y_G[usize::from(g)] + TAB_Y_B[usize::from(b)] + ROUND) >> 16;
    clamp_u8(Y_BLACK as i32 + v, Y_BLACK, Y_WHITE)
}
/// 4 个像素的 Cb 平均
///
/// # 必须与 [`crate::convert`] 的 `avg_cb` **逐字同构**
///
/// 那边用的是「4 个**无钳位偏移**相加、加 128、最后钳一次」：
///
/// ```text
/// Cb = clamp(128 + (Σ raw_cb(px_i)) / 4)
/// ```
///
/// 直觉上「先各自 `rgb_to_cb`（自带 128 与钳位）再平均」应该等价，但**不等价**：
/// Rust 的整数除法向零截断，负偏移会往上取。例：四个偏移 `-3,-3,-3,-2`，
/// 和为 `-11`：
///
/// | 写法 | 计算 | 结果 |
/// | --- | --- | --- |
/// | 先加 128 再平均 | `(125+125+125+126)/4 = 501/4` | **125** |
/// | 先平均再加 128 | `128 + (-11/4) = 128-2` | **126** |
///
/// 这正是 `matches_materialized_scale_2to1` 最初报「UV 差 1」的原因：
/// Y 平面全对，只有 UV 在负偏移样本上差。必须用后一种写法。
#[inline]
#[allow(clippy::too_many_arguments)]
fn avg4_cb(
    r00: u8,
    g00: u8,
    b00: u8,
    r01: u8,
    g01: u8,
    b01: u8,
    r10: u8,
    g10: u8,
    b10: u8,
    r11: u8,
    g11: u8,
    b11: u8,
) -> u8 {
    let s = crate::convert::raw_cb(r00, g00, b00)
        + crate::convert::raw_cb(r01, g01, b01)
        + crate::convert::raw_cb(r10, g10, b10)
        + crate::convert::raw_cb(r11, g11, b11);
    clamp_u8(C_NEUTRAL as i32 + s / 4, C_MIN, C_MAX)
}

/// 4 个像素的 Cr 平均（与 [`avg4_cb`] 同构，理由见那里）
#[inline]
#[allow(clippy::too_many_arguments)]
fn avg4_cr(
    r00: u8,
    g00: u8,
    b00: u8,
    r01: u8,
    g01: u8,
    b01: u8,
    r10: u8,
    g10: u8,
    b10: u8,
    r11: u8,
    g11: u8,
    b11: u8,
) -> u8 {
    let s = crate::convert::raw_cr(r00, g00, b00)
        + crate::convert::raw_cr(r01, g01, b01)
        + crate::convert::raw_cr(r10, g10, b10)
        + crate::convert::raw_cr(r11, g11, b11);
    clamp_u8(C_NEUTRAL as i32 + s / 4, C_MIN, C_MAX)
}

/// 最近邻采样：目标像素 `d`（共 `d_size` 个）对应源像素
///
/// 取目标像素**中心**映射到的源坐标：`((2d+1)*s) / (2*d_size)`。
/// 用「中心」而不是「左边缘」（`d*s/d_size`）是为了避免整体偏移半个像素 ——
/// 4K→1080p 时两者结果相同，但 1080p→720p 这种非整数倍下，
/// 左边缘公式会让画面整体偏左上。
#[inline]
fn nearest_src(d: usize, s: usize, d_size: usize) -> usize {
    (((2 * d + 1) * s) / (2 * d_size)).min(s - 1)
}

/// Y 平面：最近邻
fn scale_nearest_y(
    bgra: &[u8],
    stride: usize,
    plan: &ScalePlan,
    cw: usize,
    ch: usize,
    dst: &mut [u8],
) {
    let sw = plan.src_width as usize;
    let sh = plan.src_height as usize;
    let dw = plan.dst_width as usize;
    let dh = plan.dst_height as usize;

    for dy in 0..ch {
        let dy_valid = dy.min(dh - 1);
        let sy = nearest_src(dy_valid, sh, dh);
        let row = &bgra[sy * stride..sy * stride + sw * 4];
        let out = &mut dst[dy * cw..dy * cw + cw];
        for (dx, px) in out[..dw].iter_mut().enumerate() {
            let sx = nearest_src(dx, sw, dw);
            let i = sx * 4;
            let v = (TAB_Y_R[usize::from(row[i + 2])]
                + TAB_Y_G[usize::from(row[i + 1])]
                + TAB_Y_B[usize::from(row[i])]
                + ROUND)
                >> 16;
            *px = clamp_u8(Y_BLACK as i32 + v, Y_BLACK, Y_WHITE);
        }
        if cw > dw {
            let last = out[dw - 1];
            for v in &mut out[dw..] {
                *v = last;
            }
        }
    }
}

/// UV 交织平面：最近邻
///
/// 取 2x2 目标像素各自的最近邻源像素，再按标准规则合成 UV ——
/// 与面积平均路径只差「采样方式」，色度处理逻辑完全一致。
fn scale_nearest_uv(
    bgra: &[u8],
    stride: usize,
    plan: &ScalePlan,
    cw: usize,
    ch: usize,
    dst: &mut [u8],
) {
    let sw = plan.src_width as usize;
    let sh = plan.src_height as usize;
    let dw = plan.dst_width as usize;
    let dh = plan.dst_height as usize;
    let uvw = cw / 2;
    let uvh = ch / 2;
    let uv_base = cw * ch;

    for vy in 0..uvh {
        let out = &mut dst[uv_base + vy * cw..uv_base + vy * cw + cw];
        let uv_row = &mut out[..uvw * 2];
        // 钳位公式与 [`crate::convert::bgra_to_nv12`] 逐字一致，理由同
        // [`scale_area_uv`] 里的注释
        let ty0 = (vy * 2).min(dh - 1);
        let ty1 = (vy * 2 + 1).min(dh - 1);
        let sy0 = nearest_src(ty0, sh, dh);
        let sy1 = nearest_src(ty1, sh, dh);
        let r0 = &bgra[sy0 * stride..sy0 * stride + sw * 4];
        let r1 = &bgra[sy1 * stride..sy1 * stride + sw * 4];

        for vx in 0..uvw {
            let tx0 = (vx * 2).min(dw - 1);
            let tx1 = (vx * 2 + 1).min(dw - 1);
            let sx0 = nearest_src(tx0, sw, dw);
            let sx1 = nearest_src(tx1, sw, dw);
            let p = |row: &[u8], sx: usize| {
                let i = sx * 4;
                (row[i + 2], row[i + 1], row[i])
            };
            let (r00, g00, b00) = p(r0, sx0);
            let (r01, g01, b01) = p(r0, sx1);
            let (r10, g10, b10) = p(r1, sx0);
            let (r11, g11, b11) = p(r1, sx1);
            uv_row[vx * 2] = avg4_cb(r00, g00, b00, r01, g01, b01, r10, g10, b10, r11, g11, b11);
            uv_row[vx * 2 + 1] =
                avg4_cr(r00, g00, b00, r01, g01, b01, r10, g10, b10, r11, g11, b11);
        }
    }
}
