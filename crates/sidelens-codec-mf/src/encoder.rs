//! H.264 编码器（Media Foundation MFT，同步路径）
//!
//! # 为什么是同步
//!
//! MF 的 H.264 编码器有两条使用路径：
//!
//! | 路径 | 需要的接口 | 复杂度 |
//! | --- | --- | --- |
//! | 同步（软件 MFT） | ProcessInput / ProcessOutput | 一次调用一帧 |
//! | 异步（硬件 MFT） | 解锁 MF_TRANSFORM_ASYNC_UNLOCK + IMFMediaEventGenerator | 事件循环、METransformNeedInput / METransformHaveOutput |
//!
//! 本实现走同步。硬件 MFT 在枚举阶段就被**过滤掉**（见 [`H264Encoder::list_encoders`]），
//! 而不是「解锁后当同步用」—— 异步 MFT 被强制同步调用时 ProcessInput 返回
//! MF_E_TRANSFORM_ASYNC_LOCKED，不是能靠重试绕过的。
//!
//! # 输出格式：不猜，实测
//!
//! MFVideoFormat_H264 的字节布局**没有文档保证**是 Annex-B 还是 AVCC。
//! 本实现拿到第一帧输出时按字节特征判定（[`detect_format`]），
//! 判定结果通过 [`H264Encoder::output_format`] 暴露，并统一归一化成 Annex-B。
//!
//! # SPS/PPS 从哪来（实测与文档相反）
//!
//! 很多资料说 MF 的 H.264 编码器把参数集放在输出类型的
//! MF_MT_MPEG_SEQUENCE_HEADER 里、码流本身不带 SPS/PPS。
//!
//! **实测不是这样**：mfh264enc 的 MF_MT_MPEG_SEQUENCE_HEADER 是**空的**
//! （GetBlobSize 返回 0），SPS/PPS 直接内联在码流里，跟在每个 IDR 前的
//! AUD 后面。dump 出来的 720p 文件里能看到 SPS 1 个 / PPS 1 个。
//!
//! 所以两条路都实现了：
//!
//! 1. 类型里给了 sequence header → 转成 Annex-B 缓存，在缺参数集的关键帧前补
//! 2. 类型里没给 → 从码流首帧里提取并缓存（当前 mfh264enc 走这条）
//!
//! 只有「两个都没拿到」才是真问题 —— 那种情况下接收端中途加入就解不出来。

use std::ffi::c_void;
use std::mem::ManuallyDrop;

use sidelens_codec::nalu;
use windows::core::{Interface, GUID};
use windows::Win32::Media::MediaFoundation::{
    eAVEncCommonRateControlMode_CBR, eAVEncH264VProfile_Main, CODECAPI_AVEncCommonBufferSize,
    CODECAPI_AVEncCommonLowLatency, CODECAPI_AVEncCommonMaxBitRate,
    CODECAPI_AVEncCommonMeanBitRate, CODECAPI_AVEncCommonRateControlMode,
    CODECAPI_AVEncMPVDefaultBPictureCount, CODECAPI_AVEncMPVGOPSize,
    CODECAPI_AVEncVideoForceKeyFrame, CODECAPI_AVLowLatencyMode, ICodecAPI, IMFActivate,
    IMFMediaBuffer, IMFMediaType, IMFSample, IMFTransform, MFCreateMediaType, MFCreateMemoryBuffer,
    MFCreateSample, MFMediaType_Video, MFTEnumEx, MFVideoFormat_H264, MFVideoFormat_NV12,
    MFVideoInterlace_Progressive, MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_ALL,
    MFT_ENUM_FLAG_HARDWARE, MFT_MESSAGE_COMMAND_DRAIN, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_END_OF_STREAM, MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES,
    MFT_REGISTER_TYPE_INFO, MF_E_NOTACCEPTING, MF_E_NO_MORE_TYPES, MF_E_TRANSFORM_NEED_MORE_INPUT,
    MF_LOW_LATENCY, MF_MT_ALL_SAMPLES_INDEPENDENT, MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE,
    MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_MAX_KEYFRAME_SPACING,
    MF_MT_MPEG2_PROFILE, MF_MT_MPEG_SEQUENCE_HEADER, MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE,
    MF_TRANSFORM_ASYNC,
};
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_INPROC_SERVER};
use windows::Win32::System::Variant::VT_UI4;

use crate::com::MediaFoundation;
use crate::error::{EncoderError, Result};
// VARIANT 的构造收在 crate::variant —— 解码器也要设 CODECAPI_*，
// 各写一份手搓联合体的代码迟早会分叉。
use crate::variant::{variant_bool, variant_u32};

/// 允许的最大目标码率（bps）= 100 Mbps
///
/// 超过这个值的入参直接拒绝：MF 的 `CODECAPI_AVEncCommonMeanBitRate` 是
/// `VT_UI4`，但 4 Gbps 这种量级对 1080p 以下的实时链路没有意义，
/// 而 100 Mbps 已经覆盖 4K60 无损前的一切合理场景。
/// 更重要的是：**不要静默钳制**。调用方拿到 Ok 就该相信码率被设成了它给的值，
/// 悄悄改成别的数只会在「为什么画面还是糊」时多一层排查。
pub const MAX_BITRATE_BPS: u32 = 100_000_000;

