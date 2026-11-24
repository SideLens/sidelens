//! 视频解码器包装（daemon 侧接线层）
//!
//! # 为什么要有这一层，而不是在 udp_server.rs 里直接用 H264Decoder
//!
//! crates/sidelens-codec-mf 的 [@@H264Decoder@@] 是**纯解码器**：
//! 它只回答「这段 Annex-B 解出多少字节」。daemon 需要的是另外几件事，
//! 而且每一件都踩过坑：
//!
//! | daemon 要什么 | 裸 H264Decoder 给什么 | 本模块补什么 |
//! | --- | --- | --- |
//! | 开关（不改 daemon 行为） | 无 | SIDELENS_H264_DECODE 环境变量 |
//! | 复用输出缓冲 | decode() 每次新分配 Vec | decode() 返回借用，缓冲复用 |
//! | 分辨率中途变更 | **不报错**，内部静默重协商 | 检测尺寸跳变并重建 |
//! | 首帧是 P 帧（中途接入） | **不报错**，返回 Ok(()) + 0 字节 | 返回 [@@DecodeError::NeedKeyframe@@] |
//! | 垃圾输入 | **不报错**，返回 Ok(()) + 0 字节 | 返回 [@@DecodeError::Malformed@@] |
//! | 统计 | frames_in / frames_out | decoded / decode_failed / bytes_out / total_us / rebuilds |
//!
//! 后三条「**不报错**」是实测出来的，不是猜的 —— 见下面各节。
//!
//! # 实测：H264Decoder 遇到分辨率变更**不返回错误**
//!
//! 任务书假设「H264Decoder 遇到新 SPS 会报 STREAM_CHANGE 或返回 Err」。
//! 读 crates/sidelens-codec-mf/src/decoder.rs 再跑探针，结论是**两者都不是**：
//!
//! decoder.rs:807 的 drain_outputs 把 MF_E_TRANSFORM_STREAM_CHANGE
//! **自己消化掉了**：
//!
//!   Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
//!       self.stream_change_count += 1;
//!       self.renotiate_output()?;      // 重新 SetOutputType + 读回新尺寸
//!       continue;                       // 继续排空，不向上抛
//!   }
//!
//! renotiate_output()（decoder.rs:821）成功时把 out_width / out_height
//! 换成新值、丢掉旧输出缓冲，然后 decode_into 正常返回 Ok(()).
//!
//! 本机探针（同一个解码器实例，640x360 -> 1280x720）：
//!
//!   B[0] ok  out= 1382400 size (640, 368) -> (1280, 720) sc=2
//!   B[1] ok  out= 1382400 size (1280, 720) -> (1280, 720) sc=2
//!   ...
//!   stream_change delta = 1
//!   final size=(1280, 720) display=(1280, 720)
//!
//! 也就是说：**解码器自己处理好了，一帧都没丢**（1382400 = 1280x720x1.5，
//! 正好一整帧）。decode_into 返回 Ok(()).
//!
//! ## 那为什么这一层还要「重建」？
//!
//! 因为 renegotiate **不是不可能失败**。renotiate_output() 里的
//! set_output_type() 走三级回退（自报类型 -> 覆盖帧尺寸 -> 自造 NV12），
//! 硬件解码器 / 尺寸越界 / 输出格式不匹配时它返回 Err，那个问号
//! 会直接冒泡成 decode_into 的 Err。这时裸解码器已经处于
//! 「输出类型没协商上」的坏状态，后续每一帧都会失败 —— 不重建就是整路黑屏。
//!
//! 所以本模块的策略是**双保险**：
//!
//! 1. 尺寸跳变但解码成功（实测的常态）-> 记一次重建、清缓冲、换新实例。
//!    重建后**重放这一帧**（分辨率切换必然伴随 SPS + IDR，重放能出帧）。
//! 2. 解码返回 Err -> 同样重建 + 重放一次；重放还失败就把错误抛给调用方。
//!
//! 重建成本在毫秒级（H264Decoder::new = CoCreateInstance + 两次 SetType），
//! 而分辨率切换是低频事件（手机旋转、切前后摄像头），代价可以接受。
//!
//! # 实测：首帧是 P 帧时解码器**不报错**
//!
//!   P-only: Ok(())  out=0 bytes  frames_out=0  (没有报错)
//!
//! 中途接入（接收端在 P 帧处起解）时，解码器安静地吞掉这一帧，
//! 返回 Ok(()) 和 0 字节。调用方看到的只是「这一帧没画面」，
//! **没有任何信号**说明「你需要一个关键帧」。
//!
//! 这正是本模块存在的核心理由：在喂给解码器**之前**先看 NALU 类型，
//! 首帧不是 SPS/IDR 就返回 [@@DecodeError::NeedKeyframe@@]，
//! 让 daemon 去走关键帧请求流程，而不是干等。
//!
//! # 实测：垃圾输入解码器也**不报错**
//!
//!   junk: Ok(()) out=0 frames_out=0
//!   junk#0: Ok(()) out=0
//!   junk#1: Ok(()) out=0
//!   junk#2: Ok(()) out=0
//!
//! H264Decoder 对垃圾是**宽容**的（MFT 本身就把不认识的字节当噪声跳过）。
//! 但「整段连起始码都没有」可以确定不是 Annex-B，本模块在入口就拦下并
//! 返回 [@@DecodeError::Malformed@@] —— 比让 daemon 把「解不出来」
//! 和「不是码流」混为一谈要好。
//!
//! # 关于「花屏检测」：检测不到，也不假装能
//!
//! 丢包导致 P 帧缺参考帧时，解码器通常**不报错**（实测丢掉第 3 帧后，
//! 后续 11 帧全部 Ok、frames_out=11）。它拿旧参考帧硬解，输出是花屏
//! 但字节数完全正常。
//!
//! H264Decoder 没有暴露任何「这帧是坏的」信号，MFT 也没有
//! （MFSampleExtension_CleanPoint 只标关键帧，不标损坏）。要检测花屏
//! 只能上 SSIM/PSNR 比对，那需要参考帧，实时链路里没有。
//!
//! 所以本模块的立场是：**解码器说成功就是成功**。丢包的影响由上游
//! （FEC / NACK / 抖动缓冲）负责，解码层不越界。
//!
//! # 用法（udp_server.rs 的 SyncVerdict::Play 分支）
//!
//!   let mut dec = VideoDecoder::from_env();   // None = 未启用，行为不变
//!   if let Some(dec) = dec.as_mut() {
//!       match dec.decode(&frame.payload, frame.pts_us) {
//!           Ok(nv12) => { /* NV12 平面，长度 = w * h * 3 / 2 */ }
//!           Err(DecodeError::NeedKeyframe { .. }) => { /* 请求关键帧 */ }
//!           Err(e) => { /* 计入失败，继续跑 */ }
//!       }
//!   }
//!
//! # 接线时主控要改的两处（本模块自己不碰这两个文件）
//!
//! 1. apps/sidelensd/Cargo.toml 的 [target.'cfg(windows)'.dependencies] 加一行
//!    sidelens-codec-mf = { workspace = true }
//!    （与 apps/sidelenssend/Cargo.toml 的写法一致；不加会 E0433）
//! 2. apps/sidelensd/src/main.rs 加 mod video_decode;
//!
//! # 模块级 allow(dead_code)
//!
//! 接线之前所有 pub 项在 bin crate 里都算死代码（bin crate 没有「外部可见」
//! 这回事，pub 不构成可达性）。接线完成后删掉下面这行即可。

