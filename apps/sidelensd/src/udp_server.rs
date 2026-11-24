//! UDP 接收服务
//!
//! # 链路
//!
//! ```text
//! UDP 数据报 → Reassembler（分片重组）
//!            → 缺片超时 → NackGenerator → 回发 NACK
//!            → 完整帧 → ┬ Video   → JitterBuffer → AvSync 裁决 → 出队
//!                       └ Audio   → AudioJitterBuffer → 出队 → 推进 AvSync 主时钟
//! ```
//!
//! # 音频为什么要单独一个缓冲
//!
//! 视频的 `JitterBuffer` 按**到达时间**出队（`pop_at` 比较的是
//! `now - arrived_at`），这对视频没问题：视频帧之间互相独立，
//! 按到达顺序播放最多让画面抖一下。
//!
//! 音频不行。音频帧必须按**自己的 PTS** 播放，否则播放节奏会跟着
//! 网络到达顺序走，听起来就是忽快忽慢的音调漂移。而且 `AvSync` 需要
//! `pts + duration` 来推进主时钟 —— 到达时间给不出 duration。
//!
//! 所以音频走 `AudioJitterBuffer`：按 PTS 有序插入、`pop_due(now)` 按
//! 时间取出、四种结局（Ok/Late/Overflow/Discontinuity）各有对应计数。
//!
//! # 两个容易做错的点
//!
//! 1. **NACK 的目的地址**：UDP 是无连接的，必须记住每个 sender 的地址。
//!    本实现只支持单发送端（手机推流给电脑），多发送端需要按流 ID 区分。
//! 2. **重组超时检查频率**：太慢会让缺片帧白等，太快会误报。
//!    用 5ms 检查一次，与 NackGenerator 的默认延迟一致。

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[cfg(windows)]
use crate::video_decode::{DecodeError, VideoDecoder};
use anyhow::Result;
use sidelens_codec::audio::{AudioFrame, AudioJitterBuffer, OpusConfig, PushOutcome};
use sidelens_codec::bitrate::{BitrateConfig, BitrateTracker};
use sidelens_codec_opus::OpusDecoder;
use sidelens_core::congestion::{CongestionController, Feedback};
/// FEC 组状态保留多少帧（约 260ms @30fps）
///
/// 见 `FecReceiver::prune_stale` 的说明：这是修「组状态从不回收」
/// 那个内存泄漏时选的保留窗口。8 帧远大于正常的乱序跨度（1-2 帧），
/// 又远小于内存问题的量级（不保留 = 无限增长）。
///
/// 实测依据（`perf-audit` 子代理的计数器数据）：`tracked_groups()` 与 `frames_in`
/// 严格 1:1 增长，30fps/20s → 601 个组，60fps/100s → 5973 个组，永不下降；
/// 进程私有内存同步涨 0.27-0.52 MB/s。
/// 关键帧请求的最小间隔（微秒）
///
/// 200ms ≈ 6 帧 @30fps。再短会让关键帧请求本身挤占带宽 ——
/// 关键帧比 P 帧大 30 倍（39KB vs 1.2KB），请求过密会把链路塞满，
/// 反而让更多关键帧丢掉（正反馈）。
const KEYFRAME_REQ_MIN_INTERVAL_US: u64 = 200_000;

const FEC_KEEP_FRAMES: u32 = 8;

use sidelens_core::fec::FecConfig;
use sidelens_core::fec_link::{FecPacket, FecReceiver};
use sidelens_core::fragment::Reassembler;
use sidelens_core::jitter::QueuedFrame;
use sidelens_core::{AvSync, JitterBuffer, JitterConfig, NackGenerator, SyncVerdict};
use sidelens_crypto::{FrameHead, FrameSealer, ReplayGuard, Role, TrafficKeys};
use sidelens_proto::{encode_frame_owned, FrameSpec, FrameType};
use sidelens_transport::UdpReceiver;
use tokio::net::UdpSocket;
use tracing::{debug, info, warn};

/// 渲染器的统一包装（第五十二轮）
///
/// # 为什么需要这层
///
/// GDI 和 GPU 两个渲染器的接口**形状不同**：
///
/// | | GDI (`sidelens-render`) | GPU (`sidelens-render-gpu`) |
/// | --- | --- | --- |
/// | 呈现 | `present(&Nv12View)` | `present(&[u8], coded, display)` |
/// | 尺寸来源 | 从 `Nv12View` 里读 | 显式传参 |
/// | 模式查询 | `mode() -> RendererMode` | `mode() -> GpuMode` |
///
/// 主循环里不想为这个差异分叉（那条路径每帧都走），所以在接入点收成一个枚举。
/// 代价是每次 `present` 多一次 match —— 相对 437µs 的呈现成本可以忽略。
///
/// # 为什么不用 trait
///
/// `dyn Renderer` 要求两个 crate 有一个共同的 trait，而它们分属不同子代理，
/// 各自的错误类型也不同（`RenderError` vs `GpuError`）。为统一错误再包一层
/// `Box<dyn Error>` 会让「渲染失败的具体原因」在日志里变模糊 —— 而这个项目
/// 的日志是用来定位问题的，不能糊。
///
/// # 为什么 GPU 分支要 Box
///
/// clippy 实测：`GpuRenderer` 至少 8584 字节，`Renderer` 只有 128 字节，
/// 枚举整体被撑到 8592 字节。这个值存在栈上（`Option<RendererKind>`），
/// 每次 `as_mut()` 都按这个大小传递引用偏移 —— 虽然不拷贝，但栈帧和
/// 缓存行都被白白占掉。Box 之后枚举变成 16 字节。
///
/// 代价是每次 `present` 多一次指针解引用（GPU 渲染本身 437µs，
/// 解引用是纳秒级，可以忽略）。
/// 渲染失败的分类
///
/// 只有「设备丢失」是可恢复的 —— 其余（尺寸不符、纹理创建失败）
/// 重试也一样失败，调用方该降级。
#[cfg(windows)]
#[derive(Debug)]
enum RenderFailure {
    /// GPU 设备丢失，可以重建
    DeviceRemoved { hresult: u32 },
    /// 其它错误，不可恢复
    Other(String),
}

#[cfg(windows)]
impl std::fmt::Display for RenderFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RenderFailure::DeviceRemoved { hresult } => {
                write!(f, "GPU 设备丢失（0x{hresult:08X}）")
            }
            RenderFailure::Other(m) => write!(f, "{m}"),
        }
    }
}

#[cfg(windows)]
enum RendererKind {
    Gdi(sidelens_render::Renderer),
    Gpu(Box<sidelens_render_gpu::GpuRenderer>),
}

#[cfg(windows)]
impl RendererKind {
    /// 建 GDI 窗口；失败降级 headless（保持第三十九轮的行为）
    fn new_gdi_or_headless(title: &str, w: u32, h: u32) -> Option<Self> {
        match sidelens_render::Renderer::new(title, w, h) {
            Ok(r) => {
                info!(width = w, height = h, "渲染窗口已创建（GDI）");
                Some(Self::Gdi(r))
            }
            Err(e) => {
                warn!(error = %e, "渲染窗口创建失败（无桌面环境？），改走 headless");
                sidelens_render::Renderer::headless(w, h)
                    .ok()
                    .map(Self::Gdi)
            }
        }
    }

    /// 建 GPU 窗口（独立顶层窗口）
    fn new_gpu(title: &str, w: u32, h: u32) -> Result<Self, sidelens_render_gpu::GpuError> {
        sidelens_render_gpu::GpuRenderer::new(title, w, h).map(|r| Self::Gpu(Box::new(r)))
    }

    /// 建 GPU **子窗口**：嵌进 `parent` 的客户区 `(x, y)` 处
    ///
    /// 第五十四轮新增。参数口径见 `GpuRenderer::new_child`：全是父窗口
    /// 客户区的物理像素，与 `sidelens_ui::MainWindow::video_rect()` 一致。
    fn new_gpu_child(
        parent: isize,
        title: &str,
        x: i32,
        y: i32,
        w: u32,
        h: u32,
    ) -> Result<Self, sidelens_render_gpu::GpuError> {
        sidelens_render_gpu::GpuRenderer::new_child(parent, title, x, y, w, h)
            .map(|r| Self::Gpu(Box::new(r)))
    }

    /// headless：GDI 的 headless 是纯转换，不需要 GPU
    fn headless(w: u32, h: u32) -> Self {
        match sidelens_render::Renderer::headless(w, h) {
            Ok(r) => Self::Gdi(r),
            // headless 只会在尺寸为 0 时失败，这里给的是非 0 尺寸。
            // 真失败了也没有更差的降级选项，直接 panic 说明前提被破坏。
            Err(e) => panic!("headless 渲染器创建失败（尺寸 {w}x{h}）：{e}"),
        }
    }

    /// 呈现一帧 NV12
    ///
    /// `view` 里带着 coded/display 尺寸，GPU 分支直接取用 —— 两边看到的是
    /// 同一份几何信息，只是传递方式不同。
    /// # 为什么返回 `RenderFailure` 而不是 `String`
    ///
    /// 设备丢失是**唯一可恢复**的 GPU 错误 —— 调用方需要区分
    /// 「该重建」和「该放弃」。返回 `String` 的话只能靠字符串匹配，
    /// 错误信息一改就失效。
    fn present(&mut self, view: &sidelens_render::Nv12View<'_>) -> Result<(), RenderFailure> {
        match self {
            Self::Gdi(r) => r
                .present(view)
                .map_err(|e| RenderFailure::Other(e.to_string())),
            Self::Gpu(r) => r
                .present(
                    view.data(),
                    (view.coded_width(), view.coded_height()),
                    (view.display_width(), view.display_height()),
                )
                .map_err(|e| match e {
                    sidelens_render_gpu::GpuError::DeviceRemoved { hresult } => {
                        RenderFailure::DeviceRemoved { hresult }
                    }
                    other => RenderFailure::Other(other.to_string()),
                }),
        }
    }

    /// 处理窗口消息。返回 false = 用户关窗
    fn pump_events(&mut self) -> bool {
        match self {
            Self::Gdi(r) => r.pump_events(),
            Self::Gpu(r) => r.pump_events(),
        }
    }

    /// 设备丢失后重建整个渲染器，返回新窗口句柄
    fn recover(&mut self) -> Result<Option<isize>, String> {
        match self {
            // GDI 没有 D3D11 设备，不存在「设备丢失」
            Self::Gdi(_) => Ok(None),
            Self::Gpu(r) => r.recover().map_err(|e| e.to_string()),
        }
    }

    /// 窗口尺寸变化
    ///
    /// GDI 路径不需要（`StretchDIBits` 每帧按当前窗口尺寸画）；
    /// GPU 路径必须重建 swap chain，否则 back buffer 是旧尺寸。
    fn resize(&mut self, w: u32, h: u32) -> Result<(), String> {
        match self {
            // GDI 的 `present` 每次按 `Nv12View` 的 display 尺寸画，
            // 窗口变化会自动生效 —— 这里什么都不用做。
            Self::Gdi(_) => Ok(()),
            Self::Gpu(r) => r.resize(w, h).map_err(|e| e.to_string()),
        }
    }

    /// 已呈现的帧数
    fn frames(&self) -> u64 {
        match self {
            Self::Gdi(r) => r.frames(),
            Self::Gpu(r) => r.frames(),
        }
    }

    /// 模式名（日志用）
    fn mode_name(&self) -> &'static str {
        match self {
            Self::Gdi(r) => r.mode().as_str(),
            Self::Gpu(r) => match r.mode() {
                sidelens_render_gpu::GpuMode::Window => "gpu-window",
                sidelens_render_gpu::GpuMode::Offscreen => "gpu-offscreen",
                sidelens_render_gpu::GpuMode::Headless => "gpu-headless",
            },
        }
    }

    /// GPU 适配器名（GDI 分支返回空串）
    fn gpu_adapter_name(&self) -> &str {
        match self {
            Self::Gdi(_) => "",
            Self::Gpu(r) => r.adapter_name(),
        }
    }

    /// 交换链格式（GDI 分支返回 "bgra-dib"）
    fn gpu_format(&self) -> &'static str {
        match self {
            Self::Gdi(_) => "bgra-dib",
            Self::Gpu(r) => r.swapchain_format(),
        }
    }
}

/// 延迟样本滑动窗口
#[derive(Debug, Default)]
pub struct LatencyWindow {
    samples: std::collections::VecDeque<f64>,
}

impl LatencyWindow {
    pub fn push(&mut self, ms: f64, cap: usize) {
        if !ms.is_finite() || ms < 0.0 {
            return;
        }
        if self.samples.len() == cap {
            self.samples.pop_front();
        }
        self.samples.push_back(ms);
    }

    pub fn percentile(&self, p: f64) -> f64 {
        if self.samples.is_empty() {
            return 0.0;
        }
        let mut v: Vec<f64> = self.samples.iter().copied().collect();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let idx = ((v.len() as f64 - 1.0) * p).round() as usize;
        v[idx.min(v.len() - 1)]
    }

    pub fn summary(&self) -> (f64, f64, f64) {
        (
            self.percentile(0.50),
            self.percentile(0.95),
            self.percentile(0.99),
        )
    }
}

/// UDP 服务端统计
#[derive(Debug, Default)]
pub struct UdpServerStats {
    pub frames_in: AtomicU64,
    pub nack_sent: AtomicU64,
    pub seqs_requested: AtomicU64,
    pub frames_dropped_missing: AtomicU64,
    pub expired_partials: AtomicU64,
    /// 收到的音频帧数（进缓冲前）
    pub audio_frames: AtomicU64,
    /// 因迟到被音频缓冲丢弃的帧数
    pub audio_dropped_late: AtomicU64,
    /// 时间戳跳变（采集重启 / 换设备）次数
    pub audio_discontinuity: AtomicU64,
    /// 音频缓冲溢出（丢最旧一帧）次数
    pub audio_overflow: AtomicU64,
    /// 从音频缓冲取出、喂给 AvSync 的帧数
    pub audio_played: AtomicU64,
    /// AvSync 判为 Drop 的视频帧数（落后音频太多）
    pub video_dropped_late: AtomicU64,
    /// AvSync 判为 Wait 的视频帧数（超前音频太多）
    pub video_waited: AtomicU64,
    /// 被重放窗口拒绝的帧数（加密开启时才有意义）
    ///
    /// 非零不一定是攻击：NACK 重传会让同一个 seq 到达两次，
    /// 而重传是正常行为。所以这个计数需要与 `nack_sent` 一起看。
    pub replay_rejected: AtomicU64,
    /// 解密失败的帧数（认证失败 / 密钥不匹配）
    ///
    /// 非零意味着「有人在改包」或「两端密钥不一致」。前者是安全事件，
    /// 后者是配置错误 —— 两者都该立刻停下来查。
    pub decrypt_failed: AtomicU64,
    /// Opus 解码成功的帧数（`SIDELENS_OPUS=1` 时才有意义）
    pub audio_decoded: AtomicU64,
    /// Opus 解码失败（坏包）的帧数
    pub audio_decode_failed: AtomicU64,
    /// PLC 补出的帧数（本层不产生，留给将来「丢包直接 PLC」用）
    pub audio_concealed: AtomicU64,
}