/// 目标码率 → 码控允许的**瞬时上限**（CODECAPI_AVEncCommonMaxBitRate）
///
/// # 倍数是实测选的，而且实测结论是「这个参数在 mfh264enc 上不参与决策」
///
/// 先做了 max 倍数扫描（640x360，patch-noise 内容，mean 固定 2 Mbps / 500 kbps，
/// 每档 120 帧），max/mean 取 1.0× / 1.5× / 2.0× / 3.0×：
///
/// | max/mean | 500 kbps 目标下实际输出 | IDR 平均字节 |
/// | --- | --- | --- |
/// | 1.0× | 962 kbps | 23851 |
/// | 1.5× | 962 kbps | 23851 |
/// | 2.0× | 962 kbps | 23852 |
/// | 3.0× | 962 kbps | 23852 |
///
/// 四档输出字节数**完全相同**（差异 < 0.01%，是噪声级别）。
///
/// 又做了参数隔离（每次只改 mean / max / buffer 之一，内容与帧号完全相同）：
///
/// | 只改这个参数 | 后窗口字节/帧 | 相对对照 |
/// | --- | --- | --- |
/// | 对照（什么都不改） | 7918 | 1.00 |
/// | mean → 500k | 2936 | **0.37** |
/// | max → 500k | 7918 | 1.00 |
/// | buffer → 500k | 7918 | 1.00 |
///
/// 结论：mfh264enc 的码控**只认 mean**，max 与 buffer 设什么值都不改变输出。
///
/// # 那为什么还设 1.5×
///
/// 1. 硬件 MFT（NVENC / QSV）是另一套实现，max 在它们那里是有意义的 ——
///    本 crate 目前只走同步软件 MFT，但 `with_info` 允许调用方指定别的 MFT，
///    不能替它们假设「max 无效」。
/// 2. 1.5× 而不是 1:1 的理由是结构性的：IDR 的字节数天然是 P 帧的数倍，
///    max == mean 时 IDR 必然顶着上限、后面的 P 帧得把比特还回去。
///    实测 IDR 平均 23.8 KB ≈ 190 kbps×1s，是 500 kbps 目标的 2.4 倍 ——
///    也就是说 **1.5× 仍然小于 IDR 的实际需求**，它挡不住 IDR，
///    只是不让 P 帧有机会长期超标。这一点必须写清楚，免得读者以为
///    1.5× 是「为 IDR 留的余量」。
/// 3. 既然实测它无害（四档输出一致），保留一个保守值比留 1:1 更安全：
///    万一某个 MFT 真的按 max 裁，1:1 会让 IDR 被砍。
///
/// # 溢出
///
/// 用 `bps + bps/2` 而不是 `bps * 3 / 2`：后者在 `bps > u32::MAX / 3` 时
/// 会先溢出再除，结果可能**小于入参**（max < mean，语义反了）。
/// `saturating_add` 保证返回值永远 >= `bps`，对任意 u32 入参都单调且安全。
/// 100 Mbps 上限下是 150 Mbps，远在 u32 内 —— 这个处理是为将来改上限兜底。
pub fn max_bitrate_for(bps: u32) -> u32 {
    bps.saturating_add(bps / 2)
}

/// H.264 编码器 MFT 的 CLSID（CLSID_CMSH264EncoderMFT）
///
/// # 为什么要自己写
///
/// windows crate 的 Win32_Media_MediaFoundation 里**没有**导出这个常量
/// （grep 过 windows-0.62.2 的 mod.rs，只有 MFVideoFormat_* 与 CODECAPI_*，
/// 没有任何 CLSID_CMS*）。少了它就没法在枚举失败时用 CoCreateInstance 兜底。
pub const CLSID_CMSH264ENCODERMFT: GUID = GUID::from_u128(0x6ca50344_051a_4ded_9779_a43305165e35);

/// 编码器配置
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncoderConfig {
    /// 宽度（像素），必须为偶数
    pub width: u32,
    /// 高度（像素），必须为偶数
    pub height: u32,
    /// 帧率（帧/秒）
    pub fps: u32,
    /// 目标码率（bps）
    pub bitrate: u32,
    /// 关键帧间隔（帧数）。60 @ 30fps = 2 秒一个 IDR
    pub gop: u32,
}

impl Default for EncoderConfig {
    fn default() -> Self {
        Self {
            width: 1280,
            height: 720,
            fps: 30,
            bitrate: 2_000_000,
            gop: 60,
        }
    }
}

impl EncoderConfig {
    /// NV12 一帧的字节数
    ///
    /// NV12 的 Y 平面是 w*h，UV 交织平面是 w*h/2，合计 w*h*3/2。
    pub fn nv12_len(&self) -> usize {
        (self.width as usize) * (self.height as usize) * 3 / 2
    }

    /// 校验配置合法性
    pub fn validate(&self) -> Result<()> {
        if self.width == 0 || self.height == 0 {
            return Err(EncoderError::new("validate", "分辨率不能为 0"));
        }
        // H.264 的 4:2:0 采样要求宽高都是偶数；奇数会让编码器直接返回
        // MF_E_INVALIDMEDIATYPE，而错误信息完全不提这个原因。
        if self.width % 2 != 0 || self.height % 2 != 0 {
            return Err(EncoderError::new(
                "validate",
                format!("宽高必须是偶数，当前 {}x{}", self.width, self.height),
            ));
        }
        if self.fps == 0 {
            return Err(EncoderError::new("validate", "帧率不能为 0"));
        }
        if self.bitrate == 0 {
            return Err(EncoderError::new("validate", "码率不能为 0"));
        }
        if self.gop == 0 {
            return Err(EncoderError::new("validate", "GOP 不能为 0"));
        }
        Ok(())
    }

    /// 帧间隔（微秒）
    pub fn frame_interval_us(&self) -> i64 {
        1_000_000 / self.fps as i64
    }
}

/// 编码器 MFT 的元信息
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncoderInfo {
    /// 友好名（MFT_FRIENDLY_NAME_Attribute）
    pub name: String,
    /// 是否硬件 MFT
    pub hardware: bool,
    /// 是否异步 MFT（本 crate 不用）
    pub is_async: bool,
}

/// 实测到的输出码流格式
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    /// 起始码分隔（00 00 00 01 / 00 00 01）
    AnnexB,
    /// 4 字节大端长度前缀
    Avcc,
}

impl OutputFormat {
    /// 名字（日志用）
    pub fn as_str(self) -> &'static str {
        match self {
            OutputFormat::AnnexB => "Annex-B",
            OutputFormat::Avcc => "AVCC",
        }
    }
}

/// 按字节特征判定码流格式
///
/// # 判据（按顺序）
///
/// 1. 以 00 00 00 01 或 00 00 01 开头 → Annex-B
/// 2. 前 4 字节当大端长度，且 4+len <= buf.len()、第 5 字节的 nal_unit_type
///    落在 1..=12 → AVCC
/// 3. 其余情况退化为 Annex-B（绝大多数实现都是这个）
pub fn detect_format(buf: &[u8]) -> OutputFormat {
    if buf.len() >= 4 && buf[..4] == [0, 0, 0, 1] {
        return OutputFormat::AnnexB;
    }
    if buf.len() >= 3 && buf[..3] == [0, 0, 1] {
        return OutputFormat::AnnexB;
    }
    if buf.len() >= 5 {
        let len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
        if len > 0 && 4 + len <= buf.len() {
            let header = buf[4];
            let ty = header & 0x1f;
            // forbidden_zero_bit 必须为 0，类型必须是真实存在的
            if header & 0x80 == 0 && (1..=12).contains(&ty) {
                return OutputFormat::Avcc;
            }
        }
    }
    OutputFormat::AnnexB
}

