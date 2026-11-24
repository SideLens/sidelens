//! H.264 序列参数集（SPS）解析
//!
//! # 为什么必须自己解析 SPS
//!
//! 接收端在**不解码**的前提下要知道三件事：
//!
//! 1. 这一路视频的真实显示分辨率（决定虚拟摄像头输出什么尺寸）
//! 2. 帧率（决定抖动缓冲的目标深度）
//! 3. 分辨率有没有变（手机旋转、切换前后摄像头 → 解码器必须重建）
//!
//! 这三件事全部躺在 SPS 里，而 SPS 是**比特级**的指数哥伦布编码，
//! 不能按字节读。所以本模块自带一个比特读取器。
//!
//! # 两个最常见的错误
//!
//! ## 1. 忘了 cropping
//!
//! H.264 的编码尺寸必须是 16 的整数倍。1920x1080 的视频实际编码成
//! **1920x1088**，多出来的 8 行靠 `frame_cropping_flag` 里的
//! `frame_crop_bottom_offset = 4` 裁掉（单位是 CropUnitY = 2）。
//!
//! 不算 cropping 就会报 1088，虚拟摄像头输出 1088 行，画面底部多一条绿边。
//! 这个错误极其常见，有专门测试盯着。
//!
//! ## 2. 忘了 frame_mbs_only_flag
//!
//! `frame_mbs_only_flag = 0` 表示隔行（场编码），此时
//! `pic_height_in_map_units_minus1` 数的是**宏块对行**，真实高度要 **×2**。
//! 同时 CropUnitY 也会再乘一次 `(2 - frame_mbs_only_flag)`。
//!
//! # 不可信输入
//!
//! SPS 来自网络，随时可能是垃圾。本模块的铁律：
//!
//! - **绝不 panic**：所有越界都返回 [`SpsError`]
//! - **绝不死循环**：`ue(v)` 的前导零超过 [`MAX_UE_LEADING_ZEROS`] 立即报错
//! - **绝不无界分配**：循环次数全部有上限（scaling list、cpb_cnt、HRD）
//!
//! # 语法表
//!
//! 逐条对照 ITU-T H.264 (06/2019) 7.3.2.1.1 `seq_parameter_set_data()`，
//! 顺序错一个字段后面全错 —— 所以这里**不允许"大概对就行"**。

#![forbid(unsafe_code)]

use std::fmt;

use crate::nalu::{split_annexb, NaluType};

/// `ue(v)` / `se(v)` 允许的最大前导零个数
///
/// 32 个前导零时码字还能用 u64 算出来（随后被 u32 范围检查拦掉）；
/// 33 个及以上直接判畸形 —— 再往后只会是攻击载荷，不可能是真实码流。
pub const MAX_UE_LEADING_ZEROS: u32 = 32;

/// 派生分辨率的上限（像素）
///
/// 8K 是 7680，这里留了充足余量。超过就说明 SPS 字段是垃圾值，
/// 与其返回一个荒谬的分辨率去分配缓冲区，不如报错。
const MAX_DIMENSION: u64 = 1 << 16;

/// HRD 里 `cpb_cnt_minus1` 的合法上限（规范：0..=31）
const MAX_CPB_COUNT_MINUS1: u32 = 31;

/// POC 类型 1 里 `num_ref_frames_in_pic_order_cnt_cycle` 的合法上限（规范：0..=255）
const MAX_POC_CYCLE: u32 = 255;

/// 带扩展语法（chroma_format_idc / bit_depth / scaling matrix）的 profile
///
/// 名单来自规范 7.3.2.1.1 的条件判断，**必须一字不差**：
/// 漏一个 profile 就会把后面所有字段读错位。
const HIGH_PROFILE_IDCS: [u8; 13] = [100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135];

/// SPS 解析错误
///
/// 所有变体都表示「这份 SPS 不能用」，调用方应当保留上一份有效参数集
/// 继续工作，而不是让整条链路崩掉。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpsError {
    /// 载荷为空（连 NALU 头都没有）
    EmptyNalu,
    /// NAL 类型不是 7（SPS）
    NotSps {
        /// 实际读到的 nal_unit_type
        nal_unit_type: u8,
    },
    /// 比特流提前结束
    UnexpectedEnd {
        /// 出错时读到的比特位置
        at_bit: usize,
    },
    /// `ue(v)` 前导零过多，典型的畸形/攻击输入
    LeadingZerosTooMany {
        /// 开始读这个语法元素时的比特位置
        at_bit: usize,
        /// 实际读到的前导零个数
        leading_zeros: u32,
    },
    /// 语法元素超出本实现接受的范围
    ValueOutOfRange {
        /// 字段名
        field: &'static str,
        /// 读到的值
        value: u64,
    },
    /// cropping 偏移不小于编码尺寸，裁完就没画面了
    InvalidCropping {
        /// 编码宽度
        coded_width: u64,
        /// 编码高度
        coded_height: u64,
    },
}

