//! 跨平台 PCM 格式描述、校验与设备格式转换
//!
//! 这个模块一行平台 API 都不碰：Linux / macOS 上照样能编译、能跑测试。
//!
//! # 为什么和 sidelens-audio-out 各有一份 PcmFormat
//!
//! 两个 crate 的 `PcmFormat` 语义完全一致（`sample_rate` + `channels`），
//! 但**不做跨 crate 依赖**：采集与播放是两条互不相干的链路，
//! 让 capture 依赖 playback 会在依赖图上制造一个假关系，
//! 将来任一方要单独发布/裁剪都会卡住。
//! 代价是 40 行重复代码，收益是依赖方向干净。
//!
//! # 为什么转换逻辑必须放在这里，而不是 wasapi.rs 里
//!
//! 「f32 → i16 怎么 clamp」「多声道怎么降混」是最容易写错、
//! 又最容易验证的部分。放进 `cfg(windows)` 里就等于放弃了
//! 在 Linux CI 上跑它们的机会 —— 而这类错误（溢出爆音、声道相位抵消）
//! 恰恰是只在真机上听得出、测不出来的那类 bug。
//!
//! 所以：**纯算术在这里，指针搬运在 wasapi.rs**。

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
/// 采样类型固定为 i16 —— 这是 Opus 编码器的输入格式，
/// 也是 [`AudioCapture::read`](crate::AudioCapture::read) 唯一会吐出来的类型。
/// 设备侧如果是 float32，由本 crate 内部转换，调用方不用关心。
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
    /// 只用于常量与测试里的「已知合法」值；用户输入请用 [`PcmFormat::checked`]。
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

    /// 是否是 SideLens 的规范格式（48kHz 单声道，Opus 编码器默认输入）
    pub fn is_canonical(&self) -> bool {
        self.sample_rate == 48_000 && self.channels == 1
    }
}

impl Default for PcmFormat {
    /// Opus 编码器的默认输入格式
    fn default() -> Self {
        Self::new(48_000, 1)
    }
}

impl std::fmt::Display for PcmFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}Hz/{}ch/i16", self.sample_rate, self.channels)
    }
}

/// 设备混音格式
///
/// WASAPI 共享模式给的是一份 `WAVEFORMATEX`，采样率、声道数、位深、
/// 是不是浮点都**由驱动决定**，跟我们要的 48kHz/1ch/i16 没有关系。
/// 本机实测是 48000Hz / 2ch / 32bit float —— 也就是说采集路径上
/// **必然**有一次「双声道浮点 → 单声道 i16」的转换。
///
/// 单独一个结构体而不是塞进 `PcmFormat` 的理由：`PcmFormat` 是我们
/// **请求**的格式，这个是我们**实际拿到**的格式，两者语义相反，
/// 混在一个类型里迟早会有人把 `device_format()` 的返回值当成 `format()` 用。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceFormat {
    /// 设备采样率（Hz）
    pub sample_rate: u32,
    /// 设备声道数
    pub channels: u16,
    /// 每个采样的位数（16 / 32）
    pub bits_per_sample: u16,
    /// 采样是 IEEE float 还是整数
    pub is_float: bool,
}

impl DeviceFormat {
    /// 构造
    pub const fn new(
        sample_rate: u32,
        channels: u16,
        bits_per_sample: u16,
        is_float: bool,
    ) -> Self {
        Self {
            sample_rate,
            channels,
            bits_per_sample,
            is_float,
        }
    }

    /// 采样在内存里的容器类型
    ///
    /// 只认 32bit float 与 16bit int 两种：这是共享模式混音格式的
    /// 全部现实情况（24bit 会被驱动提升成 32bit 容器）。
    /// 其它组合明确报错，而不是猜一个 —— 猜错的结果是把浮点位模式
    /// 当整数解释，听起来是满幅白噪声。
    pub fn sample_kind(&self) -> Result<SampleKind, AudioFormatError> {
        match (self.is_float, self.bits_per_sample) {
            (true, 32) => Ok(SampleKind::F32),
            (false, 16) => Ok(SampleKind::I16),
            (is_float, bits_per_sample) => Err(AudioFormatError::UnsupportedBitDepth {
                bits_per_sample,
                is_float,
            }),
        }
    }