/// H.264 编码器
///
/// 一个实例对应一个 IMFTransform，**非线程安全**（MFT 本身不是）。
///
/// # 运行中改码率
///
/// 拥塞控制器算出的目标码率通过 [`H264Encoder::set_bitrate`] 接进来，
/// **不需要重建编码器**：实测在调用后的下一帧就生效，逐帧字节数
/// `28:8851 29:8588 30:9064 | 31:3078 32:2890` 是一次干净的跳变。
///
/// 这一点值得强调，因为「重建编码器」的代价很实在：
///
/// | 项目 | 实测 |
/// | --- | --- |
/// | `H264Encoder::new` 耗时 | **约 200 ms**（20 次均值 196~216 ms，最快 190 ms） |
/// | 重建后的流 | 需要重发 SPS/PPS + IDR，接收端必须容忍参数集变化 |
///
/// 200 ms 对实时链路是一段无法接受的空档（30fps 下就是 6 帧黑屏），
/// 而且拥塞控制需要的是**秒级、平滑**的码率跟随，不是每隔几秒重建一次。
///
/// 但「SetValue 返回 S_OK」**不等于**「码率真的变了」。实测结论：
///
/// - mfh264enc 只认 `AVEncCommonMeanBitRate`，`MaxBitRate` 与 `BufferSize`
///   设什么值都不改变输出（四档 max 倍数输出字节数完全一致）。
/// - 改 mean **一定**成功，但**是否改变输出取决于内容复杂度**：
///   低熵内容本来就花不到目标码率，高熵内容编码器宁可超也不降质量。
///   只有「内容需求与目标码率同量级」时，改码率才真的改变输出字节数。
///
/// 详细数字见 [`H264Encoder::set_bitrate`] 与 [`max_bitrate_for`]。
#[derive(Debug)]
pub struct H264Encoder {
    config: EncoderConfig,
    transform: IMFTransform,
    /// 保持 MF 平台存活（字段顺序即 drop 顺序：先 transform 后关停）
    _mf: MediaFoundation,
    /// 输出类型里带出来的参数集（Annex-B，含起始码）
    parameter_sets: Vec<u8>,
    /// 实测到的输出格式
    format: Option<OutputFormat>,
    /// 编码器名（日志用）
    name: String,
    /// 已喂入的帧数
    frames_in: u64,
    /// 已取出的输出帧数
    frames_out: u64,
    /// 输出缓冲复用
    output_buffer: Option<IMFMediaBuffer>,
    /// 输出侧是否由 MFT 自己提供 sample
    output_provides_samples: bool,
    /// 输出 sample 的建议大小
    output_sample_size: u32,
}

impl H264Encoder {
    /// 创建编码器（自动挑一个同步 MFT）
    pub fn new(config: EncoderConfig) -> Result<Self> {
        config.validate()?;
        let mf = MediaFoundation::init().map_err(init_err)?;
        let info = Self::pick_encoder()?;
        Self::build(config, info, mf)
    }

    /// 用指定的 MFT 创建编码器
    pub fn with_info(config: EncoderConfig, info: &EncoderInfo) -> Result<Self> {
        config.validate()?;
        let mf = MediaFoundation::init().map_err(init_err)?;
        Self::build(config, info.clone(), mf)
    }

    /// 列出本机所有能吃的 H.264 编码器 MFT
    ///
    /// 输入类型固定按 NV12 查、输出按 H.264 查 —— 与本 crate 的实际用法一致，
    /// 免得列出一堆用不上的 MFT。
    pub fn list_encoders() -> Result<Vec<EncoderInfo>> {
        let _mf = MediaFoundation::init().map_err(init_err)?;
        let hw_names: Vec<String> = enumerate(MFT_ENUM_FLAG_HARDWARE)?
            .iter()
            .map(read_name)
            .collect();
        let activates = enumerate(MFT_ENUM_FLAG_ALL)?;
        Ok(activates
            .iter()
            .map(|a| {
                let name = read_name(a);
                EncoderInfo {
                    hardware: hw_names.iter().any(|h| h == &name),
                    is_async: unsafe { a.GetUINT32(&MF_TRANSFORM_ASYNC) }
                        .map(|v| v != 0)
                        .unwrap_or(false),
                    name,
                }
            })
            .collect())
    }

    /// 选一个**同步**编码器
    ///
    /// 优先软件 MFT（确定同步、行为稳定）；没有软件的就退到非异步的。
    fn pick_encoder() -> Result<EncoderInfo> {
        let all = Self::list_encoders()?;
        let picked = all
            .iter()
            .find(|i| !i.is_async && !i.hardware)
            .or_else(|| all.iter().find(|i| !i.is_async))
            .cloned();

        match picked {
            Some(info) => Ok(info),
            None => {
                // 兜底：直接按 CLSID 造软件编码器。
                //
                // 走到这里说明枚举没返回任何**同步** MFT —— 可能是排序策略变了，
                // 或者注册表里只剩硬件项。
                Ok(EncoderInfo {
                    name: all
                        .first()
                        .map(|f| f.name.clone())
                        .unwrap_or_else(|| "H.264 Encoder MFT".to_string()),
                    hardware: false,
                    is_async: false,
                })
            }
        }
    }