impl fmt::Display for SpsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SpsError::EmptyNalu => f.write_str("SPS 载荷为空"),
            SpsError::NotSps { nal_unit_type } => {
                write!(f, "NAL 类型是 {nal_unit_type}，不是 SPS(7)")
            }
            SpsError::UnexpectedEnd { at_bit } => {
                write!(f, "SPS 在比特位置 {at_bit} 处提前结束")
            }
            SpsError::LeadingZerosTooMany {
                at_bit,
                leading_zeros,
            } => write!(
                f,
                "SPS 在比特位置 {at_bit} 处出现 {leading_zeros} 个前导零（上限 {MAX_UE_LEADING_ZEROS}）"
            ),
            SpsError::ValueOutOfRange { field, value } => {
                write!(f, "SPS 字段 {field} 的值 {value} 超出允许范围")
            }
            SpsError::InvalidCropping {
                coded_width,
                coded_height,
            } => write!(
                f,
                "SPS 的 cropping 偏移不小于编码尺寸 {coded_width}x{coded_height}"
            ),
        }
    }
}

impl std::error::Error for SpsError {}

/// VUI 里本层关心的字段
///
/// VUI 其余部分（色彩描述、chroma_loc、bitstream_restriction 等）会被
/// **正确跳过但不保存** —— 跳过是为了让 timing info 之后的字段能对齐，
/// 不保存是因为 SideLens 用不到。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VuiInfo {
    /// aspect_ratio_idc（255 表示 Extended_SAR，此时看 sar_width/height）
    pub aspect_ratio_idc: Option<u8>,
    /// 采样宽高比分子（仅 Extended_SAR 有效）
    pub sar_width: u16,
    /// 采样宽高比分母（仅 Extended_SAR 有效）
    pub sar_height: u16,
    /// 全范围（0-255）还是有限范围（16-235）
    pub video_full_range_flag: bool,
    /// 是否存在 timing info
    pub timing_info_present_flag: bool,
    /// 一个时钟 tick 有多少个时间单位
    pub num_units_in_tick: u32,
    /// 时钟频率（Hz）
    pub time_scale: u32,
    /// 固定帧率标志
    pub fixed_frame_rate_flag: bool,
    /// NAL HRD 参数存在
    pub nal_hrd_parameters_present_flag: bool,
    /// VCL HRD 参数存在
    pub vcl_hrd_parameters_present_flag: bool,
    /// 低延迟 HRD
    pub low_delay_hrd_flag: bool,
    /// 是否存在 pic_struct（影响帧/场重排）
    pub pic_struct_present_flag: bool,
    /// 是否存在 bitstream restriction
    pub bitstream_restriction_flag: bool,
}

/// 解析后的 SPS
///
/// 字段名与规范 7.3.2.1.1 一一对应，便于逐条核对。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpsInfo {
    /// 编码 profile（66=Baseline, 77=Main, 100=High）
    pub profile_idc: u8,
    /// 约束标志字节（constraint_set0..5 + 2 个保留位）
    pub constraint_flags: u8,
    /// 编码 level，真实 level = level_idc / 10
    pub level_idc: u8,
    /// 参数集 id（0..=31）
    pub seq_parameter_set_id: u32,
    /// 色度采样格式（0=单色, 1=4:2:0, 2=4:2:2, 3=4:4:4）
    pub chroma_format_idc: u32,
    /// 4:4:4 时是否三平面独立编码（为真时 ChromaArrayType 视为 0）
    pub separate_colour_plane_flag: bool,
    /// 亮度位深减 8
    pub bit_depth_luma_minus8: u32,
    /// 色度位深减 8
    pub bit_depth_chroma_minus8: u32,
    /// 变换旁路（无损编码）
    pub qpprime_y_zero_transform_bypass_flag: bool,
    /// frame_num 位宽减 4
    pub log2_max_frame_num_minus4: u32,
    /// POC 类型（0/1/2，3 保留）
    pub pic_order_cnt_type: u32,
    /// POC 类型 0 时的 POC LSB 位宽减 4
    pub log2_max_pic_order_cnt_lsb_minus4: Option<u32>,
    /// POC 类型 1：delta_pic_order_always_zero_flag
    pub delta_pic_order_always_zero_flag: bool,
    /// POC 类型 1：非参考帧的 POC 偏移
    pub offset_for_non_ref_pic: i64,
    /// POC 类型 1：顶场到底场的 POC 偏移
    pub offset_for_top_to_bottom_field: i64,
    /// POC 类型 1：POC 循环长度
    pub num_ref_frames_in_pic_order_cnt_cycle: u32,
    /// 参考帧缓冲上限
    pub max_num_ref_frames: u32,
    /// 是否允许 frame_num 出现空洞
    pub gaps_in_frame_num_value_allowed_flag: bool,
    /// 图像宽度（宏块数减 1）
    pub pic_width_in_mbs_minus1: u32,
    /// 图像高度（map unit 数减 1）
    pub pic_height_in_map_units_minus1: u32,
    /// 1 = 只有帧（逐行）；0 = 支持场（隔行）
    pub frame_mbs_only_flag: bool,
    /// 宏块自适应帧场编码
    pub mb_adaptive_frame_field_flag: bool,
    /// 8x8 运动补偿推断
    pub direct_8x8_inference_flag: bool,
    /// 是否存在裁剪
    pub frame_cropping_flag: bool,
    /// 左裁剪偏移（单位 CropUnitX）
    pub frame_crop_left_offset: u32,
    /// 右裁剪偏移（单位 CropUnitX）
    pub frame_crop_right_offset: u32,
    /// 上裁剪偏移（单位 CropUnitY）
    pub frame_crop_top_offset: u32,
    /// 下裁剪偏移（单位 CropUnitY）
    pub frame_crop_bottom_offset: u32,
    /// VUI（不存在时为 None）
    pub vui: Option<VuiInfo>,
}

