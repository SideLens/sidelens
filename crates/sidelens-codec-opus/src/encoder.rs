//! Opus 编码器封装
//!
//! 把 `audiopus` 的裸 API 包成「喂 PCM 拿一帧字节」的形状，并把
//! 三个容易踩的地方显式化：
//!
//! 1. **lookahead 必须暴露出来**（见 crate 文档）
//! 2. **输出缓冲必须由调用方复用**，不能每帧分配
//! 3. **帧长必须校验**，不能信任调用方传进来的值
//!
//! # DTX / in-band FEC 为什么走裸 ctl
//!
//! `audiopus` 0.2.0 的 `Encoder` 有 `set_inband_fec` / `set_packet_loss_perc`，
//! 但**没有** `set_dtx`。不过它把底层 ctl 入口暴露成了公开方法：
//!
//! ```text
//! pub fn set_encoder_ctl_request(&mut self, request: i32, value: i32) -> Result<()>
//! ```
//!
//! 加上 `pub use audiopus_sys as ffi;`，可以直接发 `OPUS_SET_DTX_REQUEST`（4016）。
//! 这样既不用加依赖，也不用 unsafe —— `opus_encoder_ctl` 的变参由 audiopus 转发。
//!
//! 每次 set 之后都**回读**一次确认生效：ctl 返回 `Ok` 只代表 libopus 没报错，
//! 不代表值被采纳（例如某些构建里 DTX 被编译掉）。

use audiopus::{coder::Encoder, ffi, Application, Bitrate, Channels, SampleRate};
use sidelens_codec::audio::OpusConfig;

use crate::packet;

/// 编码错误
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EncodeError {
    /// 输入采样数与配置的帧长不符
    ///
    /// Opus 要求每次编码恰好一整帧。给多了会报 `BufferTooSmall`（含义完全不直观），
    /// 给少了会编码出半个帧 —— 所以这里自己先校验。
    WrongSampleCount {
        /// 配置要求的采样数
        expected: usize,
        /// 实际传入的
        got: usize,
    },
    /// 输出缓冲太小，装不下这一帧
    OutputTooSmall {
        /// 缓冲大小
        capacity: usize,
        /// 编码器估计需要的字节数
        needed: usize,
    },
    /// 编码器内部错误
    Codec(String),
    /// 配置非法
    BadConfig(String),
}

impl std::fmt::Display for EncodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EncodeError::WrongSampleCount { expected, got } => {
                write!(f, "采样数不对：期望 {expected}，实际 {got}")
            }
            EncodeError::OutputTooSmall { capacity, needed } => {
                write!(f, "输出缓冲不足：容量 {capacity}，需要 {needed}")
            }
            EncodeError::Codec(e) => write!(f, "Opus 编码失败：{e}"),
            EncodeError::BadConfig(e) => write!(f, "Opus 配置非法：{e}"),
        }
    }
}

impl std::error::Error for EncodeError {}

/// 一帧编码结果
///
/// `bytes` 是写进输出缓冲的字节数；`dtx` 表示这一帧被 DTX 抑制
/// （包里没有实际的语音数据）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncodedFrame {
    /// 写入输出缓冲的字节数
    pub bytes: usize,
    /// 是否是 DTX 抑制帧
    ///
    /// 判据是 [`packet::is_dtx`]（数据字节为 0），**不是**长度阈值。
    /// 实测 60ms 帧下 DTX 包是 2 字节，按 `len <= channels` 判会全漏。
    pub dtx: bool,
    /// 包里装了几个内部帧
    ///
    /// 20ms 帧恒为 1；40ms 为 2；60ms 为 3。见 [`packet::frame_count`]。
    pub frames: usize,
}

/// 编码器
pub struct OpusEncoder {
    inner: Encoder,
    config: OpusConfig,
    /// 编码器内部延迟（采样数）
    lookahead: usize,
    /// 累计编码帧数与字节数
    frames: u64,
    bytes: u64,
    /// DTX 开关（回读确认过的值）
    dtx: bool,
    /// in-band FEC 开关（回读确认过的值）
    inband_fec: bool,
    /// 期望丢包率（写进码流，决定冗余量）
    packet_loss_perc: u8,
    /// 被 DTX 抑制的帧数
    dtx_frames: u64,
    /// DTX 帧占用的字节数
    dtx_bytes: u64,
}