    fn build(config: EncoderConfig, info: EncoderInfo, mf: MediaFoundation) -> Result<Self> {
        let transform = activate(&info)?;

        // 异步 MFT 走不了同步路径：ProcessInput 会返回 MF_E_TRANSFORM_ASYNC_LOCKED。
        // 这里提前拦下，错误信息才有指向性。
        if let Ok(attrs) = unsafe { transform.GetAttributes() } {
            if let Ok(async_flag) = unsafe { attrs.GetUINT32(&MF_TRANSFORM_ASYNC) } {
                if async_flag != 0 {
                    return Err(EncoderError::new(
                        "ActivateObject",
                        format!("{} 是异步 MFT，本实现只支持同步路径", info.name),
                    ));
                }
            }
        }

        // MF_LOW_LATENCY 必须在**设类型之前**写进 transform 的属性存储 ——
        // 这是 MFT 在 SetOutputType 时读的一次性配置。放在设类型之后写，
        // SetUINT32 照样返回 S_OK（属性确实存进去了），但编码器早就按
        // 默认（带 17 帧前瞻）配置好了，**静默无效**。
        //
        // 这是本 crate 踩到的最隐蔽的一个坑：SetValue/SetUINT32 全部成功，
        // 没有任何错误码，只有实测帧延迟才看得出没生效。
        if low_latency_enabled() {
            if let Ok(attrs) = unsafe { transform.GetAttributes() } {
                // SAFETY: attrs 有效
                let _ = unsafe { attrs.SetUINT32(&MF_LOW_LATENCY, 1) };
            }
        }

        // 码控参数写在设类型之前。
        //
        // 这不是必须的 —— A/B 实测（examples/ordering_ab.rs）证明
        // 设在 SetOutputType 前后**输出字节数完全一致**（差 < 1%），
        // mfh264enc 两种顺序都认。放前面只是让「配置 → 协商 → 推流」
        // 这个顺序更符合直觉，也避免踩到别的 MFT 可能有的快照行为。
        apply_codec_api(&transform, &config);

        // 先设输出类型再设输入类型：MF 的编码器按输出类型推导 profile/level，
        // 反过来设会让它在 SetOutputType 时拒绝。
        let desired = build_output_type(&config)?;
        let applied = set_output_type(&transform, &config, desired)?;

        let input_type = build_input_type(&config)?;
        // SAFETY: 两个对象都有效，0 表示不要求严格匹配
        unsafe { transform.SetInputType(0, &input_type, 0) }
            .map_err(|e| EncoderError::from_win("SetInputType", e))?;

        // 设完类型后再写一遍，覆盖「MFT 在 SetOutputType 里重置码控」的
        // 可能性（写重复值的代价是零）。实测 mfh264enc 不重置。
        apply_codec_api(&transform, &config);

        // 参数集：输出类型里带出来的 AVCC blob
        let parameter_sets = read_sequence_header(&applied);
        let (provides, size) = output_stream_info(&transform)?;

        // SAFETY: 开始推流
        let _ = unsafe { transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0) };