impl SpsInfo {
    /// 从**完整 NALU**（含 1 字节 nal_header）解析 SPS
    ///
    /// 允许前面带 Annex-B 起始码（`00 00 01` / `00 00 00 01`），
    /// 因为调用方常常直接拿原始片段喂进来。
    ///
    /// # 错误
    ///
    /// 任何畸形输入都返回 [`SpsError`]，**不会 panic**。
    pub fn parse(nalu: &[u8]) -> Result<Self, SpsError> {
        let body = strip_start_code(nalu);
        let (&header, payload) = body.split_first().ok_or(SpsError::EmptyNalu)?;
        let nal_unit_type = header & 0x1f;
        if nal_unit_type != 7 {
            return Err(SpsError::NotSps { nal_unit_type });
        }

        // 反 emulation prevention：把编码器插入的 0x03 去掉，还原成真正的 RBSP。
        // 不做这一步，任何含 00 00 03 的 SPS 都会读错。
        let rbsp = unescape_rbsp(payload);
        let mut r = BitReader::new(&rbsp);

        let profile_idc = r.read_bits(8)? as u8;
        let constraint_flags = r.read_bits(8)? as u8;
        // 规范 7.3.2.1.1：constraint_set0..5 之后是 reserved_zero_2bits，
        // 「shall be equal to 0」。这两位的值只在两种情况下非零：
        // 1) 码流被位翻转打坏  2) 有人在灌垃圾。
        // 两者都不该被当成参数集接受，所以这里直接拒。
        if constraint_flags & 0b11 != 0 {
            return Err(SpsError::ValueOutOfRange {
                field: "reserved_zero_2bits",
                value: u64::from(constraint_flags & 0b11),
            });
        }
        let level_idc = r.read_bits(8)? as u8;
        let seq_parameter_set_id = r.read_ue()?;
        if seq_parameter_set_id > 31 {
            return Err(SpsError::ValueOutOfRange {
                field: "seq_parameter_set_id",
                value: u64::from(seq_parameter_set_id),
            });
        }

        let mut info = SpsInfo {
            profile_idc,
            constraint_flags,
            level_idc,
            seq_parameter_set_id,
            // 非 high profile 时这些字段在码流里不存在，按规范取默认值
            chroma_format_idc: 1,
            separate_colour_plane_flag: false,
            bit_depth_luma_minus8: 0,
            bit_depth_chroma_minus8: 0,
            qpprime_y_zero_transform_bypass_flag: false,
            log2_max_frame_num_minus4: 0,
            pic_order_cnt_type: 0,
            log2_max_pic_order_cnt_lsb_minus4: None,
            delta_pic_order_always_zero_flag: false,
            offset_for_non_ref_pic: 0,
            offset_for_top_to_bottom_field: 0,
            num_ref_frames_in_pic_order_cnt_cycle: 0,
            max_num_ref_frames: 0,
            gaps_in_frame_num_value_allowed_flag: false,
            pic_width_in_mbs_minus1: 0,
            pic_height_in_map_units_minus1: 0,
            frame_mbs_only_flag: true,
            mb_adaptive_frame_field_flag: false,
            direct_8x8_inference_flag: false,
            frame_cropping_flag: false,
            frame_crop_left_offset: 0,
            frame_crop_right_offset: 0,
            frame_crop_top_offset: 0,
            frame_crop_bottom_offset: 0,
            vui: None,
        };

        // ---- high profile 扩展块（7.3.2.1.1 第一段条件语法） ----
        if HIGH_PROFILE_IDCS.contains(&profile_idc) {
            let chroma_format_idc = r.read_ue()?;
            if chroma_format_idc > 3 {
                return Err(SpsError::ValueOutOfRange {
                    field: "chroma_format_idc",
                    value: u64::from(chroma_format_idc),
                });
            }
            info.chroma_format_idc = chroma_format_idc;
            if chroma_format_idc == 3 {
                info.separate_colour_plane_flag = r.read_bit()? == 1;
            }
            info.bit_depth_luma_minus8 = r.read_ue()?;
            info.bit_depth_chroma_minus8 = r.read_ue()?;
            info.qpprime_y_zero_transform_bypass_flag = r.read_bit()? == 1;

            if r.read_bit()? == 1 {
                // 8 个矩阵（i<6 是 16 项，i=6/7 是 64 项）；4:4:4 多 4 个 64 项
                let matrix_count = if chroma_format_idc == 3 { 12 } else { 8 };
                for i in 0..matrix_count {
                    if r.read_bit()? == 1 {
                        skip_scaling_list(&mut r, if i < 6 { 16 } else { 64 })?;
                    }
                }
            }
        }

        // ---- 帧号与 POC ----
        info.log2_max_frame_num_minus4 = r.read_ue()?;
        info.pic_order_cnt_type = r.read_ue()?;
        match info.pic_order_cnt_type {
            0 => {
                info.log2_max_pic_order_cnt_lsb_minus4 = Some(r.read_ue()?);
            }
            1 => {
                info.delta_pic_order_always_zero_flag = r.read_bit()? == 1;
                info.offset_for_non_ref_pic = r.read_se()?;
                info.offset_for_top_to_bottom_field = r.read_se()?;
                info.num_ref_frames_in_pic_order_cnt_cycle = r.read_ue()?;
                // 规范上限 255；显式拦一道，防止恶意值把循环拖成 CPU 炸弹
                if info.num_ref_frames_in_pic_order_cnt_cycle > MAX_POC_CYCLE {
                    return Err(SpsError::ValueOutOfRange {
                        field: "num_ref_frames_in_pic_order_cnt_cycle",
                        value: u64::from(info.num_ref_frames_in_pic_order_cnt_cycle),
                    });
                }
                for _ in 0..info.num_ref_frames_in_pic_order_cnt_cycle {
                    // offset_for_ref_frame[i]，本层不用但必须消费掉
                    let _ = r.read_se()?;
                }
            }
            _ => {
                // 2 = 只用 frame_num 推导；3 = 保留值。两者都没有额外字段，
                // 语法结构与 2 相同，所以不报错（真实码流偶有 3）。
            }
        }

        // ---- 尺寸 ----
        info.max_num_ref_frames = r.read_ue()?;
        info.gaps_in_frame_num_value_allowed_flag = r.read_bit()? == 1;
        info.pic_width_in_mbs_minus1 = r.read_ue()?;
        info.pic_height_in_map_units_minus1 = r.read_ue()?;
        info.frame_mbs_only_flag = r.read_bit()? == 1;
        if !info.frame_mbs_only_flag {
            info.mb_adaptive_frame_field_flag = r.read_bit()? == 1;
        }
        info.direct_8x8_inference_flag = r.read_bit()? == 1;

        // ---- cropping ----
        info.frame_cropping_flag = r.read_bit()? == 1;
        if info.frame_cropping_flag {
            info.frame_crop_left_offset = r.read_ue()?;
            info.frame_crop_right_offset = r.read_ue()?;
            info.frame_crop_top_offset = r.read_ue()?;
            info.frame_crop_bottom_offset = r.read_ue()?;
        }

        // ---- VUI ----
        if r.read_bit()? == 1 {
            info.vui = Some(parse_vui(&mut r)?);
        }

        info.validate_geometry()?;
        Ok(info)
    }