#![allow(dead_code)]

use std::fmt;
use std::time::Instant;

use sidelens_codec::nalu::{split_annexb, NaluType};
use sidelens_codec_mf::{DecoderConfig, EncoderError, H264Decoder};
use tracing::{debug, info, warn};

/// 启用视频解码的环境变量（与 SIDELENS_OPUS 同风格）
///
/// 只有 1 或 true（大小写不敏感）才算启用。未启用时
/// [@@VideoDecoder::from_env@@] 返回 None，daemon 行为与接线前**完全一致**。
pub const ENV_ENABLE: &str = "SIDELENS_H264_DECODE";

/// 首次协商用的候选宽度（真实尺寸由 SPS 决定）
pub const ENV_WIDTH: &str = "SIDELENS_H264_WIDTH";

/// 首次协商用的候选高度
pub const ENV_HEIGHT: &str = "SIDELENS_H264_HEIGHT";

/// 首次协商用的候选帧率
pub const ENV_FPS: &str = "SIDELENS_H264_FPS";

/// 候选宽度默认值
pub const DEFAULT_WIDTH: u32 = 1280;

/// 候选高度默认值
pub const DEFAULT_HEIGHT: u32 = 720;

/// 候选帧率默认值
pub const DEFAULT_FPS: u32 = 30;

