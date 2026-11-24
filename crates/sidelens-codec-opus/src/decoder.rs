//! Opus 解码器封装
//!
//! # 丢包隐藏（PLC）是必须的，不是可选的
//!
//! 音频不走 NACK（20ms 的空洞补不回来），所以丢包是**常态**。
//! Opus 内建 PLC：传 `None` 作为输入 + `fec = true`，它会用前一帧的
//! 状态外推出一段「听起来像话」的音频，把裂缝糊过去。
//!
//! 实测：给一个空包，解码器补出 1920 samples（20ms 立体声 / 960 单声道）。
//!
//! 不调 PLC 直接跳过丢的帧会怎样：波形在拼接点有台阶 → **爆音**，
//! 人耳对爆音的敏感度远高于对 20ms 外推失真的敏感度。
//!
//! # 三种解码模式
//!
//! | 输入 | `fec` | 语义 |
//! | --- | --- | --- |
//! | `Some` | `false` | 正常解码 |
//! | `None` | `true` | PLC，用前一帧外推 |
//! | `Some` | `true` | **in-band FEC 修复**：用当前帧里的冗余修复上一帧 |
//!
//! 第三种是唯一的「事后修复」通道：它要求编码端开了 in-band FEC，
//! 且当前帧里确实带了 LBRR（SILK / hybrid 模式才有）。
//!
//! # DTX 与空洞
//!
//! DTX 期间发送端不发包（或只发 1 字节的 TOC 帧）。接收端看到的是
//! **PTS 连续、但帧数变少**。这**不会**触发 `PushOutcome::Discontinuity`
//! —— 跳变判定只看 PTS 差值，不看帧数。
//!
//! 真正的代价在别处：DTX 帧解码出来是静音，`on_audio` 仍按 20ms 推进
//! 主时钟，所以音画同步不受影响。见 `tests/tuning.rs` 的实测。

use audiopus::{coder::Decoder, Channels, SampleRate};
use sidelens_codec::audio::OpusConfig;

/// 解码错误
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// 输出缓冲太小
    OutputTooSmall {
        /// 缓冲能装的采样数
        capacity: usize,
        /// 这一帧需要的采样数
        needed: usize,
    },
    /// 码流损坏
    Codec(String),
    /// 配置非法
    BadConfig(String),
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::OutputTooSmall { capacity, needed } => {
                write!(f, "输出缓冲不足：容量 {capacity} 采样，需要 {needed}")
            }
            DecodeError::Codec(e) => write!(f, "Opus 解码失败：{e}"),
            DecodeError::BadConfig(e) => write!(f, "Opus 配置非法：{e}"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// 解码结果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decoded {
    /// 解出的采样数
    ///
    /// # 单位是「每声道」，不是交织总数（实测确认）
    ///
    /// libopus 的 `opus_decode` 返回的是 `frame_size`，即**每个声道**的
    /// 采样数。立体声 20ms @48kHz 返回 960，而 `out` 里实际写入
    /// 960 × 2 = 1920 个 i16。
    ///
    /// 按「返回值 = 交织总数」理解会让立体声每帧只消费一半数据 ——
    /// 表现为右声道静音或音调翻倍，而且单声道测试全绿、完全看不出来。
    pub samples: usize,
    /// 这一帧是不是 PLC 补出来的（原始包丢了）
    pub concealed: bool,
    /// 这一帧是不是用 in-band FEC 修复出来的
    ///
    /// 与 `concealed` 互斥：PLC 是「没有任何数据，纯外推」，
    /// FEC 是「用当前帧里的冗余重建上一帧」，后者音质好得多。
    pub fec_recovered: bool,
}

/// 解码器
pub struct OpusDecoder {
    inner: Decoder,
    config: OpusConfig,
    /// 累计解码帧数
    frames: u64,
    /// PLC 补出的帧数
    concealed: u64,
    /// in-band FEC 修复的帧数
    fec_recovered: u64,
    /// 解码失败次数（坏包）
    failed: u64,
}

impl std::fmt::Debug for OpusDecoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpusDecoder")
            .field("config", &self.config)
            .field("frames", &self.frames)
            .field("concealed", &self.concealed)
            .field("fec_recovered", &self.fec_recovered)
            .field("failed", &self.failed)
            .finish()
    }
}