    /// 编码尺寸（未裁剪，宏块对齐）
    ///
    /// 1920x1080 的视频这里会返回 `(1920, 1088)`。
    pub fn coded_resolution(&self) -> (u32, u32) {
        let (w, h) = self.coded_resolution_u64();
        (saturate_u32(w), saturate_u32(h))
    }

    /// **最终显示分辨率**（已应用 cropping）
    ///
    /// 这是虚拟摄像头应该输出的尺寸。1920x1080 的视频返回 `(1920, 1080)`，
    /// 不是 1088。
    pub fn resolution(&self) -> (u32, u32) {
        let (coded_w, coded_h) = self.coded_resolution_u64();
        let (crop_unit_x, crop_unit_y) = self.crop_unit();
        let crop_x = crop_unit_x
            * (u64::from(self.frame_crop_left_offset) + u64::from(self.frame_crop_right_offset));
        let crop_y = crop_unit_y
            * (u64::from(self.frame_crop_top_offset) + u64::from(self.frame_crop_bottom_offset));
        (
            saturate_u32(coded_w.saturating_sub(crop_x)),
            saturate_u32(coded_h.saturating_sub(crop_y)),
        )
    }

    /// 从 VUI timing info 反算帧率
    ///
    /// 规范公式：`frame_rate = time_scale / (2 * num_units_in_tick)`。
    ///
    /// 没有 VUI、没有 timing info、或 `num_units_in_tick == 0`（畸形）时返回 None。
    pub fn frame_rate(&self) -> Option<f64> {
        let vui = self.vui?;
        if !vui.timing_info_present_flag || vui.num_units_in_tick == 0 || vui.time_scale == 0 {
            return None;
        }
        Some(f64::from(vui.time_scale) / (2.0 * f64::from(vui.num_units_in_tick)))
    }