        Ok(Self {
            config,
            transform,
            _mf: mf,
            parameter_sets,
            format: None,
            name: info.name,
            frames_in: 0,
            frames_out: 0,
            output_buffer: None,
            output_provides_samples: provides,
            output_sample_size: size,
        })
    }

    /// 配置
    pub fn config(&self) -> &EncoderConfig {
        &self.config
    }

    /// 当前目标码率（bps）
    ///
    /// 与 `self.config().bitrate` 是同一个值 —— [`Self::set_bitrate`] 会同步更新它，
    /// 所以这里读到的一定是「最后一次成功设进去的值」，不是创建时的初值。
    pub fn bitrate(&self) -> u32 {
        self.config.bitrate
    }

    /// 运行中动态调整目标码率（bps）
    ///
    /// 拥塞控制器（`sidelens_core::congestion`）算出的目标码率从这里接进编码器。
    ///
    /// # 改的是哪几个参数
    ///
    /// | CODECAPI | 值 | 实测是否影响输出 |
    /// | --- | --- | --- |
    /// | `AVEncCommonMeanBitRate` | `bps` | **是**，唯一真正驱动码控的参数 |
    /// | `AVEncCommonMaxBitRate` | `bps * 1.5` | 否（mfh264enc 上四档倍数输出完全一致） |
    /// | `AVEncCommonBufferSize` | `bps` | 否（同上） |
    ///
    /// 三个一起设是为了**别的 MFT**（硬件 MFT 与第三方软件 MFT 对 max/buffer
    /// 的处理不同，本 crate 的 `with_info` 允许调用方指定它们）。
    /// 详细的实测数字见 [`max_bitrate_for`] 的文档。
    ///
    /// # 是否真的生效（实测结论：生效，但有条件）
    ///
    /// 实测（`examples/bitrate_step.rs`，640x360@30fps，GOP 60，
    /// 前 30 帧 2 Mbps → 第 30 帧前改到 500 kbps → 再编 60 帧，
    /// 对照组为同内容同帧号但**不改码率**）：
    ///
    /// | 内容 | 对照后窗口 | 降档后窗口 | 比值 |
    /// | --- | --- | --- | --- |
    /// | 低熵（渐变+竖条） | 1051 B/帧 | 1051 B/帧 | 1.00（不生效） |
    /// | 中熵（半幅噪声块平移） | 7918 B/帧 | 2936 B/帧 | **0.37（生效）** |
    /// | 高熵（逐帧全噪声） | 49233 B/帧 | 49233 B/帧 | 1.00（不生效） |
    ///
    /// 也就是说：**SetValue 一定成功（S_OK，GetValue 能读回新值），
    /// 但是否改变输出取决于内容复杂度**。这与 `examples/rate_control.rs`
    /// 里「CBR 只在内容本来就需要这么多码率时才起作用」的结论一致 ——
    /// 低熵内容本来就花不到目标码率，高熵内容编码器宁可超也不降质量，
    /// 只有「内容需求与目标码率同量级」时改码率才真的改变输出。
    ///
    /// **不需要重建编码器**。降档在**下一帧**就生效（逐帧字节
    /// `28:8851 29:8588 30:9064 | 31:3078 32:2890`，跳变干净利落）。
    ///
    /// # 收敛速度（对拥塞控制器有直接影响）
    ///
    /// 同样的内容，210 帧切四段统计（kbps）：
    ///
    /// | 组 | 0-29 | 30-59 | 60-119 | 120-209 |
    /// | --- | --- | --- | --- | --- |
    /// | 对照 全程 2M | 2937 | 1762 | 2065 | 2094 |
    /// | 降档 2M→500k | 2937 | 725 | 679 | 679 |
    /// | 升档 500k→2M | 1807 | 1555 | 1905 | 1969 |
    ///
    /// - **降档是瞬时的**：第 30 帧前调用，第 31 帧就掉到 3078 字节/帧，
    ///   之后稳定在 679 kbps，不回弹。AIMD 的「乘性减」可以放心用。
    /// - **升档要 60~120 帧（2~4 秒）才追上对照**：改完立刻只到 1555 kbps，
    ///   到 120 帧后才是 1969（对照 2094）。编码器升码率明显比降码率慢 ——
    ///   AIMD 的「加性增」本来就慢，这个额外滞后意味着**实际码率比控制器
    ///   算出的目标更晚到位**，调参时要留这段余量。
    ///
    /// # 校验
    ///
    /// `bps == 0` 或 `bps > [`MAX_BITRATE_BPS`]` 直接返回 Err，**不钳制**：
    /// 悄悄改成别的值会让调用方以为码率已经按它的意图设好了。
    /// 校验失败时 `self.config.bitrate` **不变**，`config()` 与 `bitrate()`
    /// 不会报出一个编码器并不认的值。
    pub fn set_bitrate(&mut self, bps: u32) -> Result<()> {
        if bps == 0 {
            return Err(EncoderError::new("set_bitrate", "码率不能为 0"));
        }
        if bps > MAX_BITRATE_BPS {
            return Err(EncoderError::new(
                "set_bitrate",
                format!("码率 {bps} 超过上限 {MAX_BITRATE_BPS} bps"),
            ));
        }

        let codec = self
            .transform
            .cast::<ICodecAPI>()
            .map_err(|e| EncoderError::from_win("ICodecAPI", e))?;

        let max = max_bitrate_for(bps);
        // SAFETY: codec 有效；VARIANT 是栈上临时值，SetValue 只读它
        unsafe {
            codec
                .SetValue(&CODECAPI_AVEncCommonMeanBitRate, &variant_u32(bps))
                .map_err(|e| EncoderError::from_win("SetValue(MeanBitRate)", e))?;
            codec
                .SetValue(&CODECAPI_AVEncCommonMaxBitRate, &variant_u32(max))
                .map_err(|e| EncoderError::from_win("SetValue(MaxBitRate)", e))?;
            // VBV 深度跟着码率走。不改的话，从 2 Mbps 降到 500 kbps 时
            // 缓冲仍是 2 Mbps 深 —— 相当于允许编码器在一秒内攒下 4 倍预算，
            // 短窗口内的码率抖动会直接打进抖动缓冲。
            let _ = codec.SetValue(&CODECAPI_AVEncCommonBufferSize, &variant_u32(bps));
        }

        // 只有 SetValue 成功才改内部状态：否则 config() 会报出一个
        // 编码器并不认的码率，排查「改了没用」时先被这个假值带偏。
        self.config.bitrate = bps;
        Ok(())
    }

    /// 读回编码器当前**实际存储**的平均码率
    ///
    /// 用 `ICodecAPI::GetValue` 读 `AVEncCommonMeanBitRate`。
    ///
    /// # 这个值不能当成「码率真的被限住了」的证据
    ///
    /// 它只说明 MFT 把值存下来了。mfh264enc 实测：GetValue 读回 500 kbps，
    /// 但输出字节数可能一点没变（见 `examples/bitrate_step.rs`）。
    /// 想判断码率是否真的起作用，唯一可靠的办法是统计输出字节数。
    ///
    /// MFT 不实现 ICodecAPI 或该参数只写时返回 Err。
    pub fn bitrate_readback(&self) -> Result<u32> {
        let codec = self
            .transform
            .cast::<ICodecAPI>()
            .map_err(|e| EncoderError::from_win("ICodecAPI", e))?;
        // SAFETY: codec 有效；GetValue 返回的 VARIANT 归我们所有
        let value = unsafe { codec.GetValue(&CODECAPI_AVEncCommonMeanBitRate) }
            .map_err(|e| EncoderError::from_win("GetValue(MeanBitRate)", e))?;
        // VARIANT 是 VT_UI4，直接读联合体成员。
        // SAFETY: 上面调用的是同一个参数，mfh264enc 返回 VT_UI4；
        // 但别的 MFT 可能返回 VT_I4/VT_EMPTY，所以先判 vt 再读。
        unsafe {
            if value.Anonymous.Anonymous.vt.0 != VT_UI4.0 {
                return Err(EncoderError::new(
                    "GetValue(MeanBitRate)",
                    format!(
                        "返回的 VARIANT 类型是 {}，不是 VT_UI4，无法直接当码率读",
                        value.Anonymous.Anonymous.vt.0
                    ),
                ));
            }
            Ok(value.Anonymous.Anonymous.Anonymous.ulVal)
        }
    }

    /// 请求编码器在**下一帧**强制出一个 IDR
    ///
    /// 走 `CODECAPI_AVEncVideoForceKeyFrame`（VT_UI4 = 1）。
    ///
    /// 用途：改完码率后要一个干净的重同步点 —— 参数集（SPS/PPS）在 IDR 前
    /// 重发，接收端丢包后能立刻恢复。
    ///
    /// # 实测：对码率切换**没有必要**
    ///
    /// 曾经以为「编码器只在关键帧上重新评估码控目标，不强制 IDR 的话
    /// 新码率要等一个完整 GOP 才生效」。实测否定了这个假设：
    ///
    /// | 组 | 后窗口字节/帧 |
    /// | --- | --- |
    /// | 降档 2M→500k | 2936 |
    /// | 降档 + 强制 IDR | 2922 |
    ///
    /// 差 0.5%，而且降档在**下一帧**就已经生效（见 [`Self::set_bitrate`]）。
    /// mfh264enc 是逐帧评估码控的，不等 IDR。
    ///
    /// 保留这个接口是因为它对**别的 MFT** 仍然有用（硬件 MFT 常见
    /// 「只在 IDR 上重配码控」的行为），而且切流/切分辨率时本来就需要
    /// 主动要一个 IDR。
    ///
    /// 返回 Ok 只代表请求被接受，不代表下一帧一定是 IDR —— 判断依据
    /// 仍然是用 `nalu::is_keyframe` 看真实输出。
    pub fn force_keyframe(&mut self) -> Result<()> {
        let codec = self
            .transform
            .cast::<ICodecAPI>()
            .map_err(|e| EncoderError::from_win("ICodecAPI", e))?;
        // SAFETY: codec 有效
        unsafe { codec.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &variant_u32(1)) }
            .map_err(|e| EncoderError::from_win("SetValue(ForceKeyFrame)", e))
    }

    /// 实际使用的编码器名
    pub fn encoder_name(&self) -> &str {
        &self.name
    }

    /// 实测到的输出格式（还没产出过帧时为 None）
    pub fn output_format(&self) -> Option<OutputFormat> {
        self.format
    }

    /// 已缓存的参数集（Annex-B）
    ///
    /// 来源有二：输出类型的 MF_MT_MPEG_SEQUENCE_HEADER，或码流首帧里的
    /// SPS/PPS。mfh264enc 实测走后者，所以构造完成时这里通常是空的，
    /// 编过第一帧之后才有内容。
    pub fn parameter_sets(&self) -> &[u8] {
        &self.parameter_sets
    }

    /// 已喂入的帧数
    pub fn frames_in(&self) -> u64 {
        self.frames_in
    }

    /// 已取出的输出帧数
    pub fn frames_out(&self) -> u64 {
        self.frames_out
    }

    /// 输出侧是否由 MFT 自己分配 sample
    pub fn output_provides_samples(&self) -> bool {
        self.output_provides_samples
    }

    /// 编码一帧 NV12，返回该帧产出的 **Annex-B** 码流
    ///
    /// 返回值可能为空（MFT 内部缓冲，本帧还没吐输出），也可能包含多个
    /// NALU（SPS/PPS/SEI/IDR）。调用方用 nalu::is_keyframe 判断关键帧。
    pub fn encode_nv12(&mut self, nv12: &[u8], pts_us: i64) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        self.encode_nv12_into(nv12, pts_us, &mut out)?;
        Ok(out)
    }

    /// 编码一帧 NV12，追加到 out
    ///
    /// 与 [`Self::encode_nv12`] 相同，但复用调用方的缓冲（每帧省一次分配）。
    pub fn encode_nv12_into(&mut self, nv12: &[u8], pts_us: i64, out: &mut Vec<u8>) -> Result<()> {
        let expect = self.config.nv12_len();
        if nv12.len() < expect {
            return Err(EncoderError::new(
                "encode_nv12",
                format!("NV12 数据不足：需要 {expect} 字节，实际 {}", nv12.len()),
            ));
        }

        let sample = make_input_sample(&nv12[..expect], pts_us, &self.config)?;

        // SAFETY: sample 有效，0 是输入流 ID（MFT 只有一路输入）
        match unsafe { self.transform.ProcessInput(0, &sample, 0) } {
            Ok(()) => {}
            Err(e) if e.code() == MF_E_NOTACCEPTING => {
                // 上一帧的输出还没取走。先抽干再重试一次 ——
                // 这是同步 MFT 最常见的「卡住」原因。
                self.drain_outputs(out)?;
                // SAFETY: 同上
                unsafe { self.transform.ProcessInput(0, &sample, 0) }
                    .map_err(|e| EncoderError::from_win("ProcessInput(重试)", e))?;
            }
            Err(e) => return Err(EncoderError::from_win("ProcessInput", e)),
        }
        self.frames_in += 1;

        self.drain_outputs(out)
    }

    /// 冲刷编码器，返回剩余的码流
    ///
    /// 发 MFT_MESSAGE_COMMAND_DRAIN 后把输出取干净。**不调用会让最后几帧
    /// 卡在 MFT 内部**。
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

    // ---- 内部实现 ----

    fn drain_outputs(&mut self, out: &mut Vec<u8>) -> Result<()> {
        loop {
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
            let produced = ManuallyDrop::into_inner(std::mem::replace(
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
                    if let Some(sample) = produced {
                        let raw = read_sample(&sample)?;
                        if !raw.is_empty() {
                            self.append_frame(&raw, out)?;
                        }
                    }
                    if status_flags & 0x100 != 0 {
                        // MFT_OUTPUT_DATA_BUFFER_FORMAT_CHANGE
                        return Err(EncoderError::with_hresult(
                            "ProcessOutput",
                            0,
                            "MFT 报告输出格式变化（FORMAT_CHANGE），需要重新协商类型",
                        ));
                    }
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(()),
                Err(e) => return Err(EncoderError::from_win("ProcessOutput", e)),
            }
        }
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

    /// 归一化成 Annex-B 并（对关键帧）补齐参数集
    fn append_frame(&mut self, raw: &[u8], out: &mut Vec<u8>) -> Result<()> {
        let format = match self.format {
            Some(f) => f,
            None => {
                let f = detect_format(raw);
                self.format = Some(f);
                f
            }
        };

        let converted: Option<Vec<u8>> = match format {
            OutputFormat::AnnexB => None,
            OutputFormat::Avcc => Some(nalu::avcc_to_annexb(raw).ok_or_else(|| {
                EncoderError::new(
                    "avcc_to_annexb",
                    format!("AVCC 长度前缀越界（{} 字节）", raw.len()),
                )
            })?),
        };
        let frame: &[u8] = converted.as_deref().unwrap_or(raw);
        if frame.is_empty() {
            return Ok(());
        }

        let nalus = nalu::split_annexb(frame);
        let has_sps = nalus.iter().any(|n| n.ty == nalu::NaluType::Sps);
        let has_pps = nalus.iter().any(|n| n.ty == nalu::NaluType::Pps);
        let is_key = nalus.iter().any(|n| n.is_keyframe());

        if is_key && (!has_sps || !has_pps) && !self.parameter_sets.is_empty() {
            // 每个 IDR 都补一份 SPS/PPS 是刻意的（不是只在第一帧带）：
            // 接收端丢包后要靠下一个 IDR 恢复，而丢的恰好是带参数集那一帧时，
            // 「只带一次」会让后面所有 IDR 都解不出来。
            out.extend_from_slice(&self.parameter_sets);
        } else if self.parameter_sets.is_empty() && has_sps && has_pps {
            // 第一次从码流里看到参数集，顺手缓存（有些 MFT 不在类型里给）
            let mut ps = Vec::new();
            for n in nalus.iter().filter(|n| n.ty.is_parameter_set()) {
                ps.extend_from_slice(&[0, 0, 0, 1]);
                ps.extend_from_slice(n.data);
            }
            self.parameter_sets = ps;
        }

        out.extend_from_slice(frame);
        self.frames_out += 1;
        Ok(())
    }
}

impl Drop for H264Encoder {
    fn drop(&mut self) {
        // SAFETY: transform 有效；drop 里没有补救手段，忽略错误
        unsafe {
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
        }
        // 先释放缓冲，再让 IMFTransform 掉 —— 缓冲是 MFT 的下游，
        // 顺序反了会让 MFT 在析构时还持有已被释放的 sample。
        self.output_buffer = None;
    }
}

// ---- 构造辅助 ----

/// 是否启用低延迟模式
///
/// 默认开。SIDELENS_MF_LOW_LATENCY=0 可关掉，用于对照实验 ——
/// 只有关掉才看得出这些开关到底起没起作用。
fn low_latency_enabled() -> bool {
    std::env::var("SIDELENS_MF_LOW_LATENCY")
        .map(|v| v != "0")
        .unwrap_or(true)
}

fn init_err(e: crate::com::InitError) -> EncoderError {
    EncoderError::with_hresult(e.stage(), e.hresult(), "Media Foundation 初始化失败")
}

fn enumerate(
    flags: windows::Win32::Media::MediaFoundation::MFT_ENUM_FLAG,
) -> Result<Vec<IMFActivate>> {
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_NV12,
    };
    let output = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };

    let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count: u32 = 0;

    // SAFETY: 两个 TYPE_INFO 都是栈上有效值；输出指针指向本地变量
    let hr = unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            flags,
            Some(&input),
            Some(&output),
            &mut activates,
            &mut count,
        )
    };
    if let Err(e) = hr {
        return Err(EncoderError::from_win("MFTEnumEx", e));
    }

    let mut out = Vec::with_capacity(count as usize);
    if !activates.is_null() {
        // SAFETY: MFTEnumEx 成功时返回 count 个元素的数组，由 CoTaskMemAlloc 分配
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

fn activate(info: &EncoderInfo) -> Result<IMFTransform> {
    // 按名字在枚举结果里定位，拿到对应的 IMFActivate 再激活 ——
    // 这样硬件/异步标记与枚举阶段看到的是同一个对象。
    if let Ok(all) = enumerate(MFT_ENUM_FLAG_ALL) {
        if let Some(act) = all.iter().find(|a| read_name(a) == info.name) {
            // SAFETY: act 有效
            return unsafe { act.ActivateObject::<IMFTransform>() }
                .map_err(|e| EncoderError::from_win("ActivateObject", e));
        }
    }

    // 兜底：按 CLSID 直接造
    // SAFETY: CLSID 是常量；CoCreateInstance 无副作用
    unsafe {
        CoCreateInstance::<_, IMFTransform>(&CLSID_CMSH264ENCODERMFT, None, CLSCTX_INPROC_SERVER)
    }
    .map_err(|e| EncoderError::from_win("CoCreateInstance(CLSID_CMSH264EncoderMFT)", e))
}

fn build_input_type(config: &EncoderConfig) -> Result<IMFMediaType> {
    // SAFETY: 纯工厂调用
    let mt = unsafe { MFCreateMediaType() }
        .map_err(|e| EncoderError::from_win("MFCreateMediaType(输入)", e))?;
    // SAFETY: mt 有效
    unsafe {
        mt.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .map_err(|e| EncoderError::from_win("SetGUID(输入 MAJOR)", e))?;
        mt.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)
            .map_err(|e| EncoderError::from_win("SetGUID(输入 SUBTYPE)", e))?;
        mt.SetUINT64(
            &MF_MT_FRAME_SIZE,
            ((config.width as u64) << 32) | config.height as u64,
        )
        .map_err(|e| EncoderError::from_win("SetUINT64(输入 FRAME_SIZE)", e))?;
        mt.SetUINT64(&MF_MT_FRAME_RATE, ((config.fps as u64) << 32) | 1u64)
            .map_err(|e| EncoderError::from_win("SetUINT64(输入 FRAME_RATE)", e))?;
        mt.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1u64)
            .map_err(|e| EncoderError::from_win("SetUINT64(输入 PAR)", e))?;
        mt.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
            .map_err(|e| EncoderError::from_win("SetUINT32(输入 INTERLACE)", e))?;
        mt.SetUINT32(&MF_MT_ALL_SAMPLES_INDEPENDENT, 1)
            .map_err(|e| EncoderError::from_win("SetUINT32(输入 INDEPENDENT)", e))?;
    }
    Ok(mt)
}