/// 解码错误
///
/// # 为什么 NeedKeyframe 要单独一个变体
///
/// 调用方对这两种错误的反应**完全不同**：
///
/// | 错误 | 调用方该做什么 |
/// | --- | --- |
/// | [@@Self::NeedKeyframe@@] | 发关键帧请求（PLI / IDR request），**不要**计成丢包 |
/// | [@@Self::Malformed@@] | 计成协议错误，继续收下一帧 |
/// | [@@Self::Backend@@] / [@@Self::Rebuild@@] | 计成解码失败，连续多次则考虑关掉解码 |
///
/// 混成一个 Err(String) 就只能靠字符串匹配，那是维护灾难。
#[derive(Debug)]
pub enum DecodeError {
    /// 首帧（或重建后的第一帧）不是关键帧，解码器无法起解
    ///
    /// 接收端从中途接入、或者丢包丢掉了整个 IDR 时会出现。
    /// 调用方应请求关键帧，而不是丢弃这一帧就算了。
    NeedKeyframe {
        /// 这一段码流里第一个 NALU 的类型（日志用）
        first_nal: String,
        /// 输入字节数
        len: usize,
    },
    /// 输入根本不是 Annex-B 码流（连起始码都没有）
    Malformed {
        /// 输入字节数
        len: usize,
    },
    /// 底层解码器报错（原始 HRESULT 保留在 [@@EncoderError@@] 里）
    Backend(EncoderError),
    /// 重建解码器失败
    ///
    /// 到这一步说明 Media Foundation 本身出了问题（COM 挂了 / MFT 被卸载），
    /// 调用方通常应该关掉解码并记一条 error 日志。
    Rebuild(EncoderError),
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::NeedKeyframe { first_nal, len } => write!(
                f,
                "需要关键帧才能起解（{len} 字节，首个 NALU = {first_nal}）"
            ),
            DecodeError::Malformed { len } => {
                write!(f, "不是 Annex-B 码流（{len} 字节，找不到任何起始码）")
            }
            DecodeError::Backend(e) => write!(f, "解码失败：{e}"),
            DecodeError::Rebuild(e) => write!(f, "解码器重建失败：{e}"),
        }
    }
}

impl std::error::Error for DecodeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            DecodeError::Backend(e) | DecodeError::Rebuild(e) => Some(e),
            _ => None,
        }
    }
}

/// 一段码流的「可起解性」分类
///
/// 只看 NALU 类型，不解码。依据是 H.264 的接入点规则：
/// 要解出第一帧画面，解码器必须先拿到 **SPS**（参数集）和 **IDR**（关键帧）。
/// 只有 PPS 不够 —— 没有 SPS 就没有分辨率与宏块布局。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FrameKind {
    /// 含 SPS 或 IDR：可以从这里起解
    Keyframe,
    /// 只有 P/B 切片：需要先等关键帧
    NonKeyframe,
    /// 连起始码都没有：不是 Annex-B
    Malformed,
}

impl FrameKind {
    /// 日志用的一行描述
    fn describe(self) -> &'static str {
        match self {
            FrameKind::Keyframe => "SPS/IDR",
            FrameKind::NonKeyframe => "非关键帧切片",
            FrameKind::Malformed => "无起始码",
        }
    }
}

/// 判断一段 Annex-B 能不能作为解码起点
///
/// # 为什么复用 sidelens_codec::nalu::split_annexb 而不是自己扫
///
/// 起始码有 3 字节（00 00 01）和 4 字节（00 00 00 01）两种，还要处理
/// 「NALU 内部恰好出现 00 00 01」这种必须靠 emulation prevention byte
/// 才能区分的情况。split_annexb 已经处理过这些，有 130+ 个测试守着。
/// 这里再写一套只会分叉。
///
/// sidelens-codec 本来就在 sidelensd 的依赖里（音频链路在用），
/// 不需要改 Cargo.toml。
fn classify(annex_b: &[u8]) -> FrameKind {
    let nalus = split_annexb(annex_b);
    if nalus.is_empty() {
        // 没有任何起始码 —— 确定不是 Annex-B。
        // 注意 split_annexb 对「无起始码」返回空而不是「整段一个 NALU」，
        // 这是它实测修正过的行为，见 nalu.rs:195 的注释。
        return FrameKind::Malformed;
    }
    for nalu in &nalus {
        match nalu.ty {
            // 见到 SPS 就先放行：编码器把 SPS/PPS 跟在 IDR 前发，
            // 两者通常在同一帧里。真解不出来会走 Backend 分支。
            NaluType::Sps | NaluType::IdrSlice => return FrameKind::Keyframe,
            _ => {}
        }
    }
    FrameKind::NonKeyframe
}

/// 取第一个 NALU 的类型名（错误信息用）
fn first_nal_type(annex_b: &[u8]) -> String {
    match split_annexb(annex_b).first() {
        Some(n) => n.ty.to_string(),
        None => "无".to_string(),
    }
}

/// 从环境变量读一个 u32，非法时回退默认值并 warn
fn env_u32(key: &str, default: u32) -> u32 {
    match std::env::var(key) {
        Ok(s) => match s.trim().parse::<u32>() {
            Ok(v) => v,
            Err(_) => {
                warn!(key, value = %s, default, "环境变量不是合法整数，使用默认值");
                default
            }
        },
        Err(_) => default,
    }
}

/// 环境变量是否开启了解码
///
/// 判定与 udp_server.rs 里 SIDELENS_OPUS 完全一致：1 或 true。
/// 其他值（包括 0、空串、未设置）一律视为关闭。
fn env_enabled() -> bool {
    match std::env::var(ENV_ENABLE) {
        Ok(v) => v == "1" || v.eq_ignore_ascii_case("true"),
        Err(_) => false,
    }
}

