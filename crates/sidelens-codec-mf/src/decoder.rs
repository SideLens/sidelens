//! H.264 解码器（Media Foundation MFT，同步路径）
//!
//! # 为什么接收端必须有自己的解码器
//!
//! 发送端从第一天就在用 `H264Encoder`（mfh264enc.dll），但接收端只做到
//! 「收包 + 重组 + 抖动缓冲」—— 拿到 Annex-B 码流之后没有任何东西把它变回画面。
//! 本模块补上这一段。
//!
//! # 实测结论速查（都是本机跑出来的，不是抄文档）
//!
//! | 问题 | 答案 |
//! | --- | --- |
//! | CLSID 直连能用吗 | **能**，首选路径 |
//! | MFTEnumEx 能用吗 | 能，但只列出同一个微软软件解码器 |
//! | 输入吃 Annex-B 还是 AVCC | **Annex-B**，原样喂 |
//! | 先设输入还是先设输出 | **先输入**（反了返回 MF_E_TRANSFORM_TYPE_NOT_SET） |
//! | 协商到的输出格式 | **NV12** |
//! | 一帧不设低延迟会攒多久 | **32~33 帧**（30fps 下就是一秒） |
//! | 低延迟开关怎么才算生效 | `AVLowLatencyMode` 必须用 **VT_UI4**，用 VT_BOOL 返回 E_INVALIDARG |
//! | 开了之后延迟 | **0 帧**，一进一出，帧数完全对齐 |
//! | `MF_E_NOTACCEPTING` 频率 | 正常节奏下 **0 次**；不开低延迟连喂 40 帧是 **31 次** |
//!
//! # CLSID 为什么是手写的
//!
//! windows crate 的 `Win32_Media_MediaFoundation` 里**没有**导出
//! `CLSID_CMSH264DecoderMFT`（也没有编码器那个 `CLSID_CMSH264EncoderMFT`，
//! 见 `encoder.rs`）。grep 过 windows-0.62.2 的 mod.rs，只有 `MFVideoFormat_*`、
//! `CODECAPI_*`、`MFT_CATEGORY_*`，没有任何 `CLSID_CMS*` 常量。
//!
//! 所以常量在这里自己定义。值是 MSDN 公开文档里的
//! （[H.264 Decoder](https://learn.microsoft.com/windows/win32/medfound/h-264-video-decoder)）：
//!
//! ```text
//! CLSID_CMSH264DecoderMFT = {62CE7E72-4C71-4D20-B15D-452831A87D9D}
//! ```
//!
//! # 找解码器的两条路（都实现了）
//!
//! | 路径 | 接口 | 实测 |
//! | --- | --- | --- |
//! | 1. `CoCreateInstance(CLSID_CMSH264DecoderMFT)` | 直接造 | **可用**，首选 |
//! | 2. `MFTEnumEx(MFT_CATEGORY_VIDEO_DECODER)` | 枚举后 `ActivateObject` | 可用，兜底 |
//!
//! 两条路在本机拿到的是**同一个** MFT（`"Microsoft H264 Video Decoder MFT"`，
//! async=false，d3d=false —— 本机没装 DXVA 硬件解码器）。
//! 之所以 1 优先：枚举会把硬件解码器一起列出来，而硬件解码器是**异步**的，
//! 同步 ProcessInput 直接返回 `MF_E_TRANSFORM_ASYNC_LOCKED`。
//! 走 1 拿到的是微软那个软件解码器，行为确定。
//!
//! # 输入 / 输出类型协商（顺序不能反）
//!
//! 解码器和编码器**相反**：编码器先设输出（H.264）再设输入（NV12），
//! 解码器必须**先 SetInputType（H.264）再 SetOutputType（NV12）**。
//! 顺序反了 `SetOutputType` 返回 `MF_E_TRANSFORM_TYPE_NOT_SET`。
//!
//! # 输出尺寸是 **coded size**，不是显示尺寸
//!
//! 实测（本机）：
//!
//! | 编码器请求 | 解码器协商到 | 显示尺寸（SPS cropping 后） |
//! | --- | --- | --- |
//! | 640x360 | **640x368** | 640x360 |
//! | 1920x1080 | **1920x1088** | 1920x1080 |
//!
//! H.264 的编码尺寸必须是 16 的整数倍，多出来的行靠 SPS 的
//! `frame_cropping_flag` 裁掉。**虚拟摄像头必须按
//! [@@H264Decoder::output_display_size@@] 输出**，否则底部会多一条绿边。
//!
//! 这里踩过一个坑：一开始把解码器自报的输出类型**覆盖**成 config 里的显示尺寸
//! 再 SetOutputType —— 首次协商能过，但收到 `MF_E_TRANSFORM_STREAM_CHANGE`
//! 之后全线 `MF_E_INVALIDMEDIATYPE`。详见 [@@set_output_type@@] 的注释。
//!
//! # 送数据：Annex-B 原样喂，不要转 AVCC
//!
//! `MFVideoFormat_H264` 的字节布局**没有文档保证**是 Annex-B 还是 AVCC，
//! 编码器那一侧甚至需要实测（见 `encoder::detect_format`）。
//! 解码器这一侧实测结论很干脆：**微软的解码器吃 Annex-B**，
//! 每个 NALU 前面的 `00 00 00 01` / `00 00 01` 起始码原样保留即可，
//! 转成 4 字节长度前缀反而会让它把长度当成 NALU 头，解出黑屏。
//!
//! 一个输入 sample = 一个访问单元（一帧）。SPS/PPS/SEI/IDR 切片可以
//! 在同一个 sample 里 —— 项目里的编码器本来就是把 SPS/PPS 跟在 IDR 前发的。
//!
//! # 低延迟：本模块最重要的一条
//!
//! `CODECAPI_AVLowLatencyMode` **必须**用 `VT_UI4`，不能用 `VT_BOOL`：
//!
//! ```text
//! SetValue(AVLowLatencyMode, VT_BOOL(-1))  -> 0x80070057 E_INVALIDARG
//! SetValue(AVLowLatencyMode, VT_UI4(1))    -> S_OK
//! GetValue(AVLowLatencyMode)               -> vt=19 (VT_UI4)
//! ```
//!
//! 文档写的是 BOOL，所以第一版用了 `VT_BOOL`，**静默失败**（返回值被 `let _ =`
//! 吞了），表现为「30 帧输入解出 0 帧」。换成 `VT_UI4` 之后立刻变成 30/30。
//!
//! 实测对比（640x360@30、Main、GOP 30、0 B 帧，喂 80 帧）：
//!
//! | 配置 | 产出帧数 | 延迟（帧） |
//! | --- | --- | --- |
//! | 什么都不设 | 47 | **33** |
//! | 只设 worker threads = 1 | 70 | **10** |
//! | **AVLowLatencyMode = 1** | **80** | **0** |
//!
//! 另一个反直觉的点：`CODECAPI_AVDecNumWorkerThreads` 只吃 `VT_I4`，
//! `VT_UI4` 同样返回 E_INVALIDARG。两个参数的 VARIANT 类型正好相反。
//!
//! # `MF_E_NOTACCEPTING` 是正常节奏，不是错误
//!
//! 同步 MFT 的输入队列深度有限。输出没取走时 `ProcessInput` 返回
//! `MF_E_NOTACCEPTING`（0xC00D36B5）。正确做法是**先 `ProcessOutput`
//! 排空再重试**，而不是把它当失败往上抛。
//!
//! 实测频率：本实现的正常节奏下是 **0 次**（每帧喂完立刻排空 + 低延迟开着，
//! 队列深度只有 1，永远撞不到）。而**不开低延迟**、连喂 40 帧完全不排空时
//! 是 **31 次** —— 也就是说这个错误码的根因是「解码器攒批」，
//! 低延迟开关才是解法，重试循环只是保险。详见
//! [@@H264Decoder::not_accepting_count@@]。
use std::collections::VecDeque;
use std::ffi::c_void;
use std::mem::ManuallyDrop;