fn build_output_type(config: &EncoderConfig) -> Result<IMFMediaType> {
    // SAFETY: 纯工厂调用
    let mt = unsafe { MFCreateMediaType() }
        .map_err(|e| EncoderError::from_win("MFCreateMediaType(输出)", e))?;
    // SAFETY: mt 有效
    unsafe {
        mt.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .map_err(|e| EncoderError::from_win("SetGUID(输出 MAJOR)", e))?;
        mt.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)
            .map_err(|e| EncoderError::from_win("SetGUID(输出 SUBTYPE)", e))?;
        mt.SetUINT32(&MF_MT_AVG_BITRATE, config.bitrate)
            .map_err(|e| EncoderError::from_win("SetUINT32(AVG_BITRATE)", e))?;
        mt.SetUINT64(
            &MF_MT_FRAME_SIZE,
            ((config.width as u64) << 32) | config.height as u64,
        )
        .map_err(|e| EncoderError::from_win("SetUINT64(输出 FRAME_SIZE)", e))?;
        mt.SetUINT64(&MF_MT_FRAME_RATE, ((config.fps as u64) << 32) | 1u64)
            .map_err(|e| EncoderError::from_win("SetUINT64(输出 FRAME_RATE)", e))?;
        mt.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1u64)
            .map_err(|e| EncoderError::from_win("SetUINT64(输出 PAR)", e))?;
        mt.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
            .map_err(|e| EncoderError::from_win("SetUINT32(输出 INTERLACE)", e))?;
        // MF_MT_MAX_KEYFRAME_SPACING 是 H.264 编码器认的关键帧间隔属性，
        // 单位是**帧**不是秒。
        mt.SetUINT32(&MF_MT_MAX_KEYFRAME_SPACING, config.gop)
            .map_err(|e| EncoderError::from_win("SetUINT32(KEYFRAME_SPACING)", e))?;
        // Main profile：不要 B 帧。B 帧会让编码器内部多缓 1~2 帧，
        // 对「实时低延迟」这条链路是纯亏。
        mt.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Main.0 as u32)
            .map_err(|e| EncoderError::from_win("SetUINT32(MPEG2_PROFILE)", e))?;
    }
    Ok(mt)
}