impl UdpServerStats {
    pub fn snapshot(&self) -> UdpServerSnapshot {
        UdpServerSnapshot {
            frames_in: self.frames_in.load(Ordering::Relaxed),
            nack_sent: self.nack_sent.load(Ordering::Relaxed),
            seqs_requested: self.seqs_requested.load(Ordering::Relaxed),
            frames_dropped_missing: self.frames_dropped_missing.load(Ordering::Relaxed),
            expired_partials: self.expired_partials.load(Ordering::Relaxed),
            audio_frames: self.audio_frames.load(Ordering::Relaxed),
            audio_dropped_late: self.audio_dropped_late.load(Ordering::Relaxed),
            audio_discontinuity: self.audio_discontinuity.load(Ordering::Relaxed),
            audio_overflow: self.audio_overflow.load(Ordering::Relaxed),
            audio_played: self.audio_played.load(Ordering::Relaxed),
            video_dropped_late: self.video_dropped_late.load(Ordering::Relaxed),
            video_waited: self.video_waited.load(Ordering::Relaxed),
            replay_rejected: self.replay_rejected.load(Ordering::Relaxed),
            decrypt_failed: self.decrypt_failed.load(Ordering::Relaxed),
            audio_decoded: self.audio_decoded.load(Ordering::Relaxed),
            audio_decode_failed: self.audio_decode_failed.load(Ordering::Relaxed),
            audio_concealed: self.audio_concealed.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct UdpServerSnapshot {
    pub frames_in: u64,
    pub nack_sent: u64,
    pub seqs_requested: u64,
    pub frames_dropped_missing: u64,
    pub expired_partials: u64,
    pub audio_frames: u64,
    pub audio_dropped_late: u64,
    pub audio_discontinuity: u64,
    pub audio_overflow: u64,
    pub audio_played: u64,
    pub video_dropped_late: u64,
    pub video_waited: u64,
    pub replay_rejected: u64,
    pub decrypt_failed: u64,
    pub audio_decoded: u64,
    pub audio_decode_failed: u64,
    pub audio_concealed: u64,
}

/// 从 `FrameType::Config` 帧的 JSON 载荷里解析 FEC 配置
///
/// 发送端发的是 `{"fec_group_size":8,"fec_redundant":2}`。
///
/// # 为什么手工解析而不是 serde_json
///
/// 两个字段都是纯数字，没有转义、嵌套、浮点。手写解析的收益是
/// **对畸形输入完全免疫**：任何解析不出来的字段都退回默认值，
/// 不会因为一个坏包 panic 或把配置改成 0（`group_size = 0` 会让
/// `index / group_size` 直接除零 panic）。
///
/// 返回 `None` 表示两个字段都没解析出来（不是 FEC 配置帧）。
fn parse_fec_config_payload(payload: &[u8], default: &FecConfig) -> Option<FecConfig> {
    let text = std::str::from_utf8(payload).ok()?;

    /// 取 `"key":` 之后的第一个非负整数
    fn field(text: &str, key: &str) -> Option<usize> {
        let at = text.find(key)? + key.len();
        let rest = &text[at..];
        // 跳过 key 与值之间的 `":` / 空白
        let start = rest.find(|c: char| c.is_ascii_digit())?;
        let digits: String = rest[start..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        digits.parse().ok()
    }

    let group = field(text, r#""fec_group_size""#);
    let redundant = field(text, r#""fec_redundant""#);
    if group.is_none() && redundant.is_none() {
        return None;
    }

    Some(FecConfig {
        group_size: group.unwrap_or(default.group_size),
        redundant: redundant.unwrap_or(default.redundant),
        max_packet_size: sidelens_core::SAFE_MTU_PAYLOAD,
    })
}

/// 解析 Config 帧里的 `audio_codec` 声明
///
/// # 为什么单独一个函数而不是塞进 `parse_fec_config_payload`
///
/// 两者是**无关的信息**：FEC 字段缺失/非法时不该影响 codec 声明生效。
/// 而且 `parse_fec_config_payload` 的契约是「两个 FEC 字段都没解析出来
/// 就返回 None」，把 codec 塞进去会破坏那个契约。
///
/// # 返回值
///
/// - `Some("opus")` / `Some("pcm")`：发送端明确声明了
/// - `None`：没有这个字段（老版本发送端），或值不认识
///
/// 不认识的值返回 `None` 而不是报错：新版本可能加新 codec，
/// 老接收端静默忽略比 panic 好。
fn parse_audio_codec_payload(payload: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(payload).ok()?;
    let key = r#""audio_codec""#;
    let at = text.find(key)? + key.len();
    let rest = &text[at..];
    // 跳过 `:` 与空白，然后读引号内的字符串
    let q1 = rest.find('"')?;
    let after = &rest[q1 + 1..];
    let q2 = after.find('"')?;
    let codec = &after[..q2];
    if codec.is_empty() {
        return None;
    }
    Some(codec.to_ascii_lowercase())
}

/// 解析 Config 帧里的 `audio_frame_ms`
///
/// # 为什么帧长必须由发送端告知
///
/// 接收端需要它做三件事，全部与时间线正确性有关：
///
/// | 用途 | 用错帧长的后果 |
/// | --- | --- |
/// | 解码器缓冲大小 | 60ms 帧只要 960 采样缓冲 → 解码报 "buffer too small"，一帧都解不出来 |
/// | `AudioJitterBuffer` 的 `frame_duration_us` | AvSync 主时钟每帧少推进 40ms，音画逐渐错位 |
/// | `AudioFrame::new` 的 duration | 同上 |
///
/// # 返回值
///
/// `None` 表示没有这个字段（老版本发送端）或值不合法。
/// 上层收到 `None` 时保持当前配置不变。
fn parse_audio_frame_ms(payload: &[u8]) -> Option<f64> {
    let text = std::str::from_utf8(payload).ok()?;
    let key = r#""audio_frame_ms""#;
    let at = text.find(key)? + key.len();
    let rest = &text[at..];
    // 跳过分隔符与空白，读一个浮点数（可能是 2.5 这种）
    let start = rest.find(|c: char| c.is_ascii_digit())?;
    // `.chars()` 的闭包参数是 `&char`，不能标 `char`（否则 E0631）
    let num: String = rest[start..]
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let ms: f64 = num.parse().ok()?;
    // 0 表示「不发音频」，合法但不该拿去配置解码器
    if ms <= 0.0 {
        return None;
    }
    Some(ms)
}

/// 运行 UDP 接收循环
/// 解析 `SIDELENS_PSK` 环境变量（64 位十六进制 → 32 字节）
///
/// 与发送端 `parse_psk` 保持一致的校验规则，但错误信息更明确 ——
/// 接收端配置错了会表现为「所有帧解密失败」，那个症状很难指向
/// 「密钥格式不对」，所以这里把话说清楚。
fn parse_psk_env(s: &str) -> Option<[u8; 32]> {
    let s = s.trim();
    if s.len() != 64 {
        warn!(
            len = s.len(),
            "SIDELENS_PSK 必须是 64 位十六进制字符（32 字节），已忽略 —— 将不启用解密"
        );
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        let hi = (s.as_bytes()[i * 2] as char).to_digit(16);
        let lo = (s.as_bytes()[i * 2 + 1] as char).to_digit(16);
        match (hi, lo) {
            (Some(h), Some(l)) => *byte = ((h << 4) | l) as u8,
            _ => {
                warn!(index = i * 2, "SIDELENS_PSK 含非十六进制字符，已忽略");
                return None;
            }
        }
    }
    Some(out)
}

pub async fn run(port: u16, report_every: Duration) -> Result<()> {
    let receiver = UdpReceiver::bind(port).await?;
    let actual_port = receiver.port()?;
    info!(port = actual_port, "SideLens UDP 已启动");

    let socket: Arc<UdpSocket> = receiver.socket();
    let mut asm = Reassembler::new(30.0, 128);
    let mut nackgen = NackGenerator::new(5.0, 32);
    let mut jb = JitterBuffer::new(JitterConfig::default());

    let mut net_latency = LatencyWindow::default();
    let mut playable = LatencyWindow::default();
    let mut hold = LatencyWindow::default();
    // 音频缓冲深度窗口（毫秒），日志里的 audio_depth_ms 取 p50/p95
    let mut audio_depth = LatencyWindow::default();

    let stats = Arc::new(UdpServerStats::default());
    let mut last_report = Instant::now();

    // 码率统计：用真实到达的帧大小算实际带宽占用，
    // 这是码率自适应的输入（不能只靠发送端自报）
    let mut bitrate = BitrateTracker::new(BitrateConfig::default(), 32);

    // FEC 接收端：解冗余包、恢复丢失的分片。
    //
    // 只有发送端启用了 FEC 才会收到 `FrameType::Fec` 帧；
    // 没收到时这个对象一直空转，无开销。
    //
    // # 配置从哪来
    //
    // 发送端按每帧的分片数动态选 `group_size`，所以接收端**必须知道当前值**：
    // 它用 `index / group_size` 把帧内分片下标折算成组内下标，用
    // `i % redundant` 建 XOR 方程组。对不上就恢复出静默的垃圾数据。
    //
    // 协商走 `FrameType::Config` 帧（发送端在组大小变化时发两份），
    // 载荷是 `{"fec_group_size":8,"fec_redundant":2}`。
    //
    // 收到 Config 之前用下面的默认值。默认值与发送端 `--fec` 的常见档位对齐，
    // 让「发送端还没发 Config」的头几十毫秒不至于完全失效。
    let default_fec_config = FecConfig {
        group_size: std::env::var("SIDELENS_FEC_GROUP")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(10),
        redundant: std::env::var("SIDELENS_FEC_REDUNDANT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(2),
        max_packet_size: sidelens_core::SAFE_MTU_PAYLOAD,
    };
    let mut fec_config = default_fec_config;
    let mut fec_rx = FecReceiver::new(fec_config).ok();
    let mut fec_recovered_total = 0u64;
    // 配置切换次数：> 1 说明组大小在抖，会让跨配置的组全部作废
    let mut fec_config_updates = 0u64;
    // (group, redundant) → 用了多少帧。自适应是否在抖，看这个分布：
    // 只有一项说明稳定；一堆说明迟滞没压住（接收端每次切换都要丢掉未完成的组）。
    let mut fec_group_usage: std::collections::BTreeMap<(usize, usize), u64> =
        std::collections::BTreeMap::new();
    // FEC 组状态回收掉的组数（诊断内存行为：持续增长说明回收在工作）
    let mut fec_groups_pruned = 0u64;

    // ── 帧解密与重放防护（可选，环境变量 `SIDELENS_PSK` 开启）──
    //
    // # 为什么用环境变量而不是命令行
    //
    // 密钥出现在命令行里会被 `ps` / 任务管理器 / shell history 记下来。
    // 环境变量同样不是安全的密钥存储（同用户的其他进程可读），
    // 但至少不会留在历史文件里。生产路径应该走 X25519 握手，
    // 这里只是让「加密链路能跑通」这件事现在就可验证。
    //
    // # 顺序：先验重放，再解密
    //
    // 重放检测不需要密钥，先做能省掉一次无用的解密尝试；
    // 更重要的是：重放的帧密文是**合法的**，解密一定成功 ——
    // 如果顺序反了，重放帧会先被解密、再被丢弃，白白消耗 CPU，
    // 而且统计上会看到「解密成功但帧被丢」这种费解的现象。
    let mut sealer = std::env::var("SIDELENS_PSK")
        .ok()
        .and_then(|s| parse_psk_env(&s))
        .map(|master| {
            FrameSealer::new(
                TrafficKeys::from_master(&master, Role::Responder),
                Role::Responder,
            )
        });
    let mut replay = ReplayGuard::default();
    if let Some(s) = sealer.as_ref() {
        info!(role = %s.role(), "帧解密已启用（ChaCha20-Poly1305）");
    }

    // ── Opus 解码器（可选，与 `SIDELENS_PSK` 一样走环境变量）──
    //
    // # 为什么用环境变量而不是自动检测
    //
    // 接收端**无法从码流判断**载荷是 Opus 还是原始 PCM ——
    // 两者都是不透明的字节。协议帧头里也没有编解码器字段。
    //
    // 自动检测（比如「长度像 Opus 帧就当 Opus」）会在
    // 「Opus 帧恰好 1920 字节」时误判，而那个长度是可能的
    // （1275 字节上限 × 2 声道 = 2550，1920 在范围内）。
    //
    // 所以必须显式配置。将来协议里加 codec 字段后可以去掉。
    // 帧长：从 `SIDELENS_OPUS_FRAME_MS` 读（默认 20）。
    //
    // # 为什么必须可配（第三十一轮修的）
    //
    // 下面五处原本全部硬编码 `OpusConfig::default()`（= 20ms）：
    //
    // | 位置 | 后果（发送端用 60ms 帧时） |
    // | --- | --- |
    // | 解码器 `frame_samples` | 缓冲只要 960 采样，解码报 "buffer too small"，`audio_decoded=0` |
    // | `opus_pcm` 长度 | 同上 |
    // | `AudioJitterBuffer` 的 `frame_duration_us` | 每帧少推进 40ms，AvSync 主时钟漂移 |
    // | `AudioFrame::new` 的 duration | 同上 |
    //
    // 实测（`opus-wire` 子代理）：发送端 `--opus-frame-ms 60` 时
    // `audio_decode_failed=134/134`，一帧都没解出来。
    let opus_frame_ms: f64 = std::env::var("SIDELENS_OPUS_FRAME_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|v: &f64| sidelens_codec_opus::is_allowed_frame_ms(*v))
        .unwrap_or(20.0);
    let opus_cfg = OpusConfig {
        frame_ms: opus_frame_ms,
        ..OpusConfig::default()
    };
    // 校验：帧长非法时回退默认，而不是让后面的 `frame_samples()` 算错
    let mut opus_cfg = if opus_cfg.validate().is_ok() {
        opus_cfg
    } else {
        warn!(frame_ms = opus_frame_ms, "音频帧长非法，回退 20ms");
        OpusConfig::default()
    };
    if (opus_cfg.frame_ms - 20.0).abs() > f64::EPSILON {
        info!(
            frame_ms = opus_cfg.frame_ms,
            frame_samples = opus_cfg.frame_samples(),
            frame_duration_us = opus_cfg.frame_duration_us(),
            "音频帧长非默认值（SIDELENS_OPUS_FRAME_MS）"
        );
    }

    let mut opus_dec = match std::env::var("SIDELENS_OPUS") {
        Ok(v) if v == "1" || v.eq_ignore_ascii_case("true") => match OpusDecoder::new(opus_cfg) {
            Ok(d) => {
                info!("Opus 解码已启用（SIDELENS_OPUS=1）");
                Some(d)
            }
            Err(e) => {
                warn!(error = %e, "Opus 解码器创建失败，将按原始 PCM 处理");
                None
            }
        },
        _ => None,
    };
    // 解码输出缓冲（复用）。长度必须按**当前帧长**算，
    // 固定 960 采样会让 40/60ms 帧解码报 "buffer too small"。
    let mut opus_pcm = vec![0i16; opus_cfg.frame_samples() as usize];

    // 音频输出（第四十一轮接入）。
    //
    // `SIDELENS_AUDIO_OUT=1` 启用声卡输出；`headless` 只累计计数；
    // 未设置时为 `None`（只解码不播放，与之前行为一致）。
    //
    // 创建失败时降级为 headless 而**不中断**接收 —— 没声卡的环境（CI / 容器）
    // 也要能跑完整链路。
    //
    // # 为什么整个块都门控（第四十一轮）
    //
    // `sidelens-audio-out` 在 `Cargo.toml` 里放在
    // `[target.'cfg(windows)'.dependencies]` —— 它是 WASAPI 专属。
    // 非 Windows 上这个 crate 不存在，直接引用会 E0433。
    //
    // 所以这里用「两个 cfg 分支各定义一个 `audio_sink`」的写法，
    // 与 `video_dec` / `renderer` 一致。
    #[cfg(windows)]
    let mut audio_sink = match std::env::var("SIDELENS_AUDIO_OUT") {
        Ok(v) if v == "1" || v.eq_ignore_ascii_case("true") => {
            let fmt = sidelens_audio_out::PcmFormat {
                sample_rate: opus_cfg.sample_rate,
                channels: opus_cfg.channels as u16,
            };
            match sidelens_audio_out::AudioSink::new(fmt) {
                Ok(s) => {
                    info!(
                        sample_rate = fmt.sample_rate,
                        channels = fmt.channels,
                        latency_ms = format!("{:.1}", s.latency_ms()),
                        "音频输出已启用（WASAPI 共享模式）"
                    );
                    Some(s)
                }
                Err(e) => {
                    warn!(error = %e, "声卡打开失败，降级为 headless（只计数不发声）");
                    Some(sidelens_audio_out::AudioSink::headless(fmt))
                }
            }
        }
        Ok(v) if v.eq_ignore_ascii_case("headless") => {
            let fmt = sidelens_audio_out::PcmFormat {
                sample_rate: opus_cfg.sample_rate,
                channels: opus_cfg.channels as u16,
            };
            info!("音频输出 headless 模式（只计数不发声）");
            Some(sidelens_audio_out::AudioSink::headless(fmt))
        }
        _ => None,
    };
    #[cfg(not(windows))]
    let mut audio_sink: Option<()> = None;
    let mut audio_write_errors: u64 = 0;
    let mut audio_write_stalls: u64 = 0;
    // ── 音频链路 ──────────────────────────────────────────────
    //
    // 音频抖动缓冲与视频的 JitterBuffer 是两套完全不同的东西：
    //
    // - 视频按「到达时间 + target_ms」出队，因为视频帧互相独立，
    //   按到达顺序播放最多让画面抖一下
    // - 音频必须按**自己的 PTS** 出队，否则播放节奏会跟着网络到达
    //   顺序走，听感是忽快忽慢的音调漂移；而且 AvSync 要 duration
    //   才能推进主时钟，到达时间给不出这个值
    //
    // target_ms 取 40ms：20ms 一帧，容量 = ceil(40/20)+1 = 3 帧。
    // 既能吸收一帧的乱序，又不会把延迟堆起来（人耳对 40ms 以内
    // 的缓冲无感）。
    let audio_target_ms: f64 = std::env::var("SIDELENS_AUDIO_TARGET_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(40.0);
    let mut audio_jb = AudioJitterBuffer::new(opus_cfg, audio_target_ms).ok();
    if let Some(jb) = audio_jb.as_ref() {
        info!(
            target_ms = jb.target_ms(),
            capacity_frames = jb.capacity_frames(),
            frame_duration_us = jb.config().frame_duration_us(),
            "音频抖动缓冲已就绪"
        );
    } else {
        warn!(
            target_ms = audio_target_ms,
            "音频抖动缓冲创建失败（配置非法），音频帧将被丢弃"
        );
    }

    // 音画同步：音频是主时钟，视频按 PTS 对齐。
    //
    // 参数用默认值（max_drift 40ms / drop_threshold 120ms）：
    // 视频超前音频 40ms 以上就等一等，落后 120ms 以上就丢 ——
    // 这两个数字来自「人眼对 40ms 内的画面超前无感、对 120ms 的
    // 滞后开始觉得口型对不上」。
    let mut av = AvSync::default();
    // 视频解码器（第三十八轮接入）。
    //
    // `SIDELENS_H264_DECODE=1` 启用；未启用时 `None`，视频路径行为完全不变
    // （只统计延迟，不解码）。
    //
    // # 为什么解码放在 `SyncVerdict::Play` 分支
    //
    // 那是「这一帧确定要播」的唯一时刻。在收包处解码会白解将被丢弃的帧；
    // 在出队后、裁决前解码则可能解了又被 Drop，浪费 400µs/帧。
    #[cfg(windows)]
    let mut video_dec = VideoDecoder::from_env();
    #[cfg(windows)]
    let mut video_decoded: u64 = 0;
    #[cfg(windows)]
    let mut video_decode_failed: u64 = 0;
    #[cfg(windows)]
    let mut video_need_keyframe: u64 = 0;
    #[cfg(windows)]
    let mut last_keyframe_req_us: Option<u64> = None;
    // 渲染器（第三十九轮接 GDI，第五十二轮加 GPU）。
    //
    // `SIDELENS_RENDER` 的取值：
    //
    // | 值 | 行为 |
    // | --- | --- |
    // | `gpu` | D3D11 + DXGI flip-model，NV12 零拷贝上屏（实测 p50 437µs） |
    // | `1` / `true` | GDI `StretchDIBits`（历史路径，p50 7456µs） |
    // | `headless` | 只做转换不显示（CI / 无桌面） |
    // | 未设置 | 不渲染 |
    //
    // # 为什么 GPU 不是默认值
    //
    // 1. 第三十九轮到第五十一轮的所有渲染数字（`blit_ns`、`render_frames`）
    //    都是 GDI 路径测的，默认值一改那些数字全部失去可比性。
    // 2. GPU 路径依赖 D3D11 设备。远程桌面、虚拟机、老显卡上可能拿不到
    //    硬件设备（会退 WARP，慢一个数量级），显式选择比静默降级好。
    //
    // 依赖 `video_dec` —— 没有解码器就没有 NV12 可渲染。
    #[cfg(windows)]
    let mut render_failed: u64 = 0;
    // 设备丢失后的重建次数。
    //
    // 超过 3 次就不再试 —— 否则「每帧都丢失、每帧都重建」会变成死循环，
    // 比直接降级更糟（CPU 全花在重建上，画面永远出不来）。
    #[cfg(windows)]
    let mut render_recoveries: u64 = 0;

    // 状态面板（第五十三轮建，第五十四轮改成「渲染器嵌进去」）。
    //
    // # 为什么面板必须先于渲染器创建
    //
    // GPU 渲染器现在是**子窗口**模式：它需要父窗口句柄和客户区矩形。
    // 这两个都由面板提供（`hwnd()` 和 `video_rect()`）。第五十三轮时渲染器
    // 还是独立顶层窗口，所以顺序无所谓；现在顺序是硬约束。
    //
    // # 为什么默认关闭
    //
    // 1. 第三十九到五十二轮的所有延迟数字都是「无 UI」下测的，默认开会让
    //    那些数字失去可比性（多一个窗口合成器参与）
    // 2. CI / 无桌面环境里建窗口会失败，默认关闭省掉一条降级分支
    //
    // # 为什么建失败不中断
    //
    // 这是个接收服务：界面没了照样该收包。`StatusPanel::new` 返回 None 时
    // 只打一条 warn，主循环照常跑。
    #[cfg(windows)]
    let mut panel: Option<crate::status_panel::StatusPanel> = match std::env::var("SIDELENS_UI") {
        Ok(v) if v == "1" || v.eq_ignore_ascii_case("true") => {
            crate::status_panel::StatusPanel::new("SideLens")
        }
        _ => None,
    };
    // 渲染器（第三十九轮接 GDI，第五十二轮加 GPU，第五十四轮加子窗口）。
    //
    // `SIDELENS_RENDER` 的取值：
    //
    // | 值 | 行为 |
    // | --- | --- |
    // | `gpu` | D3D11 + DXGI flip-model，NV12 零拷贝（实测 p50 437µs） |
    // | `1` / `true` | GDI `StretchDIBits`（历史路径，p50 7456µs） |
    // | `headless` | 只做转换不显示（CI / 无桌面） |
    // | 未设置 | 不渲染 |
    //
    // # 面板开着时：嵌进视频区（子窗口）
    //
    // 面板算好的 `video_rect()` 是**父窗口客户区的物理像素**，正好是
    // `new_child` 要的口径。这样视频显示在面板中间，而不是另开一个窗口
    // 盖住标签和按钮。
    //
    // # 面板没开时：独立窗口（与第五十二轮行为一致）
    //
    // 依赖 `video_dec` —— 没有解码器就没有 NV12 可渲染。
    #[cfg(windows)]
    let mut renderer: Option<RendererKind> = {
        let (w, h) = video_dec
            .as_ref()
            .map(|d| d.output_display_size())
            .unwrap_or((1280, 720));
        match std::env::var("SIDELENS_RENDER") {
            Ok(v) if v.eq_ignore_ascii_case("gpu") => {
                let built = match panel.as_ref() {
                    Some(p) => {
                        let (vx, vy, vw, vh) = p.video_rect();
                        info!(
                            video_x = vx,
                            video_y = vy,
                            video_w = vw,
                            video_h = vh,
                            "GPU 渲染器嵌入面板视频区（子窗口）"
                        );
                        RendererKind::new_gpu_child(
                            p.hwnd(),
                            "SideLensVideo",
                            vx,
                            vy,
                            vw.max(16) as u32,
                            vh.max(16) as u32,
                        )
                    }
                    None => RendererKind::new_gpu("SideLens", w, h),
                };
                match built {
                    Ok(r) => {
                        info!(
                            width = w,
                            height = h,
                            adapter = r.gpu_adapter_name(),
                            format = r.gpu_format(),
                            "GPU 渲染已启用（D3D11 + flip-model）"
                        );
                        Some(r)
                    }
                    Err(e) => {
                        warn!(error = %e, "GPU 渲染不可用，降级到 GDI");
                        RendererKind::new_gdi_or_headless("SideLens", w, h)
                    }
                }
            }
            Ok(v) if v == "1" || v.eq_ignore_ascii_case("true") => {
                RendererKind::new_gdi_or_headless("SideLens", w, h)
            }
            Ok(v) if v.eq_ignore_ascii_case("headless") => Some(RendererKind::headless(w, h)),
            _ => None,
        }
    };
    // 音频 PTS 的本地参照点。
    //
    // # 为什么需要它（关键）
    //
    // 发送端的 PTS 是「发送端 now_us() 起步的绝对时间戳」，而 pop_due
    // 要的 now_us 是本机时钟。两台机器（甚至同机两进程的时钟源）之间
    // 存在固定偏移，直接拿本地 now_us 去和远端 PTS 比较，会让所有帧
    // 一开始就判成「迟到」或「遥遥无期」。
    //
    // 所以第一帧音频到达时，把「本地到达时刻 - 该帧 PTS」记成偏移，
    // 之后 pop_due 用「本地 now - 偏移」换算到发送端时基上。
    // 这与视频侧 `JitterBuffer` 用 `arrived_at_us` 做相对判断是同一个思路，
    // 只是音频必须换算成绝对 PTS 才能算 AvSync 的偏移。
    let mut audio_clock_offset_us: Option<i64> = None;

    // AvSync 判 Wait 的视频帧暂存区。
    //
    // # 为什么需要暂存而不是直接跳过
    //
    // 「超前就跳过」等于把这一帧永久扔掉 —— 而它只是**现在**不该播，
    // 等到时间了就完全正常。直接丢会让视频在音频慢下来时持续掉帧，
    // 越掉越花。所以先放这里，下个 tick 再裁决一次。
    //
    // 上限 8 帧（约 270ms @30fps）：堆超过这个数说明音频时钟根本没在
    // 前进（音频断流），再等下去只会让视频延迟无限增长，此时按到达顺序
    // 放行并计数。
    let mut video_waiting: std::collections::VecDeque<QueuedFrame> =
        std::collections::VecDeque::new();
    const VIDEO_WAIT_MAX: usize = 8;

    // 拥塞控制：输入延迟样本 + 丢包率，输出建议码率。
    //
    // 注意方向：这个值要**回传给发送端**（通过 STATS 帧），
    // 发送端据此调整编码码率。接收端自己不能改码率。
    let mut cc = CongestionController::default();

    // 链路丢包率统计。
    //
    // # 关键区分：链路丢包率 ≠ 最终未恢复丢包率
    //
    // 实测踩坑：最初用「缺片数 / 总包数」算丢包率，5% 链路丢包时
    // 读数是 0.000 —— 因为 NACK 把丢的包全补回来了，最终一个没丢。
    //
    // 但拥塞控制要的是**链路当时的丢包率**：它反映的是「链路已经
    // 挤到开始丢包了」，而不是「我们的恢复机制是否成功」。
    // 用残留丢包率会让控制器把拥塞链路误判为健康，一路升码率。
    //
    // 所以这里统计的是「曾经缺过分片的帧占比」。
    //
    // # 但这仍然不能直接喂给控制器（第二次踩坑）
    //
    // 5% 的**包级**丢包，经过 7 个分片后，帧级丢包率是：
    //
    //     1 - 0.95^7 ≈ 30%
    //
    // 实测正是 0.30。但控制器的 `loss_threshold = 0.10` 是按**包级**
    // 定的（业界惯例），直接喂帧级数字会让它以为链路烂了 6 倍，
    // 一路把码率压到地板。
    //
    // 正确做法是反解回包级：
    //
    //     p_packet = 1 - (1 - P_frame)^(1/n)
    //
    // 其中 n 是每帧的分片数。n 可以从帧大小估算。
    let mut missing_frames_in_window: usize = 0;
    let mut received_frames_in_window: usize = 0;
    // 本窗口内收到的关键帧数（用于日志里的 kf_ratio）
    let mut keyframes_in_window: usize = 0;
    // 本窗口内收到的 FEC 冗余包数
    let mut fec_reds_in_window: usize = 0;
    // AvSync 判为 Play 的视频帧数（累积）
    let mut av_synced: u64 = 0;
    // 拥塞控制决策次数与降码率次数（累积，仅用于日志）
    let mut cc_decisions: u64 = 0;
    let mut cc_decreases: u64 = 0;
    // 窗口纪元：每次窗口清零就 +1。控制器据此判断统计是否更新过。
    let mut window_epoch: u64 = 0;
    let mut last_cc_window: u64 = u64::MAX;
    // 最近一次实测的包级丢包率（0-1），随 `RttReply` 回传给发送端。
    //
    // 为什么单独存一份而不是直接用 `loss_ratio`：那个变量在报告块里
    // 才算出来，而 `RttReply` 是在**收包分支**里发的 —— 两者不在同一个
    // 作用域，也没有固定的先后顺序（探测包可能在任何时刻到达）。
    let mut loss_ratio_hint: f64 = 0.0;
    // 诊断：音频 PTS 相对发送端时基的领先量（微秒）

    // 本窗口内累计的分片总数（用于算平均每帧几个分片）
    let mut fragments_in_window: usize = 0;
    let mut window_reset_at = Instant::now();

    // 记住发送端地址（UDP 无连接，回 NACK 需要它）
    let mut peer: Option<SocketAddr> = None;

    // 已完成的最高 seq：用于快速缺片检测（后来的帧到了 = 前面的不会再来了）
    let mut highest_completed: u32 = 0;

    let mut buf = vec![0u8; 65536];

    // 出队/超时检查节拍。这个值直接决定"抖动缓冲之外"的额外延迟：
    // 5ms 时平均白等 2.5ms，1ms 时 0.5ms。
    // 高于 NACK 的延迟（默认 5ms）也不会更差，但更细的粒度对出队更有利。
    let tick_ms: u64 = std::env::var("SIDELENS_TICK_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);
    let mut ticker = tokio::time::interval(Duration::from_millis(tick_ms));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    info!(tick_ms, "出队节拍");

    loop {
        tokio::select! {
            biased;

            // 出队 + 重组超时检查
            _ = ticker.tick() => {
                let now = sidelens_proto::now_us();

                // 0) 处理 UI 消息。**必须每 tick 调** ——
                // `pump` 用 `PeekMessageW`（非阻塞），不调的话窗口不响应
                // 任何输入，表现为「界面卡死」（而业务其实还在跑）。
                //
                // 返回值忽略：用户关窗口只关界面，daemon 继续在后台接收。
                #[cfg(windows)]
                if let Some(p) = panel.as_mut() {
                    // resize 必须转给渲染器：swap chain 的 back buffer 还是旧尺寸，
                    // 继续 Present 会被 DWM 拉伸（表现为「拖动窗口时画面变形」）。
                    if let Some((vw, vh)) = p.pump() {
                        if let Some(r) = renderer.as_mut() {
                            match r.resize(vw, vh) {
                                Ok(()) => info!(width = vw, height = vh, "渲染器已跟随窗口尺寸"),
                                Err(e) => warn!(error = %e, width = vw, height = vh, "渲染器 resize 失败"),
                            }
                        }
                    }
                }

                // 1) FEC 组状态回收（实测发现的真实内存泄漏）
                //
                // `FecReceiver` 的 `groups` / `group_meta` / `frame_meta`
                // 三张表**此前从未被清理过** —— daemon 里一次都没调过清理函数。
                //
                // 实测：`tracked_groups` 与 `frames_in` 严格 1:1
                // （30fps×20s → 601:601；60fps×100s → 5973:5973），
                // 内存 3.56 → 55.45 MB 单调上涨不回落。
                //
                // 64KB 帧切 55 片时单帧约 64KB，30fps 下每秒 1.9MB，
                // 十分钟 1.1GB —— 不会立刻崩，但会把跑了半天的接收端
                // 拖进 GC 式卡顿。
                //
                // 保留 32 帧（约 1 秒 @30fps）足够覆盖任何合理的乱序与重传。
                if let Some(rx) = fec_rx.as_mut() {
                    let pruned = rx.prune_stale(highest_completed, 32);
                    if pruned > 0 {
                        debug!(pruned, tracked = rx.tracked_groups(), "回收过期 FEC 组");
                    }
                }

                // 1a) 快速缺片检测：后续帧已到，缺片的帧不会补齐，立刻报缺
                for (stream, seq) in asm.poll_stale(highest_completed, 3) {
                    nackgen.mark_missing(seq, now);
                    stats.expired_partials.fetch_add(1, Ordering::Relaxed);
                    stats.frames_dropped_missing.fetch_add(1, Ordering::Relaxed);
                    missing_frames_in_window += 1;
                    debug!(stream, seq, "快速缺片");
                }

                // 1b) 兜底：超时检测（应对"最后一帧就缺片"这种没有后续帧可参照的情况）
                for (stream, seq) in asm.expire(now) {
                    nackgen.mark_missing(seq, now);
                    stats.expired_partials.fetch_add(1, Ordering::Relaxed);
                    stats.frames_dropped_missing.fetch_add(1, Ordering::Relaxed);
                    missing_frames_in_window += 1;
                    debug!(stream, seq, "重组超时");
                }

                // 2) 发出到期的 NACK
                if let Some(req) = nackgen.poll(now, 0) {
                    if let Some(p) = peer {
                        let payload = req.to_bytes();
                        if let Ok(bytes) = encode_frame_owned(&FrameSpec::new(FrameType::Nack, &payload)) {
                            if socket.send_to(&bytes, p).await.is_ok() {
                                stats.nack_sent.fetch_add(1, Ordering::Relaxed);
                                stats.seqs_requested.fetch_add(req.len() as u64, Ordering::Relaxed);
                            }
                        }
                    }
                }

                // 3) 拥塞控制决策
                //
                // # 为什么放在 1ms tick 里而不是 2 秒统计块里（实测踩坑）
                //
                // 最初把 `cc.update()` 放在 `last_report.elapsed() >= report_every`
                // （2 秒）分支里，理由是「码率调整不需要那么频繁」。
                //
                // 但延迟窗口只有 400ms 跨度。每 2 秒决策一次意味着：
                //
                //     0ms ─────── 400ms ─────────────────── 2000ms
                //     │ 窗口覆盖 │      1.6 秒的样本从未参与决策  │
                //     │          │                              │
                //     └─ 决策 ───┘                              └─ 下次决策
                //
                // 一次持续 500ms 的拥塞如果落在两次决策之间，**完全被忽略**。
                //
                // 所以改成每个 tick（1ms）都决策。开销可忽略（几十条浮点运算），
                // 而响应延迟从「最多 2 秒」降到「1 毫秒」。
                //
                // 注意：`update` 内部的 `dt_sec` 会因此变得很小（1ms），
                // 加性增的增量也按比例变小 —— 这是正确的，增速按时间守恒。
                let now_cc = now;
                let denom_cc = received_frames_in_window + missing_frames_in_window;
                let frame_loss_cc = if denom_cc == 0 {
                    0.0
                } else {
                    missing_frames_in_window as f64 / denom_cc as f64
                };
                let avg_frags_cc = if received_frames_in_window == 0 {
                    1.0
                } else {
                    (fragments_in_window as f64 / received_frames_in_window as f64).max(1.0)
                };
                let loss_cc = if frame_loss_cc <= 0.0 {
                    0.0
                } else if frame_loss_cc >= 1.0 {
                    1.0
                } else {
                    1.0 - (1.0 - frame_loss_cc).powf(1.0 / avg_frags_cc)
                };
                let (p50_cc, _, _) = net_latency.summary();
                // 关键：只有「统计窗口刚好滚过一次」才算新数据。
                //
                // # 为什么不能用「收到新包」当新鲜判据（第六个坑）
                //
                // 第一版用 `received_frames_in_window != last` 判断，结果
                // 每 33ms 来一帧就判一次「新鲜」，10 秒里降了 44 次 ——
                // 因为丢包率是**累积统计**，一个新包不会改变它的含义。
                //
                // 正确判据：窗口计数器被**清零重算**过，才算新统计。
                let window_id = window_epoch;
                let fresh = window_id != last_cc_window;
                last_cc_window = window_id;
                let decision = cc.update(
                    &Feedback {
                        loss_ratio: loss_cc,
                        rtt_ms: p50_cc,
                        fresh,
                    },
                    now_cc,
                );
                // 只有 fresh 的决策才算「真决策」—— 非 fresh 时控制器
                // 只看不发，计数进去会让 cc_decisions 变成 tick 数（虚高）
                if fresh {
                    cc_decisions += 1;
                    if matches!(
                        decision.reason,
                        sidelens_core::congestion::Reason::DelayGradient
                            | sidelens_core::congestion::Reason::Loss
                            | sidelens_core::congestion::Reason::DelayAndLoss
                    ) {
                        cc_decreases += 1;
                    }
                }

                // 4) 音频出队 —— **必须在视频之前**。
                //
                // 顺序不能反：音频是主时钟，视频这一 tick 该不该播取决于
                // 音频已经播到哪儿。先放视频再推进音频时钟，视频永远拿
                // 上一 tick 的音频位置做裁决，等于凭空引入一个 tick 的偏差。
                //
                // pop_due 每次只出一帧，所以这里循环取到 None —— 这样每帧
                // 之间都能单独喂 on_audio，而不会被一次性灌一堆。
                if let Some(offset) = audio_clock_offset_us {
                    // 本地时钟 → 发送端时基
                    let send_timebase_now =
                        (now as i64 - offset).max(0) as u64;
                    if let Some(b) = audio_jb.as_mut() {
                        // # 关键：播放时钟必须整体推后 target_ms
                        //
                        // `AudioJitterBuffer` 的文档明确写了它**不**自动加
                        // 这个延迟 —— 真正的延迟由上层在调用 pop_due 时体现。
                        //
                        // 不推后的后果（实测踩过）：音频帧到达时它的 PTS 已经
                        // 「到期」（本机与发送端时基只差 2ms），于是每帧一到
                        // 就被立刻取走，缓冲深度恒为 0，`audio_depth_ms=0.0/0.0`。
                        // 那等于没有抖动缓冲：任何一帧乱序或迟到都直接变成
                        // 爆音，而且 AvSync 的主时钟会跟着网络到达节奏抖动。
                        // target_ms 是 f64（Opus 的 2.5ms 帧长逼出来的），
                        // 换算成微秒时四舍五入，避免 40.0 * 1000.0 的浮点误差
                        let target_us = (b.target_ms() * 1000.0).round() as u64;
                        let playout_now = send_timebase_now.saturating_sub(target_us);
                        while let Some(af) = b.pop_due(playout_now) {
                            // # 解码放在这里而不是收包处
                            //
                            // 三个候选位置（见 docs/opus.md）：
                            // 1. 收到即解 —— 解码在网络线程，阻塞收包
                            // 2. **抖动缓冲出队才解**（当前选择）—— 解码在播放路径，天然节流
                            // 3. 独立解码任务 —— 多一个队列、多一次拷贝
                            //
                            // 选 2 的理由：抖动缓冲已经知道「什么时候该播」，
                            // 在这里解码不需要额外的同步机制。而且解码耗时
                            // （8KB 帧 8.74µs 量级）远小于 20ms 帧间隔，
                            // 放在 1ms tick 里不会拖慢 tick。
                            //
                            // # 解码失败怎么办
                            //
                            // **不能让 tick 卡住**：一帧坏数据不该影响后续播放。
                            // 失败就跳过这一帧的 on_audio —— 主时钟少推进 20ms，
                            // 视频会短暂等待，比整条链路停摆好。
                            // 解码出的采样数（供后面的声卡写入用）。
                            //
                            // # 为什么单独存一个变量
                            //
                            // `r` 只在 `Ok(r)` 分支里可见，而声卡写入在分支外
                            // （要保证「解码失败也要推进主时钟」的语义不变）。
                            let mut decoded_samples: usize = 0;
                            if let Some(d) = opus_dec.as_mut() {
                                match d.decode(Some(&af.payload), &mut opus_pcm) {
                                    Ok(r) => {
                                        stats.audio_decoded.fetch_add(1, Ordering::Relaxed);
                                        if r.concealed {
                                            stats.audio_concealed.fetch_add(1, Ordering::Relaxed);
                                        }
                                        decoded_samples = r.samples;
                                    }
                                    Err(e) => {
                                        stats.audio_decode_failed.fetch_add(1, Ordering::Relaxed);
                                        debug!(seq = af.seq, error = %e, "Opus 解码失败，跳过本帧");
                                        continue;
                                    }
                                }
                            }
                            // 推进主时钟：AvSync 用 pts + duration 定位
                            // 推进主时钟：AvSync 用 pts + duration 定位
                            //
                            // 用「发送端时基的当前时刻」推进主时钟 —— 不是直接用 PTS。
                            // 见 `AvSync::clock_offset_us` 的文档：`pop_due` 的 while
                            // 循环会在某个 tick 一次补出多帧音频，若 clock 取
                            // 「已出队帧的 PTS」，它会阶梯式跳跃（实测单次跳 60ms），
                            // 让 drift 抖动到 -43ms 并最终稳定超过 -120ms 阈值。
                            av.on_audio_at(af.pts_us, af.duration_us, Some(send_timebase_now));
                            // 送到声卡（第四十一轮接入）。
                            //
                            // # 为什么放在解码之后、`audio_played` 之前
                            //
                            // 解码出的 PCM 是这一帧的最终形态；写入声卡是「播放」的
                            // 实际动作。放在 `audio_played += 1` 之前，语义上
                            // 「写成功才算播了」。
                            //
                            // # 写入失败/缓冲满怎么办
                            //
                            // **不能让 tick 卡住**：音频一帧 20ms，缓冲满说明声卡跟不上，
                            // 丢掉这一帧比阻塞整个接收循环好。`write` 是非阻塞的，
                            // 返回 `Ok(0)` 表示没写进去（不计失败，计 stall）。
                            // 面板的「启用音频」开关：关掉时**只跳过写入**，
                            // 不跳过解码与统计 —— 用户关音频是为了静音，
                            // 不是为了让音频链路的计数变成 0（那样看不出问题）。
                            #[cfg(windows)]
                            let audio_muted = panel.as_ref().is_some_and(|p| !p.audio_enabled());
                            #[cfg(not(windows))]
                            let audio_muted = false;
                            #[cfg(windows)]
                            if let Some(sink) = audio_sink.as_mut().filter(|_| !audio_muted) {
                                // `samples` 是**每声道**采样数（见 opus 模块文档：
                                // 单声道时 960，立体声时也是 960 而不是 1920）。
                                let n = decoded_samples * opus_cfg.channels as usize;
                                let n = n.min(opus_pcm.len());
                                match sink.write(&opus_pcm[..n]) {
                                    Ok(0) => audio_write_stalls += 1,
                                    Ok(_) => {}
                                    Err(e) => {
                                        if audio_write_errors % 60 == 0 {
                                            debug!(error = %e, "音频写入失败");
                                        }
                                        audio_write_errors += 1;
                                    }
                                }
                            }
                            stats.audio_played.fetch_add(1, Ordering::Relaxed);
                        }

                        // 缓冲深度每 tick 都采一次（而不是只在出队后采）。
                        // 定义为排队中所有帧的时长之和，不是 PTS 跨度 ——
                        // 跨度会把丢包留下的空洞算成存货，让日志误以为
                        // 还有音频可播，实际那段时间根本没有数据。
                        audio_depth.push(b.depth_ms(), 4000);
                    }
                }

                // 渲染窗口的消息泵（第三十九轮）。
                //
                // 必须在 tick 里调 —— GDI 的 `StretchDIBits` 不泵消息的话，
                // 窗口会「未响应」（系统在 5 秒后把标题栏变灰）。
                //
                // 用户关窗时 `pump_events` 返回 false：销毁渲染器但**不中断**
                // 接收 —— 关窗只是不想看画面，不该把服务停了。
                #[cfg(windows)]
                if let Some(r) = renderer.as_mut() {
                    if !r.pump_events() {
                        info!("渲染窗口已关闭，停止显示（接收继续）");
                        renderer = None;
                    }
                }

                // 5) 视频出队 + AvSync 裁决
                //
                // 先处理上一 tick 因「超前」被暂存的帧：它们现在可能已经
                // 到期了。放回队列头部之前必须按 PTS 重新插入 —— 队列
                // 内部按 PTS 有序，直接 push_back 会破坏这个不变量。
                while let Some(frame) = jb.pop() {
                    match av.judge(frame.pts_us) {
                        SyncVerdict::Play => {
                            // 解码（第三十八轮接入）。
                            //
                            // 这是 `frame.payload` 第一次被真正使用 —— 此前
                            // 视频帧出队后只统计延迟就丢弃了。
                            #[cfg(windows)]
                            if let Some(dec) = video_dec.as_mut() {
                                // 尺寸必须在 `decode` **之后**取（第四十轮修）。
                                //
                                // # 为什么不能在 decode 之前
                                //
                                // 解码器建的时候用的是**候选尺寸**（`SIDELENS_H264_WIDTH/HEIGHT`
                                // 或默认 1280x720），要等首个 SPS 才知道码流真实分辨率。
                                //
                                // 实测（640x360 码流 + 默认 1280x720 候选）：
                                //
                                // ```text
                                // decode 前: output_size() = (1280, 720)
                                // 实际输出: nv12_len = 353280 = 640*368*1.5
                                // → 按 (1280,720) 校验要 1382400 字节 → 失败
                                // ```
                                //
                                // 后果是首帧渲染失败一次（`render_failed=1`）。
                                // 之后每帧都重新取尺寸，所以第二帧起正常。
                                //
                                // # 借用冲突怎么办（E0502）
                                //
                                // `decode` 返回的 `&[u8]` 借用了 `dec`。解法是
                                // **先把长度和尺寸拷出来，再调 renderer** ——
                                // 渲染器是另一个对象，不冲突。
                                // 尺寸快照：每帧解码**前**取一次，用于渲染校验。
                                //
                                // # 为什么还是取「前」的（第四十轮权衡）
                                //
                                // `decode` 返回的 `&[u8]` 借用了 `dec`，之后调
                                // `dec.output_size()` 会 E0502。所以只能在 decode 前取。
                                //
                                // 代价：**首帧**取到的还是候选尺寸（如 1280x720），
                                // 而实际输出是码流真实尺寸（640x360）→ 校验失败 → 跳过渲染。
                                // 第二帧起尺寸已重协商，正常。实测只丢首帧 1 帧画面。
                                let (coded_w, coded_h) = dec.output_size();
                                let (disp_w, disp_h) = dec.output_display_size();
                                match dec.decode(&frame.payload, frame.pts_us) {
                                    Ok(nv12) => {
                                        video_decoded += 1;
                                        // NV12 已就绪。渲染层（`sidelens-render`）
                                        // 可以吃它，但当前 daemon 还没有消费端 ——
                                        // 解码结果在这里「落地」但不显示。
                                        // 渲染（第三十九轮接入）。
                                        //
                                        // # 为什么在这里而不是「解码后立刻」
                                        //
                                        // 已经在 `SyncVerdict::Play` 分支里了 ——
                                        // 这一帧确定要播，所以解码 + 渲染都是必要的。
                                        //
                                        // # 为什么用 `output_display_size()` 而不是 `output_size()`
                                        //
                                        // `output_size()` 是 coded size（16 的倍数），
                                        // 640x360 会变成 640x368、1080p 会变成 1920x1088。
                                        // 按 coded 出图会在底部多一条绿边。
                                        // `Nv12View` 同时带两个尺寸，`to_rgba` 只转 display 区域。
                                        if let Some(r) = renderer.as_mut() {
                                            match sidelens_render::Nv12View::new(
                                                nv12,
                                                (coded_w, coded_h),
                                                (disp_w, disp_h),
                                            ) {
                                                Ok(view) => {
                                                    if let Err(e) = r.present(&view) {
                                                        // 渲染失败不该中断接收 ——
                                                        // 用户可能只是把窗口关了。
                                                        render_failed += 1;
                                                        match e {
                                                            RenderFailure::DeviceRemoved { hresult } => {
                                                                // 设备丢失是**唯一可恢复**的错误，
                                                                // 而且不重建就永久黑屏。
                                                                // 触发重建，最多连续试 3 次（避免死循环）。
                                                                render_recoveries += 1;
                                                                if render_recoveries <= 3 {
                                                                    warn!(
                                                                        hresult = format!("0x{hresult:08X}"),
                                                                        attempt = render_recoveries,
                                                                        "GPU 设备丢失，尝试重建渲染器"
                                                                    );
                                                                    match r.recover() {
                                                                        Ok(_hwnd) => {
                                                                            // 新窗口句柄暂时用不到 —— 子窗口模式下
                                                                            // 渲染器自己记住了挂载位置，父窗口不变。
                                                                            // 独立窗口模式下 hwnd 变了，但 daemon
                                                                            // 不持有它（只有渲染器自己用）。
                                                                            info!("渲染器已重建");
                                                                        }
                                                                        Err(err) => {
                                                                            warn!(error = %err, "重建失败，本帧放弃渲染");
                                                                        }
                                                                    }
                                                                } else if render_recoveries == 4 {
                                                                    warn!("重建连续失败 3 次，停止尝试（降级为只解码不渲染）");
                                                                }
                                                            }
                                                            RenderFailure::Other(m) => {
                                                                if render_failed % 60 == 0 {
                                                                    debug!(error = %m, "渲染失败");
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                                Err(e) => {
                                                    // 前 3 次用 warn 打全信息 —— 首帧失败是已知现象
                                                    // （解码器尺寸协商未完成），需要能看见具体数值。
                                                    if render_failed < 3 {
                                                        warn!(
                                                            coded_w, coded_h,
                                                            disp_w, disp_h,
                                                            nv12_len = nv12.len(),
                                                            expect_len = (coded_w as usize) * (coded_h as usize) * 3 / 2,
                                                            error = %e,
                                                            "NV12 视图创建失败（尺寸不匹配？）"
                                                        );
                                                    }
                                                    render_failed += 1;
                                                }
                                            }
                                        }
                                    }
                                    Err(DecodeError::NeedKeyframe { .. }) => {
                                        // 中途接入 / 丢关键帧：请求一个，不计成失败。
                                        video_need_keyframe += 1;
                                        // 立即请求，但**限流**：两个请求之间至少隔
                                        // `KEYFRAME_REQ_MIN_INTERVAL_US`。
                                        //
                                        // # 为什么不能每帧都请求（第一版是 % 30）
                                        //
                                        // 每 30 次才请求一次太慢：5% 丢包 + 1280x720 下
                                        // 实测 `need_keyframe=107`（约 2 个 GOP 全毁），
                                        // 而每 30 帧才请求一次意味着要等 500ms 才可能恢复。
                                        //
                                        // # 为什么不能无限制请求
                                        //
                                        // 关键帧比 P 帧大 30 倍（39KB vs 1.2KB），无限制请求
                                        // 会把链路塞满，反而让更多关键帧丢掉 —— 正反馈。
                                        let now_req = sidelens_proto::now_us();
                                        let can_request = last_keyframe_req_us
                                            .map(|t| now_req.saturating_sub(t) >= KEYFRAME_REQ_MIN_INTERVAL_US)
                                            .unwrap_or(true);
                                        if can_request {
                                            last_keyframe_req_us = Some(now_req);
                                            let payload = serde_json::json!({
                                                "reason": "decode-need-keyframe"
                                            })
                                            .to_string();
                                            if let Ok(bytes) = encode_frame_owned(
                                                &FrameSpec::new(
                                                    FrameType::KeyframeReq,
                                                    payload.as_bytes(),
                                                ),
                                            ) {
                                                if let Some(p) = peer {
                                                    let _ = socket.send_to(&bytes, p).await;
                                                }
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        video_decode_failed += 1;
                                        if video_decode_failed % 60 == 1 {
                                            debug!(
                                                seq = frame.seq,
                                                error = %e,
                                                "视频解码失败（前几次会打印）"
                                            );
                                        }
                                    }
                                }
                            }
                            av_synced += 1;
                            let played_ms = sidelens_proto::now_us()
                                .saturating_sub(frame.send_ts_us) as f64 / 1000.0;
                            playable.push(played_ms, 4000);
                            let held_ms = sidelens_proto::now_us()
                                .saturating_sub(frame.arrived_at_us) as f64 / 1000.0;
                            hold.push(held_ms, 4000);
                        }
                        SyncVerdict::Drop => {
                            // 视频落后音频太多：丢弃。人眼对掉一帧几乎无感，
                            // 而硬播会让口型明显对不上。
                            stats.video_dropped_late.fetch_add(1, Ordering::Relaxed);
                        }
                        SyncVerdict::Wait => {
                            // 视频超前：暂存到下个 tick 再裁决，不直接丢。
                            stats.video_waited.fetch_add(1, Ordering::Relaxed);
                            if video_waiting.len() < VIDEO_WAIT_MAX {
                                video_waiting.push_back(frame);
                            }
                            // 超上限时直接放行（不 push），否则音频断流期间
                            // 视频会永远等下去，延迟无限增长
                        }
                    }
                }

                // 把暂存帧放回视频抖动缓冲，等下个 tick 重新裁决。
                //
                // 用 push_at 而不是 push：push 会把 arrived_at_us 刷成当前
                // 时刻，于是这一帧的「已持有时间」被清零，要重新等满
                // target_ms 才能出队 —— 表现就是每次 Wait 都白等一个 target。
                while let Some(frame) = video_waiting.pop_front() {
                    jb.push_at(frame, now);
                }
            }

            // 收包
            res = receiver.recv_datagram(&mut asm, &mut buf) => {
                let Some(dg) = res else { continue };
                peer = Some(dg.from);

                // FEC：每个**分片到达时**就喂给 FecReceiver。
                //
                // 这是本轮最关键的一处修正 —— 之前把喂 FEC 的代码放在
                // 「重组完成后」的分支里，那时 `f.payload` 已是拼好的整帧，
                // 分片头早没了，`push_fragment` 拿到的是错误的数据段，
                // 恢复必然失败（实测 `fec_recovered=0`）。
                //
                // **音频不走 FEC**：FEC 的分组是跨帧的，而音频帧只有 20ms，
                // 为它攒一个组要等 10 帧 = 200ms，比丢一帧的代价高一个量级。
                // 而且音频不重传，FEC 恢复出来的帧也没有重传链路去兜底。
                if dg.ty == FrameType::Video {
                    if let Some(rx) = fec_rx.as_mut() {
                        // # 必须在这里回收组状态（本轮修的第二个真 bug）
                        //
                        // 生产路径此前**从未**调用过任何清理函数：
                        // `drop_before_frame` 只出现在测试与 `e2e_pipeline` 里。
                        // 于是 `groups` / `group_meta` / `frame_meta` 三张表
                        // 随帧数无限增长。
                        //
                        // 实测（`perf-audit` 的计数器）：`tracked_groups()` 与 `frames_in`
                        // 严格 1:1 增长 —— 30fps/20s 是 601 个组，60fps/100s 是 5973 个，
                        // 永不下降；进程私有内存同步涨 0.27-0.52 MB/s。64KB 帧（55 片）
                        // 更狠：单帧就是一个完整 FecDecoder 的分片拷贝，约 64KB。
                        //
                        // 保留 8 帧（约 260ms @30fps）：远大于乱序跨度，
                        // 又远小于内存问题的量级。
                        fec_groups_pruned += rx.prune_stale(dg.seq, FEC_KEEP_FRAMES) as u64;
                        rx.push_fragment_with_meta(sidelens_core::FragmentInfo {
                            frame_seq: dg.seq as u16,
                            index: dg.fragment_index as usize,
                            data: &dg.data,
                            count: dg.fragment_count,
                            total_size: dg.total_size,
                            pts_us: dg.pts_us,
                            send_ts_us: dg.send_ts_us,
                            flags: dg.flags,
                        });
                    }
                }

                let Some(inbound_frame) = dg.completed else { continue };
                let mut f = inbound_frame;

                // ── 重放检查 + 解密（必须在所有记账之前）──────────────
                //
                // # 为什么顺序是「重放 → 解密」而不是反过来
                //
                // 重放帧的密文是**合法的**：密钥对、标签对，解密一定成功。
                // 所以解密先做的话，重放帧会先花一次 CPU 再被丢弃，而且
                // 统计上会出现「解密成功但帧被丢」这种费解的现象。
                //
                // 重放检测不需要密钥，放在最前面最省。
                //
                // # 为什么在这里而不是更早
                //
                // 这里拿到的 `f.payload` 是**重组后的完整载荷**（密文 + 16B 标签）。
                // 更早的位置（`dg.data`）是单个分片的数据段，解密需要完整载荷。
                //
                // # FEC 的密文问题
                //
                // FEC 恢复出的分片也是密文的一部分 —— 因为它作用在分片级，
                // 而分片是加密之后切的。所以 FEC 与加密天然兼容，不需要
                // 任何特殊处理。这也是「加密必须在分片之前」的直接推论。
                if let Some(s) = sealer.as_mut() {
                    if f.ty.is_media() {
                        // 1. 重放检查
                        if let Err(e) = replay.check_and_mark(f.stream_id, f.seq) {
                            stats.replay_rejected.fetch_add(1, Ordering::Relaxed);
                            debug!(seq = f.seq, stream = f.stream_id, error = %e, "重放包已丢弃");
                            continue;
                        }

                        // 2. 解密
                        let head = FrameHead {
                            ty: f.ty as u8,
                            flags: f.flags,
                            stream_id: f.stream_id,
                            seq: f.seq,
                            pts_us: f.pts_us,
                            send_ts_us: f.send_ts_us,
                        };
                        if let Err(e) = s.open_payload(&head, &mut f.payload) {
                            stats.decrypt_failed.fetch_add(1, Ordering::Relaxed);
                            debug!(seq = f.seq, error = %e, "解密失败，丢弃本帧");
                            continue;
                        }
                    }
                }

                // ── 音频分流（必须放在所有视频记账之前）─────────────
                //
                // # 为什么必须先分流（实测踩到的真 bug）
                //
                // 音频与视频的 seq 是**各自独立的计数器**，但两者共用下面
                // 那套按 seq 记账的结构。音频 50 帧/秒、视频 30 帧/秒，
                // 音频 seq 很快就跑到视频前面去，于是：
                //
                // 1. `highest_completed` 被音频 seq 抬高 →
                //    `asm.poll_stale()` 立刻认为所有在途的视频半成品都
                //    「不可能再补齐」→ 全部判缺片发 NACK。实测 5 秒里
                //    expired=48 / dropped=48，而链路丢包只有 2%。
                // 2. `nackgen.mark_received(f.seq)` 用一个不相干的命名空间
                //    撤销视频的缺失记录 → 真丢的视频包被静默「治愈」。
                // 3. `bitrate` / `received_frames_in_window` 把音频算进
                //    视频帧率 → rx_fps 显示 79（= 30 视频 + 50 音频），
                //    拥塞控制的平均分片数也被音频的 2 片拉低。
                //
                // 所以音频在这里就地处理完并 continue，不碰下面任何一项。
                if f.ty == FrameType::Audio {
                    stats.audio_frames.fetch_add(1, Ordering::Relaxed);
                    let now = sidelens_proto::now_us();

                    // 建立本地 ↔ 发送端时基的偏移（只在第一帧确立）
                    if audio_clock_offset_us.is_none() {
                        audio_clock_offset_us = Some(now as i64 - f.pts_us as i64);
                        info!(
                            offset_us = audio_clock_offset_us.unwrap_or(0),
                            first_pts_us = f.pts_us,
                            "音频时基已对齐（本地时钟 - 发送端 PTS）"
                        );
                    }

                    // `with_arrival` 把本地到达时刻一起填进去 ——
                    // 音频抖动缓冲的 `adapt()` 靠它算网络抖动。
                    //
                    // # 为什么必须填（不填会让自适应反向工作）
                    //
                    // `AudioJitterBuffer::observe_jitter` 用
                    // `|(R_i - R_{i-1}) - (S_i - S_{i-1})|` 算抖动，
                    // `R` 来自 `arrived_at_us`、`S` 来自 `pts_us`。
                    //
                    // 不填时 `arrived_at_us == 0` 是哨兵值，样本被跳过 ——
                    // 于是 `jitter_ewma` 永远是 0，`adapt()` 会把 `target_ms`
                    // 一路压到下限 20ms。这在干净链路上是好事，
                    // 但链路变差时缓冲已经太浅，开始丢帧且无法自愈。
                    let arrived = sidelens_proto::now_us();
                    let af = AudioFrame::with_arrival(
                        f.pts_us,
                        // duration 由协议帧带不出来，用 Opus 配置反算。
                        // 帧长可配（`--opus-frame-ms`），所以必须用 `opus_cfg`
                        // 而不是硬编码 20000 —— 见第三十一轮的修复。
                        opus_cfg.frame_duration_us(),
                        f.seq,
                        arrived,
                        f.payload,
                    );

                    match audio_jb.as_mut().map(|b| b.push(af)) {
                        Some(PushOutcome::Ok) => {}
                        Some(PushOutcome::Late) => {
                            stats.audio_dropped_late.fetch_add(1, Ordering::Relaxed);
                            debug!(seq = f.seq, "音频帧迟到，已丢弃");
                        }
                        Some(PushOutcome::Overflow) => {
                            stats.audio_overflow.fetch_add(1, Ordering::Relaxed);
                            debug!(seq = f.seq, "音频缓冲溢出，丢了最旧一帧");
                        }
                        Some(PushOutcome::Discontinuity) => {
                            stats.audio_discontinuity.fetch_add(1, Ordering::Relaxed);
                            // 时间线断了：缓冲已自愈，但 AvSync 的主时钟还停在
                            // 旧时间线上，必须一并重置，否则视频会按一个已经
                            // 不存在的音频位置对齐。
                            av.reset();
                            audio_clock_offset_us = Some(now as i64 - f.pts_us as i64);
                            warn!(
                                seq = f.seq,
                                pts_us = f.pts_us,
                                "音频时间戳跳变，已重建缓冲与音画同步基准"
                            );
                        }
                        None => {}
                    }
                    continue;
                }

                match f.ty {
                    FrameType::Video | FrameType::Retransmit => {
                        stats.frames_in.fetch_add(1, Ordering::Relaxed);
                        highest_completed = highest_completed.max(f.seq);

                        let now = sidelens_proto::now_us();
                        let lat_ms = now.saturating_sub(f.send_ts_us) as f64 / 1000.0;
                        net_latency.push(lat_ms, 4000);

                        // 喂给拥塞控制器：
                        // send_ts_us 是发送端时间戳，now 是本地时间戳。
                        // 两者不同源（差一个固定偏移），但最小二乘斜率
                        // 会把偏移消掉 —— 前提是偏移恒定。
                        cc.add_delay_sample(f.send_ts_us, now);

                        received_frames_in_window += 1;
                        // 估算这一帧的分片数（向上取整，至少 1）
                        let frags = f
                            .payload
                            .len()
                            .div_ceil(sidelens_core::SAFE_MTU_PAYLOAD)
                            .max(1);
                        fragments_in_window += frags;

                        // 记录实际到达的字节数
                        bitrate.record(
                            f.payload.len(),
                            sidelens_proto::now_us(),
                            f.is_keyframe(),
                        );

                        if f.is_keyframe() {
                            keyframes_in_window += 1;
                        }

                        // 收到就撤销 NACK 记录（乱序自愈）
                        nackgen.mark_received(f.seq);


                        let outcome = jb.push(QueuedFrame {
                            ty: f.ty,
                            seq: f.seq,
                            pts_us: f.pts_us,
                            send_ts_us: f.send_ts_us,
                            arrived_at_us: 0,
                            flags: f.flags,
                            // 重传帧不参与抖动估计（见 QueuedFrame::is_retransmit）
                            is_retransmit: f.ty == FrameType::Retransmit,
                            payload: f.payload,
                        });

                        if outcome.needs_keyframe {
                            let payload = serde_json::json!({"reason": "jitter-overflow"}).to_string();
                            if let Ok(bytes) = encode_frame_owned(&FrameSpec::new(FrameType::KeyframeReq, payload.as_bytes())) {
                                if let Some(p) = peer {
                                    let _ = socket.send_to(&bytes, p).await;
                                }
                            }
                        }
                    }
                    FrameType::Fec => {
                        // 冗余包：解出 FecPacket，交给 FecReceiver 尝试恢复。
                        //
                        // 恢复出来的分片要**送回重组器** —— 这一步是关键，
                        // 否则 FEC 白做了：重组器不知道这些分片已经可用。
                        let Some(pkt) = FecPacket::decode(&f.payload) else {
                            debug!("FEC 包解析失败，丢弃");
                            continue;
                        };
                        fec_reds_in_window += 1;
                        let Some(rx) = fec_rx.as_mut() else { continue };
                        let recovered = rx.push_redundant(&pkt);

                        // 恢复出的分片必须**送回重组器**，否则 FEC 等于白做 ——
                        // 重组器不知道这些分片已经可用，会一直等它们，
                        // 最后还是靠 NACK 补，白白浪费了 FEC 的 0 延迟优势。
                        //
                        // 恢复出的载荷长度是「组内最长包」长度（尾部补 0），
                        // 所以要用分片头里的 `total_size` 反算真实长度 —— 见下。
                        // 恢复出的分片先缓存起来，等该帧的分片头信息齐了再送回重组器。
                        //
                        // # 为什么不能立刻送回
                        //
                        // `DatagramPiece` 要求载荷开头是 8 字节分片头（`count` 与
                        // `total_size` 都在里面），而 FEC 只保护分片的**数据部分**。
                        // 恢复时我们手里没有这两个值 —— 它们在丢失的分片头里。
                        //
                        // 可行做法：从**同组已收到的其他分片**里读分片头，
                        // 拿到 `count` 与 `total_size`，再拼出完整分片。
                        // 这也是为什么 FEC 恢复必须和重组器共享状态。
                        // 恢复出的分片必须**送回重组器**，否则 FEC 等于白做 ——
                        // 重组器不知道这些分片已可用，会一直等，最后还是靠 NACK 补，
                        // 白白浪费了 FEC 的 0 延迟优势。
                        //
                        // 关键难点：FEC 只保护分片的**数据部分**，没有分片头。
                        // 而 `DatagramPiece` 需要 `count` 与 `total_size`。
                        // 这两个值从同组已收到的分片头里回读（`group_meta`）。
                        let gid = pkt.group_id();
                        let meta = fec_rx.as_ref().and_then(|r| r.group_meta(gid));
                        // 帧时间戳也必须还原，否则重组出的帧会被抖动缓冲判为「太迟」。
                        // 见 `FecReceiver::frame_meta` 的说明。
                        let ts = fec_rx.as_ref().and_then(|r| r.frame_meta(gid));
                        for (idx, data) in &recovered {
                            fec_recovered_total += 1;
                            let (Some((count, total_size)), Some((pts_us, send_ts_us, frame_flags))) =
                                (meta, ts)
                            else {
                                debug!(
                                    frame = pkt.frame_seq,
                                    idx,
                                    "FEC 恢复分片但缺元数据，无法重建"
                                );
                                continue;
                            };
                            // 重建完整分片：8 字节分片头 + 数据
                            let mut full =
                                Vec::with_capacity(sidelens_core::FRAGMENT_HEADER_SIZE + data.len());
                            sidelens_core::FragmentHeader {
                                index: *idx as u16,
                                count,
                                total_size,
                            }
                            .encode(&mut full);
                            full.extend_from_slice(data);

                            let piece = sidelens_core::DatagramPiece {
                                stream_id: 0,
                                seq: pkt.frame_seq as u32,
                                ty: FrameType::Video,
                                // 必须带上原帧的 flags（关键帧位），
                                // 否则恢复出的关键帧会被当成普通帧，
                                // needs_keyframe 逻辑失效 —— 见 FragmentInfo::flags
                                flags: sidelens_proto::FLAG_FRAGMENT | frame_flags,
                                pts_us,
                                send_ts_us,
                                payload: &full,
                            };
                            // 关键：重组出的帧必须进抖动缓冲，否则 FEC 补回来的帧
                            // 只是被计数，永远不会被播放 —— 用户看到的画面不变。
                            if let Some(mut frame) = asm.push(piece, sidelens_proto::now_us()) {
                                // # FEC 恢复的帧也必须解密（实测抓到的真 bug）
                                //
                                // 正常路径的解密在 `dg.completed` 之后，而 FEC
                                // 恢复走的是**另一条路**：分片由冗余包还原、直接
                                // 喂给重组器，重组结果根本没经过那段代码。
                                //
                                // 症状（负面测试暴露）：两端密钥故意不匹配时，
                                // `decrypt_failed=212` 却仍有 `av_synced=14` ——
                                // 那 14 帧是 FEC 恢复出来的**密文**，被当成明文
                                // 送进了抖动缓冲。画面上的表现是周期性花屏，
                                // 而且频率与丢包率正相关，极易被误判为"解码器问题"。
                                //
                                // 顺序同样必须是「先重放检查、再解密」，
                                // 与正常路径保持一致 —— 两条路的判据不能有两套。
                                if let Some(s) = sealer.as_mut() {
                                    if replay.check_and_mark(frame.stream_id, frame.seq).is_err() {
                                        stats.replay_rejected.fetch_add(1, Ordering::Relaxed);
                                        continue;
                                    }
                                    let head = FrameHead {
                                        ty: frame.ty as u8,
                                        flags: frame.flags,
                                        stream_id: frame.stream_id,
                                        seq: frame.seq,
                                        pts_us: frame.pts_us,
                                        send_ts_us: frame.send_ts_us,
                                    };
                                    if s.open_payload(&head, &mut frame.payload).is_err() {
                                        stats.decrypt_failed.fetch_add(1, Ordering::Relaxed);
                                        continue;
                                    }
                                }

                                stats.frames_in.fetch_add(1, Ordering::Relaxed);
                                highest_completed = highest_completed.max(frame.seq);
                                jb.push(QueuedFrame {
                                    ty: frame.ty,
                                    seq: frame.seq,
                                    pts_us: frame.pts_us,
                                    send_ts_us: frame.send_ts_us,
                                    arrived_at_us: 0,
                                    flags: frame.flags,
                                    is_retransmit: false,
                                    payload: frame.payload,
                                });
                            }
                            debug!(
                                frame = pkt.frame_seq,
                                group = pkt.group_in_frame,
                                idx,
                                len = data.len(),
                                "FEC 恢复分片已送回重组器"
                            );
                        }
                    }
                    FrameType::Config => {
                        // 发送端通知 FEC 配置（`group_size` 与 `redundant` 都可能变）。
                        //
                        // # 为什么不能整块重建 `FecReceiver`（本轮改的）
                        //
                        // 原来这里是 `fec_rx = Some(FecReceiver::new(new_cfg))`。
                        // `group_size` 时代这样没问题：它只在「帧变大」时涨一次，
                        // 一次会话几次而已。
                        //
                        // **`redundant` 自适应之后不一样了** —— 它会随丢包率估计
                        // 反复升降，而每次重建都会把 `group_meta` / `frame_meta`
                        // 一起清掉。那两张表存的是**帧级**信息（`count` /
                        // `total_size` / `pts_us` / `send_ts_us` / `flags`），
                        // 与 `group_size` / `redundant` 完全无关。清掉之后，
                        // 切换瞬间在途的那些组即使恢复出分片，也拼不出
                        // `DatagramPiece`（缺 `count` / `total_size`），只能丢弃。
                        //
                        // 改成 `FecReceiver::set_config`：只清 `groups`（每个解码器的
                        // 下标语义绑死了旧配置，必须重建），保留两张 meta 表。
                        //
                        // 代价仍然存在：切换时未完成的组作废。所以发送端必须有迟滞，
                        // 否则 Config 帧的频率会高到让 FEC 永远处于「刚被清空」的状态。
                        // 音频编解码器声明（第三十轮新增）。
                        //
                        // 接收端**无法从码流判断**音频载荷是 Opus 还是原始 PCM ——
                        // 两者都是不透明字节，协议帧头里也没有 codec 字段。
                        // 原来的做法是让用户设环境变量 `SIDELENS_OPUS=1`，
                        // 忘了设的后果是：Opus 码流被当成 16-bit PCM 播成噪声，
                        // 而 `audio_played` 照样增长 —— 统计数字看不出异常。
                        //
                        // 独立于 FEC 配置解析：即使 FEC 字段缺失或非法，
                        // codec 声明也应该生效（两者是无关的信息）。
                        if let Some(codec) = parse_audio_codec_payload(&f.payload) {
                            let want_opus = codec == "opus";
                            let has_dec = opus_dec.is_some();
                            if want_opus && !has_dec {
                                match OpusDecoder::new(opus_cfg) {
                                    Ok(d) => {
                                        opus_dec = Some(d);
                                        info!("按 Config 声明启用 Opus 解码（发送端说 audio_codec=opus）");
                                    }
                                    Err(e) => {
                                        warn!(error = %e, "Opus 解码器创建失败，将按原始 PCM 处理");
                                    }
                                }
                            } else if !want_opus && has_dec {
                                // 发送端改成 PCM 了：关掉解码器，否则会把 PCM 当 Opus 解
                                opus_dec = None;
                                info!("按 Config 声明关闭 Opus 解码（发送端说 audio_codec=pcm）");
                            }
                        }
                        // 帧长声明（第三十一轮新增）。
                        //
                        // 发送端 `--opus-frame-ms` 改了帧长之后，接收端必须跟着变：
                        // 解码器缓冲按 `frame_samples()` 分配，`AudioJitterBuffer`
                        // 按 `frame_duration_us()` 推进 AvSync 主时钟。两者用错
                        // 帧长的后果分别是「一帧都解不出来」和「音画逐渐错位」。
                        //
                        // 帧长变了要**重建**解码器与抖动缓冲 —— 它们把帧长存进了
                        // 内部状态（缓冲大小、duration），改不了只能换。
                        if let Some(ms) = parse_audio_frame_ms(&f.payload) {
                            if (ms - opus_cfg.frame_ms).abs() > 1e-9 {
                                let new_cfg = OpusConfig {
                                    frame_ms: ms,
                                    ..OpusConfig::default()
                                };
                                if new_cfg.validate().is_ok() {
                                    let old_ms = opus_cfg.frame_ms;
                                    opus_cfg = new_cfg;
                                    // 解码器：按新帧长重建（旧的缓冲大小不对）
                                    if opus_dec.is_some() {
                                        opus_dec = OpusDecoder::new(opus_cfg).ok();
                                    }
                                    opus_pcm = vec![0i16; opus_cfg.frame_samples() as usize];
                                    // 抖动缓冲：同理。target_ms 不变，但容量会按新
                                    // 帧长重算（`ceil(target/frame)+1`）。
                                    audio_jb =
                                        AudioJitterBuffer::new(opus_cfg, audio_target_ms).ok();
                                    info!(
                                        from_ms = old_ms,
                                        to_ms = ms,
                                        frame_samples = opus_cfg.frame_samples(),
                                        frame_duration_us = opus_cfg.frame_duration_us(),
                                        "按 Config 声明调整音频帧长，已重建解码器与抖动缓冲"
                                    );
                                } else {
                                    warn!(frame_ms = ms, "Config 里的帧长非法，保持当前配置");
                                }
                            }
                        }


                        match parse_fec_config_payload(&f.payload, &default_fec_config) {
                            Some(new_cfg) if new_cfg == fec_config => {
                                // 重复的 Config（发送端发了两份防丢），忽略
                                debug!(
                                    group = new_cfg.group_size,
                                    redundant = new_cfg.redundant,
                                    "FEC 配置未变，忽略重复的 Config 帧"
                                );
                            }
                            Some(new_cfg) => {
                                // 先记下旧值：下面 fec_config 会被覆盖，
                                // 覆盖后再读就是新值了（这个顺序错过一次）
                                let prev_group = fec_config.group_size;
                                let prev_redundant = fec_config.redundant;
                                // 热切换；`fec_rx` 为 None（默认配置非法）时补建一个
                                let applied = match fec_rx.as_mut() {
                                    Some(rx) => rx.set_config(new_cfg).is_ok(),
                                    None => match FecReceiver::new(new_cfg) {
                                        Ok(rx) => {
                                            fec_rx = Some(rx);
                                            true
                                        }
                                        Err(_) => false,
                                    },
                                };
                                if applied {
                                    fec_config = new_cfg;
                                    fec_config_updates += 1;
                                    *fec_group_usage
                                        .entry((new_cfg.group_size, new_cfg.redundant))
                                        .or_insert(0) += 1;
                                    info!(
                                        new_group = new_cfg.group_size,
                                        new_redundant = new_cfg.redundant,
                                        prev_group,
                                        prev_redundant,
                                        "FEC 配置已更新 group={} redundant={}",
                                        new_cfg.group_size,
                                        new_cfg.redundant
                                    );
                                } else {
                                    warn!(
                                        group = new_cfg.group_size,
                                        redundant = new_cfg.redundant,
                                        "FEC 配置非法，继续用旧配置"
                                    );
                                }
                            }
                            None => {
                                warn!("Config 帧里没有可用的 FEC 字段，忽略");
                            }
                        }
                    }
                    FrameType::Hello => {
                        info!(from = %peer.map(|p| p.to_string()).unwrap_or_default(), "握手");
                    }
                    FrameType::RttProbe => {
                        // 四时间戳协商：立刻回 t2/t3
                        let t2 = sidelens_proto::now_us();
                        if let Some(probe) = sidelens_core::RttProbe::from_bytes(&f.payload) {
                            // 顺便带上码率建议 —— 拥塞控制器在本端跑，
                            // 但执行码率的是发送端的编码器。见 `RttReply` 文档。
                            //
                            // `cc.target_bps()` 是控制器当前的决策值；刚启动
                            // 还没决策过时它返回初始值（不是 0），对发送端
                            // 也是合理的起点。
                            let reply = sidelens_core::RttReply::new(
                                probe.nonce,
                                probe.t1_us,
                                t2,
                                sidelens_proto::now_us(),
                            )
                            .with_rate_hint(cc.target_bps() as u64, loss_ratio_hint);
                            if let Ok(bytes) = encode_frame_owned(&FrameSpec::new(
                                FrameType::RttReply,
                                &reply.to_bytes(),
                            )) {
                                if let Some(p) = peer {
                                    let _ = socket.send_to(&bytes, p).await;
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        if last_report.elapsed() >= report_every {
            // 抖动缓冲自适应：按实测抖动调整 `target_ms`。
            //
            // # 为什么放在这里（而不是每 tick 都调）
            //
            // `adapt` 自带 200ms 驻留期，每 tick 调用也只在驻留期满后才动 ——
            // 但每 tick 调用意味着要每 tick 读一次 `now_us()`，
            // 而 `now_us()` 在 Windows 上是 `QueryPerformanceCounter`，
            // 1ms tick 下就是每秒 1000 次额外系统调用。
            //
            // 报告间隔是 2s，虽然比驻留期长 10 倍，但 `adapt` 是**幂等**的：
            // 它每次都从当前 `jitter_ewma` 重算目标值，漏掉的中间态不影响
            // 最终收敛点，只影响收敛速度（2s 一步，从 20 降到 10 需要 5 步）。
            //
            // # 为什么不放进 `fresh` 分支
            //
            // `fresh` 是「窗口纪元变了」，与抖动无关。抖动 EWMA 是跨窗口
            // 连续的，放进去会让调整频率跟着窗口边界跳。
            if let Some((old, new)) = jb.adapt(sidelens_proto::now_us()) {
                debug!(
                    from_ms = format!("{old:.2}"),
                    to_ms = format!("{new:.2}"),
                    jitter_ms = format!("{:.2}", jb.stats.avg_jitter_ms),
                    "抖动缓冲目标延迟已调整"
                );
            }

            // 音频抖动缓冲自适应（`audio-adapt` 子代理交付，本轮接入）。
            //
            // 与视频侧同一套思路但常量不同（见 `AudioJitterBuffer::adapt` 文档）：
            // 下限 20ms（视频 10）、单步降幅 10ms（视频 2）、驻留 500ms（视频 200）、
            // 安全系数 2.0（视频 2.5）。
            //
            // 音频比视频更怕 `target_ms` 震荡：它直接改变迟到窗口，
            // 把本该补播的帧判成迟到 —— 听感上是爆音。所以驻留期长 2.5 倍。
            if let Some(b) = audio_jb.as_mut() {
                if let Some((old, new)) = b.adapt(sidelens_proto::now_us()) {
                    debug!(
                        from_ms = format!("{old:.2}"),
                        to_ms = format!("{new:.2}"),
                        jitter_ms = format!("{:.2}", b.jitter_ms()),
                        "音频抖动缓冲目标深度已调整"
                    );
                }
            }

            // 帧级丢包率
            let denom = received_frames_in_window + missing_frames_in_window;
            let frame_loss = if denom == 0 {
                0.0
            } else {
                missing_frames_in_window as f64 / denom as f64
            };

            // 平均每帧分片数（窗口内实测，不是硬编码）
            let avg_frags = if received_frames_in_window == 0 {
                1.0
            } else {
                (fragments_in_window as f64 / received_frames_in_window as f64).max(1.0)
            };

            // 反解成包级丢包率：p = 1 - (1 - P)^(1/n)
            let loss_ratio = if frame_loss <= 0.0 {
                0.0
            } else if frame_loss >= 1.0 {
                1.0
            } else {
                1.0 - (1.0 - frame_loss).powf(1.0 / avg_frags)
            };
            // 存一份给 `RttReply` 用（见 `loss_ratio_hint` 的说明）
            loss_ratio_hint = loss_ratio;
            let (p50, p95, p99) = net_latency.summary();
            let (q50, q95, _) = playable.summary();
            let (h50, h95, _) = hold.summary();
            let s = stats.snapshot();
            let healed = nackgen.healed_before_request;
            let (a50, a95, _) = audio_depth.summary();
            let audio_target = audio_jb.as_ref().map(|b| b.target_ms()).unwrap_or(0.0);
            let audio_qlen = audio_jb.as_ref().map(|b| b.len()).unwrap_or(0);
            let audio_clock_ms = av
                .audio_clock_us()
                .map(|c| format!("{:.1}", c as f64 / 1000.0))
                .unwrap_or_else(|| "-".into());

            // 接收帧率（窗口内）
            let rx_fps = if window_reset_at.elapsed().as_secs_f64() > 0.0 {
                received_frames_in_window as f64 / window_reset_at.elapsed().as_secs_f64()
            } else {
                0.0
            };
            // 关键帧占比：为 0 说明一直没收到关键帧，画面无法起播
            let kf_ratio = if received_frames_in_window == 0 {
                0.0
            } else {
                keyframes_in_window as f64 / received_frames_in_window as f64
            };
            let kbps = format!("{:.0}", bitrate.bitrate_bps() / 1000.0);
            let target_kbps = cc.target_bps() / 1000.0;
            let cc_state = format!("{:?}", cc.state());
            let slope = cc.slope().unwrap_or(0.0);
            // 视频解码 + 渲染统计（第三十八/三十九轮）。
            //
            // # 为什么整块都放进 `cfg` 的 `let` 里（第四十轮修的）
            //
            // 原来的写法是「`cfg(windows)` 的 let + 无 cfg 的元组解构 +
            // `cfg(not(windows))` 的兜底」。**这在非 Windows 上编译失败**：
            //
            // ```text
            // error[E0425]: cannot find value `video_decoded` in this scope
            // error[E0425]: cannot find value `video_decode_failed` in this scope
            // error[E0425]: cannot find value `video_need_keyframe` in this scope
            // error[E0425]: cannot find value `video_dec` in this scope
            // ```
            //
            // 因为 `video_decoded` 等变量本身是 `cfg(windows)` 的 `let`，
            // 在非 Windows 上不存在；而那个元组解构没有门控，先报错。
            //
            // 兜底分支存在**不等于**配对正确 —— 这是 `xplat-audit` 子代理
            // 在 4 个非 Windows target 上实测出来的（`cargo check --target
            // x86_64-unknown-linux-gnu` 报 5 个 E0425）。
            #[cfg(windows)]
            let render_mode = renderer.as_ref().map(|r| r.mode_name()).unwrap_or("off");
            #[cfg(windows)]
            let (dec_ok, dec_fail, dec_need_kf, dec_us, dec_mbps, render_frames, render_failed) = {
                // 解码输出吞吐：累计 NV12 字节 / 会话时长
                let mbps = video_dec
                    .as_ref()
                    .map(|d| {
                        d.bytes_out() as f64
                            / 1e6
                            / window_reset_at.elapsed().as_secs_f64().max(0.001)
                    })
                    .unwrap_or(0.0);
                (
                    video_decoded,
                    video_decode_failed,
                    video_need_keyframe,
                    video_dec.as_ref().map(|d| d.avg_decode_us()).unwrap_or(0.0),
                    mbps,
                    renderer.as_ref().map(|r| r.frames()).unwrap_or(0),
                    render_failed,
                )
            };
            #[cfg(not(windows))]
            let render_mode = "off";
            #[cfg(not(windows))]
            let (dec_ok, dec_fail, dec_need_kf, dec_us, dec_mbps, render_frames, render_failed) =
                (0u64, 0u64, 0u64, 0.0f64, 0.0f64, 0u64, 0u64);

            // 音频输出统计（第四十一轮）。cfg 属性不能写在宏参数中间，
            // 所以先算成局部变量，再作为普通具名参数喂给 warn!
            #[cfg(windows)]
            let (audio_written, audio_out_latency_ms) = (
                audio_sink
                    .as_ref()
                    .map(|s| s.samples_written())
                    .unwrap_or(0),
                format!(
                    "{:.1}",
                    audio_sink.as_ref().map(|s| s.latency_ms()).unwrap_or(0.0)
                ),
            );
            #[cfg(not(windows))]
            let (audio_written, audio_out_latency_ms) = (0u64, format!("{:.1}", 0.0f64));

            warn!(
                frames = s.frames_in,
                net_ms = format!("{:.2}/{:.2}/{:.2}", p50, p95, p99),
                playable_ms = format!("{q50:.2}/{q95:.2}"),
                hold_ms = format!("{h50:.2}/{h95:.2}"),
                jitter = jb.len(),
                target_ms = format!("{:.1}", jb.target_ms()),
                jitter_adapt = jb.stats.adaptations,
                jitter_up = jb.stats.adapt_ups,
                jitter_down = jb.stats.adapt_downs,
                jitter_ewma_ms = format!("{:.2}", jb.stats.avg_jitter_ms),
                kbps = %kbps,
                rx_fps = format!("{rx_fps:.1}"),
                kf_ratio = format!("{kf_ratio:.3}"),
                nack_sent = s.nack_sent,
                nack_seqs = s.seqs_requested,
                healed,
                expired = s.expired_partials,
                dropped = s.frames_dropped_missing,
                loss = format!("{loss_ratio:.3}"),
                frame_loss = format!("{frame_loss:.3}"),
                avg_frags = format!("{avg_frags:.1}"),
                fec_reds = fec_reds_in_window,
                fec_recovered = fec_recovered_total,
                fec_group = fec_config.group_size,
                fec_redundant = fec_config.redundant,
                fec_cfg_updates = fec_config_updates,
                // 组:冗余:帧数 —— 自适应是否在抖，看这一列有几个不同项
                fec_group_usage = %fec_group_usage
                    .iter()
                    .map(|((g, r), n)| format!("{g}:{r}:{n}"))
                    .collect::<Vec<_>>()
                    .join(","),
                fec_groups_pruned,
                audio_frames = s.audio_frames,
                audio_played = s.audio_played,
                audio_qlen,
                audio_depth_ms = format!("{a50:.1}/{a95:.1}"),
                audio_target_ms = format!("{audio_target:.1}"),
                // 音频自适应的四个字段，与视频侧同名（解析脚本不用写两套）
                audio_adapt = audio_jb.as_ref().map(|b| b.stats().adaptations).unwrap_or(0),
                audio_adapt_up = audio_jb.as_ref().map(|b| b.stats().adapt_ups).unwrap_or(0),
                audio_adapt_down = audio_jb.as_ref().map(|b| b.stats().adapt_downs).unwrap_or(0),
                audio_jitter_ms = format!(
                    "{:.2}",
                    audio_jb.as_ref().map(|b| b.jitter_ms()).unwrap_or(0.0)
                ),
                audio_dropped_late = s.audio_dropped_late,
                audio_overflow = s.audio_overflow,
                audio_discontinuity = s.audio_discontinuity,
                audio_clock_ms = %audio_clock_ms,
                audio_decoded = s.audio_decoded,
                audio_decode_failed = s.audio_decode_failed,
                audio_concealed = s.audio_concealed,
                // 音频输出（第四十一轮）。未启用时全 0。
                audio_written = audio_written,
                audio_write_stalls = audio_write_stalls,
                audio_write_errors = audio_write_errors,
                audio_out_latency_ms = audio_out_latency_ms,
                av_synced,
                video_dropped_late = s.video_dropped_late,
                video_decoded = dec_ok,
                video_decode_failed = dec_fail,
                video_need_keyframe = dec_need_kf,
                decode_us = format!("{dec_us:.0}"),
                // 解码输出吞吐（MB/s）。`bytes_out` 是解码器累计的 NV12 字节数。
                decode_mbps = format!("{:.2}", dec_mbps),
                // 渲染统计（第三十九轮）。未启用时全 0。
                render_frames = render_frames,
                render_failed = render_failed,
                // 渲染后端名（`gdi-window` / `gpu-window` / `headless`）。
                // 这条字段让「这一轮的延迟数字是哪条路径测的」在日志里自证，
                // 不用回头翻环境变量。
                render_mode = render_mode,
                // 诊断：drift 极值（微秒）。30fps 下 drop 的根因排查用
                drift_us = format!("{}/{}", av.min_drift_us, av.max_drift_us),
                calls = format!("{}/{}", av.judge_calls, av.on_audio_calls),
                video_waited = s.video_waited,
                video_waiting = video_waiting.len(),
                replay_rejected = s.replay_rejected,
                decrypt_failed = s.decrypt_failed,
                target_kbps = format!("{target_kbps:.0}"),
                cc_state = %cc_state,
                cc_decisions,
                cc_decreases,
                slope = format!("{slope:.4}"),
                "UDP 统计"
            );

            // 窗口清零：丢包率是**窗口内**统计，不清零会变成累积值，
            // 拥塞控制看到的是「历史平均」而不是「当前链路状态」
            missing_frames_in_window = 0;
            received_frames_in_window = 0;
            fragments_in_window = 0;
            keyframes_in_window = 0;
            fec_reds_in_window = 0;
            window_epoch += 1;
            window_reset_at = Instant::now();
            bitrate.reset();
            last_report = Instant::now();

            // 面板文字同步。放在统计窗口末尾而不是每帧：
            // 每帧（16ms）调 `SetWindowTextW` 会让窗口过程不断重绘，
            // 实测吃掉 2-3% CPU —— 而人眼根本看不清 60Hz 刷新的文字。
            #[cfg(windows)]
            if let Some(p) = panel.as_mut() {
                let peer_text = peer.map(|a| a.to_string()).unwrap_or_default();
                // 视频尺寸从解码器取（没有解码器时给 0，面板显示「-」）
                let (vw, vh) = video_dec
                    .as_ref()
                    .map(|d| d.output_display_size())
                    .unwrap_or((0, 0));
                p.update(
                    peer.is_some(),
                    &peer_text,
                    &crate::status_panel::PanelStats {
                        playable_p50: q50,
                        playable_p95: q95,
                        loss_pct: loss_ratio * 100.0,
                        target_ms: jb.target_ms(),
                        width: vw,
                        height: vh,
                        frames: dec_ok,
                        fps: rx_fps,
                        mbps: bitrate.bitrate_bps() / 1e6,
                    },
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    //! 只覆盖**纯解析函数**（不依赖网络、不依赖 Opus 库）。
    //!
    //! 端到端行为在 `tools/verify-*.ps1` 里验证 —— 那些需要真起收发两端。

    use super::*;

    // ── parse_audio_codec_payload（第三十轮新增）──────────────
    //
    // 这个函数修的是一个真实缺口：接收端无法从码流判断音频是 Opus 还是
    // 原始 PCM，原来只能靠环境变量手动声明。忘了设的后果是
    // `audio_decoded=0` 但 `audio_played` 正常增长 —— 统计看不出异常。

    #[test]
    fn codec_opus_is_parsed() {
        let p = br#"{"fec_group_size":8,"fec_redundant":2,"audio_codec":"opus"}"#;
        assert_eq!(parse_audio_codec_payload(p), Some("opus".to_string()));
    }

    #[test]
    fn codec_pcm_is_parsed() {
        let p = br#"{"fec_group_size":8,"fec_redundant":2,"audio_codec":"pcm"}"#;
        assert_eq!(parse_audio_codec_payload(p), Some("pcm".to_string()));
    }

    #[test]
    fn codec_is_case_insensitive() {
        let p = br#"{"audio_codec":"OPUS"}"#;
        assert_eq!(parse_audio_codec_payload(p), Some("opus".to_string()));
    }

    #[test]
    fn codec_null_yields_none() {
        // 发送端不发音频时写 `null`（JSON 合法值，没有引号）
        let p = br#"{"fec_group_size":8,"audio_codec":null}"#;
        assert_eq!(
            parse_audio_codec_payload(p),
            None,
            "null 应返回 None（没有引号可读）"
        );
    }

    #[test]
    fn missing_codec_yields_none() {
        // 老版本发送端：只有 FEC 字段
        let p = br#"{"fec_group_size":8,"fec_redundant":2}"#;
        assert_eq!(parse_audio_codec_payload(p), None);
    }

    #[test]
    fn malformed_payload_does_not_panic() {
        // 各种畸形输入都必须安全返回，不能 panic
        for p in [
            &b""[..],
            &b"{"[..],
            &b"audio_codec"[..],
            &b"audio_codec:"[..],
            &b"audio_codec:\""[..],
            &b"audio_codec:\"\""[..],
            &b"\xff\xfe\x00"[..],           // 非 UTF-8
            &b"{\"audio_codec\":\"\"}"[..], // 空字符串
        ] {
            let _ = parse_audio_codec_payload(p);
        }
    }

    #[test]
    fn unknown_codec_passes_through_lowercased() {
        // 新版本可能加新 codec：老接收端不该报错，交给上层决定
        let p = br#"{"audio_codec":"AAC"}"#;
        assert_eq!(parse_audio_codec_payload(p), Some("aac".to_string()));
    }

    #[test]
    fn codec_parsing_is_independent_of_fec_fields() {
        // 关键契约：FEC 字段缺失/非法不该影响 codec 声明。
        // 这正是「为什么单独一个函数」的理由。
        let no_fec = br#"{"audio_codec":"opus"}"#;
        assert_eq!(parse_audio_codec_payload(no_fec), Some("opus".to_string()));

        let bad_fec = br#"{"fec_group_size":"abc","audio_codec":"opus"}"#;
        assert_eq!(
            parse_audio_codec_payload(bad_fec),
            Some("opus".to_string()),
            "FEC 字段非法时 codec 仍应解析出来"
        );
    }

    // ── parse_fec_config_payload 的回归保护 ──────────────────
    //
    // 新增 codec 字段后，原有解析必须不受影响 —— 老发送端发的
    // 两字段 JSON 仍然要能正确解析。

    #[test]
    fn fec_config_still_parses_without_codec() {
        let def = FecConfig {
            group_size: 8,
            redundant: 1,
            max_packet_size: sidelens_core::SAFE_MTU_PAYLOAD,
        };
        let p = br#"{"fec_group_size":16,"fec_redundant":3}"#;
        let cfg = parse_fec_config_payload(p, &def).expect("应能解析");
        assert_eq!(cfg.group_size, 16);
        assert_eq!(cfg.redundant, 3);
    }

    #[test]
    fn fec_config_ignores_codec_field() {
        let def = FecConfig {
            group_size: 8,
            redundant: 1,
            max_packet_size: sidelens_core::SAFE_MTU_PAYLOAD,
        };
        let p = br#"{"fec_group_size":32,"fec_redundant":2,"audio_codec":"opus"}"#;
        let cfg = parse_fec_config_payload(p, &def).expect("应能解析");
        assert_eq!(cfg.group_size, 32, "codec 字段不该干扰数字解析");
        assert_eq!(cfg.redundant, 2);
    }

    #[test]
    fn fec_config_returns_none_when_both_missing() {
        let def = FecConfig {
            group_size: 8,
            redundant: 1,
            max_packet_size: sidelens_core::SAFE_MTU_PAYLOAD,
        };
        // 只有 codec 字段：不是 FEC 配置帧
        let p = br#"{"audio_codec":"opus"}"#;
        assert!(
            parse_fec_config_payload(p, &def).is_none(),
            "没有 FEC 字段时应返回 None"
        );
    }

    // ── parse_audio_frame_ms（第三十一轮新增）─────────────────
    //
    // 这个函数修的是一个真实缺口：接收端把帧长硬编码成 20ms，
    // 发送端用 60ms 帧时解码报 "buffer too small"，134 帧全失败。

    #[test]
    fn frame_ms_60_is_parsed() {
        let p = br#"{"fec_group_size":0,"audio_codec":"opus","audio_frame_ms":60}"#;
        assert_eq!(parse_audio_frame_ms(p), Some(60.0));
    }

    #[test]
    fn frame_ms_fractional_is_parsed() {
        // 2.5ms 是合法的 Opus 帧长
        let p = br#"{"audio_codec":"opus","audio_frame_ms":2.5}"#;
        assert_eq!(parse_audio_frame_ms(p), Some(2.5));
    }

    #[test]
    fn frame_ms_zero_yields_none() {
        // 0 表示「不发音频」，合法但不该拿去配置解码器
        let p = br#"{"audio_codec":null,"audio_frame_ms":0}"#;
        assert_eq!(parse_audio_frame_ms(p), None);
    }

    #[test]
    fn frame_ms_missing_yields_none() {
        let p = br#"{"fec_group_size":8,"audio_codec":"opus"}"#;
        assert_eq!(parse_audio_frame_ms(p), None);
    }

    #[test]
    fn frame_ms_malformed_does_not_panic() {
        for p in [
            &b""[..],
            &b"audio_frame_ms"[..],
            &b"audio_frame_ms:"[..],
            &b"audio_frame_ms:abc"[..],
            &b"\xff\xfe"[..],
        ] {
            let _ = parse_audio_frame_ms(p);
        }
    }

    #[test]
    fn frame_ms_and_codec_are_independent() {
        // 两个字段各自独立解析：任何一个缺失都不影响另一个
        let only_ms = br#"{"audio_frame_ms":40}"#;
        assert_eq!(parse_audio_frame_ms(only_ms), Some(40.0));
        assert_eq!(parse_audio_codec_payload(only_ms), None);

        let only_codec = br#"{"audio_codec":"pcm"}"#;
        assert_eq!(parse_audio_frame_ms(only_codec), None);
        assert_eq!(
            parse_audio_codec_payload(only_codec),
            Some("pcm".to_string())
        );
    }
}