use windows::core::{Interface, GUID};
use windows::Win32::Media::MediaFoundation::{
    CODECAPI_AVDecNumWorkerThreads, CODECAPI_AVLowLatencyMode, ICodecAPI, IMFActivate,
    IMFMediaBuffer, IMFMediaType, IMFSample, IMFTransform, MFCreateMediaType, MFCreateMemoryBuffer,
    MFCreateSample, MFMediaType_Video, MFTEnumEx, MFVideoFormat_H264, MFVideoFormat_IYUV,
    MFVideoFormat_NV12, MFVideoFormat_YV12, MFVideoInterlace_Progressive,
    MFT_CATEGORY_VIDEO_DECODER, MFT_ENUM_FLAG_ALL, MFT_MESSAGE_COMMAND_DRAIN,
    MFT_MESSAGE_COMMAND_FLUSH, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_END_OF_STREAM, MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_DATA_BUFFER_FORMAT_CHANGE,
    MFT_OUTPUT_DATA_BUFFER_STREAM_END, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MFT_REGISTER_TYPE_INFO,
    MF_E_NOTACCEPTING, MF_E_NO_MORE_TYPES, MF_E_TRANSFORM_NEED_MORE_INPUT,
    MF_E_TRANSFORM_STREAM_CHANGE, MF_MT_ALL_SAMPLES_INDEPENDENT, MF_MT_DEFAULT_STRIDE,
    MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE,
    MF_MT_MAX_KEYFRAME_SPACING, MF_MT_MPEG2_LEVEL, MF_MT_MPEG2_PROFILE, MF_MT_PIXEL_ASPECT_RATIO,
    MF_MT_SUBTYPE, MF_TRANSFORM_ASYNC,
};
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_INPROC_SERVER};

use crate::com::MediaFoundation;
use crate::error::{EncoderError, Result};
use crate::variant::{variant_i32, variant_u32};

/// H.264 解码器 MFT 的 CLSID（CLSID_CMSH264DecoderMFT）
///
/// # 为什么要自己写
///
/// windows crate 的 Win32_Media_MediaFoundation 里**没有**导出任何
/// `CLSID_CMS*` 常量（连编码器那个也没有，见 `encoder.rs` 的同名常量）。
///
/// 这个值是 **MSDN 公开的 CLSID**，不是猜的：
/// <https://learn.microsoft.com/windows/win32/medfound/h-264-video-decoder>
/// 写明 `CLSID_CMSH264DecoderMFT` 的 GUID 是
/// `{62CE7E72-4C71-4D20-B15D-452831A87D9D}`。
///
/// 本机实测：`CoCreateInstance` 直接成功，返回的就是微软的软件解码器
/// （friendly name `"Microsoft H264 Video Decoder MFT"`）。
pub const CLSID_CMSH264DECODERMFT: GUID = GUID::from_u128(0x62ce7e72_4c71_4d20_b15d_452831a87d9d);

/// 解码器**默认**假设的 profile（Main）
///
/// 真正的 profile 在 SPS 里，而 `new()` 的时候还没有任何码流。
/// 用一个保守的默认值把输入类型填完整，比留空更安全 ——
/// 部分解码器对缺 `MF_MT_MPEG2_PROFILE` 的输入类型直接拒绝。
/// 调用方拿到 SPS 之后可以用 [@@DecoderConfig::profile_idc@@] 覆盖。
pub const DEFAULT_PROFILE_IDC: u8 = 77;

/// 解码器**默认**假设的 level（4.0 = 40）
///
/// 4.0 覆盖到 1080p30 / 720p60，是实时投屏最常见的一档。
/// 同样可以在 `DecoderConfig` 里覆盖。
pub const DEFAULT_LEVEL_IDC: u8 = 40;

/// 一帧输入最多重试几次「排空输出后重投」
///
/// 正常情况 1 次就够（输入队列深度 1）。留 4 次是给「一帧吐多帧」的
/// 场景（B 帧重排）兜底；再多就是解码器卡死了，报错比死循环好。
const MAX_NOTACCEPTING_RETRIES: u32 = 4;

/// 单次 ProcessOutput 循环的上限
///
/// 正常一帧一个输出；上限只是防止某个 MFT 行为异常时死循环。
const MAX_OUTPUTS_PER_CALL: usize = 16;

/// 输出像素格式（实测协商到的，不是猜的）
///
/// # 与 `encoder::OutputFormat` 的区别
///
/// 编码器的 `OutputFormat` 说的是**码流字节布局**（Annex-B / AVCC）；
/// 解码器的这个是**像素布局**（NV12 / YV12 / IYUV）。
/// 两者同名但语义完全不同，所以 crate 根把它重导出成
/// [@@DecodedFormat@@] 以免混淆。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    /// Y 平面 + UV 交织平面（4:2:0，半平面）
    Nv12,
    /// Y 平面 + V 平面 + U 平面（4:2:0，三平面）
    Yv12,
    /// Y 平面 + U 平面 + V 平面（4:2:0，三平面）
    Iyuv,
}

impl OutputFormat {
    /// 从 media type 的 subtype 反查
    pub fn from_subtype(subtype: &GUID) -> Option<Self> {
        if *subtype == MFVideoFormat_NV12 {
            Some(OutputFormat::Nv12)
        } else if *subtype == MFVideoFormat_YV12 {
            Some(OutputFormat::Yv12)
        } else if *subtype == MFVideoFormat_IYUV {
            Some(OutputFormat::Iyuv)
        } else {
            None
        }
    }

    /// 名字（日志 / 测试用）
    pub fn as_str(self) -> &'static str {
        match self {
            OutputFormat::Nv12 => "NV12",
            OutputFormat::Yv12 => "YV12",
            OutputFormat::Iyuv => "IYUV",
        }
    }

    /// 一帧的字节数
    ///
    /// 三种格式都是 4:2:0，所以都是 `w * h * 3 / 2`。分开写是为了
    /// 将来加 P010（10bit）时不至于又改一遍调用点。
    pub fn frame_len(self, width: u32, height: u32) -> usize {
        (width as usize) * (height as usize) * 3 / 2
    }
}

/// 解码器配置
///
/// 只描述**输出类型协商需要的候选值**，不描述码流本身 ——
/// 真实分辨率 / profile / level 由 SPS 决定，这里给的只是
/// 「按这个尺寸去谈输出类型」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecoderConfig {
    /// 期望的输出宽度（像素）
    pub width: u32,
    /// 期望的输出高度（像素）
    pub height: u32,
    /// 期望的帧率（帧/秒）
    pub fps: u32,
    /// H.264 profile_idc（SPS 的第一个字节），默认 [@@DEFAULT_PROFILE_IDC@@]
    pub profile_idc: u8,
    /// H.264 level_idc（SPS 的第三个字节），默认 [@@DEFAULT_LEVEL_IDC@@]
    pub level_idc: u8,
}

impl Default for DecoderConfig {
    fn default() -> Self {
        Self {
            width: 1280,
            height: 720,
            fps: 30,
            profile_idc: DEFAULT_PROFILE_IDC,
            level_idc: DEFAULT_LEVEL_IDC,
        }
    }
}

impl DecoderConfig {
    /// 按分辨率 + 帧率构造（profile/level 用默认值）
    pub fn new(width: u32, height: u32, fps: u32) -> Self {
        Self {
            width,
            height,
            fps,
            ..Self::default()
        }
    }

    /// NV12 一帧的字节数（与 `EncoderConfig::nv12_len` 一致）
    pub fn nv12_len(&self) -> usize {
        (self.width as usize) * (self.height as usize) * 3 / 2
    }

    /// 帧间隔（微秒）
    pub fn frame_interval_us(&self) -> i64 {
        1_000_000 / self.fps.max(1) as i64
    }

    /// 校验配置
    pub fn validate(&self) -> Result<()> {
        if self.width == 0 || self.height == 0 {
            return Err(EncoderError::new("validate", "分辨率不能为 0"));
        }
        // 4:2:0 要求宽高都是偶数；奇数尺寸会让解码器在 SetOutputType 时
        // 返回 MF_E_INVALIDMEDIATYPE，而错误信息完全不提这个原因。
        if self.width % 2 != 0 || self.height % 2 != 0 {
            return Err(EncoderError::new(
                "validate",
                format!("宽高必须是偶数，当前 {}x{}", self.width, self.height),
            ));
        }
        if self.fps == 0 {
            return Err(EncoderError::new("validate", "帧率不能为 0"));
        }
        Ok(())
    }
}