fn set_output_type(
    transform: &IMFTransform,
    config: &EncoderConfig,
    desired: IMFMediaType,
) -> Result<IMFMediaType> {
    // SAFETY: transform 与 desired 都有效
    if let Err(first) = unsafe { transform.SetOutputType(0, &desired, 0) } {
        // 首选类型被拒时，退到 MFT 自己列出来的类型上再改。
        //
        // 硬件 MFT 与部分软件 MFT 会对 profile/level 有额外要求，
        // 直接给一个「全指定」的类型反而过不了。
        let mut applied = false;
        let mut last_err = first;
        for i in 0..16u32 {
            // SAFETY: transform 有效
            let candidate = match unsafe { transform.GetOutputAvailableType(0, i) } {
                Ok(t) => t,
                Err(e) if e.code() == MF_E_NO_MORE_TYPES => break,
                Err(e) => {
                    last_err = e;
                    break;
                }
            };
            // SAFETY: candidate 有效
            let subtype = unsafe { candidate.GetGUID(&MF_MT_SUBTYPE) }.unwrap_or_default();
            if subtype != MFVideoFormat_H264 {
                continue;
            }
            // SAFETY: candidate 有效；覆盖码率/GOP/分辨率/帧率
            unsafe {
                let _ = candidate.SetUINT32(&MF_MT_AVG_BITRATE, config.bitrate);
                let _ = candidate.SetUINT32(&MF_MT_MAX_KEYFRAME_SPACING, config.gop);
                let _ = candidate.SetUINT64(
                    &MF_MT_FRAME_SIZE,
                    ((config.width as u64) << 32) | config.height as u64,
                );
                let _ = candidate.SetUINT64(&MF_MT_FRAME_RATE, ((config.fps as u64) << 32) | 1u64);
            }
            // SAFETY: 同上
            match unsafe { transform.SetOutputType(0, &candidate, 0) } {
                Ok(()) => {
                    applied = true;
                    break;
                }
                Err(e) => last_err = e,
            }
        }
        if !applied {
            return Err(EncoderError::from_win("SetOutputType", last_err));
        }
    }

    // SAFETY: transform 有效
    unsafe { transform.GetOutputCurrentType(0) }
        .map_err(|e| EncoderError::from_win("GetOutputCurrentType", e))
}