    /// 一帧占多少字节
    pub fn frame_bytes(&self) -> usize {
        self.channels.max(1) as usize * (self.bits_per_sample as usize / 8).max(1)
    }

    /// 多少帧对应多少毫秒
    pub fn duration_ms(&self, frames: u32) -> f64 {
        if self.sample_rate == 0 {
            return 0.0;
        }
        frames as f64 * 1000.0 / self.sample_rate as f64
    }
}

impl std::fmt::Display for DeviceFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}Hz/{}ch/{}{}",
            self.sample_rate,
            self.channels,
            self.bits_per_sample,
            if self.is_float { "f" } else { "i" }
        )
    }
}

/// 采样在设备缓冲区里的容器类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleKind {
    /// 32 位浮点（共享模式混音格式的常态）
    F32,
    /// 16 位有符号整数
    I16,
}

/// 格式校验失败
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioFormatError {
    /// 采样率超出 [`MIN_SAMPLE_RATE`, `MAX_SAMPLE_RATE`]
    SampleRateOutOfRange {
        /// 请求的采样率
        sample_rate: u32,
    },
    /// 声道数超出 [`MIN_CHANNELS`, `MAX_CHANNELS`]
    ChannelsOutOfRange {
        /// 请求的声道数
        channels: u16,
    },
    /// 设备位深不受支持
    UnsupportedBitDepth {
        /// 设备的位数
        bits_per_sample: u16,
        /// 设备是不是浮点
        is_float: bool,
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
            AudioFormatError::UnsupportedBitDepth {
                bits_per_sample,
                is_float,
            } => write!(
                f,
                "不支持的设备位深：{bits_per_sample}bit{}（只支持 32bit float 与 16bit int）",
                if *is_float { " float" } else { " int" }
            ),
        }
    }
}

impl std::error::Error for AudioFormatError {}

/// f32（[-1, 1]）转 i16，**饱和**而不是回绕
///
/// # 为什么必须 clamp
///
/// 浮点音频的合法范围是 [-1, 1]，但真实设备送来的数据**会越界**：
/// 驱动侧的软限幅、混音器叠加、Dolby 后处理都可能产生 1.02 这样的值。
/// `as i16` 在 Rust 里是饱和转换（不是 C 的未定义行为），但先乘 32768
/// 再饱和的话，1.02 会被截成 32767 —— 那是满幅方波，听感是刺耳的爆音。
/// 所以先 clamp 到 [-1, 1] 再缩放，让越界样本被**压平**成满幅，
/// 而不是继续溢出到反向满幅。
///
/// # 为什么用 32768 而不是 32767
///
/// i16 的范围是 [-32768, 32767]，不对称。用 32768 做缩放能让
/// -1.0 精确映射到 -32768，代价是 +1.0 会算出 32768 需要 clamp 回 32767。
/// 用 32767 则 -1.0 只到 -32767，损失 1 个 LSB 的负向动态范围。
/// 两者都只差 0.003dB，选 32768 是为了「负半周不削顶」。
///
/// # NaN
///
/// `f32::clamp` 对 NaN 返回 NaN，`NaN as i16` 在 Rust 里是 0，
/// 不会 UB —— 但那依赖语言细节，这里显式挡掉，行为一眼可见。
pub fn f32_to_i16(s: f32) -> i16 {
    if !s.is_finite() {
        return 0;
    }
    let scaled = (s * 32_768.0).round();
    scaled.clamp(-32_768.0, 32_767.0) as i16
}

/// i16 转 f32（[-1, 1]）
///
/// 与 [`f32_to_i16`] 对称：除以 32768 而不是 32767，保证
/// `i16::MIN` 精确映射到 -1.0，且往返转换不会因四舍五入抖动。
pub fn i16_to_f32(s: i16) -> f32 {
    s as f32 / 32_768.0
}