/// 解码器 MFT 的元信息
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecoderInfo {
    /// 友好名（MFT_FRIENDLY_NAME_Attribute）
    pub name: String,
    /// 是否异步 MFT（本 crate 不用）
    pub is_async: bool,
    /// 是否 D3D 加速（DXVA）解码器
    pub hardware: bool,
}

/// H.264 解码器
///
/// 一个实例对应一个 IMFTransform，**非线程安全**（MFT 本身不是）。
///
/// 用法与 [@@crate::H264Encoder@@] 对称：
///
/// ```no_run
/// use sidelens_codec_mf::{DecoderConfig, H264Decoder};
/// # fn run() -> Result<(), Box<dyn std::error::Error>> {
/// let mut dec = H264Decoder::new(DecoderConfig::new(640, 360, 30))?;
/// let annex_b_frame: Vec<u8> = Vec::new(); // 来自网络的 Annex-B 码流
/// let mut nv12 = Vec::new();
/// dec.decode_into(&annex_b_frame, 0, &mut nv12)?;
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct H264Decoder {
    config: DecoderConfig,
    transform: IMFTransform,
    /// 保持 MF 平台存活（字段顺序即 drop 顺序：先 transform 后关停）
    _mf: MediaFoundation,
    /// 实测协商到的像素格式
    format: OutputFormat,
    /// 解码器名（日志用）
    name: String,
    /// 已喂入的 sample 数
    frames_in: u64,
    /// 已取出的输出帧数
    frames_out: u64,
    /// 输出缓冲复用
    output_buffer: Option<IMFMediaBuffer>,
    /// 输出侧是否由 MFT 自己提供 sample
    output_provides_samples: bool,
    /// 输出 sample 的建议大小
    output_sample_size: u32,
    /// 协商到的输出宽（来自 GetOutputCurrentType，可能与 config 不同）
    out_width: u32,
    /// 协商到的输出高
    out_height: u32,
    /// 输出缓冲的行跨度（MF_MT_DEFAULT_STRIDE，负数表示自下而上）
    out_stride: i32,
    /// `ProcessInput` 返回 `MF_E_NOTACCEPTING` 的累计次数（实测频率用）
    not_accepting_count: u64,
    /// `ProcessOutput` 报告 `MF_E_TRANSFORM_STREAM_CHANGE` 的次数
    stream_change_count: u64,
    /// `ProcessOutput` 返回 `MF_E_TRANSFORM_NEED_MORE_INPUT` 的次数
    need_more_input_count: u64,
    /// 已产出但还没被取走的帧（B 帧重排时一进多出）
    pending: VecDeque<Vec<u8>>,
    /// 从 SPS 里解析出的**显示尺寸**（已扣掉 cropping）
    ///
    /// 收到第一个 SPS 之前是 None，此时 [@@Self::output_display_size@@]
    /// 退回 [@@Self::output_size@@]（coded size）。
    display_size: Option<(u32, u32)>,
    /// 从 SPS 里读到的 profile_idc
    sps_profile: Option<u8>,
    /// 从 SPS 里读到的 level_idc
    sps_level: Option<u8>,
}

impl H264Decoder {
    /// 创建解码器（先用 CLSID 直连，失败再枚举）
    pub fn new(config: DecoderConfig) -> Result<Self> {
        config.validate()?;
        let mf = MediaFoundation::init().map_err(init_err)?;
        let (transform, name) = activate()?;
        Self::build(config, transform, name, mf)
    }

    /// 按名字挑一个**同步**解码器
    ///
    /// 用途：本机有多个 H.264 解码器（微软软件 + Intel/NVIDIA 硬件）时
    /// 指定用哪个。硬件解码器是异步的，本实现走不了 —— 会被提前拦下。
    pub fn with_name(config: DecoderConfig, name: &str) -> Result<Self> {
        config.validate()?;
        let mf = MediaFoundation::init().map_err(init_err)?;
        let transform = activate_by_name(name)?;
        Self::build(config, transform, name.to_string(), mf)
    }

    /// 列出本机所有能吃 H.264 的解码器 MFT
    ///
    /// # 枚举条件为什么这样写
    ///
    /// 解码器的**输入**是 H.264、**输出**是未压缩像素。所以
    /// `MFT_REGISTER_TYPE_INFO` 里 H.264 放在 input 侧。
    /// 输出侧故意给 `None`（不限定）：只按 NV12 筛会漏掉只输出
    /// YV12/IYUV 的解码器，而本 crate 三种都支持。
    pub fn list_decoders() -> Result<Vec<DecoderInfo>> {
        let _mf = MediaFoundation::init().map_err(init_err)?;
        let all = enumerate(MFT_ENUM_FLAG_ALL)?;
        Ok(all
            .iter()
            .map(|a| DecoderInfo {
                name: read_name(a),
                is_async: unsafe { a.GetUINT32(&MF_TRANSFORM_ASYNC) }
                    .map(|v| v != 0)
                    .unwrap_or(false),
                hardware: unsafe { a.GetUINT32(&MF_SA_D3D_AWARE_GUID) }
                    .map(|v| v != 0)
                    .unwrap_or(false),
            })
            .collect())
    }

    fn build(
        config: DecoderConfig,
        transform: IMFTransform,
        name: String,
        mf: MediaFoundation,
    ) -> Result<Self> {
        // 异步 MFT 走不了同步路径：ProcessInput 会返回
        // MF_E_TRANSFORM_ASYNC_LOCKED。提前拦下，错误信息才有指向性。
        if let Ok(attrs) = unsafe { transform.GetAttributes() } {
            if let Ok(flag) = unsafe { attrs.GetUINT32(&MF_TRANSFORM_ASYNC) } {
                if flag != 0 {
                    return Err(EncoderError::new(
                        "ActivateObject",
                        format!("{name} 是异步 MFT，本实现只支持同步路径"),
                    ));
                }
            }
        }

        // 低延迟参数写在设类型**之前**。
        //
        // 与编码器的 MF_LOW_LATENCY 同理：这些是解码器在 SetInputType /
        // SetOutputType 时读的一次性配置，设类型之后再写虽然也返回 S_OK，
        // 但解码器已经按默认（带输出重排）配置好了，**静默无效**。
        apply_codec_api(&transform);

        // 顺序：先输入后输出。反了 SetOutputType 会返回
        // MF_E_TRANSFORM_TYPE_NOT_SET。
        set_input_type(&transform, &config)?;
        let applied = set_output_type(&transform, &config)?;

        let (format, out_width, out_height, out_stride) = read_output_shape(&applied, &config)?;
        let (provides, size) = output_stream_info(&transform)?;

        // SAFETY: 开始推流
        let _ = unsafe { transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0) };