impl std::fmt::Debug for OpusEncoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpusEncoder")
            .field("config", &self.config)
            .field("lookahead", &self.lookahead)
            .field("frames", &self.frames)
            .field("bytes", &self.bytes)
            .field("dtx", &self.dtx)
            .field("inband_fec", &self.inband_fec)
            .field("packet_loss_perc", &self.packet_loss_perc)
            .field("dtx_frames", &self.dtx_frames)
            .field("dtx_bytes", &self.dtx_bytes)
            .finish()
    }
}

impl OpusEncoder {
    /// 按 `OpusConfig` 创建编码器
    ///
    /// `bitrate_bps` 是目标码率。Opus 不保证精确命中 —— 实测 32kbps 目标
    /// 得到 34.8kbps（+8.8%），这是它的 VBR 行为，不是 bug。
    ///
    /// 默认 **DTX 关、in-band FEC 关**，与历史行为一致。
    pub fn new(config: OpusConfig, bitrate_bps: i32) -> Result<Self, EncodeError> {
        config.validate().map_err(EncodeError::BadConfig)?;

        let sample_rate = match config.sample_rate {
            8000 => SampleRate::Hz8000,
            12000 => SampleRate::Hz12000,
            16000 => SampleRate::Hz16000,
            24000 => SampleRate::Hz24000,
            48000 => SampleRate::Hz48000,
            other => {
                return Err(EncodeError::BadConfig(format!(
                    "Opus 只支持 8/12/16/24/48 kHz，收到 {other}"
                )))
            }
        };
        let channels = match config.channels {
            1 => Channels::Mono,
            2 => Channels::Stereo,
            other => {
                return Err(EncodeError::BadConfig(format!(
                    "Opus 只支持 1/2 声道，收到 {other}"
                )))
            }
        };

        // Application::Voip 而不是 Audio：
        //
        // - Audio：为音乐优化，保留更多高频细节，码率效率略低
        // - Voip：为语音优化，更强的噪声抑制，对丢包更鲁棒
        //
        // SideLens 的主场景是「摄像头 + 麦克风」的通话/直播，
        // 语音清晰度比音乐保真重要。
        let mut inner = Encoder::new(sample_rate, channels, Application::Voip)
            .map_err(|e| EncodeError::Codec(e.to_string()))?;
        inner
            .set_bitrate(Bitrate::BitsPerSecond(bitrate_bps))
            .map_err(|e| EncodeError::Codec(e.to_string()))?;

        let lookahead = inner
            .lookahead()
            .map_err(|e| EncodeError::Codec(e.to_string()))? as usize;

        Ok(Self {
            inner,
            config,
            lookahead,
            frames: 0,
            bytes: 0,
            dtx: false,
            inband_fec: false,
            packet_loss_perc: 0,
            dtx_frames: 0,
            dtx_bytes: 0,
        })
    }

    // ─────────────────────── DTX ───────────────────────

    /// 开关 DTX（不连续传输）
    ///
    /// 开启后，libopus 的 VAD 判定为「无语音」的帧会被压成只有 TOC 的
    /// 1 字节帧（单声道），甚至 0 字节。
    ///
    /// # 实测行为（48kHz 单声道 20ms 32kbps Voip）
    ///
    /// - 前 **5 帧**仍正常编码（`NB_SPEECH_FRAMES_BEFORE_DTX`）
    /// - 之后每帧返回 **1 字节**
    /// - 连续 **20 帧** DTX 之后强制发一帧真实的（`MAX_CONSECUTIVE_DTX`），
    ///   所以静音期不是「零带宽」，而是「每 ~21 帧一帧真数据」
    ///
    /// 这个「周期性刷新」是协议要求：解码器不能无限期靠 PLC 外推。
    pub fn set_dtx(&mut self, enable: bool) -> Result<(), EncodeError> {
        self.inner
            .set_encoder_ctl_request(ffi::OPUS_SET_DTX_REQUEST, i32::from(enable))
            .map_err(|e| EncodeError::Codec(e.to_string()))?;
        // 回读：ctl 返回 Ok 不等于值被采纳
        let actual = self
            .inner
            .encoder_ctl_request(ffi::OPUS_GET_DTX_REQUEST)
            .map_err(|e| EncodeError::Codec(e.to_string()))?;
        self.dtx = actual == 1;
        Ok(())
    }