/// 按环境变量组装候选配置
///
/// 这些值只是**协商候选**，真实分辨率由码流里的 SPS 决定，
/// 解码器会在收到第一个 IDR 时重协商（实测 stream_change 从 0 变 1）。
fn config_from_env() -> DecoderConfig {
    let width = env_u32(ENV_WIDTH, DEFAULT_WIDTH);
    let height = env_u32(ENV_HEIGHT, DEFAULT_HEIGHT);
    let fps = env_u32(ENV_FPS, DEFAULT_FPS);
    let cfg = DecoderConfig::new(width, height, fps);
    // 4:2:0 要求偶数；validate 还会挡 0。非法就整套回退默认，
    // 而不是只改坏掉的那一个字段 —— 混搭出来的配置更难解释。
    if cfg.validate().is_ok() {
        return cfg;
    }
    warn!(
        width,
        height, fps, "解码器候选尺寸非法（需偶数且非 0），回退 1280x720@30"
    );
    DecoderConfig::new(DEFAULT_WIDTH, DEFAULT_HEIGHT, DEFAULT_FPS)
}

/// 视频解码器包装
///
/// 一个实例对应一个 IMFTransform，**非线程安全**（MFT 本身不是）。
/// daemon 里它应该跟抖动缓冲在同一个任务上，不要跨线程共享。
///
/// 输出缓冲（nv12）是复用的：decode() 返回的是 self.nv12 的借用，
/// 下一次调用会覆盖它。调用方要么当场消费，要么自己 to_vec()。
#[derive(Debug)]
pub struct VideoDecoder {
    /// 底层解码器（分辨率变更 / 输出类型失效时整个换掉）
    inner: H264Decoder,
    /// 复用的 NV12 输出缓冲
    nv12: Vec<u8>,
    /// 首次协商用的候选配置（重建时以它为基础）
    cfg: DecoderConfig,
    /// 成功解出的**帧数**（一进多出时会一次加多）
    decoded: u64,
    /// 解码失败的**调用次数**
    decode_failed: u64,
    /// 累计输出的字节数
    bytes_out: u64,
    /// 累计解码耗时（微秒），含失败与重建
    total_us: u64,
    /// decode() 真正送进解码器的次数（不含空输入 / 提前返回的 NeedKeyframe）
    calls: u64,
    /// 解码器重建次数
    rebuilds: u64,
    /// 是否已经见过可起解的关键帧
    seen_keyframe: bool,
    /// 上一次**稳定出帧**时的输出尺寸
    ///
    /// 用来区分「首次协商收敛」和「真的换分辨率」：
    /// 解码器初始 output_size() 是 config 尺寸（640x360），
    /// 收到第一个 IDR 后会变成 coded size（640x368）—— 这是**收敛**，不是切换。
    last_size: Option<(u32, u32)>,
}

impl VideoDecoder {
    /// 按环境变量创建；未启用或创建失败时返回 None
    ///
    /// # 为什么失败也只 warn 不 panic
    ///
    /// 解码是**增强能力**，不是 daemon 的必需路径。N 版 Windows /
    /// Server Core 可能没有 H.264 Decoder MFT，Media Foundation 也可能
    /// 被组策略关掉。这些情况下 daemon 应该继续跑（至少还能统计延迟、
    /// 还能做音频），而不是起不来。
    pub fn from_env() -> Option<Self> {
        if !env_enabled() {
            debug!(env = ENV_ENABLE, "视频解码未启用");
            return None;
        }
        let cfg = config_from_env();
        match Self::new(cfg) {
            Ok(dec) => {
                info!(
                    decoder = dec.inner.decoder_name(),
                    format = dec.inner.output_format().as_str(),
                    width = dec.inner.output_size().0,
                    height = dec.inner.output_size().1,
                    fps = cfg.fps,
                    "H.264 解码已启用（SIDELENS_H264_DECODE=1）"
                );
                Some(dec)
            }
            Err(e) => {
                warn!(error = %e, "H.264 解码器创建失败，视频解码保持关闭");
                None
            }
        }
    }

    /// 用给定配置直接创建（测试与显式配置用）
    pub fn new(cfg: DecoderConfig) -> Result<Self, DecodeError> {
        let inner = H264Decoder::new(cfg).map_err(DecodeError::Backend)?;
        let cap = inner.output_frame_len();
        Ok(Self {
            nv12: Vec::with_capacity(cap),
            cfg,
            decoded: 0,
            decode_failed: 0,
            bytes_out: 0,
            total_us: 0,
            calls: 0,
            rebuilds: 0,
            seen_keyframe: false,
            last_size: None,
            inner,
        })
    }