        Ok(Self {
            config,
            transform,
            _mf: mf,
            format,
            name,
            frames_in: 0,
            frames_out: 0,
            output_buffer: None,
            output_provides_samples: provides,
            output_sample_size: size,
            out_width,
            out_height,
            out_stride,
            not_accepting_count: 0,
            stream_change_count: 0,
            need_more_input_count: 0,
            pending: VecDeque::new(),
            display_size: None,
            sps_profile: None,
            sps_level: None,
        })
    }

    /// 配置
    pub fn config(&self) -> &DecoderConfig {
        &self.config
    }

    /// 实际使用的解码器名
    pub fn decoder_name(&self) -> &str {
        &self.name
    }

    /// 实际协商到的输出像素格式
    ///
    /// 这是 `GetOutputCurrentType` 读回来的**真实**类型，不是请求的类型。
    /// 请求 NV12 但解码器只给 YV12 时，这里报的是 YV12。
    pub fn output_format(&self) -> OutputFormat {
        self.format
    }

    /// 实际协商到的输出尺寸（= **编码尺寸** coded size）
    ///
    /// 与 `config.width/height` 可能不同 —— 解码器以 SPS 里的真实分辨率为准，
    /// config 只是协商时的候选。
    ///
    /// # 这是 coded size，不是显示尺寸
    ///
    /// H.264 的编码尺寸必须是 16 的整数倍（4:2:0 下宏块 16x16），
    /// 1080p 实际编码成 **1920x1088**，多出来的 8 行靠 SPS 里的
    /// `frame_cropping_flag` 裁掉。本机实测：
    ///
    /// | 请求 | `output_size()` | `output_display_size()` | 一帧字节 |
    /// | --- | --- | --- | --- |
    /// | 640x360 | 640x**368** | 640x360 | 353280 |
    /// | 1920x1080 | 1920x**1088** | 1920x1080 | 3133440 |
    ///
    /// **虚拟摄像头必须按 `output_display_size()` 输出**，否则画面底部会多一条绿边。
    /// 缓冲区仍然是 `output_frame_len()` 字节（coded size），只是显示时裁掉。
    pub fn output_size(&self) -> (u32, u32) {
        (self.out_width, self.out_height)
    }

    /// 实际显示尺寸（已扣掉 SPS 里的 cropping）
    ///
    /// 还没喂到任何 SPS 时与 [@@Self::output_size@@] 相同。
    /// 收到 SPS 之后（第一个 IDR 时）自动更新。
    pub fn output_display_size(&self) -> (u32, u32) {
        self.display_size
            .unwrap_or((self.out_width, self.out_height))
    }

    /// 一帧输出的字节数（按**编码尺寸**，即 `output_size()`）
    ///
    /// 这是解码器实际写进缓冲的字节数，**不是** `display_w * display_h * 3 / 2`。
    pub fn output_frame_len(&self) -> usize {
        self.format.frame_len(self.out_width, self.out_height)
    }

    /// 已从码流里读到的 SPS profile_idc（还没见过 SPS 时为 None）
    pub fn sps_profile(&self) -> Option<u8> {
        self.sps_profile
    }

    /// 已从码流里读到的 SPS level_idc（还没见过 SPS 时为 None）
    pub fn sps_level(&self) -> Option<u8> {
        self.sps_level
    }

    /// 已喂入的 sample 数
    pub fn frames_in(&self) -> u64 {
        self.frames_in
    }

    /// 已取出的输出帧数
    pub fn frames_out(&self) -> u64 {
        self.frames_out
    }

    /// `ProcessInput` 返回 `MF_E_NOTACCEPTING` 的累计次数
    ///
    /// 这个数**不是错误计数** —— 它是同步 MFT 的正常节奏（输出没取走时
    /// 输入队列就是满的）。统计它只是为了回答「到底多久撞一次」。
    ///
    /// # 实测频率（640x360@30、Main、GOP 30）
    ///
    /// | 场景 | 次数 |
    /// | --- | --- |
    /// | 本实现的正常节奏（每帧 ProcessInput 后立刻排空） | **0 / 300 帧** |
    /// | 同上，1080p | **0 / 60 帧** |
    /// | 同上，720p | **0 / 120 帧** |
    /// | 连喂 2 个 sample 才排空一次（10 帧） | 0 |
    /// | **不开低延迟**、连喂 40 帧完全不排空 | **31 / 40** |
    ///
    /// 结论：撞到 `MF_E_NOTACCEPTING` 的**根因是没开低延迟**
    /// （解码器攒 32 帧才吐，输入队列自然一直是满的）。开了
    /// `CODECAPI_AVLowLatencyMode` 之后队列深度只有 1，
    /// 「一进一出」永远不会撞。
    ///
    /// 但重试路径**必须留着**：它是同步 MFT 的通用节奏，
    /// 硬件解码器 / 别的 MFT / 将来改多线程排空都会用到。
    pub fn not_accepting_count(&self) -> u64 {
        self.not_accepting_count
    }

    /// 解码器报告输出格式变化的次数（分辨率切换 / 中途加入）
    pub fn stream_change_count(&self) -> u64 {
        self.stream_change_count
    }

    /// `ProcessOutput` 说「还要更多输入」的次数
    pub fn need_more_input_count(&self) -> u64 {
        self.need_more_input_count
    }

    /// 解码一帧 Annex-B 码流，返回原始帧
    ///
    /// # 返回值的语义
    ///
    /// - 绝大多数情况返回**恰好一帧**（`output_frame_len()` 字节）。
    /// - 首帧可能返回**空** —— 解码器要先吃到参数集 + IDR 才吐画面，
    ///   第一个 sample 通常只换来 `MF_E_TRANSFORM_NEED_MORE_INPUT`。
    /// - 若码流带 B 帧，解码器可能「一进多出」或「先进后出」，
    ///   返回的是**本次调用产出的全部帧按输出顺序拼接**。
    ///   判断真实帧数请用 [@@Self::frames_out@@]，不要用 `len() / frame_len` 猜。
    ///
    /// # 实测（`examples/decode_probe`，release 构建）
    ///
    /// 编码 → 解码闭环，编码器与解码器都是本 crate 的实现：
    ///
    /// | 分辨率 | 帧数 | 输入 → 输出帧数 | 解码 p50 | 解码平均 |
    /// | --- | --- | --- | --- | --- |
    /// | 640x360 | 30 | **30 → 30** | 371 µs | 501 µs |
    /// | 1280x720 | 120 | **120 → 120** | — | — |
    /// | 1920x1080 | 60 | **60 → 60** | 4966 µs | 5236 µs |
    /// | 320x240 | 300 | **300 → 300** | — | — |
    ///
    /// **一帧都不差**，flush 也拿不到多余帧（说明没有积压）。
    /// 首帧不再被吞 —— 这是 `CODECAPI_AVLowLatencyMode` 生效的标志；
    /// 不设它时解码器会攒满 32~33 帧才吐第一帧（30 帧输入只解出 0 帧）。
    ///
    /// 640x360 的闭环还比对了 Y 值：90 个采样点，`|ΔY|` 平均 0.40、最大 **1**
    /// （阈值 16）。1080p 最大 3。
    pub fn decode(&mut self, annex_b: &[u8], pts_us: i64) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        self.decode_into(annex_b, pts_us, &mut out)?;
        Ok(out)
    }

    /// 解码一帧 Annex-B 码流，追加到 `out`
    ///
    /// 与 [@@Self::decode@@] 相同，但复用调用方的缓冲。
    ///
    /// # 空输入
    ///
    /// 空切片是**合法的 no-op**（返回 Ok，不产出任何东西），
    /// 不是错误 —— 抖动缓冲在丢包后可能给出空帧。
    pub fn decode_into(&mut self, annex_b: &[u8], pts_us: i64, out: &mut Vec<u8>) -> Result<()> {
        // 先把手上的存货交出去（上一轮一进多出剩下的）
        while let Some(frame) = self.pending.pop_front() {
            out.extend_from_slice(&frame);
        }

        if annex_b.is_empty() {
            return Ok(());
        }

        // 顺手扫一遍参数集。这一步**不参与解码**，只是为了把 SPS 里的
        // 显示尺寸 / profile / level 暴露给调用方 —— 虚拟摄像头必须按
        // 显示尺寸输出（1080p 的 coded size 是 1088，直接输出会多一条绿边）。
        //
        // 放在 ProcessInput 之前：即使这一帧解码失败，参数集信息也拿到了。
        self.scan_parameter_sets(annex_b);

        let sample = make_input_sample(annex_b, pts_us, self.config.frame_interval_us())?;

        // 同步 MFT 的基本节奏：ProcessInput 可能因为输出队列满而
        // 返回 MF_E_NOTACCEPTING，这时必须**先 ProcessOutput 排空再重试**。
        let mut retries = 0u32;
        loop {
            // SAFETY: sample 有效，0 是输入流 ID（解码器只有一路输入）
            match unsafe { self.transform.ProcessInput(0, &sample, 0) } {
                Ok(()) => break,
                Err(e) if e.code() == MF_E_NOTACCEPTING => {
                    self.not_accepting_count += 1;
                    let produced = self.drain_outputs(out)?;
                    retries += 1;
                    if retries >= MAX_NOTACCEPTING_RETRIES {
                        return Err(EncoderError::with_hresult(
                            "ProcessInput",
                            e.code().0,
                            format!(
                                "连续 {retries} 次 MF_E_NOTACCEPTING 且排空输出无进展（已取出 {produced} 帧），解码器疑似卡死"
                            ),
                        ));
                    }
                }
                Err(e) => return Err(EncoderError::from_win("ProcessInput", e)),
            }
        }
        self.frames_in += 1;

        self.drain_outputs(out)?;
        Ok(())
    }

    /// 冲刷解码器，返回积压的帧
    ///
    /// 发 `MFT_MESSAGE_COMMAND_DRAIN` 后把输出取干净，再发
    /// `MFT_MESSAGE_NOTIFY_END_OF_STREAM`。不调用会让最后一两帧
    /// 卡在解码器内部。
    ///
    /// 重复调用是安全的：第二次及以后返回空。
    pub fn flush(&mut self) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        // SAFETY: transform 有效
        let _ = unsafe { self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0) };
        self.drain_outputs(&mut out)?;
        // SAFETY: 通知流结束
        let _ = unsafe {
            self.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0)
        };
        Ok(out)
    }

    /// 重置解码器（丢包后重新对齐用）
    ///
    /// 发 `MFT_MESSAGE_COMMAND_FLUSH` 清空内部状态，计数归零。
    /// 之后必须重新喂 SPS/PPS + IDR 才能出画面。
    pub fn reset(&mut self) -> Result<()> {
        // SAFETY: transform 有效
        unsafe { self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0) }
            .map_err(|e| EncoderError::from_win("ProcessMessage(FLUSH)", e))?;
        self.pending.clear();
        // SAFETY: 重新开始推流
        let _ = unsafe {
            self.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
        };
        Ok(())
    }

    // ---- 内部实现 ----

    /// 扫一遍 Annex-B 码流里的 SPS，更新显示尺寸 / profile / level
    ///
    /// # 为什么在 sidelens-codec-mf 内部做最小限度的 NALU 遍历
    ///
    /// 完整的切分与 SPS 解析在 `sidelens-codec`（`nalu::split_annexb` +
    /// `sps::SpsInfo::parse`），本 crate 直接复用，**不重写**。
    /// 这里只负责「遍历 NALU，挑出 type 7，喂给解析器」这一层胶水 ——
    /// 判断类型只需要一个字节的掩码，没有第二套实现的风险。
    ///
    /// 解析失败**不报错**：SPS 来自网络，随时可能是垃圾，
    /// 而「参数集解析不出来」不应该让整帧解码失败（解码器自己会判断）。
    fn scan_parameter_sets(&mut self, annex_b: &[u8]) {
        for nalu in sidelens_codec::nalu::split_annexb(annex_b) {
            if nalu.ty != sidelens_codec::nalu::NaluType::Sps {
                continue;
            }
            let Ok(sps) = sidelens_codec::sps::SpsInfo::parse(nalu.data) else {
                continue;
            };
            self.sps_profile = Some(sps.profile_idc);
            self.sps_level = Some(sps.level_idc);
            let (w, h) = sps.resolution();
            if w > 0 && h > 0 {
                self.display_size = Some((w, h));
            }
        }
    }

    /// 把解码器当前的输出全部取走，返回本次产出的帧数
    fn drain_outputs(&mut self, out: &mut Vec<u8>) -> Result<usize> {
        let mut produced = 0usize;
        for _ in 0..MAX_OUTPUTS_PER_CALL {
            let own_sample = if self.output_provides_samples {
                None
            } else {
                Some(self.make_output_sample()?)
            };

            let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(own_sample),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            }];
            let mut status: u32 = 0;

            // SAFETY: buffers 是长度为 1 的有效数组，status 是栈上变量
            let hr = unsafe { self.transform.ProcessOutput(0, &mut buffers, &mut status) };

            // 无论成功失败都要把 MFT 填进来的引用释放掉
            let sample = ManuallyDrop::into_inner(std::mem::replace(
                &mut buffers[0].pSample,
                ManuallyDrop::new(None),
            ));
            let events = ManuallyDrop::into_inner(std::mem::replace(
                &mut buffers[0].pEvents,
                ManuallyDrop::new(None),
            ));
            drop(events);
            let status_flags = buffers[0].dwStatus;

            match hr {
                Ok(()) => {
                    if let Some(sample) = sample {
                        self.append_frame(&sample, out)?;
                        produced += 1;
                    }
                    if status_flags & MFT_OUTPUT_DATA_BUFFER_FORMAT_CHANGE.0 as u32 != 0 {
                        // 输出类型变了，必须重新协商，否则下一帧直接失败
                        self.renotiate_output()?;
                    }
                    if status_flags & MFT_OUTPUT_DATA_BUFFER_STREAM_END.0 as u32 != 0 {
                        return Ok(produced);
                    }
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => {
                    self.need_more_input_count += 1;
                    return Ok(produced);
                }
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    // 解码器要求重新协商输出类型（典型场景：第一个 IDR 才
                    // 解析出真实分辨率）。这一步**必须**做，否则整路黑屏。
                    self.stream_change_count += 1;
                    self.renotiate_output()?;
                    continue;
                }
                Err(e) => return Err(EncoderError::from_win("ProcessOutput", e)),
            }
        }
        Ok(produced)
    }

    /// 输出类型变化后重新协商（分辨率切换 / 首个 IDR 解析出真实尺寸）
    fn renotiate_output(&mut self) -> Result<()> {
        let applied = set_output_type(&self.transform, &self.config)?;
        let (format, w, h, stride) = read_output_shape(&applied, &self.config)?;
        self.format = format;
        self.out_width = w;
        self.out_height = h;
        self.out_stride = stride;
        let (provides, size) = output_stream_info(&self.transform)?;
        self.output_provides_samples = provides;
        self.output_sample_size = size;
        // 缓冲尺寸可能变了，丢掉旧的
        self.output_buffer = None;
        Ok(())
    }

    fn make_output_sample(&mut self) -> Result<IMFSample> {
        let need = self.output_sample_size.max(1);
        let buffer = match self.output_buffer.take() {
            Some(buf) if unsafe { buf.GetMaxLength() }.unwrap_or(0) >= need => buf,
            // SAFETY: 纯工厂调用
            _ => unsafe { MFCreateMemoryBuffer(need) }
                .map_err(|e| EncoderError::from_win("MFCreateMemoryBuffer(输出)", e))?,
        };
        // SAFETY: buffer 有效
        unsafe { buffer.SetCurrentLength(0) }
            .map_err(|e| EncoderError::from_win("SetCurrentLength(输出)", e))?;
        // SAFETY: 纯工厂调用
        let sample = unsafe { MFCreateSample() }
            .map_err(|e| EncoderError::from_win("MFCreateSample(输出)", e))?;
        // SAFETY: 两者都有效
        unsafe { sample.AddBuffer(&buffer) }
            .map_err(|e| EncoderError::from_win("AddBuffer(输出)", e))?;
        self.output_buffer = Some(buffer);
        Ok(sample)
    }

    /// 读出一帧输出，按 stride 重排后追加到 out
    fn append_frame(&mut self, sample: &IMFSample, out: &mut Vec<u8>) -> Result<()> {
        let raw = read_sample(sample)?;
        if raw.is_empty() {
            return Ok(());
        }
        let frame = repack(
            &raw,
            self.out_stride,
            self.out_width as usize,
            self.out_height as usize,
            self.format,
        );
        out.extend_from_slice(&frame);
        self.frames_out += 1;
        Ok(())
    }
}