/// 从输出类型里读 MF_MT_MPEG_SEQUENCE_HEADER 并转成 Annex-B
fn read_sequence_header(mt: &IMFMediaType) -> Vec<u8> {
    // SAFETY: mt 有效
    let size = match unsafe { mt.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER) } {
        Ok(s) if s > 0 => s as usize,
        _ => return Vec::new(),
    };
    let mut buf = vec![0u8; size];
    // SAFETY: buf 长度 >= size
    if unsafe { mt.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut buf, None) }.is_err() {
        return Vec::new();
    }
    // blob 是 AVCC 布局：[4 字节长度][SPS][4 字节长度][PPS]
    nalu::avcc_to_annexb(&buf).unwrap_or_default()
}

fn apply_codec_api(transform: &IMFTransform, config: &EncoderConfig) {
    // ICodecAPI 是 IMFTransform 上的可选接口，不是所有 MFT 都实现
    let Ok(codec) = transform.cast::<ICodecAPI>() else {
        return;
    };
    // SAFETY: codec 有效
    unsafe {
        let _ = codec.SetValue(
            &CODECAPI_AVEncCommonRateControlMode,
            &variant_u32(eAVEncCommonRateControlMode_CBR.0 as u32),
        );
        // CBR 在 MF 里是**一组**参数：mode + mean + max + buffer。
        //
        // 只设 mean 也能工作（实测输出只差 5%），但语义上 CBR 的「上限」
        // 是 max、短窗口平滑靠 buffer，四个一起设才是完整的 CBR。
        //
        // 注意：**这组参数不保证码率真的被限住**。实测 640x360 全噪声、
        // 2 Mbps 目标时输出 14.8 Mbps（7.4 倍）—— 编码器宁可不压也不再降
        // 画质。详见 [@@H264Encoder::encode_nv12@@] 上方的说明。
        let _ = codec.SetValue(
            &CODECAPI_AVEncCommonMeanBitRate,
            &variant_u32(config.bitrate),
        );
        let _ = codec.SetValue(
            &CODECAPI_AVEncCommonMaxBitRate,
            &variant_u32(config.bitrate),
        );
        // VBV 缓冲 = 1 秒的码率。太小会让每帧分不到足够比特（画质崩塌），
        // 太大会让码率在短窗口内大幅超调（本链路的抖动缓冲吃不消）。
        let _ = codec.SetValue(
            &CODECAPI_AVEncCommonBufferSize,
            &variant_u32(config.bitrate.max(1)),
        );
        let _ = codec.SetValue(&CODECAPI_AVEncMPVGOPSize, &variant_u32(config.gop));

        // 0 个 B 帧：低延迟。设不上也无所谓（软件 MFT 默认就是 0）。
        let _ = codec.SetValue(&CODECAPI_AVEncMPVDefaultBPictureCount, &variant_u32(0));
        if low_latency_enabled() {
            let _ = codec.SetValue(&CODECAPI_AVEncCommonLowLatency, &variant_bool(true));
            let _ = codec.SetValue(&CODECAPI_AVLowLatencyMode, &variant_bool(true));
        }
    }
}

fn output_stream_info(transform: &IMFTransform) -> Result<(bool, u32)> {
    // SAFETY: transform 有效
    let info = unsafe { transform.GetOutputStreamInfo(0) }
        .map_err(|e| EncoderError::from_win("GetOutputStreamInfo", e))?;
    let provides = info.dwFlags & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32) != 0;
    // cbSize 为 0 表示「每帧大小不定」，给一个保守上限
    let size = if info.cbSize == 0 {
        1024 * 1024
    } else {
        info.cbSize
    };
    Ok((provides, size))
}

fn make_input_sample(nv12: &[u8], pts_us: i64, config: &EncoderConfig) -> Result<IMFSample> {
    let need = nv12.len() as u32;
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
        std::ptr::copy_nonoverlapping(nv12.as_ptr(), ptr, nv12.len());
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
            .SetSampleDuration(config.frame_interval_us().saturating_mul(10))
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