    /// 解码一帧 Annex-B 码流，返回 NV12 平面的借用
    ///
    /// # 返回值的长度
    ///
    /// output_size() 的 w * h * 3 / 2。注意 output_size() 是 **coded size**
    /// （1080p 是 1920x1088），显示时要按 [@@Self::output_display_size@@]
    /// 裁掉底部填充行，否则画面底部会有一条绿边。
    ///
    /// # 空输入
    ///
    /// 空切片是**合法的 no-op**：返回 Ok(空)，不计入任何统计。
    /// 抖动缓冲丢包后可能给出空帧，那不该算解码失败。
    ///
    /// # 会做什么额外判断
    ///
    /// 1. 无起始码 -> [@@DecodeError::Malformed@@]
    /// 2. 首帧（或重建后第一帧）不是 SPS/IDR -> [@@DecodeError::NeedKeyframe@@]
    /// 3. 输出尺寸跳变 / 关键帧解码报错 -> 重建解码器并**重放这一帧**
    ///
    /// 非关键帧报错**不重建**（见 decode 内部的说明），只把 Err 上抛。
    pub fn decode(&mut self, annex_b: &[u8], pts_us: u64) -> Result<&[u8], DecodeError> {
        self.nv12.clear();

        if annex_b.is_empty() {
            return Ok(&self.nv12);
        }

        let kind = classify(annex_b);
        if kind == FrameKind::Malformed {
            self.decode_failed += 1;
            debug!(
                len = annex_b.len(),
                kind = kind.describe(),
                "输入不是 Annex-B，拒绝送进解码器"
            );
            return Err(DecodeError::Malformed { len: annex_b.len() });
        }

        // ── 边界 (c)：首帧必须是关键帧 ──
        if !self.seen_keyframe {
            if kind != FrameKind::Keyframe {
                self.decode_failed += 1;
                let first_nal = first_nal_type(annex_b);
                debug!(
                    first_nal = %first_nal,
                    len = annex_b.len(),
                    "首帧不是关键帧，需要调用方请求关键帧"
                );
                return Err(DecodeError::NeedKeyframe {
                    first_nal,
                    len: annex_b.len(),
                });
            }
            self.seen_keyframe = true;
        }

        self.calls += 1;
        let started = Instant::now();
        let mut outcome = self.decode_once(annex_b, pts_us);

        // ── 边界 (a)：分辨率变更 ──
        //
        // 只在**真的出了帧**时比较尺寸：解码器首次协商收敛
        // （640x360 -> 640x368）不算切换，那是 SPS 的 cropping 在起作用。
        let mut resized = false;
        if outcome.is_ok() && !self.nv12.is_empty() {
            let after = self.inner.output_size();
            match self.last_size {
                None => self.last_size = Some(after),
                Some(prev) if prev != after => {
                    resized = true;
                    debug!(
                        from_w = prev.0,
                        from_h = prev.1,
                        to_w = after.0,
                        to_h = after.1,
                        "检测到分辨率变更"
                    );
                }
                Some(_) => {}
            }
        }

        // # 什么时候才重建：不是「一报错就重建」
        //
        // 丢包导致的 P 帧失败**不该**重建解码器 —— 重建会把 seen_keyframe
        // 打回 false，于是每丢一帧就要等一个新关键帧，故障被放大成雪崩。
        // 那种情况只需要把 Err 上抛，让调用方请求关键帧。
        //
        // 需要重建的是**解码器状态坏了**：输出类型协商失败
        // （MF_E_INVALIDMEDIATYPE / STREAM_CHANGE 的重协商失败）之后，
        // 后续每一帧都会失败，不换实例就是整路黑屏。而关键帧里带着
        // SPS + IDR，用新实例重放它一定能恢复 —— 所以判据是
        // 「出错的那一帧是不是关键帧」。
        let rebuild_needed = resized || (outcome.is_err() && kind == FrameKind::Keyframe);

        if rebuild_needed {
            // 重建 + 重放一次。重放是安全的：分辨率切换与关键帧都带
            // SPS + IDR，新解码器吃这一帧就能出画面。
            if let Err(e) = self.rebuild() {
                self.total_us += started.elapsed().as_micros() as u64;
                self.decode_failed += 1;
                return Err(e);
            }
            self.nv12.clear();
            if kind == FrameKind::Keyframe {
                self.seen_keyframe = true;
            } else {
                // 重建后的第一帧不是关键帧 —— 还是起不了解。
                self.total_us += started.elapsed().as_micros() as u64;
                self.decode_failed += 1;
                return Err(DecodeError::NeedKeyframe {
                    first_nal: first_nal_type(annex_b),
                    len: annex_b.len(),
                });
            }
            outcome = self.decode_once(annex_b, pts_us);
        }

        self.total_us += started.elapsed().as_micros() as u64;

        match outcome {
            Ok(()) => {
                let frames = self.frame_count();
                self.decoded += frames as u64;
                self.bytes_out += self.nv12.len() as u64;
                if frames > 0 {
                    self.last_size = Some(self.inner.output_size());
                }
                Ok(&self.nv12)
            }
            Err(e) => {
                // ── 边界 (b)：缺参考帧 ──
                //
                // 丢包导致的 Err 在这里**原样上抛**，不吞掉：调用方需要知道
                // 「这一帧没解出来」才能决定要不要请求关键帧。
                // 注意：解码器对缺参考帧通常**不报错**（实测丢一帧后后续
                // 全部 Ok，只是画面花屏），那种情况这里收不到 Err。
                self.decode_failed += 1;
                Err(e)
            }
        }
    }