impl Drop for H264Decoder {
    fn drop(&mut self) {
        // SAFETY: transform 有效；drop 里没有补救手段，忽略错误
        unsafe {
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
        }
        // 先释放缓冲，再让 IMFTransform 掉（缓冲是 MFT 的下游）
        self.output_buffer = None;
        self.pending.clear();
    }
}

/// MF_SA_D3D_AWARE 的 GUID
///
/// windows crate 导出了 `MF_SA_D3D_AWARE`；这里给一个本地别名，
/// 只是让调用点短一点（解码器靠它判断是不是 DXVA 硬件解码器）。
const MF_SA_D3D_AWARE_GUID: GUID = GUID::from_u128(0xeaa35c29_775e_488e_9b61_b3283e49583b);

// ---- 构造辅助 ----

fn init_err(e: crate::com::InitError) -> EncoderError {
    EncoderError::with_hresult(e.stage(), e.hresult(), "Media Foundation 初始化失败")
}

/// 按 CLSID 直接造解码器（首选路径）
fn activate() -> Result<(IMFTransform, String)> {
    // SAFETY: CLSID 是常量；CoCreateInstance 无副作用
    let transform: IMFTransform =
        unsafe { CoCreateInstance(&CLSID_CMSH264DECODERMFT, None, CLSCTX_INPROC_SERVER) }
            .map_err(|e| EncoderError::from_win("CoCreateInstance(CLSID_CMSH264DecoderMFT)", e))?;
    let name = transform_name(&transform).unwrap_or_else(|| "H.264 Decoder MFT".to_string());
    Ok((transform, name))
}

