//! 跨平台 PCM 格式描述与校验
//!
//! 这个模块一行平台 API 都不碰：Linux CI 上照样能编译、能跑测试。
//! 它存在的理由是「先校验、再开设备」—— 采样率 0 或声道 0 这种明显错误
//! 应该在进 COM 之前就被拦下，而不是变成一个看不懂的 E_INVALIDARG。
//!
//! 取值范围对齐 WASAPI / WAVEFORMATEX 的实际约束：
//!
//! | 项 | 范围 | 依据 |
//! | --- | --- | --- |
//! | 采样率 | 8000 - 192000 | 共享模式引擎只保证这个区间 |
//! | 声道 | 1 - 8 | WAVEFORMATEX 的 nChannels 上限 8（>2 必须用 EXTENSIBLE） |

/// 允许的最低采样率（Hz）
pub const MIN_SAMPLE_RATE: u32 = 8_000;
/// 允许的最高采样率（Hz）
pub const MAX_SAMPLE_RATE: u32 = 192_000;
/// 允许的最少声道数
pub const MIN_CHANNELS: u16 = 1;
/// 允许的最多声道数
pub const MAX_CHANNELS: u16 = 8;

/// PCM 格式：采样率 + 声道数
///
/// 采样类型固定为 i16（Opus 解码器的原生输出）。设备侧若是 float32，
/// 由本 crate 内部转换，调用方不用关心。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PcmFormat {
    /// 采样率（Hz）
    pub sample_rate: u32,
    /// 声道数（1 = 单声道，2 = 立体声交错）
    pub channels: u16,
}

impl PcmFormat {
    /// 构造一个不做校验的格式
    ///
    /// 只用于常量与测试里的「已知合法」值；用户输入请用 [PcmFormat::checked]。
    pub const fn new(sample_rate: u32, channels: u16) -> Self {
        Self {
            sample_rate,
            channels,
        }
    }

    /// 校验并构造
    pub fn checked(sample_rate: u32, channels: u16) -> Result<Self, AudioFormatError> {
        let fmt = Self::new(sample_rate, channels);
        fmt.validate()?;
        Ok(fmt)
    }

    /// 校验
    pub fn validate(&self) -> Result<(), AudioFormatError> {
        if !(MIN_SAMPLE_RATE..=MAX_SAMPLE_RATE).contains(&self.sample_rate) {
            return Err(AudioFormatError::SampleRateOutOfRange {
                sample_rate: self.sample_rate,
            });
        }
        if !(MIN_CHANNELS..=MAX_CHANNELS).contains(&self.channels) {
            return Err(AudioFormatError::ChannelsOutOfRange {
                channels: self.channels,
            });
        }
        Ok(())
    }

    /// 一帧（所有声道各一个采样）占多少个 i16
    pub fn frame_samples(&self) -> usize {
        self.channels.max(1) as usize
    }

    /// 一帧占多少字节（i16）
    pub fn frame_bytes(&self) -> usize {
        self.frame_samples() * 2
    }

    /// 每秒字节数
    pub fn bytes_per_sec(&self) -> u32 {
        self.sample_rate.saturating_mul(self.frame_bytes() as u32)
    }

    /// 给定交错采样数，算时长（毫秒）
    pub fn duration_ms(&self, samples: usize) -> f64 {
        if self.sample_rate == 0 || self.channels == 0 {
            return 0.0;
        }
        let frames = samples / self.frame_samples();
        frames as f64 * 1000.0 / self.sample_rate as f64
    }

    /// 给定毫秒数，算需要多少帧（向上取整，至少 1 帧）
    pub fn frames_for_ms(&self, ms: f64) -> usize {
        if self.sample_rate == 0 {
            return 0;
        }
        let f = (ms.max(0.0) * self.sample_rate as f64 / 1000.0).ceil();
        if f < 1.0 {
            1
        } else {
            f as usize
        }
    }

    /// 是否是 SideLens 的规范格式（48kHz 单声道，Opus 解码器默认输出）
    pub fn is_canonical(&self) -> bool {
        self.sample_rate == 48_000 && self.channels == 1
    }
}

impl Default for PcmFormat {
    /// Opus 解码器的默认输出格式
    fn default() -> Self {
        Self::new(48_000, 1)
    }
}

impl std::fmt::Display for PcmFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}Hz/{}ch/i16", self.sample_rate, self.channels)
    }
}

/// 格式校验失败
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioFormatError {
    /// 采样率超出 [MIN_SAMPLE_RATE, MAX_SAMPLE_RATE]
    SampleRateOutOfRange {
        /// 请求的采样率
        sample_rate: u32,
    },
    /// 声道数超出 [MIN_CHANNELS, MAX_CHANNELS]
    ChannelsOutOfRange {
        /// 请求的声道数
        channels: u16,
    },
}

impl std::fmt::Display for AudioFormatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AudioFormatError::SampleRateOutOfRange { sample_rate } => write!(
                f,
                "采样率 {sample_rate}Hz 超出允许范围 {MIN_SAMPLE_RATE}-{MAX_SAMPLE_RATE}Hz"
            ),
            AudioFormatError::ChannelsOutOfRange { channels } => write!(
                f,
                "声道数 {channels} 超出允许范围 {MIN_CHANNELS}-{MAX_CHANNELS}"
            ),
        }
    }
}

impl std::error::Error for AudioFormatError {}