    /// 单次送解码器（不含重建逻辑）
    fn decode_once(&mut self, annex_b: &[u8], pts_us: u64) -> Result<(), DecodeError> {
        self.inner
            .decode_into(annex_b, pts_us as i64, &mut self.nv12)
            .map_err(DecodeError::Backend)
    }

    /// 重建底层解码器
    ///
    /// 新尺寸从 inner.output_size() 取（重协商之后它已经是**新**尺寸），
    /// 拿它当新实例的协商候选能少一轮 STREAM_CHANGE。
    fn rebuild(&mut self) -> Result<(), DecodeError> {
        let (w, h) = self.inner.output_size();
        let mut cfg = self.cfg;
        // 尺寸必须是偶数且非 0，否则 DecoderConfig::validate 会拒。
        // 拿不到可用尺寸就退回原始候选，让解码器自己重协商。
        if w >= 2 && h >= 2 && w % 2 == 0 && h % 2 == 0 {
            cfg.width = w;
            cfg.height = h;
        }
        let new_inner = H264Decoder::new(cfg).map_err(DecodeError::Rebuild)?;
        let cap = new_inner.output_frame_len();

        self.cfg = cfg;
        self.inner = new_inner;
        self.nv12.clear();
        self.nv12.reserve(cap);
        // 新实例没有任何参考帧，必须重新等关键帧。
        self.seen_keyframe = false;
        self.last_size = None;
        self.rebuilds += 1;

        warn!(
            width = cfg.width,
            height = cfg.height,
            rebuilds = self.rebuilds,
            "H.264 解码器已重建（分辨率变更 / 输出类型失效）"
        );
        Ok(())
    }

    /// 强制重建（调用方在连续 NeedKeyframe 时可以手动重置一次）
    pub fn force_rebuild(&mut self) -> Result<(), DecodeError> {
        self.rebuild()
    }

    /// 本次输出里的帧数（一进多出时 > 1）
    fn frame_count(&self) -> usize {
        let frame_len = self.inner.output_frame_len();
        if frame_len == 0 {
            return 0;
        }
        self.nv12.len() / frame_len
    }

    /// 当前协商到的输出尺寸（**coded size**，1080p 是 1920x1088）
    ///
    /// 缓冲长度按这个算，显示时要按 [@@Self::output_display_size@@] 裁。
    pub fn output_size(&self) -> (u32, u32) {
        self.inner.output_size()
    }

    /// 当前协商到的显示尺寸（已扣掉 SPS 里的 cropping，1080p 是 1920x1080）
    ///
    /// 还没收到 SPS 时与 [@@Self::output_size@@] 相同。
    pub fn output_display_size(&self) -> (u32, u32) {
        self.inner.output_display_size()
    }

    /// 一帧输出的字节数（按 coded size）
    pub fn output_frame_len(&self) -> usize {
        self.inner.output_frame_len()
    }

    /// 实际使用的解码器名（日志用）
    pub fn decoder_name(&self) -> &str {
        self.inner.decoder_name()
    }

    /// 成功解出的帧数
    pub fn decoded(&self) -> u64 {
        self.decoded
    }

    /// 解码失败次数
    pub fn decode_failed(&self) -> u64 {
        self.decode_failed
    }

    /// 累计输出字节数
    pub fn bytes_out(&self) -> u64 {
        self.bytes_out
    }

    /// 累计解码耗时（微秒，含失败与重建）
    pub fn total_us(&self) -> u64 {
        self.total_us
    }

    /// 解码器重建次数
    pub fn rebuilds(&self) -> u64 {
        self.rebuilds
    }

    /// decode() 真正送进解码器的次数
    pub fn calls(&self) -> u64 {
        self.calls
    }

    /// 是否已经见过关键帧（重建后会回到 false）
    pub fn has_keyframe(&self) -> bool {
        self.seen_keyframe
    }