    /// 场率（隔行码流里 field rate = 2 × frame rate）
    pub fn field_rate(&self) -> Option<f64> {
        self.frame_rate().map(|f| f * 2.0)
    }

    /// ChromaArrayType（7.4.2.1.1）
    ///
    /// `separate_colour_plane_flag == 1` 时规范要求当作单色处理。
    pub fn chroma_array_type(&self) -> u32 {
        if self.separate_colour_plane_flag {
            0
        } else {
            self.chroma_format_idc
        }
    }

    /// 裁剪单位 `(CropUnitX, CropUnitY)`
    ///
    /// 7.4.2.1.1：
    ///
    /// - ChromaArrayType == 0：`CropUnitX = 1`，`CropUnitY = 2 - frame_mbs_only_flag`
    /// - 否则：`CropUnitX = SubWidthC`，`CropUnitY = SubHeightC * (2 - frame_mbs_only_flag)`
    pub fn crop_unit(&self) -> (u64, u64) {
        let field_factor = 2 - u64::from(self.frame_mbs_only_flag);
        if self.chroma_array_type() == 0 {
            (1, field_factor)
        } else {
            (self.sub_width_c(), self.sub_height_c() * field_factor)
        }
    }

    /// SubWidthC（由 chroma_format_idc 决定，与 ChromaArrayType 无关）
    fn sub_width_c(&self) -> u64 {
        match self.chroma_format_idc {
            1 | 2 => 2,
            _ => 1,
        }
    }

    /// SubHeightC（由 chroma_format_idc 决定，与 ChromaArrayType 无关）
    fn sub_height_c(&self) -> u64 {
        match self.chroma_format_idc {
            1 => 2,
            _ => 1,
        }
    }

    fn coded_resolution_u64(&self) -> (u64, u64) {
        let width = (u64::from(self.pic_width_in_mbs_minus1) + 1) * 16;
        let height = (u64::from(self.pic_height_in_map_units_minus1) + 1)
            * 16
            * (2 - u64::from(self.frame_mbs_only_flag));
        (width, height)
    }

    /// 拒绝会导致下游分配荒谬缓冲区的几何参数
    fn validate_geometry(&self) -> Result<(), SpsError> {
        let (coded_w, coded_h) = self.coded_resolution_u64();
        if coded_w == 0 || coded_h == 0 || coded_w > MAX_DIMENSION || coded_h > MAX_DIMENSION {
            return Err(SpsError::ValueOutOfRange {
                field: "coded_resolution",
                value: coded_w.max(coded_h),
            });
        }
        if self.frame_cropping_flag {
            let (crop_unit_x, crop_unit_y) = self.crop_unit();
            let crop_x = crop_unit_x
                * (u64::from(self.frame_crop_left_offset)
                    + u64::from(self.frame_crop_right_offset));
            let crop_y = crop_unit_y
                * (u64::from(self.frame_crop_top_offset)
                    + u64::from(self.frame_crop_bottom_offset));
            if crop_x >= coded_w || crop_y >= coded_h {
                return Err(SpsError::InvalidCropping {
                    coded_width: coded_w,
                    coded_height: coded_h,
                });
            }
        }
        Ok(())
    }
}

impl fmt::Display for SpsInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (w, h) = self.resolution();
        write!(
            f,
            "SPS(id={} {}x{} profile={} level={}",
            self.seq_parameter_set_id, w, h, self.profile_idc, self.level_idc
        )?;
        match self.frame_rate() {
            Some(fps) => write!(f, " {fps:.3}fps")?,
            None => f.write_str(" fps=?")?,
        }
        f.write_str(")")
    }
}

/// 一次分辨率变更
///
/// 收到这个的接收端**必须重建解码器**（AVCodecContext / MF transform），
/// 只改输出尺寸而不重建，轻则花屏，重则解码器崩溃。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolutionChange {
    /// 变更前的显示分辨率
    pub from: (u32, u32),
    /// 变更后的显示分辨率
    pub to: (u32, u32),
}

impl ResolutionChange {
    /// 是否只是横竖互换（手机旋转的典型形态）
    ///
    /// 旋转场景下宽高互换，但宏块布局完全不同，**仍然需要重建解码器**；
    /// 这个判断只用来决定要不要做额外的旋转处理。
    pub fn is_orientation_flip(&self) -> bool {
        self.from == (self.to.1, self.to.0)
    }
}

impl fmt::Display for ResolutionChange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "分辨率变更 {}x{} -> {}x{}",
            self.from.0, self.from.1, self.to.0, self.to.1
        )
    }
}