    /// 当前 DTX 状态（回读确认过的值）
    pub fn dtx(&self) -> bool {
        self.dtx
    }

    /// 被 DTX 抑制的帧数
    pub fn dtx_frames(&self) -> u64 {
        self.dtx_frames
    }

    /// DTX 帧占用的总字节数
    pub fn dtx_bytes(&self) -> u64 {
        self.dtx_bytes
    }

    /// 这一帧是不是 DTX
    ///
    /// # 为什么不用「长度 ≤ 声道数」判
    ///
    /// 实测（48kHz 单声道，DTX 开）：
    ///
    /// | 帧长 | DTX 包字节 | TOC |
    /// | --- | --- | --- |
    /// | 20ms | 1 | `0x78` |
    /// | 40ms | 1 | `0x79` |
    /// | 60ms | **2** | `0x7B` |
    ///
    /// 60ms 的 DTX 包是 2 字节（TOC + 帧数），按长度阈值判会**全漏**。
    /// 正确判据是「包里的数据字节为 0」，见 [`packet::is_dtx`]。
    fn classify(&self, buf: &[u8]) -> (bool, usize) {
        let dtx = self.dtx && packet::is_dtx(buf);
        let frames = packet::frame_count(buf).unwrap_or(1);
        (dtx, frames)
    }

    // ──────────────────── in-band FEC ────────────────────

    /// 开关 in-band FEC
    ///
    /// 开启后编码器会把**上一帧**的低码率副本（LBRR）塞进当前帧，
    /// 解码端在收到第 N 帧时可以据此修复第 N-1 帧。
    ///
    /// # 代价与限制（实测）
    ///
    /// - 码流变大（冗余就是比特），增量取决于 `packet_loss_perc`
    /// - **只有 SILK / hybrid 模式才有 LBRR**。CELT-only（高码率音乐）
    ///   模式下这个开关不起作用，实测码流字节数不变。
    pub fn set_inband_fec(&mut self, enable: bool) -> Result<(), EncodeError> {
        self.inner
            .set_inband_fec(enable)
            .map_err(|e| EncodeError::Codec(e.to_string()))?;
        let actual = self
            .inner
            .inband_fec()
            .map_err(|e| EncodeError::Codec(e.to_string()))?;
        self.inband_fec = actual;
        Ok(())
    }

    /// 当前 in-band FEC 状态
    pub fn inband_fec(&self) -> bool {
        self.inband_fec
    }

    /// 设置期望丢包率（0-100）
    ///
    /// 这个值**不改变网络行为**，只告诉编码器「你预计会丢多少」，
    /// 它会据此决定：塞多少 FEC 冗余、用多强的抗丢包编码。
    ///
    /// 0 时 in-band FEC 即使打开也不会产生冗余。
    pub fn set_packet_loss_perc(&mut self, percent: u8) -> Result<(), EncodeError> {
        if percent > 100 {
            return Err(EncodeError::BadConfig(format!(
                "丢包率必须在 0-100，收到 {percent}"
            )));
        }
        self.inner
            .set_packet_loss_perc(percent)
            .map_err(|e| EncodeError::Codec(e.to_string()))?;
        let actual = self
            .inner
            .packet_loss_perc()
            .map_err(|e| EncodeError::Codec(e.to_string()))?;
        self.packet_loss_perc = actual;
        Ok(())
    }

    /// 当前期望丢包率
    pub fn packet_loss_perc(&self) -> u8 {
        self.packet_loss_perc
    }

    // ─────────────────────── 编码 ───────────────────────

    /// 编码器内部延迟（采样数）
    ///
    /// 48kHz 下通常是 **312 采样 = 6.50ms**。
    ///
    /// # 怎么用它
    ///
    /// 音频 PTS 是「这段声音本该在什么时候播」，而编码器把输出推迟了
    /// `lookahead` 个采样。两种补偿方式：
    ///
    /// - **提前 PTS**：`pts = 采集时刻 - lookahead_us`。简单，但会让 PTS
    ///   出现负值（起始阶段），需要钳到 0。
    /// - **推后视频**：把视频的判定基准整体后移 `lookahead_us`。
    ///   更安全，但会影响 AvSync 的所有判据。
    ///
    /// 当前选择**不补偿**（6.5ms 远小于 40ms 的可察觉阈值），
    /// 但把值暴露出来，等延迟预算收紧时再处理。
    pub fn lookahead_samples(&self) -> usize {
        self.lookahead
    }