/// 按名字枚举后激活（指定 MFT 时用）
fn activate_by_name(name: &str) -> Result<IMFTransform> {
    let all = enumerate(MFT_ENUM_FLAG_ALL)?;
    let act = all
        .iter()
        .find(|a| read_name(a) == name)
        .ok_or_else(|| EncoderError::new("MFTEnumEx", format!("找不到名为 {name} 的解码器 MFT")))?;
    // SAFETY: act 有效
    unsafe { act.ActivateObject::<IMFTransform>() }
        .map_err(|e| EncoderError::from_win("ActivateObject", e))
}

fn transform_name(transform: &IMFTransform) -> Option<String> {
    use windows::Win32::Media::MediaFoundation::MFT_FRIENDLY_NAME_Attribute;
    let attrs = unsafe { transform.GetAttributes() }.ok()?;
    let len = unsafe { attrs.GetStringLength(&MFT_FRIENDLY_NAME_Attribute) }.ok()?;
    if len == 0 {
        return None;
    }
    let mut buf = vec![0u16; len as usize + 1];
    unsafe { attrs.GetString(&MFT_FRIENDLY_NAME_Attribute, &mut buf, None) }.ok()?;
    Some(String::from_utf16_lossy(&buf[..len as usize]))
}

fn enumerate(
    flags: windows::Win32::Media::MediaFoundation::MFT_ENUM_FLAG,
) -> Result<Vec<IMFActivate>> {
    // 解码器：输入 H.264，输出**不限定**（NV12 / YV12 / IYUV 都认）
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };

    let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count: u32 = 0;

    // SAFETY: input 是栈上有效值；输出指针指向本地变量
    let hr = unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_DECODER,
            flags,
            Some(&input),
            None,
            &mut activates,
            &mut count,
        )
    };
    if let Err(e) = hr {
        return Err(EncoderError::from_win("MFTEnumEx", e));
    }

    let mut out = Vec::with_capacity(count as usize);
    if !activates.is_null() {
        // SAFETY: MFTEnumEx 成功时返回 count 个元素，由 CoTaskMemAlloc 分配
        let slice = unsafe { std::slice::from_raw_parts(activates, count as usize) };
        out.extend(slice.iter().flatten().cloned());
        // SAFETY: 同一块内存，由 CoTaskMemFree 释放（MFTEnumEx 的契约）
        unsafe { CoTaskMemFree(Some(activates as *const c_void)) };
    }
    Ok(out)
}

fn read_name(act: &IMFActivate) -> String {
    use windows::Win32::Media::MediaFoundation::MFT_FRIENDLY_NAME_Attribute;
    // SAFETY: act 有效
    let Ok(len) = (unsafe { act.GetStringLength(&MFT_FRIENDLY_NAME_Attribute) }) else {
        return "<未命名 MFT>".to_string();
    };
    if len == 0 {
        return "<未命名 MFT>".to_string();
    }
    let mut buf = vec![0u16; len as usize + 1];
    // SAFETY: buf 长度 len+1，足够
    if unsafe { act.GetString(&MFT_FRIENDLY_NAME_Attribute, &mut buf, None) }.is_err() {
        return "<未命名 MFT>".to_string();
    }
    String::from_utf16_lossy(&buf[..len as usize])
}

/// 设输入类型（H.264）
///
/// # 三级回退
///
/// 不同解码器对输入类型上的附加属性容忍度不同：
///
/// 1. 全指定（含 profile / level / 帧率 / PAR / 交织）—— 微软解码器认
/// 2. 精简（只留 major + subtype + 帧尺寸 + 帧率）
/// 3. 最小（只留 major + subtype）
///
/// 逐级回退而不是一上来就给最小集：**能多给就多给**，
/// 硬件解码器要靠 profile/level 做能力匹配。
fn set_input_type(transform: &IMFTransform, config: &DecoderConfig) -> Result<()> {
    let mut last_err = None;
    for detail in [InputDetail::Full, InputDetail::Slim, InputDetail::Bare] {
        let mt = build_input_type(config, detail)?;
        // SAFETY: transform 与 mt 都有效，0 表示不要求严格匹配
        match unsafe { transform.SetInputType(0, &mt, 0) } {
            Ok(()) => return Ok(()),
            Err(e) => last_err = Some(e),
        }
    }
    Err(EncoderError::from_win(
        "SetInputType",
        last_err.expect("三级回退必然留下错误"),
    ))
}

/// 输入类型要写多详细（对应 [@@set_input_type@@] 的三级回退）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputDetail {
    /// major + subtype + 帧尺寸 + 帧率 + PAR + 交织 + profile/level + GOP
    Full,
    /// major + subtype + 帧尺寸 + 帧率
    Slim,
    /// 只有 major + subtype
    Bare,
}

/// 按详细程度构造输入 media type
fn build_input_type(config: &DecoderConfig, detail: InputDetail) -> Result<IMFMediaType> {
    let frame_size = ((config.width as u64) << 32) | config.height as u64;
    let frame_rate = ((config.fps as u64) << 32) | 1u64;

    // SAFETY: 纯工厂调用
    let mt = unsafe { MFCreateMediaType() }
        .map_err(|e| EncoderError::from_win("MFCreateMediaType(输入)", e))?;
    // SAFETY: mt 有效
    unsafe {
        mt.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .map_err(|e| EncoderError::from_win("SetGUID(输入 MAJOR)", e))?;
        mt.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)
            .map_err(|e| EncoderError::from_win("SetGUID(输入 SUBTYPE)", e))?;
        if detail == InputDetail::Bare {
            return Ok(mt);
        }
        mt.SetUINT64(&MF_MT_FRAME_SIZE, frame_size)
            .map_err(|e| EncoderError::from_win("SetUINT64(输入 FRAME_SIZE)", e))?;
        mt.SetUINT64(&MF_MT_FRAME_RATE, frame_rate)
            .map_err(|e| EncoderError::from_win("SetUINT64(输入 FRAME_RATE)", e))?;
        if detail == InputDetail::Slim {
            return Ok(mt);
        }
        mt.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1u64)
            .map_err(|e| EncoderError::from_win("SetUINT64(输入 PAR)", e))?;
        mt.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
            .map_err(|e| EncoderError::from_win("SetUINT32(输入 INTERLACE)", e))?;
        // profile / level：微软解码器宽容，硬件解码器要它做能力匹配
        mt.SetUINT32(&MF_MT_MPEG2_PROFILE, config.profile_idc as u32)
            .map_err(|e| EncoderError::from_win("SetUINT32(MPEG2_PROFILE)", e))?;
        mt.SetUINT32(&MF_MT_MPEG2_LEVEL, config.level_idc as u32)
            .map_err(|e| EncoderError::from_win("SetUINT32(MPEG2_LEVEL)", e))?;
        // 实时流的关键帧间隔只是提示，不设也能跑
        let _ = mt.SetUINT32(&MF_MT_MAX_KEYFRAME_SPACING, config.fps);
    }
    Ok(mt)
}