    /// 平均每次 decode() 的耗时（微秒）
    ///
    /// 分母是 [@@Self::calls@@]，不是 decoded —— 一进多出时
    /// 用帧数当分母会把单帧耗时算小。
    pub fn avg_decode_us(&self) -> f64 {
        if self.calls == 0 {
            return 0.0;
        }
        self.total_us as f64 / self.calls as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// 环境变量测试必须串行
    ///
    /// cargo test 默认多线程跑同一个二进制里的测试，而
    /// std::env::set_var / remove_var 是**进程级**的。两个测试同时改
    /// SIDELENS_H264_DECODE 会互相踩，表现为「单独跑过、一起跑挂」——
    /// 最难查的一类 flaky。用一把锁把它们串起来。
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        // 前面某个测试 panic 会污染锁，这里必须能恢复
        ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 小尺寸配置，让需要真实解码器的测试跑得快
    fn small_cfg() -> DecoderConfig {
        DecoderConfig::new(320, 240, 30)
    }

    #[test]
    fn from_env_returns_none_when_unset() {
        let _guard = env_guard();
        std::env::remove_var(ENV_ENABLE);
        assert!(
            VideoDecoder::from_env().is_none(),
            "未设置 {ENV_ENABLE} 时必须返回 None（daemon 行为完全不变）"
        );
    }

    #[test]
    fn from_env_returns_none_for_disabled_values() {
        let _guard = env_guard();
        for value in ["0", "", "false", "no", "2"] {
            std::env::set_var(ENV_ENABLE, value);
            assert!(
                VideoDecoder::from_env().is_none(),
                "{ENV_ENABLE}={value:?} 不该启用解码"
            );
        }
        std::env::remove_var(ENV_ENABLE);
    }

    #[test]
    fn env_enabled_accepts_one_and_true() {
        let _guard = env_guard();
        for value in ["1", "true", "TRUE", "True"] {
            std::env::set_var(ENV_ENABLE, value);
            assert!(env_enabled(), "{ENV_ENABLE}={value:?} 应当启用解码");
        }
        std::env::remove_var(ENV_ENABLE);
        assert!(!env_enabled(), "未设置时应当关闭");
    }

    #[test]
    fn config_from_env_falls_back_on_invalid() {
        let _guard = env_guard();
        // 奇数宽高：4:2:0 硬约束，validate 会拒 -> 整套回退默认
        std::env::set_var(ENV_WIDTH, "321");
        std::env::set_var(ENV_HEIGHT, "240");
        let cfg = config_from_env();
        assert_eq!(
            (cfg.width, cfg.height),
            (DEFAULT_WIDTH, DEFAULT_HEIGHT),
            "非法尺寸必须整套回退默认值"
        );

        // 非数字：**按字段**回退（width 用默认 1280，height 保留 240）。
        // 1280x240 本身是合法配置，所以不会再触发整套回退 —— 这个区别
        // 是有意的：解析失败只影响坏掉的那个字段，没必要连带丢弃好值。
        std::env::set_var(ENV_WIDTH, "not-a-number");
        std::env::set_var(ENV_HEIGHT, "240");
        let cfg = config_from_env();
        assert_eq!((cfg.width, cfg.height), (DEFAULT_WIDTH, 240));

        std::env::remove_var(ENV_WIDTH);
        std::env::remove_var(ENV_HEIGHT);
    }

    #[test]
    fn config_from_env_reads_values() {
        let _guard = env_guard();
        std::env::set_var(ENV_WIDTH, "640");
        std::env::set_var(ENV_HEIGHT, "360");
        std::env::set_var(ENV_FPS, "60");
        let cfg = config_from_env();
        assert_eq!((cfg.width, cfg.height, cfg.fps), (640, 360, 60));

        std::env::remove_var(ENV_WIDTH);
        std::env::remove_var(ENV_HEIGHT);
        std::env::remove_var(ENV_FPS);
    }

    #[test]
    fn classify_detects_keyframe_nal_types() {
        // [SPS][PPS][IDR] —— 典型关键帧
        let sps = [0x67u8, 0x42, 0x00, 0x1f];
        let pps = [0x68u8, 0xce, 0x3c, 0x80];
        let idr = [0x65u8, 0x88, 0x84, 0x00];
        let mut key = vec![0u8, 0, 0, 1];
        for n in [&sps[..], &pps[..], &idr[..]] {
            key.extend_from_slice(n);
            key.extend_from_slice(&[0, 0, 0, 1]);
        }
        assert_eq!(classify(&key), FrameKind::Keyframe);

        // 只有 P 帧
        let p = [0x41u8, 0x9a, 0x00, 0x11];
        let mut non_key = vec![0u8, 0, 0, 1];
        non_key.extend_from_slice(&p);
        assert_eq!(classify(&non_key), FrameKind::NonKeyframe);

        // 只有 PPS 也不算关键帧（没有 SPS 就没有分辨率）
        let mut pps_only = vec![0u8, 0, 0, 1];
        pps_only.extend_from_slice(&pps);
        assert_eq!(classify(&pps_only), FrameKind::NonKeyframe);

        // 连起始码都没有
        assert_eq!(classify(&[0xde, 0xad, 0xbe, 0xef]), FrameKind::Malformed);
        assert_eq!(classify(&[]), FrameKind::Malformed);
    }

    #[test]
    fn classify_accepts_three_byte_start_code() {
        // 3 字节起始码（00 00 01）必须同样识别 —— 只认 4 字节会漏掉一半 NALU
        let mut buf = vec![0u8, 0, 1];
        buf.extend_from_slice(&[0x67, 0x42, 0x00, 0x1f]);
        assert_eq!(classify(&buf), FrameKind::Keyframe);
    }

    #[test]
    fn first_nal_type_reports_head() {
        let mut buf = vec![0u8, 0, 0, 1];
        buf.extend_from_slice(&[0x67, 0x42, 0x00, 0x1f]);
        assert_eq!(first_nal_type(&buf), "SPS");
        assert_eq!(first_nal_type(&[0xde, 0xad]), "无");
    }

    #[test]
    fn malformed_input_is_err_not_panic() {
        let Ok(mut dec) = VideoDecoder::new(small_cfg()) else {
            // 没有解码器的机器（N 版 Windows / Server Core）跳过
            return;
        };
        let cases: Vec<Vec<u8>> = vec![
            vec![0xde, 0xad, 0xbe, 0xef, 0x12, 0x34],
            vec![0u8; 256],
            vec![0xff; 7],
        ];
        for (i, garbage) in cases.iter().enumerate() {
            match dec.decode(garbage, i as u64 * 33_333) {
                Err(DecodeError::Malformed { len }) => {
                    assert_eq!(len, garbage.len(), "Malformed 应带上输入长度");
                }
                other => panic!("垃圾输入 #{i} 应返回 Malformed，实际 {other:?}"),
            }
        }
        assert_eq!(dec.decode_failed(), cases.len() as u64);
        assert_eq!(dec.decoded(), 0);
        assert_eq!(dec.calls(), 0, "Malformed 不该计入 decode 调用次数");
    }

    #[test]
    fn empty_input_is_ok_and_uncounted() {
        let Ok(mut dec) = VideoDecoder::new(small_cfg()) else {
            return;
        };
        let out = dec.decode(&[], 0).expect("空输入必须是 Ok");
        assert!(out.is_empty(), "空输入不该产出字节");
        assert_eq!(dec.decoded(), 0);
        assert_eq!(dec.decode_failed(), 0);
        assert_eq!(dec.calls(), 0);
        assert_eq!(dec.bytes_out(), 0);
        assert_eq!(dec.total_us(), 0);
    }

    #[test]
    fn non_keyframe_first_is_need_keyframe() {
        let Ok(mut dec) = VideoDecoder::new(small_cfg()) else {
            return;
        };
        // 首帧只有 P 切片 —— 中途接入的典型形态
        let mut p_only = vec![0u8, 0, 0, 1];
        p_only.extend_from_slice(&[0x41, 0x9a, 0x00, 0x11, 0x22, 0x33]);
        match dec.decode(&p_only, 0) {
            Err(DecodeError::NeedKeyframe { first_nal, len }) => {
                assert_eq!(first_nal, "NON_IDR");
                assert_eq!(len, p_only.len());
            }
            other => panic!("首帧是 P 帧时应返回 NeedKeyframe，实际 {other:?}"),
        }
        assert_eq!(dec.decode_failed(), 1);
        assert_eq!(dec.calls(), 0, "NeedKeyframe 不该计入 decode 调用次数");
        assert!(!dec.has_keyframe(), "还没见过关键帧");
    }

    #[test]
    fn stats_start_at_zero() {
        let Ok(dec) = VideoDecoder::new(small_cfg()) else {
            return;
        };
        assert_eq!(dec.decoded(), 0);
        assert_eq!(dec.decode_failed(), 0);
        assert_eq!(dec.bytes_out(), 0);
        assert_eq!(dec.total_us(), 0);
        assert_eq!(dec.rebuilds(), 0);
        assert_eq!(dec.calls(), 0);
        assert_eq!(dec.avg_decode_us(), 0.0, "没调用过时平均值必须是 0");
        assert!(!dec.has_keyframe());
        // 初始尺寸来自候选配置（还没见过 SPS）
        assert_eq!(dec.output_size(), (320, 240));
        assert_eq!(dec.output_display_size(), (320, 240));
        assert_eq!(dec.output_frame_len(), 320 * 240 * 3 / 2);
    }

    #[test]
    fn decoder_name_is_non_empty() {
        let Ok(dec) = VideoDecoder::new(small_cfg()) else {
            return;
        };
        assert!(!dec.decoder_name().is_empty(), "解码器名不该是空串");
    }

    #[test]
    fn error_display_mentions_what_to_do() {
        let e = DecodeError::NeedKeyframe {
            first_nal: "NON_IDR".to_string(),
            len: 12,
        };
        let text = e.to_string();
        assert!(text.contains("关键帧"), "NeedKeyframe 的文案要点明关键帧");
        assert!(text.contains("NON_IDR"), "要点明首个 NALU 类型");

        let e = DecodeError::Malformed { len: 4 };
        assert!(e.to_string().contains("Annex-B"));
    }
}