impl OpusDecoder {
    /// 按配置创建解码器
    pub fn new(config: OpusConfig) -> Result<Self, DecodeError> {
        config.validate().map_err(DecodeError::BadConfig)?;

        let sample_rate = match config.sample_rate {
            8000 => SampleRate::Hz8000,
            12000 => SampleRate::Hz12000,
            16000 => SampleRate::Hz16000,
            24000 => SampleRate::Hz24000,
            48000 => SampleRate::Hz48000,
            other => {
                return Err(DecodeError::BadConfig(format!(
                    "Opus 只支持 8/12/16/24/48 kHz，收到 {other}"
                )))
            }
        };
        let channels = match config.channels {
            1 => Channels::Mono,
            2 => Channels::Stereo,
            other => {
                return Err(DecodeError::BadConfig(format!(
                    "Opus 只支持 1/2 声道，收到 {other}"
                )))
            }
        };

        let inner =
            Decoder::new(sample_rate, channels).map_err(|e| DecodeError::Codec(e.to_string()))?;

        Ok(Self {
            inner,
            config,
            frames: 0,
            concealed: 0,
            fec_recovered: 0,
            failed: 0,
        })
    }

    /// 解码一个包
    ///
    /// `packet` 为 `None` 表示这一帧丢了，走 PLC。
    pub fn decode(
        &mut self,
        packet: Option<&[u8]>,
        out: &mut [i16],
    ) -> Result<Decoded, DecodeError> {
        // # 输出缓冲长度是「请求解出多少采样」，不是「最多能装多少」
        //
        // 这是 audiopus / libopus 的接口语义里最容易踩的一处（实测踩到）：
        // `opus_decode` 的 `frame_size` 参数既是容量也是**期望帧长**。
        // 传一个 5760 采样的缓冲过去，PLC 会老老实实补出 5760 个采样 ——
        // 那是 120ms 的外推，听起来像一声拖长的怪响。
        //
        // 正常解码时 libopus 会按码流里真实的帧长返回（960），所以看不出来；
        // **只有 PLC 会暴露**：没有码流可读时它只能按你给的长度填。
        //
        // 所以这里必须按配置的帧长精确切缓冲，而不是"给多少用多少"。
        let frame_samples = self.config.frame_samples() as usize * self.config.channels as usize;
        if out.len() < frame_samples {
            return Err(DecodeError::OutputTooSmall {
                capacity: out.len(),
                needed: frame_samples,
            });
        }
        // 只用前 frame_samples 个采样：多给的部分必须**截掉**，
        // 否则 PLC 会填满整个缓冲。
        let out = &mut out[..frame_samples];

        // fec 参数语义（audiopus 的文档没说清，实测确认）：
        // - packet = Some, fec = false → 正常解码
        // - packet = None,  fec = true  → PLC，用前一帧外推
        // - packet = Some, fec = true  → 用**下一帧**的信息修复**上一帧**（in-band FEC）
        //
        // 第三种需要编码器端开 in-band FEC + packet_loss_perc > 0。
        let concealed = packet.is_none();
        let result = if concealed {
            self.inner.decode(None::<&[u8]>, out, true)
        } else {
            self.inner.decode(packet, out, false)
        };

        match result {
            Ok(n) => {
                self.frames += 1;
                if concealed {
                    self.concealed += 1;
                }
                Ok(Decoded {
                    samples: n,
                    concealed,
                    fec_recovered: false,
                })
            }
            Err(e) => {
                self.failed += 1;
                Err(DecodeError::Codec(e.to_string()))
            }
        }
    }