/// 跟踪 SPS 变化，检测分辨率变更
///
/// # 为什么真实场景必需
///
/// 手机旋转屏幕、切换前后摄像头、编码器自适应降分辨率，
/// 都会让发送端重新发一份 SPS。接收端如果只按第一份 SPS 建解码器，
/// 后续帧会因为宏块布局对不上而花屏，严重时直接崩。
///
/// # 用法
///
/// ```
/// use sidelens_codec::sps::ParameterSetTracker;
///
/// let mut tracker = ParameterSetTracker::new();
/// // 1280x720 baseline SPS
/// let sps = [0x67u8, 0x42, 0x00, 0x1f, 0x95, 0xa8, 0x14, 0x01, 0x6e, 0x40];
/// let mut buf = vec![0, 0, 0, 1];
/// buf.extend_from_slice(&sps);
///
/// assert!(tracker.feed_annexb(&buf).is_empty(), "首次只建立基准，不算变更");
/// assert_eq!(tracker.resolution(), Some((1280, 720)));
/// ```
#[derive(Debug, Default, Clone)]
pub struct ParameterSetTracker {
    current: Option<SpsInfo>,
    /// 最近一次成功解析出的显示分辨率
    current_resolution: Option<(u32, u32)>,
    sps_count: u32,
    change_count: u32,
    parse_error_count: u32,
}

impl ParameterSetTracker {
    /// 新建一个空跟踪器
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂一个 NALU（含 nal_header，可带起始码）
    ///
    /// - 不是 SPS → 返回 None，状态不变
    /// - SPS 畸形 → 返回 None，**状态不变**（保留上一份可用参数集）
    /// - SPS 有效且分辨率变了 → 返回 [`ResolutionChange`]
    /// - SPS 有效但分辨率没变（只是重新发了同一份）→ 返回 None
    ///
    /// 首次收到有效 SPS 只建立基准，不算变更 —— 否则每一路新流
    /// 都会误报一次「从 0x0 变成 1280x720」。
    pub fn feed(&mut self, nalu: &[u8]) -> Option<ResolutionChange> {
        self.feed_checked(nalu).ok().flatten()
    }

    /// 同 [`Self::feed`]，但把畸形 SPS 的错误暴露出来
    ///
    /// 非 SPS 的 NALU 返回 `Ok(None)`；畸形 SPS 返回 `Err` 且状态不变。
    pub fn feed_checked(&mut self, nalu: &[u8]) -> Result<Option<ResolutionChange>, SpsError> {
        if !looks_like_sps(nalu) {
            return Ok(None);
        }
        match SpsInfo::parse(nalu) {
            Ok(info) => Ok(self.accept(info)),
            Err(err) => {
                self.parse_error_count = self.parse_error_count.saturating_add(1);
                Err(err)
            }
        }
    }

    /// 喂一整段 Annex-B，返回其中发生的所有分辨率变更
    ///
    /// 正常情况下最多一个（一段码流里不会塞两份不同分辨率的 SPS），
    /// 但用 Vec 表达更诚实 —— 合并后的码流确实可能一次给多份。
    pub fn feed_annexb(&mut self, buf: &[u8]) -> Vec<ResolutionChange> {
        let mut out = Vec::new();
        for nalu in split_annexb(buf) {
            if nalu.ty != NaluType::Sps {
                continue;
            }
            if let Ok(Some(change)) = self.feed_checked(nalu.data) {
                out.push(change);
            }
        }
        out
    }

    /// 最近一次成功解析的完整 SPS
    pub fn current(&self) -> Option<&SpsInfo> {
        self.current.as_ref()
    }

    /// 最近一次成功解析的显示分辨率
    pub fn resolution(&self) -> Option<(u32, u32)> {
        self.current_resolution
    }

    /// 最近一次成功解析的帧率
    pub fn frame_rate(&self) -> Option<f64> {
        self.current.as_ref().and_then(SpsInfo::frame_rate)
    }

    /// 成功解析过的 SPS 个数（含重复）
    pub fn sps_count(&self) -> u32 {
        self.sps_count
    }

    /// 检测到的分辨率变更次数
    pub fn change_count(&self) -> u32 {
        self.change_count
    }

    /// 畸形 SPS 的次数（运维指标：持续增长说明链路在损坏）
    pub fn parse_error_count(&self) -> u32 {
        self.parse_error_count
    }