    /// lookahead 对应的微秒数
    pub fn lookahead_us(&self) -> u64 {
        (self.lookahead as u64 * 1_000_000) / self.config.sample_rate as u64
    }

    /// 编码一帧 PCM
    ///
    /// `pcm` 必须是交织的 16bit 小端采样，长度恰好等于
    /// `config.frame_samples() * config.channels`。
    ///
    /// 返回写入 `out` 的字节数。**不返回切片** —— 让调用方持有缓冲，
    /// 避免每帧分配（20ms 一帧，一小时 18 万帧）。
    ///
    /// DTX 抑制的帧返回 1（单声道），调用方可以据此决定是否发包。
    pub fn encode(&mut self, pcm: &[i16], out: &mut [u8]) -> Result<usize, EncodeError> {
        self.encode_frame(pcm, out).map(|e| e.bytes)
    }

    /// 编码一帧，并返回「这帧是不是 DTX」
    ///
    /// 与 [`encode`](Self::encode) 的唯一区别是带上了 DTX 标记。
    /// 需要区分「真帧」和「DTX 帧」时用这个 —— 例如决定是否发包。
    pub fn encode_frame(
        &mut self,
        pcm: &[i16],
        out: &mut [u8],
    ) -> Result<EncodedFrame, EncodeError> {
        let expected = self.config.frame_samples() as usize * self.config.channels as usize;
        if pcm.len() != expected {
            return Err(EncodeError::WrongSampleCount {
                expected,
                got: pcm.len(),
            });
        }

        // 上限：Opus 单帧最大 1275 字节 × 声道数（规范硬限制）。
        // 超出时 audiopus 会返回 BufferTooSmall，那个错误信息不直观，
        // 这里先自己判一次。
        let max_frame = 1275 * self.config.channels as usize;
        if out.len() < max_frame {
            return Err(EncodeError::OutputTooSmall {
                capacity: out.len(),
                needed: max_frame,
            });
        }

        let n = self
            .inner
            .encode(pcm, out)
            .map_err(|e| EncodeError::Codec(e.to_string()))?;

        let (dtx, frames) = self.classify(&out[..n]);
        self.frames += 1;
        self.bytes += n as u64;
        if dtx {
            self.dtx_frames += 1;
            self.dtx_bytes += n as u64;
        }
        Ok(EncodedFrame {
            bytes: n,
            dtx,
            frames,
        })
    }

    /// 累计编码帧数
    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// 累计输出字节数
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// 实测平均每帧字节数
    ///
    /// # DTX 会把均值拉低
    ///
    /// DTX 帧只有 1 字节，混进均值后 `avg_frame_bytes` 会远低于真实语音帧。
    /// 做带宽预算时要用 [`wire_bytes`](Self::wire_bytes)（发出去的字节），
    /// 而不是「帧数 × 均值」。
    pub fn avg_frame_bytes(&self) -> f64 {
        if self.frames == 0 {
            0.0
        } else {
            self.bytes as f64 / self.frames as f64
        }
    }

    /// 非 DTX 帧的平均字节数
    ///
    /// 这才是「语音帧多大」，不受静音期拉低。
    ///
    /// 注意 DTX 包的大小**不是** `channels`：20/40ms 是 1 字节、60ms 是 2 字节。
    /// 这里改用 `dtx_bytes` 精确扣减。
    pub fn avg_speech_frame_bytes(&self) -> f64 {
        let speech = self.frames.saturating_sub(self.dtx_frames);
        if speech == 0 {
            0.0
        } else {
            self.bytes.saturating_sub(self.dtx_bytes) as f64 / speech as f64
        }
    }

    /// 实测码率（bps），按帧时长反算
    ///
    /// **含 DTX 帧**：这等于「每帧都发」时的码率。
    pub fn measured_bitrate_bps(&self) -> f64 {
        if self.frames == 0 {
            return 0.0;
        }
        self.avg_frame_bytes() * 8.0 * self.config.frames_per_second()
    }

    /// 配置
    pub fn config(&self) -> OpusConfig {
        self.config
    }
}