    /// 用当前包里的冗余修复**上一帧**
    ///
    /// 这是 in-band FEC 的唯一用法，语义上等价于
    /// `opus_decode(..., fec=1)` 且 `packet` 是**第 N 帧**，解出来的
    /// PCM 是**第 N-1 帧**的内容。
    ///
    /// # 为什么这个 API 单独存在
    ///
    /// 它和 [`decode`](Self::decode) 的差别不只是 `fec` 标志：
    ///
    /// - `decode` 推进解码器状态（第 N 帧解出来就是第 N 帧）
    /// - `recover_previous` 也会推进状态，但产出的 PCM 对应**上一帧的时间段**
    ///
    /// 所以调用方必须在「第 N 帧丢了、第 N+1 帧到了」时：
    ///
    /// 1. 用 `recover_previous(第 N+1 帧)` 补出第 N 帧，**立刻播放**
    /// 2. 再用 `decode(第 N+1 帧)` 解出第 N+1 帧
    ///
    /// 两步都做，状态才对齐。只做第 1 步会丢掉第 N+1 帧；
    /// 只做第 2 步则第 N 帧永久丢失。
    ///
    /// # 事后修复的硬限制
    ///
    /// 第 N 帧的**播放时刻**在收到第 N+1 帧之前就到了。所以：
    ///
    /// - 抖动缓冲深度 ≤ 1 帧 → 第 N 帧已经播出去了，修好也没用
    /// - 抖动缓冲深度 ≥ 2 帧 → 可以在第 N 帧到期前用第 N+1 帧修好
    ///
    /// 实测 20ms 帧 + 40ms 抖动缓冲时，深度是 2 帧，刚好够用。
    /// 10ms 帧 + 40ms 缓冲是 4 帧，余量更足。
    pub fn recover_previous(
        &mut self,
        packet: &[u8],
        out: &mut [i16],
    ) -> Result<Decoded, DecodeError> {
        let frame_samples = self.config.frame_samples() as usize * self.config.channels as usize;
        if out.len() < frame_samples {
            return Err(DecodeError::OutputTooSmall {
                capacity: out.len(),
                needed: frame_samples,
            });
        }
        let out = &mut out[..frame_samples];

        match self.inner.decode(Some(packet), out, true) {
            Ok(n) => {
                self.frames += 1;
                self.fec_recovered += 1;
                Ok(Decoded {
                    samples: n,
                    concealed: false,
                    fec_recovered: true,
                })
            }
            Err(e) => {
                self.failed += 1;
                Err(DecodeError::Codec(e.to_string()))
            }
        }
    }

    /// 累计解码帧数
    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// PLC 补出的帧数
    ///
    /// # 这个值该怎么看
    ///
    /// 它是**丢包率的下界**：PLC 帧数 / 总帧数 ≈ 音频丢包率。
    /// 但它不等于链路丢包率 —— 重组器把分片补齐的帧不算 PLC。
    ///
    /// 如果这个值持续 > 5%，说明音频链路有问题（不是「能听」就行，
    /// 5% 的 PLC 已经能听出金属音）。
    pub fn concealed(&self) -> u64 {
        self.concealed
    }

    /// in-band FEC 修复的帧数
    pub fn fec_recovered(&self) -> u64 {
        self.fec_recovered
    }

    /// 解码失败次数（坏包）
    ///
    /// 与 PLC 不同：PLC 是「包丢了但我知道它丢了」，失败是「包在但解不开」。
    /// 后者说明码流损坏或版本不匹配，是更严重的问题。
    pub fn failed(&self) -> u64 {
        self.failed
    }

    /// 配置
    pub fn config(&self) -> OpusConfig {
        self.config
    }

    /// 复位统计
    ///
    /// # 为什么这里不重建 Opus 解码器
    ///
    /// `audiopus` 的 `Decoder` **没有** `reset_state()` 的公开方法
    /// （它只提供 `new` / `decode` / `decode_float` / `last_packet_duration` /
    /// `pitch` / `gain` / `set_gain` / `size`；`reset_state` 在
    /// `GenericCtl` trait 里，但该 trait 未导出到 crate 根）。
    ///
    /// 而「时间线断裂时清掉解码器状态」这件事仍然是需要的 ——
    /// PLC 用旧的波形外推，会把两段不相干的音频接在一起。
    /// 所以正确做法是**丢弃整个解码器对象重新 new**，而不是调一个不存在的
    /// reset。调用方拿到 `None` 就该自己重建。
    ///
    /// 这里只清统计，因为它不影响音质。
    pub fn reset_stats(&mut self) {
        self.frames = 0;
        self.concealed = 0;
        self.fec_recovered = 0;
        self.failed = 0;
    }
}