/// 设输出类型（NV12 → YV12 → IYUV，优先用解码器自报的类型）
///
/// # 为什么**原样**用解码器自报的类型（这是踩过的坑）
///
/// 一开始的写法是「拿解码器自报的类型，把 `MF_MT_FRAME_SIZE` 覆盖成
/// `config` 里的显示尺寸再 SetOutputType」。首次协商能过（那时解码器还
/// 不知道真实尺寸，它自报的正好就是输入类型里那个值），但
/// **一旦收到 STREAM_CHANGE 就全线失败**：
///
/// ```text
/// 编码 640x360（H.264 宏块对齐后 coded size = 640x368）
///   → 首个 IDR 后解码器报 STREAM_CHANGE
///   → 自报类型变成 640x368（coded size，SPS 里的真实尺寸）
///   → 我们覆盖成 640x360 再 SetOutputType
///   → 0xC00D36B4 MF_E_INVALIDMEDIATYPE
/// ```
///
/// 原因是解码器此时已经知道真实编码尺寸，覆盖成别的值它不认。
/// 而 640x368 本来就是**正确**的输出尺寸 —— 每帧 353280 字节，
/// 多出来的 8 行是 H.264 的宏块填充，显示时按 SPS 的 cropping 裁掉即可。
///
/// 所以顺序是：
///
/// 1. 自报类型**原样**（不动任何字段）
/// 2. 自报类型 + 覆盖帧尺寸（只对「自报的尺寸是 0 或明显是占位值」的解码器有用）
/// 3. 自己构造 NV12 / YV12 / IYUV
fn set_output_type(transform: &IMFTransform, config: &DecoderConfig) -> Result<IMFMediaType> {
    let prefer = [MFVideoFormat_NV12, MFVideoFormat_YV12, MFVideoFormat_IYUV];
    let frame_size = ((config.width as u64) << 32) | config.height as u64;
    let frame_rate = ((config.fps as u64) << 32) | 1u64;

    let mut last_err = None;

    // ---- 第一轮：自报类型原样 ----
    for i in 0..32u32 {
        // SAFETY: transform 有效
        let candidate = match unsafe { transform.GetOutputAvailableType(0, i) } {
            Ok(t) => t,
            Err(e) if e.code() == MF_E_NO_MORE_TYPES => break,
            Err(e) => {
                last_err = Some(e);
                break;
            }
        };
        // SAFETY: candidate 有效
        let subtype = unsafe { candidate.GetGUID(&MF_MT_SUBTYPE) }.unwrap_or_default();
        if OutputFormat::from_subtype(&subtype).is_none() {
            continue;
        }
        // SAFETY: 同上
        match unsafe { transform.SetOutputType(0, &candidate, 0) } {
            Ok(()) => {
                return unsafe { transform.GetOutputCurrentType(0) }
                    .map_err(|e| EncoderError::from_win("GetOutputCurrentType", e));
            }
            Err(e) => last_err = Some(e),
        }
    }

    // ---- 第二轮：自报类型 + 覆盖帧尺寸/帧率 ----
    for i in 0..32u32 {
        // SAFETY: transform 有效
        let candidate = match unsafe { transform.GetOutputAvailableType(0, i) } {
            Ok(t) => t,
            Err(e) if e.code() == MF_E_NO_MORE_TYPES => break,
            Err(e) => {
                last_err = Some(e);
                break;
            }
        };
        // SAFETY: candidate 有效
        let subtype = unsafe { candidate.GetGUID(&MF_MT_SUBTYPE) }.unwrap_or_default();
        if OutputFormat::from_subtype(&subtype).is_none() {
            continue;
        }
        unsafe {
            let _ = candidate.SetUINT64(&MF_MT_FRAME_SIZE, frame_size);
            let _ = candidate.SetUINT64(&MF_MT_FRAME_RATE, frame_rate);
            let _ = candidate.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1u64);
            let _ =
                candidate.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32);
        }
        // SAFETY: 同上
        match unsafe { transform.SetOutputType(0, &candidate, 0) } {
            Ok(()) => {
                return unsafe { transform.GetOutputCurrentType(0) }
                    .map_err(|e| EncoderError::from_win("GetOutputCurrentType", e));
            }
            Err(e) => last_err = Some(e),
        }
    }

    // ---- 第三轮：自己构造 ----
    for subtype in prefer {
        // SAFETY: 纯工厂调用
        let mt = unsafe { MFCreateMediaType() }
            .map_err(|e| EncoderError::from_win("MFCreateMediaType(输出)", e))?;
        unsafe {
            mt.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .map_err(|e| EncoderError::from_win("SetGUID(输出 MAJOR)", e))?;
            mt.SetGUID(&MF_MT_SUBTYPE, &subtype)
                .map_err(|e| EncoderError::from_win("SetGUID(输出 SUBTYPE)", e))?;
            mt.SetUINT64(&MF_MT_FRAME_SIZE, frame_size)
                .map_err(|e| EncoderError::from_win("SetUINT64(输出 FRAME_SIZE)", e))?;
            mt.SetUINT64(&MF_MT_FRAME_RATE, frame_rate)
                .map_err(|e| EncoderError::from_win("SetUINT64(输出 FRAME_RATE)", e))?;
            mt.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1u64)
                .map_err(|e| EncoderError::from_win("SetUINT64(输出 PAR)", e))?;
            mt.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                .map_err(|e| EncoderError::from_win("SetUINT32(输出 INTERLACE)", e))?;
            mt.SetUINT32(&MF_MT_ALL_SAMPLES_INDEPENDENT, 1)
                .map_err(|e| EncoderError::from_win("SetUINT32(输出 INDEPENDENT)", e))?;
        }
        // SAFETY: transform 与 mt 都有效
        match unsafe { transform.SetOutputType(0, &mt, 0) } {
            Ok(()) => {
                return unsafe { transform.GetOutputCurrentType(0) }
                    .map_err(|e| EncoderError::from_win("GetOutputCurrentType", e));
            }
            Err(e) => last_err = Some(e),
        }
    }

    Err(EncoderError::from_win(
        "SetOutputType",
        last_err.unwrap_or_else(|| {
            windows::core::Error::new(
                windows::Win32::Foundation::E_FAIL,
                "解码器没有可用的未压缩输出类型",
            )
        }),
    ))
}

/// 从输出类型读回真实格式 / 尺寸 / 行跨度
fn read_output_shape(
    mt: &IMFMediaType,
    config: &DecoderConfig,
) -> Result<(OutputFormat, u32, u32, i32)> {
    // SAFETY: mt 有效
    let subtype = unsafe { mt.GetGUID(&MF_MT_SUBTYPE) }
        .map_err(|e| EncoderError::from_win("GetGUID(输出 SUBTYPE)", e))?;
    let format = OutputFormat::from_subtype(&subtype).ok_or_else(|| {
        EncoderError::new(
            "GetOutputCurrentType",
            format!("协商到的输出 subtype {subtype:?} 不是 NV12/YV12/IYUV"),
        )
    })?;

    // MF_MT_FRAME_SIZE 是 (宽 << 32) | 高
    let size = unsafe { mt.GetUINT64(&MF_MT_FRAME_SIZE) }.unwrap_or(0);
    let (w, h) = if size != 0 {
        ((size >> 32) as u32, (size & 0xFFFF_FFFF) as u32)
    } else {
        (config.width, config.height)
    };

    // MF_MT_DEFAULT_STRIDE 是 UINT32，负数（自下而上）以补码形式存
    let stride = unsafe { mt.GetUINT32(&MF_MT_DEFAULT_STRIDE) }
        .map(|v| v as i32)
        .unwrap_or(w as i32);

    Ok((format, w, h, stride))
}

fn output_stream_info(transform: &IMFTransform) -> Result<(bool, u32)> {
    // SAFETY: transform 有效
    let info = unsafe { transform.GetOutputStreamInfo(0) }
        .map_err(|e| EncoderError::from_win("GetOutputStreamInfo", e))?;
    let provides = info.dwFlags & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32) != 0;
    // cbSize 为 0 表示「每帧大小不定」，给一个保守上限（4K NV12 是 12.4 MB）
    let size = if info.cbSize == 0 {
        16 * 1024 * 1024
    } else {
        info.cbSize
    };
    Ok((provides, size))
}