/// 多声道 f32 帧降混成单声道
///
/// # 为什么是「平均」而不是「取第一个声道」
///
/// 取第一个声道在**立体声等功率声像**下会直接丢一半内容：
/// 一段硬右声像的吉他（L=0, R=1）取左声道就是完全静音。
/// 用户放音乐时听起来是「只有伴奏没有主唱」，而 Wave 文件本身是好的，
/// 排查成本极高。
///
/// 平均 `(L+R)/2` 对**同相**内容（人声、底鼓，通常居中）增益为 1，
/// 对**反相**内容会互相抵消 —— 这是立体声→单声道无法回避的物理事实，
/// 任何降混矩阵都一样。真正的解法是 Dolby 的 `L + 0.707R` 之类矩阵，
/// 但那要按内容类型切换，对「共享系统声音」这个场景是过度设计。
///
/// 空帧返回 0（调用方不该传空帧，但这里不能 panic）。
pub fn downmix_f32_to_mono(frame: &[f32]) -> f32 {
    if frame.is_empty() {
        return 0.0;
    }
    let sum: f32 = frame.iter().sum();
    sum / frame.len() as f32
}

/// 多声道 i16 帧降混成单声道
///
/// 与 [`downmix_f32_to_mono`] 同一套理由。累加用 i32：
/// 8 声道 × 32767 远超 i16 范围，用 i16 累加会溢出成噪音。
pub fn downmix_i16_to_mono(frame: &[i16]) -> i16 {
    if frame.is_empty() {
        return 0;
    }
    let sum: i32 = frame.iter().map(|&s| s as i32).sum();
    (sum / frame.len() as i32) as i16
}

/// 设备帧 → 请求格式的声道映射方案
///
/// 抽成枚举而不是一串 if：这是纯决策，能单测；实际搬运数据的
/// 代码只负责按方案执行，不再判断。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MixPlan {
    /// 输出单声道：对设备帧取平均
    DownmixToMono,
    /// 输出 n 个声道，每个都填设备帧的平均值
    ///
    /// 两种来源：设备本来就是单声道（1→2 复制），
    /// 或者设备声道数少于请求数（2→6 这种，宁可给平均值也不要静音）。
    FanoutFromAverage(usize),
    /// 1:1 取设备帧的前 n 个声道
    ///
    /// 注意这是**声道直通**，不是正确的 5.1 → 立体声降混矩阵：
    /// 环绕声的中置声道（人声）会整个丢掉。所以
    /// [`AudioCapture::new`](crate::AudioCapture::new) 在打开设备时
    /// 就用 [`formats_compatible`] 拦下了采样率不一致的组合，
    /// 声道数的取舍则由调用方决定 —— 采集端只承诺「不静音、不溢位」。
    Direct(usize),
}

/// 决定设备帧到请求格式的声道映射
///
/// 优先级：请求单声道 → 降混；设备单声道或声道不足 → 平均值广播；
/// 否则 1:1 取前 n 个。
pub fn plan_for(device_channels: u16, requested_channels: u16) -> MixPlan {
    let dev = device_channels.max(1) as usize;
    let req = requested_channels.max(1) as usize;
    if req == 1 {
        MixPlan::DownmixToMono
    } else if dev == 1 || dev < req {
        MixPlan::FanoutFromAverage(req)
    } else {
        MixPlan::Direct(req)
    }
}

/// 这个设备格式能否喂给这个请求格式
///
/// 采样率必须**完全相等**：本 crate 不做重采样。
///
/// # 为什么先不做重采样
///
/// 共享模式混音格式在本机（以及绝大多数 Windows 机器）就是 48kHz，
/// 而 Opus 只吃 48kHz —— 两边一致，重采样路径根本没有被触发的机会。
/// 加一条线性插值路径会让「采样数对不上」的 bug 混进主链路，
/// 而它一旦出错是渐进的音调漂移，很难在测试里抓到。
/// 真遇到 44.1kHz 的设备，明确报 [`CaptureError::RateMismatch`]
/// 比悄悄变调好 —— 调用方至少能在日志里看到原因。
///
/// [`CaptureError::RateMismatch`]: crate::CaptureError::RateMismatch
pub fn formats_compatible(device: &DeviceFormat, requested: PcmFormat) -> bool {
    device.sample_rate == requested.sample_rate
}