    /// 清空全部状态（重新建链时调用）
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    fn accept(&mut self, info: SpsInfo) -> Option<ResolutionChange> {
        self.sps_count = self.sps_count.saturating_add(1);
        let resolution = info.resolution();
        let previous = self.current_resolution;
        self.current = Some(info);
        self.current_resolution = Some(resolution);

        match previous {
            None => None,
            Some(from) if from == resolution => None,
            Some(from) => {
                self.change_count = self.change_count.saturating_add(1);
                Some(ResolutionChange {
                    from,
                    to: resolution,
                })
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 比特读取器
// ---------------------------------------------------------------------------

/// MSB 优先的比特读取器
///
/// 只有几十行，但每一个越界都必须变成错误 —— 这是解析网络数据的底线。
#[derive(Debug, Clone)]
struct BitReader<'a> {
    data: &'a [u8],
    /// 当前比特位置（从 0 开始，MSB 优先）
    pos: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn total_bits(&self) -> usize {
        self.data.len() * 8
    }

    /// 剩余比特数
    fn bits_left(&self) -> usize {
        self.total_bits().saturating_sub(self.pos)
    }

    fn read_bit(&mut self) -> Result<u8, SpsError> {
        self.read_bits(1).map(|v| v as u8)
    }

    fn read_bits(&mut self, n: u32) -> Result<u32, SpsError> {
        if n > 32 {
            return Err(SpsError::ValueOutOfRange {
                field: "read_bits",
                value: u64::from(n),
            });
        }
        let need = n as usize;
        if self.bits_left() < need {
            return Err(SpsError::UnexpectedEnd { at_bit: self.pos });
        }
        let mut value: u32 = 0;
        for _ in 0..n {
            let byte = self.data[self.pos >> 3];
            let bit = (byte >> (7 - (self.pos & 7))) & 1;
            value = (value << 1) | u32::from(bit);
            self.pos += 1;
        }
        Ok(value)
    }

    /// 无符号指数哥伦布 `ue(v)`
    ///
    /// 前导零个数决定码长：`lz` 个 0 + 一个 1 + `lz` 位尾数，
    /// 值 = `2^lz - 1 + 尾数`。
    ///
    /// 前导零超过 [`MAX_UE_LEADING_ZEROS`] 立即报错 —— 全 0 的缓冲区
    /// 会让朴素实现读到底再 panic 或空转。
    fn read_ue(&mut self) -> Result<u32, SpsError> {
        let start = self.pos;
        let mut leading_zeros: u32 = 0;
        loop {
            if self.bits_left() == 0 {
                return Err(SpsError::UnexpectedEnd { at_bit: start });
            }
            if self.read_bit()? == 1 {
                break;
            }
            leading_zeros += 1;
            if leading_zeros > MAX_UE_LEADING_ZEROS {
                return Err(SpsError::LeadingZerosTooMany {
                    at_bit: start,
                    leading_zeros,
                });
            }
        }
        if leading_zeros == 0 {
            return Ok(0);
        }
        let suffix = u64::from(self.read_bits(leading_zeros)?);
        let value = ((1u64 << leading_zeros) - 1) + suffix;
        u32::try_from(value).map_err(|_| SpsError::ValueOutOfRange {
            field: "ue(v)",
            value,
        })
    }

    /// 有符号指数哥伦布 `se(v)`
    ///
    /// 映射：`0 -> 0, 1 -> 1, 2 -> -1, 3 -> 2, 4 -> -2, ...`
    fn read_se(&mut self) -> Result<i64, SpsError> {
        let k = i64::from(self.read_ue()?);
        Ok(if (k & 1) == 1 { (k + 1) / 2 } else { -(k / 2) })
    }
}

// ---------------------------------------------------------------------------
// 语法辅助
// ---------------------------------------------------------------------------

/// 跳过 scaling list（7.3.2.1.1.1）
///
/// 内容本层用不到，但**必须逐项消费**：每个元素一个 `se(v)`，
/// 而 `se(v)` 是变长码，漏读一个后面全错位。
fn skip_scaling_list(r: &mut BitReader<'_>, size: u32) -> Result<(), SpsError> {
    let mut last_scale: i64 = 8;
    let mut next_scale: i64 = 8;
    for _ in 0..size {
        if next_scale != 0 {
            let delta_scale = r.read_se()?;
            next_scale = (last_scale + delta_scale + 256).rem_euclid(256);
        }
        last_scale = if next_scale == 0 {
            last_scale
        } else {
            next_scale
        };
    }
    Ok(())
}

/// 跳过 HRD 参数（E.1.2）
///
/// `cpb_cnt_minus1` 是 `ue(v)`，不加上限的话一个恶意值就能让循环跑几十亿次。
fn skip_hrd_parameters(r: &mut BitReader<'_>) -> Result<(), SpsError> {
    let cpb_cnt_minus1 = r.read_ue()?;
    if cpb_cnt_minus1 > MAX_CPB_COUNT_MINUS1 {
        return Err(SpsError::ValueOutOfRange {
            field: "cpb_cnt_minus1",
            value: u64::from(cpb_cnt_minus1),
        });
    }
    let _bit_rate_scale = r.read_bits(4)?;
    let _cpb_size_scale = r.read_bits(4)?;
    for _ in 0..=cpb_cnt_minus1 {
        let _bit_rate_value_minus1 = r.read_ue()?;
        let _cpb_size_value_minus1 = r.read_ue()?;
        let _cbr_flag = r.read_bit()?;
    }
    let _initial_cpb_removal_delay_length_minus1 = r.read_bits(5)?;
    let _cpb_removal_delay_length_minus1 = r.read_bits(5)?;
    let _dpb_output_delay_length_minus1 = r.read_bits(5)?;
    let _time_offset_length = r.read_bits(5)?;
    Ok(())
}

/// 解析 VUI 参数（E.1.1）
///
/// 只保存 timing info 与几个布尔开关；其余字段照读不误，
/// 因为**跳过不等于可以少读** —— 少读一个比特，后面的 `timing_info`
/// 就会错位，帧率直接算错。
fn parse_vui(r: &mut BitReader<'_>) -> Result<VuiInfo, SpsError> {
    let mut vui = VuiInfo::default();

    if r.read_bit()? == 1 {
        let aspect_ratio_idc = r.read_bits(8)? as u8;
        vui.aspect_ratio_idc = Some(aspect_ratio_idc);
        if aspect_ratio_idc == 255 {
            vui.sar_width = r.read_bits(16)? as u16;
            vui.sar_height = r.read_bits(16)? as u16;
        }
    }
    if r.read_bit()? == 1 {
        let _overscan_appropriate_flag = r.read_bit()?;
    }
    if r.read_bit()? == 1 {
        let _video_format = r.read_bits(3)?;
        vui.video_full_range_flag = r.read_bit()? == 1;
        if r.read_bit()? == 1 {
            let _colour_primaries = r.read_bits(8)?;
            let _transfer_characteristics = r.read_bits(8)?;
            let _matrix_coefficients = r.read_bits(8)?;
        }
    }
    if r.read_bit()? == 1 {
        let _chroma_sample_loc_type_top_field = r.read_ue()?;
        let _chroma_sample_loc_type_bottom_field = r.read_ue()?;
    }
    if r.read_bit()? == 1 {
        vui.timing_info_present_flag = true;
        vui.num_units_in_tick = r.read_bits(32)?;
        vui.time_scale = r.read_bits(32)?;
        vui.fixed_frame_rate_flag = r.read_bit()? == 1;
    }
    vui.nal_hrd_parameters_present_flag = r.read_bit()? == 1;
    if vui.nal_hrd_parameters_present_flag {
        skip_hrd_parameters(r)?;
    }
    vui.vcl_hrd_parameters_present_flag = r.read_bit()? == 1;
    if vui.vcl_hrd_parameters_present_flag {
        skip_hrd_parameters(r)?;
    }
    if vui.nal_hrd_parameters_present_flag || vui.vcl_hrd_parameters_present_flag {
        vui.low_delay_hrd_flag = r.read_bit()? == 1;
    }
    vui.pic_struct_present_flag = r.read_bit()? == 1;
    vui.bitstream_restriction_flag = r.read_bit()? == 1;
    if vui.bitstream_restriction_flag {
        let _motion_vectors_over_pic_boundaries_flag = r.read_bit()?;
        let _max_bytes_per_pic_denom = r.read_ue()?;
        let _max_bits_per_mb_denom = r.read_ue()?;
        let _log2_max_mv_length_horizontal = r.read_ue()?;
        let _log2_max_mv_length_vertical = r.read_ue()?;
        let _max_num_reorder_frames = r.read_ue()?;
        let _max_dec_frame_buffering = r.read_ue()?;
    }
    Ok(vui)
}

/// 去掉 Annex-B 起始码（如果有）
fn strip_start_code(buf: &[u8]) -> &[u8] {
    if buf.starts_with(&[0, 0, 0, 1]) {
        &buf[4..]
    } else if buf.starts_with(&[0, 0, 1]) {
        &buf[3..]
    } else {
        buf
    }
}

/// 快速判断是不是 SPS（只看 nal_unit_type）
fn looks_like_sps(nalu: &[u8]) -> bool {
    strip_start_code(nalu)
        .first()
        .is_some_and(|h| (h & 0x1f) == 7)
}

/// 去掉 emulation prevention 字节（7.3.1 / 7.4.1）
///
/// 编码器在 RBSP 里出现 `00 00 0x`（x ≤ 3）时插入一个 `0x03`，
/// 防止与起始码 `00 00 01` 混淆。解析前必须还原。
fn unescape_rbsp(src: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(src.len());
    let mut zeros = 0usize;
    let mut i = 0usize;
    while i < src.len() {
        let byte = src[i];
        if zeros >= 2 && byte == 0x03 {
            // 后面必须紧跟 00/01/02/03 才是防竞争字节；
            // 否则这个 0x03 就是真实数据。
            match src.get(i + 1) {
                Some(&next) if next > 0x03 => {
                    out.push(byte);
                    zeros = 0;
                }
                _ => zeros = 0,
            }
            i += 1;
            continue;
        }
        out.push(byte);
        if byte == 0 {
            zeros += 1;
        } else {
            zeros = 0;
        }
        i += 1;
    }
    out
}

fn saturate_u32(value: u64) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}