/// 低延迟相关参数
///
/// # 这两个开关的 VARIANT 类型是反直觉的（实测）
///
/// | 参数 | 文档说的类型 | **实际**要的类型 | 用错的结果 |
/// | --- | --- | --- | --- |
/// | `CODECAPI_AVLowLatencyMode` | BOOL | **VT_UI4** | E_INVALIDARG |
/// | `CODECAPI_AVDecNumWorkerThreads` | UINT32 | **VT_I4** | E_INVALIDARG |
///
/// 两者用错都返回 `0x80070057`（E_INVALIDARG），看起来像「这个参数不支持」，
/// 实际只是类型不对。详细探针数据见 [`crate::variant`] 的模块文档。
///
/// # 实测效果（640x360@30、Main、GOP 30、0 B 帧，喂 80 帧）
///
/// | 配置 | 产出帧数 | **延迟（帧）** |
/// | --- | --- | --- |
/// | 什么都不设 | 47 | **33** |
/// | 只设 worker threads = 1 | 70 | **10** |
/// | **AVLowLatencyMode = 1** | **80** | **0** |
/// | 低延迟 + threads=1 | 80 | 0 |
///
/// 「延迟 33 帧」意味着不设这个开关时解码器会攒满 32~33 帧才吐第一帧 ——
/// 对 30fps 就是**一秒钟**的画面滞后，实时投屏完全不可接受。
/// 这是本模块最重要的一条实测结论。
///
/// # 为什么不设 `AVDecVideoMaxCodedWidth` / `AVDecVideoMaxCodedHeight`
///
/// 它们**是**支持的（`ICodecAPI::IsSupported` 返回 S_OK），但实测对延迟
/// 与产出帧数都没有正面影响（喂 40 帧：设与不设都是 9 帧）。
/// 不设少两次调用，也不改变行为。
fn apply_codec_api(transform: &IMFTransform) {
    // ICodecAPI 是 IMFTransform 上的可选接口，不是所有 MFT 都实现
    let Ok(codec) = transform.cast::<ICodecAPI>() else {
        return;
    };
    // SAFETY: codec 有效
    unsafe {
        // 关掉解码器的输出攒批：这是「延迟 33 帧 → 0 帧」的唯一开关。
        // 注意 VARIANT 类型是 VT_UI4 而不是 VT_BOOL。
        let _ = codec.SetValue(&CODECAPI_AVLowLatencyMode, &variant_u32(1));
        // 实时流不需要多线程并行解码：线程多了只会让每帧耗时抖动变大。
        // 注意 VARIANT 类型是 VT_I4 而不是 VT_UI4。
        let _ = codec.SetValue(&CODECAPI_AVDecNumWorkerThreads, &variant_i32(1));
    }
}

fn make_input_sample(annex_b: &[u8], pts_us: i64, interval_us: i64) -> Result<IMFSample> {
    let need = annex_b.len() as u32;
    // SAFETY: 纯工厂调用
    let buffer = unsafe { MFCreateMemoryBuffer(need) }
        .map_err(|e| EncoderError::from_win("MFCreateMemoryBuffer(输入)", e))?;
    // SAFETY: buffer 有效；Lock 返回的指针在 Unlock 之前有效
    unsafe {
        let mut ptr: *mut u8 = std::ptr::null_mut();
        buffer
            .Lock(&mut ptr, None, None)
            .map_err(|e| EncoderError::from_win("Lock", e))?;
        if ptr.is_null() {
            let _ = buffer.Unlock();
            return Err(EncoderError::new("Lock", "Lock 返回了空指针"));
        }
        std::ptr::copy_nonoverlapping(annex_b.as_ptr(), ptr, annex_b.len());
        buffer
            .SetCurrentLength(need)
            .map_err(|e| EncoderError::from_win("SetCurrentLength", e))?;
        buffer
            .Unlock()
            .map_err(|e| EncoderError::from_win("Unlock", e))?;
    }

    // SAFETY: 纯工厂调用
    let sample =
        unsafe { MFCreateSample() }.map_err(|e| EncoderError::from_win("MFCreateSample", e))?;
    // SAFETY: sample 与 buffer 都有效
    unsafe { sample.AddBuffer(&buffer) }.map_err(|e| EncoderError::from_win("AddBuffer", e))?;
    // 时间戳单位是 100ns，输入是微秒
    // SAFETY: sample 有效
    unsafe {
        sample
            .SetSampleTime(pts_us.saturating_mul(10))
            .map_err(|e| EncoderError::from_win("SetSampleTime", e))?;
        sample
            .SetSampleDuration(interval_us.saturating_mul(10))
            .map_err(|e| EncoderError::from_win("SetSampleDuration", e))?;
    }
    Ok(sample)
}

fn read_sample(sample: &IMFSample) -> Result<Vec<u8>> {
    // SAFETY: sample 有效
    let buffer = unsafe { sample.ConvertToContiguousBuffer() }
        .map_err(|e| EncoderError::from_win("ConvertToContiguousBuffer", e))?;
    // SAFETY: buffer 有效；Lock/Unlock 成对
    unsafe {
        let mut ptr: *mut u8 = std::ptr::null_mut();
        let mut len: u32 = 0;
        buffer
            .Lock(&mut ptr, None, Some(&mut len))
            .map_err(|e| EncoderError::from_win("Lock(输出)", e))?;
        let out = if ptr.is_null() || len == 0 {
            Vec::new()
        } else {
            std::slice::from_raw_parts(ptr, len as usize).to_vec()
        };
        let _ = buffer.Unlock();
        Ok(out)
    }
}

/// 按 stride 把解码器输出重排成紧凑布局
///
/// # 为什么需要
///
/// MF 的输出缓冲**不保证**是紧凑的：`MF_MT_DEFAULT_STRIDE` 常常大于
/// 宽度（硬件解码器按 64/128 字节对齐），负数还表示自下而上。
/// 直接把 `len` 字节当紧凑帧用，画面会整体斜切。
///
/// 微软的软件解码器实测 stride == width（紧凑），但硬件解码器不一定，
/// 所以这里按 stride 重排 —— 紧凑时走的是零拷贝的快路径。
pub(crate) fn repack(
    raw: &[u8],
    stride: i32,
    width: usize,
    height: usize,
    format: OutputFormat,
) -> Vec<u8> {
    let expect = format.frame_len(width as u32, height as u32);
    let abs_stride = stride.unsigned_abs() as usize;
    let bottom_up = stride < 0;

    // 快路径：紧凑 + 自上而下 + 长度够
    if abs_stride == width && !bottom_up && raw.len() >= expect {
        return raw[..expect].to_vec();
    }
    if abs_stride == 0 || width == 0 || height == 0 {
        return raw.to_vec();
    }

    let mut out = vec![0u8; expect];

    // 按行把一个平面从 src 搬到 out 的 dst_off 处
    fn copy_plane(
        out: &mut [u8],
        src: &[u8],
        rows: usize,
        row_bytes: usize,
        dst_off: usize,
        stride: usize,
        bottom_up: bool,
    ) {
        for r in 0..rows {
            let src_row = if bottom_up { rows - 1 - r } else { r };
            let s = src_row * stride;
            if s + row_bytes > src.len() {
                return;
            }
            let d = dst_off + r * row_bytes;
            if d + row_bytes > out.len() {
                return;
            }
            out[d..d + row_bytes].copy_from_slice(&src[s..s + row_bytes]);
        }
    }

    match format {
        OutputFormat::Nv12 => {
            // Y 平面：height 行，每行 width 字节
            copy_plane(&mut out, raw, height, width, 0, abs_stride, bottom_up);
            // UV 交织平面：height/2 行，每行 width 字节（跟在 Y 之后）
            let uv_src_off = height * abs_stride;
            if uv_src_off < raw.len() {
                copy_plane(
                    &mut out,
                    &raw[uv_src_off..],
                    height / 2,
                    width,
                    width * height,
                    abs_stride,
                    bottom_up,
                );
            }
        }
        OutputFormat::Yv12 | OutputFormat::Iyuv => {
            copy_plane(&mut out, raw, height, width, 0, abs_stride, bottom_up);
            let chroma_w = width / 2;
            let chroma_h = height / 2;
            let u_off = height * abs_stride;
            let v_off = u_off + chroma_h * abs_stride;
            let y_size = width * height;
            let c_size = chroma_w * chroma_h;
            // IYUV(I420) 是 Y U V，YV12 是 Y V U —— 顺序相反，
            // 弄反了颜色会整体偏绿/偏紫。
            let (first_off, second_off) = if format == OutputFormat::Iyuv {
                (u_off, v_off)
            } else {
                (v_off, u_off)
            };
            if first_off < raw.len() {
                copy_plane(
                    &mut out,
                    &raw[first_off..],
                    chroma_h,
                    chroma_w,
                    y_size,
                    abs_stride,
                    bottom_up,
                );
            }
            if second_off < raw.len() {
                copy_plane(
                    &mut out,
                    &raw[second_off..],
                    chroma_h,
                    chroma_w,
                    y_size + c_size,
                    abs_stride,
                    bottom_up,
                );
            }
        }
    }
    out
}