/// 设备帧的只读视图
///
/// 用来把「设备缓冲区里的裸指针」和「纯转换函数」解耦：
/// `wasapi.rs` 只负责把指针变成这个枚举，转换逻辑完全不碰 unsafe。
#[derive(Debug, Clone, Copy)]
pub enum DeviceSamples<'a> {
    /// 32bit 浮点采样（交错）
    F32(&'a [f32]),
    /// 16bit 整数采样（交错）
    I16(&'a [i16]),
}

impl DeviceSamples<'_> {
    /// 采样个数（不是帧数）
    pub fn len(&self) -> usize {
        match self {
            DeviceSamples::F32(s) => s.len(),
            DeviceSamples::I16(s) => s.len(),
        }
    }

    /// 是否为空
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// 把设备帧转换成请求格式，追加进 `out`
///
/// 返回写进去的**帧数**（不是采样数）。`out` 追加 `帧数 × requested_channels` 个 i16。
///
/// 这个函数是全 crate 唯一做数值转换的地方：wasapi 路径与测试走的是
/// 同一份代码，所以跨平台测试真的能覆盖 Windows 上的转换行为。
pub fn convert_frames(
    out: &mut Vec<i16>,
    src: DeviceSamples<'_>,
    device_channels: usize,
    requested_channels: usize,
) -> usize {
    let dev_ch = device_channels.max(1);
    let req_ch = requested_channels.max(1);
    let frames = src.len() / dev_ch;
    if frames == 0 {
        return 0;
    }
    out.reserve(frames * req_ch);

    let plan = plan_for(dev_ch as u16, req_ch as u16);
    match src {
        DeviceSamples::F32(samples) => {
            for f in 0..frames {
                let base = f * dev_ch;
                let frame = &samples[base..base + dev_ch];
                match plan {
                    MixPlan::DownmixToMono => out.push(f32_to_i16(downmix_f32_to_mono(frame))),
                    MixPlan::FanoutFromAverage(n) => {
                        let v = f32_to_i16(downmix_f32_to_mono(frame));
                        for _ in 0..n {
                            out.push(v);
                        }
                    }
                    MixPlan::Direct(n) => {
                        for s in frame.iter().take(n) {
                            out.push(f32_to_i16(*s));
                        }
                    }
                }
            }
        }
        DeviceSamples::I16(samples) => {
            for f in 0..frames {
                let base = f * dev_ch;
                let frame = &samples[base..base + dev_ch];
                match plan {
                    MixPlan::DownmixToMono => out.push(downmix_i16_to_mono(frame)),
                    MixPlan::FanoutFromAverage(n) => {
                        let v = downmix_i16_to_mono(frame);
                        for _ in 0..n {
                            out.push(v);
                        }
                    }
                    MixPlan::Direct(n) => {
                        for s in frame.iter().take(n) {
                            out.push(*s);
                        }
                    }
                }
            }
        }
    }
    frames
}

/// 往 `out` 追加 `frames × channels` 个 0
///
/// WASAPI 的 `AUDCLNT_BUFFERFLAGS_SILENT` 会把 `pData` 置空 ——
/// 此时「数据」是零，不是「没有数据」。必须显式填 0 再交给编码器，
/// 否则时间线会出现空洞（Opus 收到的是连续的 20ms 帧，缺一帧就是爆音）。
pub fn push_silence(out: &mut Vec<i16>, frames: usize, channels: usize) -> usize {
    let n = frames.saturating_mul(channels.max(1));
    out.resize(out.len() + n, 0);
    frames
}
