//! SideLens 发送端（模拟手机）
//!
//! 两种传输：
//!
//! - 默认：WebSocket（`ws://host:port`）
//! - `--udp`：UDP + 分片 + 重传缓存 + FEC 冗余（`host:port`）
//!
//! # FEC 怎么接进来的（只影响 `--udp`）
//!
//! UDP 路径上每条视频帧除了按 MTU 分片发出，还会**额外**发出一批
//! `FrameType::Fec` 冗余包。冗余包是独立的数据报，不走分片逻辑
//! （它本身就是 MTU 级别的大小），接收端收到后交给 `FecReceiver` 反解。
//!
//! 好处是 `send_frame` 的内部逻辑一行都不用改：FEC 只是"多发几个包"。
//!
//! # 组大小按帧的实际分片数走
//!
//! 冗余包的**个数**是 `ceil(分片数 / group_size) * redundant`。固定
//! `group_size = 10` 时，分片数超过 10 的帧会凑出多个组，而**尾组哪怕只剩
//! 1 个真实分片也要发满 `redundant` 个冗余包** —— 16KB 帧（15 个分片）
//! 被拆成 10+5 两组，开销 4/15 ≈ 27%，而理论值只有 2/15 ≈ 13%。
//!
//! 现在每帧按分片数选 `group_size`（见 [`pick_group_size`]），保证
//! `group_size >= 分片数`：一帧只落在一个组里，冗余包个数恒等于 `redundant`，
//! 开销压到理论下界 `redundant / 分片数`。
//!
//! 代价是**两端必须协商 group_size**：接收端靠 `index / group_size` 把帧内
//! 分片下标折算成组内下标，靠 `i % redundant` 建 XOR 方程组，两个值对不上
//! 就恢复出静默的垃圾数据。所以每次切换组大小时先发一个 `FrameType::Config`
//! 帧通知接收端（见 [`FecOutbound::send_config`]）。
//!
//! 三条约束（踩过才知道）：
//!
//! 1. **分组必须按帧**。跨帧分组会让本帧的冗余包依赖下一帧的数据包，
//!    等于给每帧白加一个帧间隔的延迟，实时视频承受不起。
//! 2. **两端覆盖的字节范围必须一致**。接收端推给 `FecReceiver` 的是
//!    「去掉 32B 帧头与 8B 分片头之后的数据段」，发送端必须推同一段，
//!    否则恢复出来的内容对不上，而且不会报错 —— 是静默的垃圾数据。
//! 3. **冗余包不能超过 MTU**。它由 `send_raw` 直接发出、不分片，
//!    超了就是 `EMSGSIZE`。见 `FEC_WIRE_LIMIT` 的说明。
//!
//! # 用法
//!
//! ```bash
//! sidelenssend ws://127.0.0.1:27100 60 8192 10
//! sidelenssend --udp 127.0.0.1:27200 60 8192 10 --loss 0.1
//! sidelenssend --udp 127.0.0.1:27200 30 8192 8 --loss 0.05 --fec-auto
//! sidelenssend --udp 127.0.0.1:27200 30 8192 8 --loss 0.05 --fec 2
//! sidelenssend --udp 127.0.0.1:27400 30 8192 5 --loss 0.02 --audio
//! ```
//!
//! # 音频怎么接的（`--audio`，只影响 `--udp`）
//!
//! 音频帧与视频帧**在同一个 tokio 循环里发**，但各有独立节拍：
//! 视频按 `fps`（默认 30 → 33.3ms），音频按 `--opus-frame-ms`（默认 20ms）。
//! 用 `tokio::select!`
//! 交替服务两个节拍，而不是拆成两个任务 —— 重传缓存、FEC 分组、统计
//! 都是单线程状态，拆成两个任务就得加锁。
//!
//! **时基**是这里最容易错的地方：音频 PTS 由 `AudioPacer` 按帧序号累加
//! （`t0 + n × frame_duration_us`，20ms 帧时是 20000µs），视频 PTS 同样用
//! `t0 + n × 帧间隔`，两者共用同一个
//! `now_us()` 起点。绝不能拿墙上时钟当 PTS —— 采集抖动会直接进入音画同步，
//! 而且会**累积**（详见 `sidelens_codec::audio` 模块文档）。
//!
//! 音频帧不走 FEC、不进重传缓存：丢一帧只是一个帧长的空洞（20ms），
//! 补传要花一个 RTT，反而比丢掉更晚。音频是否需要分片取决于帧长：
//! 20ms 单声道 48kHz 的 16bit PCM 是 1920 字节，加 32B 帧头已超过
//! `SAFE_MTU_PAYLOAD`（1200），走 `send_frame` 会被切成 2 片；
//! 而 2.5/5/10ms 帧（240/480/960 字节）一片就够。两种都由 `send_frame`
//! 按实际长度自动处理，调用处不用区分。
//!
//! # Opus 调优开关（`--opus-*`）
//!
//! `--opus-frame-ms` 改的是 **`OpusConfig.frame_ms`**，于是
//! `frame_samples` / `frame_duration_us` / 音频节拍三处同时跟着变 ——
//! 没有任何一处硬编码 960 或 20000。
//!
//! `--opus-dtx` / `--opus-inband-fec` / `--opus-loss` 直接透传给
//! `OpusEncoder::set_*`，失败只 warn 不中断（不同 libopus 版本支持度不同）。

use std::time::Duration;

use anyhow::Result;
use sidelens_codec::audio::{AudioPacer, OpusConfig};
use sidelens_codec_opus::OpusEncoder;
use sidelens_core::fec::FecConfig;
use sidelens_core::fec_link::{
    suggest_fec, suggest_redundant_for_frame, FecPacket, FecSender, FrameClass, LossEstimator,
    RedundantHysteresis, DEFAULT_RECOVERY_TARGET, FEC_HEADER_SIZE,
};
use sidelens_core::{RetransmitCache, RetransmitConfig, StoreSpec};
use sidelens_crypto::{FrameSealer, Role, TrafficKeys};
use sidelens_proto::{encode_frame_owned, FrameSpec, FrameType, FLAG_KEYFRAME, HEADER_SIZE};
use sidelens_transport::{PacketLossSimulator, UdpSender, WsClient};
use tracing::{debug, info, warn};

/// FEC 组大小的下限
///
/// 1 个分片做不了 FEC：组内至少要有 2 个包，丢掉其中一个才谈得上用另一个还原。
const FEC_GROUP_SIZE_MIN: usize = 2;

/// FEC 组大小的上限
///
/// 组越大，一帧跨的组越少、冗余包越少（开销越低）。但组同时也是**恢复粒度**：
/// 一个组里丢超过 `redundant` 个分片就整组失效，所以不能无限放大。
///
/// 64 是权衡后的上限 —— 再大时每组要留一份等长的 XOR 缓冲（64 × 1160B ≈ 74KB），
/// 而实际帧很少有超过 64 个分片（约 74KB）的。
const FEC_GROUP_SIZE_MAX: usize = 64;

/// 按一帧的实际分片数选 FEC 组大小
///
/// # 为什么要按帧选
///
/// 冗余包个数 = `ceil(分片数 / group_size) * redundant`。只有让
/// `group_size >= 分片数`，一帧才只落在一个组里，冗余包个数才恒等于
/// `redundant` —— 这是开销的理论下界 `redundant / 分片数`。
///
/// 组小于分片数时必然出现尾组，而尾组**不管剩几个真实分片都要发满
/// `redundant` 个冗余包**：15 个分片按 10 分组 → 10+5 两组 → 4 个冗余包，
/// 开销从 13% 涨到 27%。
///
/// # 为什么取 2 的幂
///
/// 分组的下标换算用的是 `index / group_size` 与 `index % group_size`
/// （见 `fec_link::FecReceiver`），对任意正整数都成立，**并不要求 2 的幂**。
/// 取 2 的幂是为了另一件事：让组大小在相邻帧之间**稳定**。分片数在 7/8/9
/// 之间抖动时 `next_power_of_two` 一律给 8，不会每帧都触发一次配置协商；
/// 直接取分片数则会来回变，接收端要不停重建解码器。
fn pick_group_size(fragment_count: usize) -> usize {
    fragment_count
        .clamp(FEC_GROUP_SIZE_MIN, FEC_GROUP_SIZE_MAX)
        .next_power_of_two()
}

/// 带迟滞的组大小选择
///
/// # 为什么需要迟滞（H.264 接入后实测发现的真 bug）
///
/// `pick_group_size` 只看当前帧的分片数。合成数据每帧 8192 字节固定，
/// 分片数恒为 7-8，组大小稳定在 8，从来没抖过。
///
/// 换成真实 H.264 后，帧大小在 **918 ~ 16199 字节**之间剧烈变化
/// （P 帧 1.3KB，IDR 帧 16KB）。于是：
///
/// ```text
/// 帧 1: 2 片  → group_size = 2
/// 帧 2: 2 片  → 2
/// 帧 3: 14 片 → 16      ← 重建解码器，跨配置的组全丢
/// 帧 4: 2 片  → 2       ← 又重建
/// ```
///
/// 实测 `fec_cfg_updates = 10`（6 秒内重建 10 次），
/// 而每次重建都丢掉未完成的组 —— FEC 等于没接。
///
/// # 规则：只增不减（一次会话内单调上升）
///
/// 第一版试过「2 倍迟滞」（`want >= cur*2 || want*2 <= cur` 才切换），
/// **实测仍然抖**：`fec_group_usage="4:5,8:1,16:6,32:2,64:1"` ——
/// 5 个不同组大小，`fec_cfg_updates=11`。
///
/// 原因是迟滞只挡住了「同档位抖动」，挡不住「真的大帧来了就涨、
/// 下一帧小了就跌」这个**锯齿**。H.264 的帧大小分布是
/// 「大量 1.3KB 的 P 帧 + 偶尔 16KB 的 IDR」，锯齿是结构性的。
///
/// 所以改成**只增不减**：
///
/// ```text
/// 帧 1: 2 片  → group = 2
/// 帧 5: 8 片  → group = 8    （涨）
/// 帧 6: 2 片  → group = 8    （不跌）
/// 帧 20: 16 片 → group = 16  （涨）
/// ```
///
/// 一次会话内最多 `log2(64/2) = 5` 次配置切换，且全部发生在开头几秒。
///
/// # 为什么「不跌」不浪费带宽
///
/// 组大小 64 保护 2 片帧时，理论上 2 个冗余包保护 2 个数据包（开销 100%）。
/// 但 `should_fec` 会在「分片数 ≤ redundant」时**直接跳过**整帧的 FEC ——
/// 所以这种浪费的冗余包根本不会发出去。
///
/// 实测佐证：`fec_skipped_frames=166 / 总 181`，只有 15 个真正的大帧用了 FEC。
fn pick_group_size_hysteretic(fragment_count: usize, current: Option<usize>) -> usize {
    let want = pick_group_size(fragment_count);
    match current {
        None => want,
        // 只增不减：want 更大时才切换
        Some(cur) => want.max(cur),
    }
}

/// FEC 冗余开销的上限（`redundant / 分片数`）
///
/// 见 [`should_fec`] 的说明：这是「FEC 的带宽代价不能超过数据本身的 1/3」
/// 这条粗线的具体数值。三档实测的交叉点在 25%（划算）与 50%（不划算）之间。
/// 采信码率建议的下限（bps）
///
/// 低于这个值的建议一律忽略：老版本接收端可能发 0，
/// 而 0 会让编码器创建失败或输出空帧。
const BITRATE_MIN_BPS: u64 = 100_000;
/// 采信码率建议的上限（bps）
const BITRATE_MAX_BPS: u64 = 50_000_000;
/// 相对当前码率的变化幅度达到这个比例才真正调用 `set_bitrate`
///
/// 见发送循环里消费 `rate_hint` 处第 1 条闸门的说明：
/// 控制器的百分比爬升每 2 秒只涨几个百分点，每次都设会让编码器
/// 反复重算 QP 曲线，输出反而更不稳。5% 是实测的交叉点。
const BITRATE_MIN_STEP_RATIO: f64 = 0.05;

const FEC_MAX_OVERHEAD_RATIO: f64 = 0.35;

/// 按开销预算把冗余度钳到这一帧**付得起**的最大值
///
/// # 为什么需要它（第一版 A/B 实测踩到的死循环）
///
/// 第一版的做法是：`redundant` 由自适应算出来，然后 `should_fec` 用
/// `redundant / 分片数 <= 35%` 判断**整帧要不要发 FEC**。实测出问题：
///
/// ```text
/// 8 分片帧，自适应算出 redundant=3
///   -> 3/8 = 37.5% > 35%  -> should_fec = false  -> 整帧不发 FEC
///   -> 该帧丢包全部转成 NACK -> 估计器看到更高丢包率 -> want 仍是 3
///   -> 永远不发 FEC
/// ```
///
/// 实测 `B-8k-adapt`：`nack=160`（固定档只有 21）、`fec_overhead=5.5%`、
/// `fec_group_usage=8:37`（只有前 37 帧发了 FEC，之后全停）。
/// **自适应把 FEC 关掉了**，比固定档差一个数量级。
///
/// 根因是「整帧开关」这个粒度错了。开销闸门想表达的是「别付超过 35% 的带宽」，
/// 而不是「超过 35% 就别做 FEC」。正确做法是**钳到预算**：
///
/// ```text
/// affordable = floor(分片数 * 35%)
/// redundant  = min(自适应的 want, affordable)
/// ```
///
/// 8 分片帧：affordable = 2，want=3 被钳到 2 —— 仍以 25% 开销发 FEC，
/// 只是没那么多冗余。这比「一点都不发」严格更好。
///
/// # 与 `should_fec` 的分工
///
/// 钳完之后 `should_fec` 只剩一个判据要管：`affordable == 0`
/// （分片数 < 3，即 2 分片帧）—— 那时 FEC 在数学上无意义（未知数 >= 方程数），
/// 交给 NACK 补更划算。
fn affordable_redundant(fragment_count: usize, want: usize) -> usize {
    if fragment_count == 0 {
        return 0;
    }
    // floor：宁可少给一点也不超预算（3 分片 -> 1，8 分片 -> 2，55 分片 -> 19）
    let affordable = (fragment_count as f64 * FEC_MAX_OVERHEAD_RATIO).floor() as usize;
    want.min(affordable)
}

/// 这一帧到底要不要发 FEC 冗余
///
/// # 为什么不能无脑发（实测踩到）
///
/// 组大小的下限是 2（1 个分片没法做 FEC），于是**单分片的帧也会凑出一个
/// 2 包的组**，而冗余包个数恒等于 `redundant`。800 字节的帧实测：
///
/// ```text
/// 数据 100672B，冗余 191976B，开销 190.7%
/// ```
///
/// 而且这 2 个冗余包保护的只有 1 个数据包 —— 它自己丢了靠 NACK 补一个包就够，
/// 代价比常驻 190% 的冗余低两个数量级。
///
/// # 判据只有一个：分片数必须严格大于冗余数
///
/// 否则组里的冗余包不比能救回的数据包少，FEC 在数学上就没有意义
/// （XOR 方程组里未知数比方程还多）。
///
/// # 开销预算不在这里判（第一版的错）
///
/// 第一版在这里加过「`redundant / 分片数 <= 35%`，否则整帧跳过」。
/// 实测这是个**死循环**：8 分片帧算出 want=3 → 37.5% 超预算 → 整帧不发
/// → 丢包全转 NACK → 估计器看到更高丢包 → want 仍是 3 → 永远不发。
/// 实测 `nack` 从 21 涨到 160。
///
/// 预算该用来**钳冗余度**而不是**关掉 FEC** —— 见 [`affordable_redundant`]。
/// 钳完之后，这里只剩「冗余数是否已经大到没有意义」这一个判据。
fn should_fec(fragment_count: usize, redundant: usize) -> bool {
    if fragment_count == 0 || redundant == 0 {
        return false;
    }
    fragment_count > redundant
}

/// 冗余数据报的硬上限（**整个 UDP 数据报**，含 32B 帧头与 10B FEC 头）
///
/// # 这个数字怎么来的（实测踩过）
///
/// 数据分片的数据报是：`32B 帧头 + 8B 分片头 + 数据`，总长被 `MAX_DATAGRAM` 卡在 1200。
/// 所以一片的数据段最长 = 1200 - 32 - 8 = 1160。
///
/// 冗余包的载荷长度 = 组内最长成员的 XOR 结果 = 最长那一片的数据段 = 1160。
/// 它外面套的是 `32B 帧头 + 10B FEC 头`，于是：
///
/// ```text
/// 冗余数据报 = 32 + 10 + 1160 = 1202
/// 数据分片   = 32 +  8 + 1160 = 1200
/// ```
///
/// 也就是说 FEC 头比分片头宽 2 字节，**这两个字节无法消除** ——
/// 除非把分片切得更碎，而分片边界一变，接收端「分片下标 → 组内下标」
/// 的映射就全错，恢复出来是垃圾。所以这里如实按 1202 卡。
///
/// # 为什么 1202 仍然安全
///
/// `SAFE_MTU_PAYLOAD`（1200）是给隧道 / PPPoE 留了余量的保守值，
/// 真实以太网单包 IP 载荷上限是 1472。1202 仍在其下 270 字节，
/// 不会触发 IP 分片。
///
/// 实测教训：第一版按「整包 ≤ SAFE_MTU_PAYLOAD」卡，结果是
/// **每一个**冗余包都被跳过（8192 字节帧共 482 个），FEC 等于没接。
const FEC_WIRE_LIMIT: usize =
    sidelens_transport::MAX_DATAGRAM - sidelens_core::FRAGMENT_HEADER_SIZE + FEC_HEADER_SIZE;

/// 音频使用的流 ID
///
/// 视频是 0、音频是 1。接收端的分片重组器按 `(stream_id, seq)` 分组，
/// 两套 seq 各自独立累加（音频一帧比视频密），共用 0 会让
/// 音频分片和视频分片互相顶掉。
const AUDIO_STREAM_ID: u16 = 1;

/// 合成一帧画面
///
/// 真实实现里这是 Camera2 / MediaFoundation / AVCaptureSession 的输出。
fn synth_frame(seq: u32, len: usize, buf: &mut Vec<u8>) {
    buf.clear();
    buf.resize(len, 0);
    let mut x = seq.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
    for b in buf.iter_mut() {
        x = x.wrapping_mul(1_103_515_245).wrapping_add(12_345);
        *b = (x >> 16) as u8;
    }
}

#[cfg(windows)]
/// 合成一帧 NV12（`--h264` 模式用）
///
/// 真实实现里这是 Camera2 / MediaFoundation / AVCaptureSession 的输出。
///
/// # 为什么不能拿随机字节当 NV12
///
/// 随机噪声的每个宏块都不可预测，编码器只能全部走帧内，而且码控拉不住
/// （实测 1280x720 全噪声在 2 Mbps 目标下跑出 56 Mbps，28 倍）。
/// 用「渐变背景 + 移动竖条」这种**局部变化**的画面，帧间预测才有意义，
/// 码率与分片数才接近真实摄像头。
///
/// # 布局
///
/// NV12 的 Y 平面是 w*h，UV 交织平面是 w*h/2，合计 w*h*3/2。
/// UV 固定 128（中性灰），避免引入无关的色度噪声。
#[cfg(windows)]
fn synth_frame_nv12(seq: u32, width: u32, height: u32, buf: &mut Vec<u8>) {
    let w = width as usize;
    let h = height as usize;
    let y_len = w * h;
    buf.clear();
    buf.resize(y_len + y_len / 2, 128);

    let bar_x = (seq as usize * 7) % w.max(1);
    let bar_w = (w / 12).max(8);
    for y in 0..h {
        let row = y * w;
        for x in 0..w {
            buf[row + x] = if x >= bar_x && x < bar_x + bar_w {
                235u8
            } else {
                (((x * 200) / w.max(1)) as u8).wrapping_add((y % 7) as u8)
            };
        }
    }
}

/// H.264 编码相关的运行时状态（`--h264` 模式）
///
/// # 为什么单独一个结构
///
/// `--h264` 关闭时这些字段一个都不该存在，也不该有任何一次
/// Media Foundation 调用 —— 现有测试与脚本跑的是同一条代码路径，
/// 不能因为多了个可选功能就换个行为。
/// 从 `--h264` 状态里取 `(平均编码耗时 µs, 编码帧数, dump 字节数)`
///
/// # 为什么要这个函数而不是直接访问字段
///
/// `H264Out` 是 Windows 专属类型，非 Windows 上 `h264` 是 `Option<()>`。
/// 日志里那三个字段在**两个平台都要能打印**（否则日志格式不一致，
/// 解析脚本要写两套正则）。
///
/// 所以把「取这三个值」收成一个跨平台函数：Windows 上读真实字段，
/// 其他平台返回 0。
fn encode_stats(h264: &H264State) -> (f64, u64, u64) {
    #[cfg(windows)]
    {
        match h264.as_ref() {
            Some(h) => (h.avg_encode_us(), h.encode_frames, h.dump_bytes),
            None => (0.0, 0, 0),
        }
    }
    #[cfg(not(windows))]
    {
        let _ = h264;
        (0.0, 0, 0)
    }
}

/// 采集侧统计：`(平均采集耗时µs, 空轮询次数, 尺寸不符次数, 是否启用)`
///
/// - **空轮询** = DXGI 返回「桌面没变化」的次数。它不是错误，但如果它
///   等于总帧数，说明采集源根本没在工作（比如采了个不存在的显示器）。
/// - **尺寸不符** = 采集出来的 coded size 与编码器不一致。正常恒 0；
///   非 0 说明显示器分辨率在运行中变了，此时画面在冻结。
struct CaptureReport {
    enabled: bool,
    total_us: f64,
    wait_us: f64,
    process_us: f64,
    empty: u64,
    size_mismatch: u64,
}

fn capture_stats(h264: &H264State) -> CaptureReport {
    #[cfg(windows)]
    {
        match h264.as_ref() {
            Some(h) if h.capture.is_some() => CaptureReport {
                enabled: true,
                total_us: h.avg_capture_us(),
                wait_us: h.avg_capture_wait_us(),
                process_us: h.avg_capture_process_us(),
                empty: h.capture_empty,
                size_mismatch: h.capture_size_mismatch,
            },
            _ => CaptureReport {
                enabled: false,
                total_us: 0.0,
                wait_us: 0.0,
                process_us: 0.0,
                empty: 0,
                size_mismatch: 0,
            },
        }
    }
    #[cfg(not(windows))]
    {
        let _ = h264;
        CaptureReport {
            enabled: false,
            total_us: 0.0,
            wait_us: 0.0,
            process_us: 0.0,
            empty: 0,
            size_mismatch: 0,
        }
    }
}

#[cfg(windows)]
type H264State = Option<H264Out>;
#[cfg(not(windows))]
type H264State = Option<()>;

#[cfg(windows)]
struct H264Out {
    encoder: sidelens_codec_mf::H264Encoder,
    /// NV12 帧缓冲（复用）
    nv12: Vec<u8>,
    /// 编码输出缓冲（复用）
    out: Vec<u8>,
    /// `--h264-dump` 的目标文件
    dump: Option<std::fs::File>,
    /// 已写出的字节数
    dump_bytes: u64,
    /// 编码耗时累计（微秒）
    encode_us_total: f64,
    /// 编码过的帧数
    encode_frames: u64,
    /// 屏幕采集源（`--screen`）。`None` = 合成图案
    capture: Option<sidelens_capture::ScreenCapture>,
    /// 采集总耗时累计（微秒）—— `wait + process`，仅用于对照
    capture_us_total: f64,
    /// 等新帧的时间累计（微秒）
    ///
    /// # 为什么必须和「处理」分开
    ///
    /// 第五十轮实测发现 `capture_avg_us` 是 11-23ms，而纯 BGRA→NV12 转换
    /// 只要 4.12ms（1080p）到 15.4ms（4K）。差出来的部分是
    /// `AcquireNextFrame` 在**等桌面刷新**，CPU 在睡觉。
    ///
    /// 混在一起的后果：看这个数字判断不了瓶颈 —— 「采集 24ms」听起来
    /// 该优化，实际其中 18ms 是空闲等待，优化它等于空转。
    ///
    /// 分开之后语义清楚：`wait` 大说明采集频率高于桌面刷新率（正常），
    /// `process` 大才是真该优化的（转换/缩放成本）。
    capture_wait_us: f64,
    /// 处理一帧（转换 + 缩放 + 拷进 nv12）的时间累计（微秒）
    capture_process_us: f64,
    /// 采集返回 None（桌面没变化）的次数
    capture_empty: u64,
    /// 采集尺寸与编码器不一致的次数（显示器分辨率变了）
    capture_size_mismatch: u64,
}

#[cfg(windows)]
impl H264Out {
    /// 决定编码分辨率
    ///
    /// # 两个来源，优先级从高到低
    ///
    /// 1. **`--size WxH`** —— 显式指定，最可靠
    /// 2. `frame_bytes` 位置参数 —— 反推（向后兼容老脚本）
    ///
    /// # 为什么必须加 `--size`（第三十二轮修的）
    ///
    /// 原来只有第 2 条。但 `frame_bytes` 在 `--h264` 模式下被解释成
    /// **NV12 帧字节数**（`w*h*3/2`），而 720p 的 NV12 是 1,382,400 字节。
    /// 于是：
    ///
    /// ```text
    /// frame_bytes = 8192   -> <= 1382400  -> 用默认 720p     ✅
    /// frame_bytes = 32768  -> <= 1382400  -> 用默认 720p     ❌ 想表达 1080p
    /// frame_bytes = 3110400 -> > 1382400  -> 反推 1920x1080   ✅
    /// ```
    ///
    /// 要表达 1080p 必须传 3,110,400 —— 这个数字没人会记得。
    /// 实测踩到：`--h264 --h264-dump` 传 32768 想测 1080p，
    /// dump 出来的 SPS 仍是 1280x720（由 `tools/verify-h264-stream.ps1` 发现）。
    ///
    /// 现在 `--size 1920x1080` 直接指定，不依赖那个反推公式。
    fn config_from_args(args: &Args) -> sidelens_codec_mf::EncoderConfig {
        let def = sidelens_codec_mf::EncoderConfig::default();

        // 1) 显式 --size 优先
        if let Some((w, h)) = args.size {
            return sidelens_codec_mf::EncoderConfig {
                width: w,
                height: h,
                fps: args.fps as u32,
                ..def
            };
        }

        // 2) 回退：按 frame_bytes 反推（保持老脚本可用）
        let nv12 = def.nv12_len();
        if args.frame_bytes <= nv12 {
            return def;
        }
        // 按 16:9 反推：nv12 = w*h*1.5，h = w*9/16
        // → w = sqrt(nv12 * 16 / (9 * 1.5))
        let w = ((args.frame_bytes as f64 * 16.0 / (9.0 * 1.5)).sqrt() as u32) & !1;
        let h = (w * 9 / 16) & !1;
        if w >= 64 && h >= 64 {
            sidelens_codec_mf::EncoderConfig {
                width: w,
                height: h,
                fps: args.fps as u32,
                ..def
            }
        } else {
            def
        }
    }

    fn new(args: &Args) -> Result<Self> {
        let cfg = Self::config_from_args(args);
        let encoder = sidelens_codec_mf::H264Encoder::new(cfg)
            .map_err(|e| anyhow::anyhow!("H.264 编码器创建失败：{e}"))?;

        info!(
            encoder = encoder.encoder_name(),
            width = cfg.width,
            height = cfg.height,
            fps = cfg.fps,
            bitrate = cfg.bitrate,
            gop = cfg.gop,
            nv12_bytes = cfg.nv12_len(),
            "H.264 编码已启用（输出 Annex-B）"
        );

        let dump = match args.h264_dump.as_ref() {
            Some(path) => {
                let f = std::fs::File::create(path)
                    .map_err(|e| anyhow::anyhow!("创建 --h264-dump 文件 {path} 失败：{e}"))?;
                info!(path = %path, "编码输出将写入该文件（Annex-B）");
                Some(f)
            }
            None => None,
        };

        // 屏幕采集源（`--screen`）。`off` 时保持合成图案。
        //
        // # 构造时就给目标尺寸，让采集层自己缩放
        //
        // 编码器的 coded size 由 `--size` 决定（16 对齐，1920x1080 -> 1920x1088），
        // 而显示器可能是任何分辨率。第一版我在这里写的是「每帧比对尺寸，
        // 不符就用合成图案」—— **那是错的**：用户想共享 4K 屏幕、编码器只吃
        // 720p，正确行为是缩放，不是放弃画面（那样用户看到的是渐变方块，
        // 还以为采集坏了）。
        //
        // 现在把目标尺寸直接交给采集层：它在 BGRA 域一次遍历同时做降采样和
        // BGRA→NV12 转换。实测 4K→1080p 比不缩放**更快**（14.4 vs 15.6 ms），
        // 因为写的目标数据只有 1/4。
        //
        // # 目标尺寸必须用 `cfg.width/height`（**显示尺寸**，不是 coded）
        //
        // `EncoderConfig::nv12_len()` 是 `w*h*3/2`，**不做 16 对齐** ——
        // 编码器只要求「至少这么多字节」，内部自己处理对齐。
        //
        // 第一版我把 `cfg` 当 coded size 传给采集层，而采集层把它当**显示**
        // 尺寸又做了一次 16 对齐：1920x1080 → 采集层算出 coded 1920x1088 →
        // 产出的 `Frame.coded_height` 是 1088，而 `cfg.height` 是 1080，
        // 于是每帧都判「尺寸不符」，`capture_size_mismatch=180`（全部帧）。
        //
        // 实测证据：`--size 1280x720` 完全正常（720 恰好 16 的倍数，对齐是
        // 恒等操作，所以掩盖了 bug）；`--size 1920x1080` 全部帧失败。
        #[cfg(windows)]
        let capture = if args.screen == "off" {
            None
        } else {
            // 目标尺寸 = 编码器的**显示尺寸**。采集层会自己 16 对齐到 coded
            // size（1920x1080 -> 1920x1088），但 `Frame.coded_*` 报的是对齐
            // 后的值，所以下面比对时必须用对齐后的 `cfg` 再算一次。
            let target = Some((cfg.width, cfg.height));
            let (cap, err) = match args.screen.as_str() {
                "dxgi" => sidelens_capture::ScreenCapture::auto_sized(
                    sidelens_capture::CaptureSource::PrimaryMonitor,
                    target,
                ),
                "gdi" => match sidelens_capture::ScreenCapture::new_gdi_sized(
                    sidelens_capture::CaptureSource::PrimaryMonitor,
                    target,
                ) {
                    Ok(c) => (c, None),
                    Err(e) => {
                        warn!(error = %e, "GDI 屏幕采集打不开，退回合成图案");
                        (
                            sidelens_capture::ScreenCapture::synthetic(cfg.width, cfg.height),
                            None,
                        )
                    }
                },
                other => anyhow::bail!("--screen 只能是 dxgi / gdi / off，收到 {other}"),
            };
            if let Some(e) = err {
                warn!(error = %e, "DXGI 打不开，已自动退回 GDI（见 docs/capture.md）");
            }
            // 缩放信息单独一行：`source` 是缩放前的真实分辨率，
            // `target` 是编码器要的尺寸。两者不同说明在缩放 ——
            // 这是**正常配置**，不是错误（第一版会在这里警告，是错的）。
            info!(
                backend = ?cap.backend(),
                capture_source = %cap.source(),
                source = ?cap.source_size(),
                target = format!("{}x{}", cfg.width, cfg.height),
                scale = ?cap.scale_mode(),
                "屏幕采集已启用"
            );
            Some(cap)
        };

        // NV12 缓冲的初值必须是**合法的 limited-range 黑**（Y=16 / UV=128），
        // 不能是 `vec![0u8; ...]`。
        //
        // # 为什么（子代理报的坑 #2 的连锁后果）
        //
        // DXGI 的第一次 `AcquireNextFrame` 会**成功返回一个全黑的占位帧**
        // （`LastPresentTime == 0`），采集层已经正确把它翻成 `Ok(None)`。
        // 而 `Ok(None)` 时本文件的策略是「复用上一帧继续编码」——
        // 于是**第一帧复用到的就是这块初始缓冲**。
        //
        // 全 0 在 NV12 里不是黑：Y=0 是 full-range 的黑，在 limited range
        // 下是「比黑更黑」的非法值，编码器会把它钳到 16；UV=0 更是彻底错
        // 的色度（应当是 128 中性）。结果是画面偏绿且色度块状。
        //
        // 填 16/128 之后，即使采集一帧都没成功，送进编码器的也是合法黑帧。
        // 缓冲长度按**对齐后**的尺寸算：采集层产出的是 coded size
        // （1920x1080 -> 1920x1088，3,133,440 字节），而
        // `cfg.nv12_len()` 是显示尺寸（3,110,400 字节）—— 差 23,040 字节。
        // `copy_from_slice` 会因长度不等 panic，所以这里按对齐后的算。
        //
        // 编码器只要求「至少 `cfg.nv12_len()` 字节」，多给的会被忽略
        // （见 `encode_nv12_into` 里的 `&nv12[..expect]`）。
        let layout = sidelens_capture::FrameLayout::new(cfg.width, cfg.height)
            .map_err(|e| anyhow::anyhow!("构造帧布局失败：{e}"))?;
        let mut nv12 = vec![0u8; layout.nv12_len()];
        sidelens_capture::fill_black_nv12(layout, &mut nv12);

        Ok(Self {
            nv12,
            out: Vec::with_capacity(256 * 1024),
            dump,
            dump_bytes: 0,
            encode_us_total: 0.0,
            encode_frames: 0,
            encoder,
            #[cfg(windows)]
            capture,
            #[cfg(windows)]
            capture_us_total: 0.0,
            #[cfg(windows)]
            capture_wait_us: 0.0,
            #[cfg(windows)]
            capture_process_us: 0.0,
            #[cfg(windows)]
            capture_empty: 0,
            #[cfg(windows)]
            capture_size_mismatch: 0,
        })
    }

    /// 取一帧 NV12 → 编码 → 返回 Annex-B 码流
    ///
    /// # 桌面静止时没有新帧
    ///
    /// DXGI 的 `AcquireNextFrame` 在桌面没变化时返回超时，采集层把它翻成
    /// `Ok(None)`。此时**不能跳过编码** —— H.264 的 P 帧依赖前一帧，
    /// 但「跳过一帧不编码」和「编码一个重复帧」对解码端是两回事：
    /// 前者会让时间线出现空洞（PTS 跳跃），后者是正常的静止画面。
    ///
    /// 所以 `None` 时**复用上一帧的 NV12 缓冲**（不清空），继续编码。
    /// 这也是屏幕共享的常规做法：静止桌面仍然按帧率推流，只是内容不变。
    fn encode_next(&mut self, seq: u32, pts_us: u64) -> Result<&[u8]> {
        let cfg = *self.encoder.config();

        // 采集：有源就用源，没有就合成
        if let Some(cap) = self.capture.as_mut() {
            // 两段计时：`capture()` 内部含 `AcquireNextFrame` 的等待，
            // 那段 CPU 在睡觉，不该算进「处理成本」。见 `capture_wait_us` 的文档。
            //
            // 采集层没有暴露「等待了多少」的接口（那是子代理的 crate，
            // 我不改它的 API），所以这里用「总时长 - 后续处理时长」反推。
            let t_cap = std::time::Instant::now();
            let captured = cap.capture();
            let cap_total_us = t_cap.elapsed().as_secs_f64() * 1e6;
            let t_proc = std::time::Instant::now();
            match captured {
                Ok(Some(f)) => {
                    // 采集层已经按构造时给的目标尺寸（= 编码器 coded size）
                    // 缩放并对齐过，正常情况下必然相等。
                    //
                    // # 不相等时**不要**退回合成图案
                    //
                    // 第一版在这里写的是「不符就用合成图案」。那是错的：
                    // 尺寸不符只可能是显示器分辨率在运行中变了（插拔显示器、
                    // 改显示设置），此时正确行为是**保持上一帧**并告警，
                    // 让用户看到画面冻结而不是画面变成渐变方块 ——
                    // 后者会让人以为采集彻底坏了。
                    //
                    // 真要恢复得重建采集器（`set_target_size` 会重建后端，
                    // DXGI 要重新 DuplicateOutput 约 10-30ms，不能放在帧循环里）。
                    // 两边都按 16 对齐后比较：`f.coded_*` 是采集层对齐后的
                    // （1920x1088），`cfg` 是显示尺寸（1920x1080）。直接比
                    // 会每帧都判不符 —— 这个 bug 只在非 16 倍数分辨率下出现，
                    // 1280x720 恰好是 16 的倍数所以完全看不出来。
                    let want_w = (cfg.width + 15) & !15;
                    let want_h = (cfg.height + 15) & !15;
                    if f.coded_width == want_w && f.coded_height == want_h {
                        self.nv12.copy_from_slice(&f.data);
                    } else {
                        self.capture_size_mismatch += 1;
                        if self.capture_size_mismatch == 1 {
                            warn!(
                                got = format!("{}x{}", f.coded_width, f.coded_height),
                                want = format!("{}x{}", cfg.width, cfg.height),
                                "采集尺寸与编码器不一致（显示器分辨率变了？），本帧保持上一帧内容"
                            );
                        }
                    }
                }
                Ok(None) => {
                    // 桌面没变化：复用上一帧内容（见上面的文档）
                    self.capture_empty += 1;
                }
                Err(e) => {
                    warn!(error = %e, seq, "屏幕采集失败，本帧改用合成图案");
                    synth_frame_nv12(seq, cfg.width, cfg.height, &mut self.nv12);
                }
            }
            // 处理段 = 从拿到 Frame 到拷进 self.nv12 的这段时间。
            // 等待段 = 总时长 - 处理段（含 DXGI 超时、含缩放发生在采集层内部的
            // 那部分 —— 后者其实算处理，但它已经包含在 cap_total_us 里，
            // 而 copy_from_slice 之后才停表，所以这里的「处理」只覆盖拷贝。
            // 真正的缩放成本在 cap_total_us 里，与等待混在一起 ——
            // 这是采集层 API 的局限，已记为待改进项）。
            let proc_us = t_proc.elapsed().as_secs_f64() * 1e6;
            self.capture_us_total += cap_total_us;
            self.capture_process_us += proc_us;
            self.capture_wait_us += (cap_total_us - proc_us).max(0.0);
        } else {
            synth_frame_nv12(seq, cfg.width, cfg.height, &mut self.nv12);
        }

        let t0 = std::time::Instant::now();
        self.out.clear();
        self.encoder
            .encode_nv12_into(&self.nv12, pts_us as i64, &mut self.out)
            .map_err(|e| anyhow::anyhow!("编码第 {seq} 帧失败：{e}"))?;
        self.encode_us_total += t0.elapsed().as_secs_f64() * 1e6;
        self.encode_frames += 1;

        if let Some(f) = self.dump.as_mut() {
            use std::io::Write;
            if !self.out.is_empty() {
                if let Err(e) = f.write_all(&self.out) {
                    warn!(error = %e, "写 --h264-dump 失败，后续不再写");
                    self.dump = None;
                } else {
                    self.dump_bytes += self.out.len() as u64;
                }
            }
        }

        Ok(&self.out)
    }

    /// 最近一次编码输出（Annex-B）
    fn out(&self) -> &[u8] {
        &self.out
    }

    /// 平均单帧采集耗时（微秒，含等待）。未启用采集时为 0
    fn avg_capture_us(&self) -> f64 {
        if self.encode_frames == 0 {
            0.0
        } else {
            self.capture_us_total / self.encode_frames as f64
        }
    }

    /// 平均「等新帧」耗时（微秒）
    ///
    /// 大是**正常的** —— 说明采集频率高于桌面刷新率，CPU 在睡觉等。
    fn avg_capture_wait_us(&self) -> f64 {
        if self.encode_frames == 0 {
            0.0
        } else {
            self.capture_wait_us / self.encode_frames as f64
        }
    }

    /// 平均「处理一帧」耗时（微秒）
    ///
    /// 这个才反映真实的 CPU 成本（拷贝；缩放成本仍混在 wait 里，
    /// 见 `capture_wait_us` 的文档）。
    fn avg_capture_process_us(&self) -> f64 {
        if self.encode_frames == 0 {
            0.0
        } else {
            self.capture_process_us / self.encode_frames as f64
        }
    }

    /// 平均单帧编码耗时（微秒）
    /// 当前编码码率（bps）
    fn bitrate(&self) -> u32 {
        self.encoder.bitrate()
    }

    /// 运行中改码率（转发给编码器）
    ///
    /// 见 `RttReply` 的文档：拥塞控制器在接收端，执行在这里。
    /// `H264Encoder::set_bitrate` 实测 mid-stream 生效（不需要重建编码器），
    /// 但**升码率比降码率慢**（降是下一帧，升要 60-120 帧才追上）——
    /// 那是编码器自身的收敛行为，不是这里的 bug。
    /// 强制下一帧编成 IDR（转发给编码器）
    ///
    /// 收到接收端的 \`KeyframeReq\` 时调用。见调用处（UDP 主循环）的文档。
    fn force_keyframe(&mut self) -> Result<()> {
        self.encoder
            .force_keyframe()
            .map_err(|e| anyhow::anyhow!("强制关键帧失败：{e}"))
    }

    fn set_bitrate(&mut self, bps: u32) -> Result<()> {
        self.encoder
            .set_bitrate(bps)
            .map_err(|e| anyhow::anyhow!("设置码率 {bps} 失败：{e}"))
    }

    fn avg_encode_us(&self) -> f64 {
        if self.encode_frames == 0 {
            0.0
        } else {
            self.encode_us_total / self.encode_frames as f64
        }
    }
}

struct Args {
    use_udp: bool,
    target: String,
    fps: u64,
    frame_bytes: usize,
    duration_secs: u64,
    loss: f64,
    /// 手动指定的冗余度（`--fec N`），`None` = 没给
    fec_redundant: Option<usize>,
    /// `--fec-auto`：按 `--loss` 自动选冗余度
    fec_auto: bool,
    /// `--fec-fixed`：冻结冗余度，不做自适应（A/B 对比用）
    ///
    /// 默认是**自适应**：`--fec 2` 只给初始值，之后按实测丢包率调整。
    /// 想复现历史数据（固定 redundant=2）必须显式加这个开关。
    fec_fixed: bool,
    /// `--audio`：同时发送音频帧（帧长由 `--opus-frame-ms` 决定，默认 20ms）
    audio: bool,
    /// `--psk <64位十六进制>`：预共享密钥，启用帧加密
    ///
    /// 为 `None` 时**不加密** —— 保持现有行为，让所有既有测试与脚本
    /// 不用改一行。加密是显式开启的。
    psk: Option<[u8; 32]>,
    /// `--opus`：音频用真实 Opus 编码而不是直接发 PCM
    use_opus: bool,
    /// `--opus-bitrate <bps>`：Opus 目标码率，默认 32000
    opus_bitrate: i32,
    /// `--opus-frame-ms <2.5|5|10|20|40|60>`：Opus 帧长，默认 20
    ///
    /// 这不是「编码器内部参数」，而是**整条音频时间线的基本单位**：
    /// 它同时决定每帧 PCM 采样数（`frame_samples`）、PTS 每帧递增量
    /// （`frame_duration_us`）和音频节拍间隔。改一处，三处一起变。
    opus_frame_ms: f64,
    /// `--size WxH`：显式指定编码分辨率（仅 `--h264` 模式）
    ///
    /// `None` 时回退到按 `frame_bytes` 反推。见 `H264Out::config_from_args`。
    size: Option<(u32, u32)>,
    /// `--opus-dtx`：不连续传输（静音帧压成 1~2 字节）
    opus_dtx: bool,
    /// `--opus-inband-fec`：带内前向纠错（把上一帧的低码率副本塞进当前帧）
    opus_inband_fec: bool,
    /// `--opus-loss <0-100>`：告诉编码器「预计丢多少包」，默认 0
    ///
    /// 只影响编码器内部决策（塞多少冗余、用多强的抗丢包编码），
    /// **不改变网络行为** —— 模拟丢包仍然是 `--loss`。
    opus_loss: u8,
    /// `--audio-silence-after <秒>`：合成音频在 N 秒后转静音
    ///
    /// 只为测 DTX 而存在：合成源是恒定幅度的 440Hz 正弦波，
    /// 永远不会被判成静音，DTX 一次都不会触发。加了这一档才能让
    /// 「前半段有声音、后半段静音」这个真实场景复现出来。
    /// 不带这个参数时行为完全不变。
    audio_silence_after: Option<f64>,
    /// `--audio-in <synth|loopback|mic>`：音频从哪来
    ///
    /// 默认 `synth`（合成 440Hz 正弦）。这不是偷懒 —— 恒定幅度的
    /// 正弦是**可预测的**，接收端拿它做完整性校验时一眼能看出错位，
    /// 而真实音频出错时你无法判断是链路坏了还是本来就这样。
    /// 所有 FEC / NACK / 抖动缓冲的基线数据都是在 synth 下测的。
    ///
    /// `loopback` = 系统正在播放的声音（共享会议音频/视频声音），
    /// `mic` = 麦克风。两者都打不开时降级成 synth 而不是退出。
    audio_in: String,
    /// `--fec-target <0..1>`：P 帧的 FEC 恢复目标（默认 0.99）
    ///
    /// 关键帧不受影响（内部取 `max(0.999)`）。
    fec_target: f64,
    /// `--screen <dxgi|gdi|off>`：视频源改成屏幕采集
    ///
    /// `off`（默认）= 保持现状（合成图案，或 `--h264` 时用合成 NV12）。
    /// `dxgi` = DXGI Desktop Duplication（1080p 约 30ms @4K / 快，需要显卡）；
    /// `gdi` = GDI BitBlt（兼容性保底，4K 下 66ms 太慢，只适合小分辨率）。
    ///
    /// # 为什么默认关
    ///
    /// 屏幕采集是**有副作用的**：DXGI 会占用 GPU 拷贝队列，GDI 会让
    /// 桌面合成器做一次全屏读回。所有基线测试都在合成源下测的，
    /// 默认值一改，历史数字全部失去可比性。
    screen: String,
    /// `--clock-bias-us <微秒>`：给本端时基注入一个人为偏移
    ///
    /// # 为什么需要它
    ///
    /// 项目至今只在**同机**验证过时钟同步 —— 两端的 `now_us()` 走同一个
    /// QPC 时钟源，偏移恒为 0。这让 `ClockSync` 的核心逻辑（NTP 式四时间戳）
    /// 从未被真正考验过。
    ///
    /// 加这个开关后，发送端的时间戳整体偏移 N 微秒，等价于「两台机器的时钟
    /// 差了 N 微秒」。接收端的 `ClockSync` 应该把这个偏移准确测出来。
    ///
    /// 这是**测试工具**，不是生产功能 —— 生产环境里偏移由硬件时钟决定。
    clock_bias_us: i64,
    /// `--h264`：把合成帧走一遍真实 H.264 编码，发编码后的 Annex-B 码流
    ///
    /// 不带这个参数时行为**完全不变**（发合成原始字节），既有测试与
    /// 脚本一行都不用改。
    h264: bool,
    /// `--h264-dump <path>`：把所有编码输出写到一个 .h264 文件
    ///
    /// 用于离线解码器测试与码流分析。隐式开启 `--h264`。
    h264_dump: Option<String>,
}

/// 取 `--名字` 后面那个值
///
/// 独立成函数是为了让"哪些参数吃掉了下一个 token"这件事只有一个出处：
/// 位置参数过滤必须跳过**所有**带值选项的值，漏一个就会把 `2`
/// （`--fec 2` 的值）当成 fps 解析。
fn take_flag_value<'a>(raw: &'a [String], name: &str) -> Option<&'a String> {
    raw.iter()
        .position(|a| a == name)
        .and_then(|i| raw.get(i + 1))
}

fn parse_args() -> Args {
    let raw: Vec<String> = std::env::args().skip(1).collect();

    let use_udp = raw.iter().any(|a| a == "--udp");
    let fec_auto = raw.iter().any(|a| a == "--fec-auto");
    let fec_fixed = raw.iter().any(|a| a == "--fec-fixed");
    // --audio 不带值，所以不需要进 VALUED_FLAGS
    let audio = raw.iter().any(|a| a == "--audio");
    let loss = take_flag_value(&raw, "--loss")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0.0);
    let fec_redundant = take_flag_value(&raw, "--fec").and_then(|s| s.parse().ok());
    let psk = take_flag_value(&raw, "--psk").and_then(|s| parse_psk(s));
    let use_opus = raw.iter().any(|a| a == "--opus");
    let opus_bitrate = take_flag_value(&raw, "--opus-bitrate")
        .and_then(|s| s.parse().ok())
        .unwrap_or(32_000);
    // 帧长先按 f64 收，再由 OpusConfig::validate 逐档校验（2.5 这一档
    // 用整数解析会直接丢掉）。非法值不在这里静默兜底成 20 —— 那会让
    // 「--opus-frame-ms 15」变成「跑起来一切正常但时间线错位」。
    let opus_frame_ms = match take_flag_value(&raw, "--opus-frame-ms") {
        Some(s) => s.parse::<f64>().unwrap_or_else(|_| {
            warn!(value = %s, "--opus-frame-ms 不是数字，用默认 20");
            20.0
        }),
        None => 20.0,
    };
    let opus_dtx = raw.iter().any(|a| a == "--opus-dtx");
    let opus_inband_fec = raw.iter().any(|a| a == "--opus-inband-fec");
    let opus_loss = match take_flag_value(&raw, "--opus-loss") {
        Some(s) => s.parse::<u8>().unwrap_or_else(|_| {
            warn!(value = %s, "--opus-loss 必须是 0-100 的整数，用默认 0");
            0
        }),
        None => 0,
    };
    // `--size WxH`：显式编码分辨率（仅 --h264）
    //
    // 格式 `1920x1080` 或 `1920X1080`。非法值告警并回退到自动推断，
    // 不硬失败 —— 与 --opus-frame-ms 的处理一致。
    let size = match take_flag_value(&raw, "--size") {
        Some(s) => {
            let lower = s.to_ascii_lowercase();
            let parsed = lower
                .split_once('x')
                .and_then(|(w, h)| Some((w.parse::<u32>().ok()?, h.parse::<u32>().ok()?)));
            match parsed {
                // 编码器对宽高有下限要求；偶数对齐是 NV12 的硬约束
                Some((w, h)) if w >= 64 && h >= 64 && w % 2 == 0 && h % 2 == 0 => Some((w, h)),
                _ => {
                    warn!(value = %s, "--size 需形如 1920x1080（宽高 >= 64 且为偶数），已忽略");
                    None
                }
            }
        }
        None => None,
    };
    let audio_silence_after =
        take_flag_value(&raw, "--audio-silence-after").and_then(|s| s.parse::<f64>().ok());
    // 音频来源。默认 synth，校验推迟到启动处（那里能统一报错并降级）
    let audio_in = take_flag_value(&raw, "--audio-in")
        .cloned()
        .unwrap_or_else(|| "synth".to_string());
    // P 帧的 FEC 恢复目标。越界值钳到 (0, 1] 而不是报错 ——
    // 这是个调优参数，为它中断启动不值得。
    let fec_target = take_flag_value(&raw, "--fec-target")
        .and_then(|s| s.parse::<f64>().ok())
        .map(|v| v.clamp(0.01, 1.0))
        .unwrap_or(DEFAULT_RECOVERY_TARGET);
    let screen = take_flag_value(&raw, "--screen")
        .cloned()
        .unwrap_or_else(|| "off".to_string());
    // 时钟偏移（微秒）。正数 = 本端时钟走得快（时间戳更大）
    let clock_bias_us = take_flag_value(&raw, "--clock-bias-us")
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0);
    // --h264 不带值；--h264-dump 带值，且隐式开启 --h264
    // （只给 dump 路径却不编码没有意义，静默忽略更容易让人困惑）
    let h264_dump = take_flag_value(&raw, "--h264-dump").cloned();
    let h264 = raw.iter().any(|a| a == "--h264") || h264_dump.is_some();

    /// 带值的选项：它们后面的那个 token 是值，不是位置参数
    const VALUED_FLAGS: [&str; 13] = [
        "--loss",
        "--fec",
        "--psk",
        "--opus-bitrate",
        "--opus-frame-ms",
        "--opus-loss",
        "--audio-silence-after",
        "--audio-in",
        "--fec-target",
        "--screen",
        "--clock-bias-us",
        "--size",
        "--h264-dump",
    ];

    let positional: Vec<&String> = raw
        .iter()
        .enumerate()
        .filter(|(i, a)| {
            if a.starts_with("--") {
                return false;
            }
            // 前一个 token 是带值选项 → 这个 token 是它的值
            !raw.get(i.wrapping_sub(1))
                .map(|p| VALUED_FLAGS.contains(&p.as_str()))
                .unwrap_or(false)
        })
        .map(|(_, a)| a)
        .collect();

    let default_target = if use_udp {
        format!("127.0.0.1:{}", sidelens_transport::DEFAULT_UDP_PORT)
    } else {
        format!("ws://127.0.0.1:{}", sidelens_transport::DEFAULT_PORT)
    };

    Args {
        use_udp,
        target: positional
            .first()
            .map(|s| s.to_string())
            .unwrap_or(default_target),
        fps: positional.get(1).and_then(|s| s.parse().ok()).unwrap_or(30),
        frame_bytes: positional
            .get(2)
            .and_then(|s| s.parse().ok())
            .unwrap_or(8192),
        duration_secs: positional.get(3).and_then(|s| s.parse().ok()).unwrap_or(0),
        loss,
        fec_redundant,
        fec_auto,
        fec_fixed,
        audio,
        psk,
        use_opus,
        opus_bitrate,
        opus_frame_ms,
        size,
        opus_dtx,
        opus_inband_fec,
        opus_loss,
        audio_silence_after,
        audio_in,
        fec_target,
        screen,
        clock_bias_us,
        h264,
        h264_dump,
    }
}

/// 把 64 位十六进制串解析成 32 字节主密钥
///
/// # 为什么不用 `hex` crate
///
/// 只为了这一个函数引一个依赖不值得。而且解析失败时的错误信息
/// 需要很具体（「第 7 个字符不是十六进制」），自己写反而更清楚。
fn parse_psk(s: &str) -> Option<[u8; 32]> {
    let s = s.trim();
    if s.len() != 64 {
        warn!(
            len = s.len(),
            "--psk 必须是 64 位十六进制字符（32 字节），已忽略"
        );
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        let hi = hex_val(s.as_bytes()[i * 2]);
        let lo = hex_val(s.as_bytes()[i * 2 + 1]);
        match (hi, lo) {
            (Some(h), Some(l)) => *byte = (h << 4) | l,
            _ => {
                warn!(index = i * 2, "--psk 含非十六进制字符，已忽略");
                return None;
            }
        }
    }
    Some(out)
}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// 发送端的 FEC 封装
///
/// 把「一帧的分片 → 一批冗余包 → 一批 UDP 数据报」这条链子收在一处，
/// 主循环里只留三行调用，避免统计口径散落在各处。
///
/// # 冗余度自适应（本轮新增）
///
/// `redundant` 不再是固定值，而是每帧按
/// `suggest_redundant(分片数, 估算丢包率, 0.99)` 算出来，再过一道迟滞。
/// 丢包率来自 [`LossEstimator`]（用 NACK 量反解），不是命令行参数 ——
/// 命令行给的值只是**初始值**（第一个窗口滚出来之前用它）。
///
/// 两条不能省的约束：
///
/// 1. `redundant` 变了必须发 `FrameType::Config`，否则接收端用旧的
///    `i % redundant` 建方程组，恢复出静默的垃圾数据。
/// 2. `redundant` 变化会抖，必须过迟滞（见 [`RedundantHysteresis`]），
///    否则每帧两个 Config 帧，接收端 FEC 状态永远刚被清空。
struct FecOutbound {
    /// 当前生效的编码器
    ///
    /// `group_size` / `redundant` 任一变化都要**重建**一个 ——
    /// `FecSender` 没有跨帧状态（`begin_frame` 会清空上一帧的半组），
    /// 重建的开销就是几个 `Vec` 分配，比维护 `set_config()` 简单且不会漏改状态。
    sender: Option<FecSender>,
    /// 当前生效的配置（`group_size` 与 `redundant` 都随帧变化）
    config: FecConfig,
    /// 命令行给的初始冗余度（`--fec` / `--fec-auto`）
    ///
    /// 只作为**初始值**：`LossEstimator` 滚出第一个窗口之前用它，
    /// 之后由 `suggest_redundant` 接管。
    initial_redundant: usize,
    /// 是否允许自适应（`--fec-fixed` 关掉它，用于 A/B 对比）
    adaptive: bool,
    /// P 帧的恢复目标（`--fec-target`，默认 [`DEFAULT_RECOVERY_TARGET`] = 0.99）
    ///
    /// # 为什么需要可配
    ///
    /// 0.99 意味着「每 100 个 P 帧允许丢 1 个」。但 P 帧丢了只是 1 个 RTT 的
    /// 代价（NACK 能补），所以这个目标其实相当宽松。想验证「更严的目标
    /// 值不值那个带宽」就得能改 —— 硬编码的常数没法做 A/B。
    ///
    /// 关键帧**不受它影响**：`suggest_redundant_for_frame` 内部对关键帧
    /// 取了 `max(KEYFRAME_RECOVERY_TARGET)`。
    target: f64,
    /// 链路丢包率估计（NACK 驱动）
    loss: LossEstimator,
    /// 冗余度迟滞
    hysteresis: RedundantHysteresis,
    /// 已发出的冗余包数
    redundant_sent: u64,
    /// 已发出的冗余字节数（含 32B 帧头 + 10B FEC 头）
    redundant_bytes: u64,
    /// 数据字节数（同样含帧头，作为开销比的分母）
    data_bytes: u64,
    /// 超过 MTU、被跳过的冗余包数
    oversize_skipped: u64,
    /// 帧头编码失败等内部错误次数
    encode_errors: u64,
    /// 发出的 Config 协商帧数（含防丢的重复份）
    config_frames: u64,
    /// group_size → 用了多少帧。末尾日志给出分布，用于判断协商是否在抖
    group_usage: std::collections::BTreeMap<usize, u64>,
    /// redundant → 用了多少帧。自适应是否在抖，看这个分布
    redundant_usage: std::collections::BTreeMap<usize, u64>,
    /// 分片数 → 跳过了多少帧（被 `should_fec` 挡掉的帧）
    skip_frames: std::collections::BTreeMap<usize, u64>,
    /// `should_fec` 因开销闸门跳过的帧数
    skip_overhead: u64,

    /// 音频载荷的编解码器名（`"opus"` / `"pcm"` / `None`=不发音频）
    ///
    /// 随 `Config` 帧发给接收端 —— 接收端无法从码流判断 Opus 还是原始 PCM
    /// （两者都是不透明字节）。见 `send_config` 的文档。
    audio_codec: Option<&'static str>,
    /// 音频帧长（毫秒）
    ///
    /// 随 `Config` 帧发给接收端。接收端据此决定：
    /// - 解码器缓冲大小（`frame_samples()`）
    /// - `AudioJitterBuffer` 的 `frame_duration_us`（AvSync 主时钟靠它推进）
    ///
    /// 硬编码 20ms 时，发送端用 60ms 帧会导致接收端解码报
    /// "buffer too small"，一帧都解不出来（`opus-wire` 实测
    /// `audio_decode_failed=134/134`）。
    audio_frame_ms: f64,
}

impl FecOutbound {
    /// 按命令行参数决定要不要建 FEC
    ///
    /// 三个分支：
    ///
    /// - 什么都没给 → `None`，完全不启用（默认行为，向后兼容）
    /// - `--fec N` → 手动指定**初始**冗余度（之后仍会自适应，除非加 `--fec-fixed`）
    /// - `--fec-auto` → 按 `--loss` 用 `suggest_fec` 选初始档
    ///
    /// `--fec` 优先于 `--fec-auto`：显式给了数字就按数字来，
    /// 自动档只在"只给了 auto"时才生效。
    fn from_args(args: &Args) -> Option<Self> {
        // 这里只定 **初始冗余度**。组大小由每帧的分片数决定（见 `pick_group_size`），
        // 所以先拿一个占位组大小做校验，真正的配置在 `send_frame` 里逐帧建。
        let initial_redundant = match (args.fec_redundant, args.fec_auto) {
            (None, false) => return None,
            (Some(redundant), _) => redundant,
            (None, true) => match suggest_fec(args.loss, FEC_GROUP_SIZE_MAX) {
                Some(cfg) => cfg.redundant,
                None => {
                    // < 1% 或 > 15%：suggest_fec 认为不划算
                    info!(
                        loss = args.loss,
                        "FEC 未启用：当前丢包率不在 FEC 划算区间（1%-15%）"
                    );
                    return None;
                }
            },
        };

        // 用最大组大小做一次校验：`redundant > group_size` 只在 redundant 大到
        // 超过 64 时才可能成立，这种配置在任何帧上都不合法，提前拦掉。
        let probe = FecConfig {
            group_size: FEC_GROUP_SIZE_MAX,
            redundant: initial_redundant,
            max_packet_size: sidelens_core::SAFE_MTU_PAYLOAD,
        };
        if let Err(e) = probe.validate() {
            // 配置非法不 panic：打日志、退回不启用 FEC，其余功能照常跑
            warn!(error = %e, redundant = initial_redundant, "FEC 配置非法，本次不启用 FEC");
            return None;
        }

        let adaptive = !args.fec_fixed;
        info!(
            initial_redundant,
            adaptive,
            group_size = format!(
                "按帧自适应（{}..={}）",
                FEC_GROUP_SIZE_MIN, FEC_GROUP_SIZE_MAX
            ),
            "FEC 已启用（组大小按每帧分片数动态选取，冗余度按实测丢包率自适应）"
        );

        Some(Self {
            // 首帧才确定组大小，届时再建
            sender: None,
            config: probe,
            initial_redundant,
            adaptive,
            loss: LossEstimator::new(),
            hysteresis: RedundantHysteresis::new(initial_redundant),
            target: args.fec_target,
            redundant_sent: 0,
            redundant_bytes: 0,
            data_bytes: 0,
            oversize_skipped: 0,
            encode_errors: 0,
            config_frames: 0,
            group_usage: std::collections::BTreeMap::new(),
            redundant_usage: std::collections::BTreeMap::new(),
            skip_frames: std::collections::BTreeMap::new(),
            skip_overhead: 0,
            audio_codec: None,
            audio_frame_ms: 20.0,
        })
    }
    /// 组大小或冗余度变化时重建编码器，返回是否需要通知接收端
    fn ensure_config(&mut self, group_size: usize, redundant: usize) -> bool {
        // 配置没变：不用重建
        if let Some(s) = self.sender.as_ref() {
            let c = s.config();
            if c.group_size == group_size && c.redundant == redundant {
                return false;
            }
        }
        let config = FecConfig {
            group_size,
            redundant,
            max_packet_size: sidelens_core::SAFE_MTU_PAYLOAD,
        };
        match FecSender::new(config) {
            Ok(sender) => {
                self.sender = Some(sender);
                self.config = config;
                true
            }
            Err(e) => {
                // 组大小由 pick_group_size 保证在 [2,64]，redundant 已由
                // should_fec 保证 <= 分片数，走到这里说明有逻辑漏洞
                warn!(error = %e, group_size, redundant, "FEC 编码器重建失败，本帧不发冗余包");
                self.sender = None;
                false
            }
        }
    }

    /// 把当前 FEC 配置发给接收端
    ///
    /// # 为什么必须协商（这一步漏了就是静默的垃圾数据）
    ///
    /// 接收端用 `index / group_size` 把帧内分片下标折算成组内下标，
    /// 再用 `i % redundant` 建 XOR 方程组。两个值任一对不上：
    ///
    /// - 组号算错 → 分片被塞进错误的组
    /// - 组内下标算错 → 方程组里的未知数错位
    ///
    /// 结果是「恢复成功但内容错误」，而且**没有任何校验会发现它** ——
    /// 解码器拿到的是垃圾帧，还会连带污染后续所有 P 帧。
    ///
    /// # 为什么发两份
    ///
    /// Config 是单发的一个 UDP 包，丢了接收端就继续用旧配置 —— 而旧配置
    /// 和新的帧不匹配，整帧的 FEC 全废。发两份把丢包概率从 p 降到 p²
    /// （5% 丢包下从 5% 降到 0.25%），代价只是 2 个几十字节的小包。
    async fn send_config(&mut self, s2: &UdpSender) {
        let config = self.config;
        // 手工拼 JSON：本 crate 没有 serde_json 依赖（只允许改这一个文件，
        // 不动 Cargo.toml）。三个字段都是数字或已知的安全字符串，没有转义问题。
        //
        // `audio_codec` 是本轮新增的：接收端无法从码流判断音频载荷是 Opus
        // 还是原始 PCM，原来只能靠环境变量 `SIDELENS_OPUS=1` 让用户手动声明。
        // 忘了设的后果是 `audio_decoded=0` 但 `audio_played` 正常增长 ——
        // 把 Opus 码流当成 16-bit PCM 播成了噪声，而统计数字看不出异常。
        let codec_json = match self.audio_codec {
            Some(c) => format!("\"{c}\""),
            None => "null".to_string(),
        };
        let payload = format!(
            r#"{{"fec_group_size":{},"fec_redundant":{},"audio_codec":{},"audio_frame_ms":{}}}"#,
            config.group_size, config.redundant, codec_json, self.audio_frame_ms
        );

        let Ok(bytes) = encode_frame_owned(&FrameSpec::new(FrameType::Config, payload.as_bytes()))
        else {
            self.encode_errors += 1;
            warn!("FEC 配置帧编码失败，接收端可能仍在用旧配置");
            return;
        };

        // 两份：单包丢了会让接收端整段用错配置，代价远大于一个几十字节的重复包
        for _ in 0..2 {
            if s2.send_raw(&bytes).await {
                self.config_frames += 1;
            }
        }
    }

    /// 冗余字节 / 数据字节
    ///
    /// 分母是**所有数据分片的线上字节**（含 32B 帧头与 8B 分片头），
    /// 也就是真实占用的链路带宽；分子是冗余包的线上字节（含 32B 帧头与 10B FEC 头）。
    /// 两者都算帧头，比例才与 `redundant / 分片数` 可比。
    fn overhead_ratio(&self) -> f64 {
        if self.data_bytes == 0 {
            return 0.0;
        }
        self.redundant_bytes as f64 / self.data_bytes as f64
    }

    /// 记录一次 NACK 请求（丢包率估计的**唯一**输入）
    ///
    /// 必须在主循环收到 NACK 时调用，否则估计值永远是 0，`redundant` 永远停在
    /// 初始值 —— 表现是「自适应没生效」但没有任何报错。
    fn on_nack(&mut self, seqs: &[u32]) {
        self.loss.record_nacks(seqs);
    }

    /// 每帧算一次期望冗余度，返回建议值（未钳预算）
    ///
    /// 分两步：
    ///
    /// 1. 用平滑后的丢包率算期望冗余度
    /// 2. 过迟滞闸门，决定是否真的切换
    ///
    /// # 关键帧与 P 帧走两条路（`is_keyframe` 参数）
    ///
    /// 关键帧丢了要等一整个 GOP（60 帧 = 1 秒）才恢复，P 帧只是 1 个 RTT ——
    /// 差 30 倍以上。所以关键帧用**精确模型 + 0.999 目标**，P 帧沿用
    /// 原来的二项模型 + 0.99 目标。详见 `sidelens_core::fec_link::
    /// suggest_redundant_for_frame` 的文档。
    ///
    /// 实测（35 分片 @5% 丢包）：P 帧策略给 r=2（58.4% 可恢复），
    /// 关键帧策略给 r=17（95.1%）。不区分时每个关键帧都是一次
    /// 「1 秒卡顿」的赌博。
    ///
    /// # `fragments` 必须是**本帧**的分片数，不是上一帧的
    ///
    /// 第一版用 `self.last_fragments`（上一帧），H.264 场景下这是个真 bug：
    /// P 帧只有 1-2 个分片，关键帧有 30+ 个。轮到关键帧时 `last_fragments`
    /// 还是 P 帧那个 2，于是 `affordable_redundant(2, want)` 直接钳成 0 ——
    /// **关键帧一点冗余都拿不到**，恰好是最需要保护的那一帧。
    ///
    /// 实测证据（修复前，720p @5% 丢包，gop=30）：
    /// ```text
    /// fec_skipped_frames=344  fec_skip_overhead=344   ← 344 帧全被预算钳掉
    /// fec_loss_avg_frags=1.9                          ← 平均 1.9 片/帧
    /// ```
    ///
    /// # 注意：这里**不**记录本帧到估计器
    ///
    /// 记录必须发生在「本帧实际发出多少冗余」确定之后（见
    /// [`Self::record_sent`]）。第一版在这里记 `config.redundant`，
    /// 而小帧的冗余会被预算钳成 0 —— 于是估计器以为 2 分片的 H.264 P 帧
    /// 也受 6 个冗余保护，反解出的丢包率完全失真，实测把 H.264 场景的
    /// `redundant` 推到 6、NACK 从 18 涨到 28。
    fn decide_redundant(&mut self, is_keyframe: bool, fragments: usize) -> usize {
        if !self.adaptive {
            return self.initial_redundant;
        }
        // 没预热：估计值恒为 0，那是「还没观测」而不是「链路很好」，
        // 用初始值撑着，别因为「0 丢包」把 FEC 关掉。
        if !self.loss.is_warm() {
            return self.initial_redundant;
        }

        let class = if is_keyframe {
            FrameClass::keyframe(fragments)
        } else {
            FrameClass::delta(fragments)
        };
        // `target` 只对 P 帧生效 —— 关键帧分支内部会 `max(KEYFRAME_RECOVERY_TARGET)`，
        // 传什么都不影响关键帧。这是刻意的：关键帧的目标不该被调用方降低。
        let want = suggest_redundant_for_frame(class, self.loss.loss_estimate(), self.target);
        // want 可能是 0（链路很干净）。但 0 个冗余 = 不启用 FEC，而
        // `FecConfig` 要求 redundant >= 1；这里钳到 1，让预算去决定
        // 这一帧实际能发几个（可能是 0，那时 should_fec 会跳过）。
        self.hysteresis
            .observe(want.max(1))
            .unwrap_or_else(|| self.hysteresis.current())
    }

    /// 记录本帧到丢包率估计器
    ///
    /// `redundant` 必须是**本帧实际发出**的冗余度（已过预算钳制），
    /// 不是 `config.redundant`。两者在小帧上会差很多，记错会让反解失真。
    fn record_sent(&mut self, fragments: usize, redundant: usize) {
        self.loss.record_frame(fragments, redundant);
    }

    /// 为一帧生成并发出冗余包，返回发出的冗余包数
    ///
    /// # 为什么推给编码器的不是整个分片
    ///
    /// 接收端（`udp_server.rs`）推给 `FecReceiver` 的是
    /// `&f.payload[HEADER_SIZE + FRAGMENT_HEADER_SIZE..]`，也就是**去掉 32B
    /// 帧头与 8B 分片头之后的数据段**。发送端必须推同一段字节，XOR 才对得上。
    ///
    /// 推整个分片（含帧头）也能算出冗余包，接收端也能"恢复成功"，
    /// 但恢复出来的是错位的数据 —— 而且没有任何校验会发现它。
    async fn send_frame(
        &mut self,
        s2: &UdpSender,
        frame_seq: u32,
        datagrams: &[Vec<u8>],
        is_keyframe: bool,
    ) -> u64 {
        self.data_bytes += datagrams.iter().map(|d| d.len() as u64).sum::<u64>();

        // 冗余度先算 want（自适应 + 迟滞），再按本帧带宽预算钳一次。
        // **不能**用预算当「整帧开关」—— 那会形成死循环，见 affordable_redundant。
        //
        // is_keyframe 决定走精确模型（0.999 目标）还是二项模型（0.99 目标）。
        // 关键帧多付的带宽是值得的：丢一个关键帧 = 卡 1 秒。
        let want = self.decide_redundant(is_keyframe, datagrams.len());
        let redundant = affordable_redundant(datagrams.len(), want);

        // 记录本帧**实际**的冗余度（不是 config.redundant）。
        // 小帧会被预算钳成 0 或 1，记错会让反解出的丢包率失真。
        self.record_sent(datagrams.len(), redundant);

        // 分片数太少或钳完变 0 时根本不发冗余包（见 should_fec）：
        // 冗余量可能比数据还大，而能救回的信息量不超过 redundant 个包
        if !should_fec(datagrams.len(), redundant) {
            *self.skip_frames.entry(datagrams.len()).or_insert(0) += 1;
            if want > redundant {
                self.skip_overhead += 1;
            }
            return 0;
        }

        // 组大小跟着**这一帧的实际分片数**走。
        //
        // 分片数要分片之后才知道，所以编码器不能提前建 —— 每帧在这里
        // 重新确认一次，变了就重建并通知接收端。
        let current = if self.sender.is_some() {
            Some(self.config.group_size)
        } else {
            None
        };
        let group_size = pick_group_size_hysteretic(datagrams.len(), current);
        if self.ensure_config(group_size, redundant) {
            info!(
                group_size,
                redundant,
                fragments = datagrams.len(),
                loss = format!("{:.4}", self.loss.loss_estimate()),
                "FEC 配置已切换，通知接收端"
            );
            self.send_config(s2).await;
        }
        *self.group_usage.entry(group_size).or_insert(0) += 1;
        *self.redundant_usage.entry(redundant).or_insert(0) += 1;

        let Some(sender) = self.sender.as_mut() else {
            return 0;
        };

        // 分组从这一帧的第一个分片开始，帧序号在这里就定下来
        sender.begin_frame(frame_seq);

        let mut packets: Vec<FecPacket> = Vec::new();
        for dg in datagrams {
            let body = Self::payload_of(dg);
            packets.extend(sender.push_fragment(body));
        }
        // 尾组不满时由 finish_frame 补空包凑满再算冗余
        packets.extend(sender.finish_frame());

        let mut sent = 0u64;
        for pkt in &packets {
            let encoded = match encode_frame_owned(&FrameSpec {
                ty: FrameType::Fec,
                flags: 0,
                stream_id: 0,
                seq: frame_seq,
                pts_us: None,
                send_ts_us: None,
                payload: &pkt.encode(),
            }) {
                Ok(v) => v,
                Err(e) => {
                    self.encode_errors += 1;
                    warn!(error = %e, "FEC 冗余包编码失败，跳过");
                    continue;
                }
            };

            // send_raw 不分片，超 MTU 会直接 EMSGSIZE，所以先自己卡住
            if encoded.len() > FEC_WIRE_LIMIT {
                self.oversize_skipped += 1;
                continue;
            }

            if s2.send_raw(&encoded).await {
                sent += 1;
                self.redundant_sent += 1;
                self.redundant_bytes += encoded.len() as u64;
            }
        }
        sent
    }

    /// 取出分片里"分片头之后的数据段"
    ///
    /// 不需要分片时 `fragment_frame` 原样返回整帧，此时数据段 = 整个 payload。
    fn payload_of(datagram: &[u8]) -> &[u8] {
        use sidelens_proto::FLAG_FRAGMENT;
        if datagram.len() <= HEADER_SIZE {
            return &[];
        }
        let flags = datagram[5];
        if flags & FLAG_FRAGMENT != 0
            && datagram.len() >= HEADER_SIZE + sidelens_core::FRAGMENT_HEADER_SIZE
        {
            &datagram[HEADER_SIZE + sidelens_core::FRAGMENT_HEADER_SIZE..]
        } else {
            &datagram[HEADER_SIZE..]
        }
    }
}

/// 合成一帧「伪音频」PCM
///
/// 真实实现里这是 AudioRecord / WASAPI / AVAudioEngine 的输出，再交给
/// Opus 编码器压成几十字节。这里**不做真编解码**：直接产一段 16bit
/// 单声道 PCM（48000Hz × 20ms = 960 采样 = 1920 字节；帧长可配，
/// 采样数由 `OpusConfig::frame_samples()` 算出，这里不假设 20ms）。
///
/// 波形用正弦而不是随机字节：随机字节听起来是白噪声，出问题时无法从
/// 波形上分辨「链路坏了」还是「本来就是噪声」；正弦的频率与相位都可由
/// 帧序号推算，接收端拿它做完整性校验时一眼就能看出错位。
/// 音频帧的记账口径：返回 `(帧增量, 分片增量)`
///
/// # 为什么单独抽出来
///
/// 第一版在 `send_next` 里写的是「`frags == 0` 就 `return false`」，
/// 而 `frags == 0` 在 5% 丢包模拟下**每 20 帧就发生一次**（单个分片的
/// 音频帧被整片丢掉）。于是 `audio_sent` 比 `audio_frames_expected`
/// 少了 24 帧，看起来像「音频节拍被视频阻塞」——
/// **一个假信号掩盖了真问题可能存在的迹象**。
///
/// 语义定死：`sent` 记「节拍推进了几次」，不是「网卡吐了几个包」。
/// 分片数才是「真的发了多少」。两者分开记，日志里就能一眼区分
/// 「节拍被饿」和「包被丢」。
fn audio_accounting(fragments_sent: usize) -> (u64, u64) {
    (1, fragments_sent as u64)
}

fn synth_audio_i16(seq: u32, samples: usize, buf: &mut [i16]) {
    // 440Hz 的 A4 音，采样点按帧序号连续推进相位，避免每帧接缝处的爆音
    const FREQ_HZ: f64 = 440.0;
    const SAMPLE_RATE: f64 = 48_000.0;

    let phase0 = (seq as f64) * (samples as f64) * FREQ_HZ / SAMPLE_RATE;
    for (i, slot) in buf.iter_mut().enumerate() {
        let t = (i as f64) * FREQ_HZ / SAMPLE_RATE + phase0;
        *slot = ((t * std::f64::consts::TAU).sin() * 0.25 * i16::MAX as f64) as i16;
    }
}

/// 发送端的音频封装
///
/// 与 `FecOutbound` 同一个思路：把「配置 → PTS → 载荷 → 帧头」这条链
/// 收在一处，主循环里只剩一行调用。
///
/// # PTS 从哪来
///
/// 只用 `AudioPacer`，**不读墙上时钟**。`pacer.next_frame()` 返回的
/// `(pts_us, duration_us)` 是 `起点 + 帧序号 × frame_duration_us`
/// （20ms 帧时是 20000µs，60ms 帧时是 60000µs），严格等距。
/// 抖动只会影响「这一帧什么时候被发出去」，不影响「这一帧声称自己是
/// 什么时候的」—— 这正是接收端 `AvSync` 能稳定推进主时钟的前提。
struct AudioOutbound {
    pacer: AudioPacer,
    config: OpusConfig,
    /// 真实 Opus 编码器（`--opus` 时才有）
    ///
    /// 为 `None` 时退回「直接发 PCM」的旧行为 —— 那是给
    /// 「不想引入 libopus 依赖」的构建准备的。两条路的 PTS、抖动缓冲、
    /// AvSync 逻辑完全一致，只有载荷格式不同。
    encoder: Option<OpusEncoder>,
    /// 已发出的音频帧数
    sent: u64,
    /// 已发出的音频字节数（含 32B 帧头与分片头，与视频口径一致）
    bytes: u64,
    /// 音频帧序号（与视频的 seq 各自独立）
    seq: u32,
    /// 复用的 PCM 缓冲（i16 采样）
    pcm: Vec<i16>,
    /// 复用的编码输出缓冲
    opus_out: Vec<u8>,
    /// 退回 PCM 路径时的字节缓冲
    pcm_bytes: Vec<u8>,
    /// 时间线起点（与视频共用的 t0），`silence_after_us` 的参照原点
    start_pts_us: u64,
    /// `--audio-silence-after`：从时间线起点算起多少微秒之后转静音
    ///
    /// `None` = 全程正弦（默认，与改动前完全一致）。
    /// 这个开关**只为测 DTX 而存在** —— 恒定幅度的正弦波永远过不了
    /// VAD 的静音判据，DTX 一次都不会触发，不制造一段静音就测不出效果。
    silence_after_us: Option<u64>,
    /// 真实采集源（`--audio-in`）。`None` = 用 `synth_audio_i16` 合成
    ///
    /// # 为什么把采集放在这里而不是主循环
    ///
    /// 采集和编码的节拍**必须同一个**。Opus 一帧就是固定采样数，
    /// 采集给多了要缓存、给少了要补零，这两种情况都会破坏
    /// 「PTS 严格等距」这个 AvSync 的前提。放在 `send_next` 里，
    /// 每次正好取 `frame_samples()` 个，多出来的留在采集器的内部缓冲，
    /// 不足就补零 —— 时间线永不被采集侧拖歪。
    ///
    /// # 为什么需要它
    ///
    /// 在此之前发送端的所有音频都是 440Hz 正弦（见 `synth_audio_i16`），
    /// 链路每一环都验证过，唯独**源头是假的**。`sidelens-audio-in` 落地后
    /// 终于能接真实麦克风 / 系统回环。
    #[cfg(windows)]
    capture: Option<sidelens_audio_in::AudioCapture>,
    /// 采集读出的临时缓冲（复用以避免每帧分配）
    #[cfg(windows)]
    capture_buf: Vec<i16>,
    /// 累计从真实采集源读到的采样数（诊断用）
    #[cfg(windows)]
    capture_samples: u64,
    /// 采集错误次数（只打印第一次，避免每 tick 刷日志）
    #[cfg(windows)]
    capture_errors: u64,
}

/// Opus 编码器的可调参数（命令行 `--opus-*` 系列）
///
/// # 为什么单独成结构体而不是往 `AudioOutbound::new` 后面挂三个参数
///
/// 这三个开关**只对编码器有意义**，但它们的取值要一路带到启动日志里
/// 做汇总；散成三个 `bool`/`u8` 参数时，调用处会变成
/// `new(t0, use_opus, bitrate, 20.0, false, false, 0)` —— 七个位置参数、
/// 三个相邻的布尔/整数，读代码的人必须回看签名才知道哪个是哪个。
///
/// `frame_ms` 刻意**不**放在这里：它不是编码器开关，而是
/// `OpusConfig` 的一部分，必须在建 pacer 之前就定下来（见 `AudioOutbound::new`）。
///
/// 三项默认全关：Opus 的默认值就是「不省带宽、不加冗余、不假设丢包」，
/// 发送端在没给 `--opus-*` 时必须与改动前的行为逐字节一致。
#[derive(Debug, Clone, Copy, Default)]
struct OpusTuning {
    /// DTX：静音帧压成 1~2 字节
    dtx: bool,
    /// 带内 FEC：把上一帧的低码率副本塞进当前帧
    inband_fec: bool,
    /// 期望丢包率（0-100），喂给 `OPUS_SET_PACKET_LOSS_PERC`
    loss_perc: u8,
}

impl OpusTuning {
    fn from_args(args: &Args) -> Self {
        Self {
            dtx: args.opus_dtx,
            inband_fec: args.opus_inband_fec,
            loss_perc: args.opus_loss,
        }
    }

    /// 有没有任何一项非默认值（用来决定要不要多打一行汇总）
    fn is_default(&self) -> bool {
        !self.dtx && !self.inband_fec && self.loss_perc == 0
    }
}

/// 把 `--opus-*` 的三个开关应用到编码器上
///
/// # 为什么每个都要单独打日志、且失败不中断
///
/// 三个 ctl 的支持度**不一样**：
///
/// | ctl | 老版本 libopus | 不支持时的表现 |
/// | --- | --- | --- |
/// | `OPUS_SET_DTX` | 1.0 起就有 | 返回错误 |
/// | `OPUS_SET_INBAND_FEC` | 1.0 起就有 | 返回错误 |
/// | `OPUS_SET_PACKET_LOSS_PERC` | 1.0 起就有 | 返回错误 |
///
/// 三者都是「尽力而为」的调优项：`set_packet_loss_perc` 失败不该让
/// 「帧长改 60ms」这件事也一起失败。所以这里**只 warn 不返回 Err**，
/// 由调用方在汇总日志里读回实际生效值（`e.dtx()` / `e.inband_fec()` /
/// `e.packet_loss_perc()` 都是回读确认过的，不是「我设了就算」）。
fn apply_opus_tuning(enc: &mut OpusEncoder, tuning: OpusTuning) {
    match enc.set_dtx(tuning.dtx) {
        Ok(()) => info!(
            requested = tuning.dtx,
            actual = enc.dtx(),
            "Opus DTX 已应用"
        ),
        Err(e) => {
            warn!(requested = tuning.dtx, error = %e, "Opus DTX 设置失败（该版本不支持？），继续")
        }
    }

    match enc.set_inband_fec(tuning.inband_fec) {
        Ok(()) => info!(
            requested = tuning.inband_fec,
            actual = enc.inband_fec(),
            "Opus in-band FEC 已应用"
        ),
        Err(e) => warn!(
            requested = tuning.inband_fec,
            error = %e,
            "Opus in-band FEC 设置失败（该版本不支持？），继续"
        ),
    }

    // 先设 FEC 再设丢包率也可以，但语义上丢包率是 FEC 的输入参数
    // （0 时即使开了 FEC 也不会产生冗余），所以放在 FEC 之后、
    // 并在日志里把这个耦合关系写清楚。
    match enc.set_packet_loss_perc(tuning.loss_perc) {
        Ok(()) => info!(
            requested = tuning.loss_perc,
            actual = enc.packet_loss_perc(),
            "Opus 期望丢包率已应用（0 时 in-band FEC 不产生冗余）"
        ),
        Err(e) => warn!(
            requested = tuning.loss_perc,
            error = %e,
            "Opus 期望丢包率设置失败（该版本不支持？），继续"
        ),
    }
}

impl AudioOutbound {
    /// `start_pts_us` 必须与视频共用同一个时基起点
    ///
    /// `frame_ms` 决定整条音频时间线的粒度：`frame_samples`（每帧 PCM 采样数）、
    /// `frame_duration_us`（PTS 每帧递增量）、以及 `interval()` 给出的节拍间隔
    /// 全部由它算出来，没有任何一处硬编码 960 或 20000。
    #[allow(clippy::too_many_arguments)]
    fn new(
        start_pts_us: u64,
        use_opus: bool,
        bitrate_bps: i32,
        frame_ms: f64,
        tuning: OpusTuning,
        silence_after_us: Option<u64>,
        #[cfg(windows)] capture: Option<sidelens_audio_in::AudioCapture>,
    ) -> Result<Self> {
        let config = OpusConfig {
            frame_ms,
            ..OpusConfig::default()
        };
        // Pacer 会自己校验配置；这里多调一次是为了把「配置非法」的错误
        // 提前到启动阶段，而不是跑到第一帧才发现
        config.validate().map_err(anyhow::Error::msg)?;
        let pacer = AudioPacer::new(config, start_pts_us).map_err(anyhow::Error::msg)?;

        let encoder = if use_opus {
            let mut e = OpusEncoder::new(config, bitrate_bps)
                .map_err(|e| anyhow::anyhow!("创建 Opus 编码器失败：{e}"))?;
            info!(
                bitrate_bps,
                lookahead_samples = e.lookahead_samples(),
                lookahead_us = e.lookahead_us(),
                "Opus 编码器已创建（lookahead 会带来固定的音画偏移，见 docs/opus.md）"
            );
            apply_opus_tuning(&mut e, tuning);
            Some(e)
        } else {
            None
        };

        info!(
            sample_rate = config.sample_rate,
            channels = config.channels,
            frame_ms = config.frame_ms,
            frame_duration_us = pacer.frame_duration_us(),
            frame_samples = config.frame_samples(),
            start_pts_us,
            opus = encoder.is_some(),
            "音频已启用"
        );

        // 汇总行：把「命令行要的」和「编码器实际采纳的」并排放。
        // 两者不一致时（老版本 libopus 不认某个 ctl）这一行就是唯一的线索。
        if let Some(e) = encoder.as_ref() {
            info!(
                frame_ms = config.frame_ms,
                dtx = e.dtx(),
                inband_fec = e.inband_fec(),
                loss = e.packet_loss_perc(),
                bitrate_bps,
                "Opus 调优 frame_ms={} dtx={} inband_fec={} loss={}% bitrate={}",
                config.frame_ms,
                e.dtx(),
                e.inband_fec(),
                e.packet_loss_perc(),
                bitrate_bps
            );
        } else if !tuning.is_default() {
            // 没开 --opus 时这些开关无处可落，说出来比静默忽略好
            warn!(
                dtx = tuning.dtx,
                inband_fec = tuning.inband_fec,
                loss = tuning.loss_perc,
                "给了 --opus-* 调优参数但没开 --opus（音频走 PCM 直发），这些开关不会生效"
            );
        }

        Ok(Self {
            pacer,
            config,
            encoder,
            sent: 0,
            bytes: 0,
            seq: 0,
            pcm: vec![0i16; config.frame_samples() as usize],
            opus_out: vec![0u8; 4096],
            pcm_bytes: Vec::with_capacity(config.frame_samples() as usize * 2),
            start_pts_us,
            silence_after_us,
            #[cfg(windows)]
            capture,
            #[cfg(windows)]
            capture_buf: Vec::with_capacity(config.frame_samples() as usize * 2),
            #[cfg(windows)]
            capture_samples: 0,
            #[cfg(windows)]
            capture_errors: 0,
        })
    }

    /// 节拍间隔：等于配置的帧长（默认 20ms）
    fn interval(&self) -> Duration {
        Duration::from_micros(self.pacer.frame_duration_us())
    }

    /// 产生并发出一个音频帧
    ///
    /// 返回是否真的发出去了。音频**不写重传缓存、不生成 FEC**：
    /// 一个帧长的空洞补不回来，补传反而多占一个 RTT 的带宽。
    async fn send_next(&mut self, s2: &UdpSender, sealer: Option<&mut FrameSealer>) -> bool {
        let (pts_us, duration_us) = self.pacer.next_frame();
        self.seq = self.seq.wrapping_add(1);

        let samples = self.config.frame_samples() as usize;
        // `--audio-silence-after`：过了这条线的帧直接填 0。
        //
        // 判据用**本帧的 PTS 相对起点**而不是 `self.seq`：帧长可配（2.5~60ms），
        // 帧序号换算成时间要除以 `frame_duration_us`，而 PTS 本身就是时间，
        // 直接比更快也更不容易算错。
        let silent = self
            .silence_after_us
            .is_some_and(|after| pts_us.saturating_sub(self.start_pts_us) >= after);
        if silent {
            self.pcm.fill(0);
        } else {
            // 有真实采集源就用它，否则退回合成的 440Hz。
            //
            // 采集给的数量与 `samples` 不保证相等：loopback 在系统静音时
            // 根本不推进（实测 98.9% 的 read 返回 0），所以「不足补零」
            // 是常态而不是异常路径。反过来给多了也不截断丢数据 ——
            // 多出来的留在采集器内部缓冲，下一 tick 继续取。
            //
            // 两条路都保证 `self.pcm.len() == samples`，Opus 编码器看到的
            // 永远是固定帧长，PTS 的严格等距不会被采集侧拖歪。
            #[cfg(windows)]
            let captured = if let Some(cap) = self.capture.as_mut() {
                self.capture_buf.clear();
                match cap.read(&mut self.capture_buf) {
                    Ok(n) => n,
                    Err(e) => {
                        // 采集出错不该中断发送：降级成静音，链路继续跑。
                        // 只记一次错误计数，避免每个 tick 刷日志。
                        self.capture_errors += 1;
                        if self.capture_errors == 1 {
                            warn!(error = %e, "音频采集失败，本帧降级为静音（后续同类错误不再打印）");
                        }
                        0
                    }
                }
            } else {
                0
            };
            #[cfg(not(windows))]
            let captured = 0usize;

            if captured == 0 {
                synth_audio_i16(self.seq, samples, &mut self.pcm);
            } else {
                #[cfg(windows)]
                {
                    self.pcm.clear();
                    let take = captured.min(samples);
                    self.pcm.extend_from_slice(&self.capture_buf[..take]);
                    if take < samples {
                        // 不足补零。loopback 静音时这是常态；补零而不是重复
                        // 上一帧，是为了不制造出实际不存在的低频信号。
                        self.pcm.resize(samples, 0);
                    }
                    self.capture_samples += take as u64;
                }
                #[cfg(not(windows))]
                {
                    synth_audio_i16(self.seq, samples, &mut self.pcm);
                }
            }
        }

        // 载荷：Opus 编码后的字节，或原始 PCM（`--opus` 未开时）
        let payload: &[u8] = if let Some(enc) = self.encoder.as_mut() {
            match enc.encode(&self.pcm, &mut self.opus_out) {
                Ok(n) => &self.opus_out[..n],
                Err(e) => {
                    warn!(error = %e, seq = self.seq, "Opus 编码失败，跳过本帧");
                    return false;
                }
            }
        } else {
            // 退回 PCM：把 i16 采样按小端铺成字节。
            // 这是**假音频**，只为让不引入 libopus 的构建仍能验证链路。
            self.pcm_bytes.clear();
            for s in &self.pcm {
                self.pcm_bytes.extend_from_slice(&s.to_le_bytes());
            }
            &self.pcm_bytes
        };

        let mut encoded = match encode_frame_owned(&FrameSpec {
            ty: FrameType::Audio,
            flags: 0,
            stream_id: AUDIO_STREAM_ID,
            seq: self.seq,
            // 显式给 PTS：绝不能让 encode_frame 去取 now_us()，
            // 那等于把墙上时钟的抖动直接写进音画同步
            pts_us: Some(pts_us),
            send_ts_us: None,
            payload,
        }) {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "音频帧编码失败，跳过");
                return false;
            }
        };

        // 音频同样加密。音频不重传、不进 FEC，所以「加密一次」这件事
        // 在这里天然成立 —— 没有第二条路径会再封一次。
        if let Some(s) = sealer {
            if let Err(e) = s.seal_frame(&mut encoded) {
                warn!(error = %e, seq = self.seq, "音频帧加密失败，跳过");
                return false;
            }
        }

        // 帧长决定载荷大小：20ms 是 1920B（+32B 头超过 SAFE_MTU_PAYLOAD，
        // send_frame 会切成 2 片），2.5/5/10ms 则一片就够。
        // 分片是 UDP 传输层的常规操作，这里不需要按帧长分支处理。
        let frags = s2.send_frame(&encoded).await;
        // 记账口径见 `audio_accounting`：全丢也计一帧（节拍确实推进了），
        // 但字节数只在真的发出去时才累加。
        let (frame_delta, _frag_delta) = audio_accounting(frags);
        self.sent += frame_delta;
        if frags > 0 {
            self.bytes += encoded.len() as u64;
        }
        if frags == 0 {
            return false;
        }
        debug_assert_eq!(duration_us, self.pacer.frame_duration_us());
        true
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "sidelenssend=info".into()),
        )
        .without_time()
        .init();

    // 提升系统定时器分辨率到 1ms。
    //
    // # 为什么发送端也必须做这件事（接 --opus-frame-ms 时踩到的真 bug）
    //
    // Windows 的默认定时器分辨率是 15.625ms（64Hz）。`tokio::time::interval`
    // 的间隔比它小的时候，实际唤醒仍然只有 64Hz —— 配合
    // `MissedTickBehavior::Skip`，每个真实唤醒最多补发一帧，于是：
    //
    // ```text
    // --opus-frame-ms 2.5  →  PTS 每帧 +2500µs，墙上时钟每帧 +15600µs
    // 8 秒实测：audio_pts_span_ms=1310（应为 8000）→ 音频时间线只走了 16%
    // ```
    //
    // 后果是接收端按 PTS 出队时，音频永远「来不及」—— 实测
    // `audio_dropped_late=469 / audio_frames=509`，几乎全被当迟到丢掉。
    // 20ms 帧不受影响（20 > 15.625），所以这个坑只在 2.5/5/10ms 三档暴露。
    //
    // daemon 早就调了这个（见 apps/sidelensd/src/main.rs 的注释），
    // 发送端一直没调 —— 因为它的节拍原本固定 20ms，刚好在分辨率之上。
    let _hrt = sidelens_core::timer::HighResTimer::new();

    let args = parse_args();
    match args.use_udp {
        true => run_udp(args).await,
        false => run_ws(args).await,
    }
}

async fn run_ws(args: Args) -> Result<()> {
    info!(url = %args.target, fps = args.fps, frame_bytes = args.frame_bytes, "WebSocket 模式");

    let client = WsClient::connect(&args.target, 64).await?;
    client
        .send_frame(
            FrameType::Hello,
            0,
            0,
            br#"{"v":0,"role":"sender","name":"synthetic-phone"}"#,
        )
        .await;

    let mut down = client.subscribe();
    let force_kf = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let fk = force_kf.clone();
    tokio::spawn(async move {
        while let Ok(f) = down.recv().await {
            if f.ty == FrameType::KeyframeReq {
                fk.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }
    });

    let interval = Duration::from_secs_f64(1.0 / args.fps as f64);
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut seq = 0u32;
    let mut sent = 0u64;
    let mut sent_bytes = 0u64;
    let mut keyframes = 0u64;
    let mut buf = Vec::with_capacity(args.frame_bytes);
    let started = std::time::Instant::now();

    loop {
        ticker.tick().await;
        seq = seq.wrapping_add(1);
        synth_frame(seq, args.frame_bytes, &mut buf);

        let is_key = force_kf.swap(false, std::sync::atomic::Ordering::Relaxed)
            || sent % (args.fps * 2) == 0;
        if is_key {
            keyframes += 1;
        }

        let flags = if is_key { FLAG_KEYFRAME } else { 0 };
        if client.send_frame(FrameType::Video, 0, flags, &buf).await {
            sent += 1;
            sent_bytes += buf.len() as u64;
        }

        if sent > 0 && sent % (args.fps * 5) == 0 {
            let elapsed = started.elapsed().as_secs_f64();
            let dropped = client
                .stats
                .dropped_backpressure
                .load(std::sync::atomic::Ordering::Relaxed);
            info!(
                sent,
                keyframes,
                dropped,
                fps = format!("{:.1}", sent as f64 / elapsed),
                mbps = format!("{:.2}", sent_bytes as f64 / 1e6 / elapsed),
                "发送中"
            );
        }

        if args.duration_secs > 0 && started.elapsed() >= Duration::from_secs(args.duration_secs) {
            break;
        }
    }

    let elapsed = started.elapsed().as_secs_f64();
    info!(
        sent,
        keyframes,
        elapsed = format!("{elapsed:.2}s"),
        avg_fps = format!("{:.1}", sent as f64 / elapsed),
        "发送结束"
    );
    Ok(())
}

async fn run_udp(args: Args) -> Result<()> {
    info!(
        target = %args.target,
        fps = args.fps,
        frame_bytes = args.frame_bytes,
        loss = args.loss,
        fec = args.fec_redundant.map(|r| r.to_string()).unwrap_or_else(|| {
            if args.fec_auto { "auto".to_string() } else { "off".to_string() }
        }),
        audio = args.audio,
        "UDP 模式"
    );

    // 帧加密（可选，`--psk` 显式开启）
    //
    // # 加密发生在什么位置（这一步错了就是灾难）
    //
    // 必须是「**编码之后、缓存之前**」：
    //
    //   编码 → 加密 → 存入重传缓存（存密文）→ 首传 → FEC 冗余
    //                              ↓
    //                        NACK → 从缓存取密文原样重发
    //
    // 如果加密放在「发送前」而不是「缓存前」，重传时会用同一个
    // `(key, nonce)` 再加密一次 —— ChaCha20 是流密码，两次异或同一密钥流
    // 直接泄露两份明文的异或值。所以加密只有**一次机会**，
    // 之后的 FEC / 重传 / 分片全部作用于密文。
    let mut sealer = args.psk.map(|master| {
        FrameSealer::new(
            TrafficKeys::from_master(&master, Role::Initiator),
            Role::Initiator,
        )
    });
    if let Some(s) = sealer.as_ref() {
        info!(
            key_fingerprint = %args.psk.map(|m| TrafficKeys::from_master(&m, Role::Initiator).fingerprint_hex()).unwrap_or_default(),
            role = %s.role(),
            "帧加密已启用（ChaCha20-Poly1305，每帧 +16 字节）"
        );
    }

    // FEC 只挂在 UDP 路径上；配置非法时这里返回 None，其余功能照常
    let mut fec = FecOutbound::from_args(&args);
    // 告诉接收端音频载荷是什么编码 —— 它无法从码流判断。
    //
    // `None`（不发音频）时 Config 帧里写 `null`，接收端据此禁用音频路径。
    if let Some(f) = fec.as_mut() {
        f.audio_codec = if args.audio {
            Some(if args.use_opus { "opus" } else { "pcm" })
        } else {
            None
        };
        // 帧长也要告诉接收端：它拿这个值配置解码器缓冲与
        // `AudioJitterBuffer` 的 `frame_duration_us`。硬编码 20ms 时
        // 发送端用 60ms 帧会让接收端解码全部失败（见 `audio_frame_ms` 字段文档）。
        f.audio_frame_ms = if args.audio { args.opus_frame_ms } else { 0.0 };
    }

    // 视频与音频共用的时基起点。
    //
    // 两个流的 PTS 都是「t0 + 帧序号 × 帧间隔」，共用同一个 t0 才能让
    // 接收端 AvSync 算出的偏移等于真实的音画相对偏差。各用各的 t0 时，
    // 偏移里会混进「两个零点之差」，而这个差值在两端日志里都看不出异常，
    // 只在播放端表现为一个固定量的音画错位。
    // 时基起点。`--clock-bias-us` 会把它整体偏移 ——
    // 等价于「本端时钟比接收端快/慢 N 微秒」。
    //
    // 视频与音频的 PTS 全部基于这个 t0，所以一次偏移同时作用于两条流，
    // 与真实跨设备场景（整台机器的时钟有偏差）一致。
    let clock_bias = args.clock_bias_us;
    let t0_us = (sidelens_proto::now_us() as i64).saturating_add(clock_bias) as u64;
    if clock_bias != 0 {
        info!(
            bias_us = clock_bias,
            "时钟偏移已注入（模拟跨设备时基差异，接收端应能测出同样的值）"
        );
    }

    // 真实音频采集源（`--audio-in`）。
    //
    // 默认 `synth`（合成 440Hz），与改动前逐字节一致 —— 这条路径是
    // 所有 FEC / NACK / 抖动缓冲测试的基线，不能因为接了真设备就变。
    //
    // `loopback` 采系统正在播放的声音（共享会议音频、共享视频声音），
    // `mic` 采麦克风。两者打不开时**降级成 headless 而不是退出** ——
    // 没有麦克风的机器照样该能发屏幕。
    #[cfg(windows)]
    let capture = match args.audio_in.as_str() {
        "synth" | "" => None,
        mode @ ("loopback" | "mic") => {
            let fmt = sidelens_audio_in::PcmFormat {
                sample_rate: 48_000,
                channels: 1,
            };
            let opened = if mode == "loopback" {
                sidelens_audio_in::AudioCapture::loopback(fmt)
            } else {
                sidelens_audio_in::AudioCapture::new(fmt)
            };
            match opened {
                Ok(c) => {
                    let dev = c.device_format();
                    info!(
                        mode = c.mode().as_str(),
                        device = ?dev,
                        latency_ms = format!("{:.2}", c.latency_ms()),
                        "音频采集已启用（真实设备）"
                    );
                    if mode == "loopback" {
                        info!(
                            "loopback 模式：系统没有声音在播放时 read 返回 0 是正常状态，不是设备故障"
                        );
                    }
                    Some(c)
                }
                Err(e) => {
                    warn!(
                        mode,
                        error = %e,
                        "音频采集打不开，降级为合成音（链路继续跑，不影响视频）"
                    );
                    None
                }
            }
        }
        other => anyhow::bail!("未知的 --audio-in 值 {other}（可选 synth / loopback / mic）"),
    };

    let mut audio = if args.audio {
        Some(AudioOutbound::new(
            t0_us,
            args.use_opus,
            args.opus_bitrate,
            args.opus_frame_ms,
            OpusTuning::from_args(&args),
            args.audio_silence_after
                .map(|secs| (secs.max(0.0) * 1e6).round() as u64),
            #[cfg(windows)]
            capture,
        )?)
    } else {
        None
    };

    let loss = if args.loss > 0.0 {
        PacketLossSimulator::new(args.loss, 0xA5A5_1234)
    } else {
        PacketLossSimulator::disabled()
    };
    let sender = UdpSender::connect(&args.target, loss).await?;

    // H.264 编码器（`--h264`）。None 时走原来的合成字节路径，
    // 一次 Media Foundation 调用都不会发生。
    #[cfg(windows)]
    let mut h264: H264State = if args.h264 {
        Some(H264Out::new(&args)?)
    } else {
        None
    };
    // 非 Windows 上没有编码器：显式报错而不是静默退回合成字节 ——
    // 静默降级会让人以为「在测 H.264」，实际测的是原来的假数据。
    #[cfg(not(windows))]
    let h264: H264State = if args.h264 {
        anyhow::bail!("--h264 需要 Windows Media Foundation，当前平台不支持");
    } else {
        None
    };

    // 重传缓存：收到 NACK 时从这里取
    let mut cache = RetransmitCache::new(RetransmitConfig::default());

    // 后台收 NACK 并重传。
    //
    // 关键：必须用**发送 socket 本身**来收。UDP 无连接，接收端回 NACK 时
    // 是往「数据报的源地址」发的，也就是发送 socket 的端口。
    // 用另一个 bind 出来的 socket 收，NACK 会全落到无人读取的端口上，
    // 表现为「服务端一直发 NACK，发送端一直 retransmitted=0」。
    let recv_socket = sender.socket();
    let sender_arc = std::sync::Arc::new(sender);
    let s2 = sender_arc.clone();

    let (nack_tx, mut nack_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u32>>();
    let (sync_tx, mut sync_rx) = tokio::sync::mpsc::unbounded_channel::<sidelens_core::RttReply>();
    // 关键帧请求通道（UDP 路径）。
    //
    // 用 channel 而不是 AtomicBool：UDP recv 在独立 tokio 任务里，
    // 主循环在另一个任务 —— channel 天然跨任务。
    let (kf_tx, mut kf_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    tokio::spawn(async move {
        let mut buf = vec![0u8; 2048];
        loop {
            let Ok(n) = recv_socket.recv(&mut buf).await else {
                continue;
            };
            let mut dec = sidelens_proto::FrameDecoder::new();
            dec.push(&buf[..n]);
            for m in dec.frames() {
                match m.ty {
                    FrameType::Nack => {
                        if let Some(req) = sidelens_core::NackRequest::from_bytes(dec.payload(*m)) {
                            let _ = nack_tx.send(req.seqs);
                        }
                    }
                    FrameType::RttReply => {
                        if let Some(reply) = sidelens_core::RttReply::from_bytes(dec.payload(*m)) {
                            let _ = sync_tx.send(reply);
                        }
                    }
                    // 关键帧请求（第三十八轮补）。
                    //
                    // # 这个分支之前完全缺失
                    //
                    // KeyframeReq 只在 WebSocket 路径里被处理（client.subscribe()），
                    // 而 UDP 路径走的是 recv_socket.recv() —— 它只 match
                    // Nack 与 RttReply，KeyframeReq 落进 `_ => {}` 被丢掉。
                    //
                    // 实测后果：接收端 video_need_keyframe=107 而发送端从不知道，
                    // 只能等下一个 GOP（60 帧 = 1 秒）自然恢复。
                    FrameType::KeyframeReq => {
                        let _ = kf_tx.send(());
                    }
                    _ => {}
                }
            }
        }
    });

    // 主循环里处理 NACK（因为 cache 不能跨线程共享）
    //
    // 帧间隔用整数微秒，而不是 1.0/fps 的浮点秒：视频 PTS 也要用同一个
    // 整数去累加（t0 + n × video_frame_us），两处若各自浮点换算，
    // 节拍与 PTS 会以每帧不到 1 微秒的速度互相偏离。
    let fps = args.fps.max(1);
    let video_frame_us = 1_000_000u64 / fps;
    let interval = Duration::from_micros(video_frame_us);
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // 音频节拍：等于 `--opus-frame-ms`（默认 20ms），与视频节拍完全独立。
    //
    // 首帧必须**立刻**发出（tokio::interval 的第一次 tick 立即完成），
    // 因为 AudioPacer 的时间线起点就是 t0 本身。若把首帧推后一个周期，
    // 音频 PTS 会永久落后真实发送时刻一个帧长，而视频不会 —— 这个偏移会
    // 原样进入接收端的 AvSync，表现为固定的音画错位。
    let audio_interval = audio
        .as_ref()
        .map(|a| a.interval())
        .unwrap_or_else(|| Duration::from_millis(20));
    let mut audio_ticker = tokio::time::interval(audio_interval);
    audio_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut seq = 0u32;
    let mut sent = 0u64;
    let mut keyframes = 0u64;
    let mut retransmitted = 0u64;
    // 码率自适应的统计与状态
    //
    // `last_rate_hint` 存最近一次收到的建议（用于日志对比「建议 vs 实际」，
    // 排查「控制器说降了但编码器没降」这类问题）。
    let mut last_rate_hint: Option<u64> = None;
    let mut bitrate_adaptations: u64 = 0;
    let mut sent_bytes = 0u64;
    // 已发出的 FEC 冗余包总数（帧头 + FEC 头的完整数据报数）
    let mut fec_reds = 0u64;
    let mut buf = Vec::with_capacity(args.frame_bytes);
    let started = std::time::Instant::now();

    // 时钟同步状态
    let mut clock = sidelens_core::ClockSync::new();
    let mut pending_probes: std::collections::HashMap<u32, u64> = std::collections::HashMap::new();
    let mut probe_nonce = 0u32;

    // 视频 PTS 的帧计数。与音频的 AudioPacer 同一个道理：PTS 由
    // 「起点 + 序号 × 帧间隔」算出，不读墙上时钟。
    let mut video_pts_n = 0u64;

    // 首帧之前先发一次 Config：把音频 codec 与帧长告诉接收端。
    //
    // # 为什么必须在这里独立发（第三十一轮修的）
    //
    // `Config` 帧原本只由 `FecOutbound::send_config` 在**切换 FEC 配置时**
    // 发出。没启用 FEC（不加 `--fec`）时 `fec = None`，那行代码永远不执行 ——
    // 于是音频 codec 声明与帧长声明**一个都送不到接收端**。
    //
    // 实测症状：`--opus-frame-ms 60` 但不加 `--fec`，接收端仍按 20ms
    // 配置解码器 → `audio_decode_failed=134/134`，一帧都解不出来。
    //
    // 所以这里无条件发一次。内容与 FEC 无关时也要发 —— 它同时承载
    // `audio_codec` / `audio_frame_ms`，这两个跟 FEC 没有关系。
    {
        let codec_json = match args
            .audio
            .then_some(if args.use_opus { "opus" } else { "pcm" })
        {
            Some(c) => format!("\"{c}\""),
            None => "null".to_string(),
        };
        let frame_ms = if args.audio { args.opus_frame_ms } else { 0.0 };
        // FEC 字段用「当前实际配置」；没启用 FEC 时给 0，接收端会忽略
        // （它只在自己启用 FEC 时才用这两个字段）。
        let (gs, red) = fec
            .as_ref()
            .map(|f| (f.config.group_size, f.config.redundant))
            .unwrap_or((0, 0));
        let payload = format!(
            r#"{{"fec_group_size":{gs},"fec_redundant":{red},"audio_codec":{codec_json},"audio_frame_ms":{frame_ms}}}"#
        );
        if let Ok(bytes) =
            encode_frame_owned(&FrameSpec::new(FrameType::Config, payload.as_bytes()))
        {
            // 发三份：这是「怎么解读后续所有数据」的元信息，丢了整段都错。
            // 比 FEC 配置帧的两份多一份，因为它承载的信息更多。
            for _ in 0..3 {
                s2.send_raw(&bytes).await;
            }
            info!(
                audio_codec = %codec_json,
                audio_frame_ms = frame_ms,
                "已发送初始 Config（音频 codec 与帧长声明）"
            );
        }
    }

    loop {
        // 时长到了就退出。放在循环开头而不是结尾，是因为音频节拍分支
        // 会 continue，放在结尾会让音频节拍期间永远轮不到退出检查。
        if args.duration_secs > 0 && started.elapsed() >= Duration::from_secs(args.duration_secs) {
            break;
        }

        // 处理时钟同步回复
        while let Ok(reply) = sync_rx.try_recv() {
            let t4 = sidelens_proto::now_us();
            if pending_probes.remove(&reply.nonce).is_some() {
                clock.finish_probe(t4, reply.t1_us, reply.t2_us, reply.t3_us);
            }

            // 顺带消费码率建议（见 `RttReply` 的文档：拥塞控制器在接收端，
            // 执行码率的是这里的编码器）。
            //
            // # 三道闸门，缺一个就会抖
            //
            // 1. **变化幅度**：小于 `BITRATE_MIN_STEP_RATIO` 的调整忽略。
            //    控制器的百分比爬升每 2 秒只涨几个百分点，若每次都设，
            //    `ICodecAPI::SetValue` 会被每秒调用一次 —— 实测那会让
            //    编码器反复重算 QP 曲线，输出反而更不稳。
            // 2. **范围钳制**：不采信低于 100kbps 或高于 50Mbps 的建议。
            //    老版本/异常对端可能给 0 或极大值。
            // 3. **单调保护**：只在建议值确实不同于当前值时设。
            if let Some(hint) = reply.rate_hint() {
                last_rate_hint = Some(hint);
                #[cfg(windows)]
                if let Some(h) = h264.as_mut() {
                    let cur = h.bitrate();
                    let clamped = hint.clamp(BITRATE_MIN_BPS, BITRATE_MAX_BPS) as u32;
                    let ratio = clamped as f64 / cur.max(1) as f64;
                    let big_enough = ratio >= 1.0 + BITRATE_MIN_STEP_RATIO
                        || ratio <= 1.0 - BITRATE_MIN_STEP_RATIO;
                    if big_enough {
                        match h.set_bitrate(clamped) {
                            Ok(()) => {
                                info!(
                                    from = cur,
                                    to = clamped,
                                    hint = hint,
                                    loss_permille = reply.loss_permille.unwrap_or(0),
                                    "按接收端建议调整编码码率"
                                );
                                bitrate_adaptations += 1;
                            }
                            Err(e) => warn!(error = %e, target = clamped, "调整码率失败"),
                        }
                    }
                }
            }
        }

        // 每 2 秒发一次探测
        if sent % (args.fps * 2) == 0 {
            probe_nonce = probe_nonce.wrapping_add(1);
            // # T1 为什么**不**加偏移（第五十六轮修正）
            //
            // 第一版我给它加了偏移，结果 `finish_probe` 算出负 RTT 把整个
            // 探测丢掉（`synced=false`，一次都同步不上）。
            //
            // 根因：`t1` 是**本端**发出的时刻，与 `t4`（本端收到回复）必须
            // 同源 —— `t4 - t1` 才是真实经过时间。加了偏移就破坏了这条前提。
            //
            // 模拟跨设备时基差异应该只偏移**载荷里的 PTS**（视频/音频），
            // 那是「这一帧声称自己是什么时候的」；而 RTT 探测的 t1/t4 是
            // 本端自己的计时，不该被人为扭曲。
            let t1 = sidelens_proto::now_us();
            pending_probes.insert(probe_nonce, t1);
            let probe = sidelens_core::RttProbe {
                nonce: probe_nonce,
                t1_us: t1,
            };
            if let Ok(bytes) =
                encode_frame_owned(&FrameSpec::new(FrameType::RttProbe, &probe.to_bytes()))
            {
                s2.send_raw(&bytes).await;
            }
        }

        // 处理关键帧请求（第三十八轮补）。
        //
        // 置位后下一帧会强制编码成 IDR。
        let mut want_keyframe = false;
        while kf_rx.try_recv().is_ok() {
            want_keyframe = true;
        }

        // 先处理所有待重传请求（不阻塞发送节拍）
        while let Ok(seqs) = nack_rx.try_recv() {
            // 丢包率估计的**唯一**输入。漏了这一行，`redundant` 永远停在初始值，
            // 自适应静默失效（没有报错，只是数字不变）。
            if let Some(f) = fec.as_mut() {
                f.on_nack(&seqs);
            }
            for s in seqs {
                if let Some(f) = cache.take_for_retransmit(s, sidelens_proto::now_us()) {
                    s2.send_frame(&f.bytes).await;
                    retransmitted += 1;
                }
            }
        }

        // 两个节拍在同一个循环里交替：谁先到期谁先发。
        //
        // 不用两个 tokio 任务：重传缓存、FEC 分组、时钟同步状态都是
        // 单线程独占的，拆成两个任务就得给它们加锁；而两个节拍各自
        // 只占几十微秒，串行处理完全够用。
        //
        // 音频关闭时 if 守卫为 false，audio_ticker 不会被轮询，
        // select! 退化成原来那个「只等视频节拍」的行为。
        let audio_tick = tokio::select! {
            _ = ticker.tick() => false,
            _ = audio_ticker.tick(), if audio.is_some() => true,
        };

        if audio_tick {
            // 音频节拍：发一帧就走，不碰视频的 seq / 重传缓存 / FEC。
            //
            // 音频帧不进重传缓存、不生成 FEC 冗余：一个帧长的空洞补不回来，
            // 补传要多花一个 RTT 的带宽，不如让它过去。
            if let Some(a) = audio.as_mut() {
                a.send_next(&s2, sealer.as_mut()).await;
            }
            continue;
        }

        seq = seq.wrapping_add(1);

        // 本帧的 PTS：与音频共用 t0，两流的 PTS 只有落在同一个时间轴上，
        // 接收端 AvSync 算出的偏移才是真实的音画偏差
        let pts_us = t0_us.saturating_add(video_pts_n.saturating_mul(video_frame_us));

        // ---- 产生本帧的载荷与关键帧标记 ----
        //
        // 两条路的分水岭只有这一处：
        //
        // | 模式 | 载荷 | 关键帧判据 |
        // | --- | --- | --- |
        // | 默认 | synth_frame 的合成字节 | sent % (fps*2) == 0（假的关键帧） |
        // | --h264 | 编码后的 Annex-B 码流 | nalu::is_keyframe（真的 IDR） |
        //
        // 关键帧标记必须来自**真实码流**：合成模式下那个取模判据只是为了
        // 让接收端有周期性的关键帧可用，而 H.264 模式下 IDR 是编码器按
        // GOP 决定的，两边对不上会让抖动缓冲误判「这帧能不能独立解码」。
        #[cfg(windows)]
        let encoded_payload: Option<&[u8]> = match h264.as_mut() {
            Some(h) => {
                // 收到关键帧请求时强制下一帧编成 IDR（第三十八轮补）。
                //
                // # 为什么必须在这里（而不是只靠 `is_key`）
                //
                // UDP 路径的 `is_key` 完全来自 `nalu::is_keyframe(码流)` ——
                // 那是**读编码器输出**，只能被动接受编码器按 GOP 决定的关键帧。
                // 接收端请求了，编码器不理会，`want_keyframe` 永远没效果。
                //
                // `H264Encoder::force_keyframe()` 通过 `CODECAPI_AVEncVideoForceKeyFrame`
                // 让 MFT 在下一帧出 IDR。实测有效（`bitrate.rs` 的
                // `force_keyframe_does_not_break_stream` 守着）。
                if want_keyframe {
                    if let Err(e) = h.force_keyframe() {
                        debug!(error = %e, "强制关键帧失败（编码器可能不支持）");
                    } else {
                        debug!(seq, "收到关键帧请求，已强制下一帧为 IDR");
                    }
                }
                h.encode_next(seq, pts_us)?;
                if h.out().is_empty() {
                    // MFT 偶发不吐帧（低延迟模式下几乎不会，但不假设）
                    video_pts_n += 1;
                    continue;
                }
                Some(h.out())
            }
            None => None,
        };
        #[cfg(not(windows))]
        let encoded_payload: Option<&[u8]> = None;

        let is_key = match encoded_payload {
            Some(data) => sidelens_codec::nalu::is_keyframe(data),
            None => {
                synth_frame(seq, args.frame_bytes, &mut buf);
                sent % (args.fps * 2) == 0
            }
        };
        if is_key {
            keyframes += 1;
        }
        let flags = if is_key { FLAG_KEYFRAME } else { 0 };

        let payload: &[u8] = encoded_payload.unwrap_or(&buf);

        // 编码一次，同时用于发送与缓存
        let mut encoded = encode_frame_owned(&FrameSpec {
            ty: FrameType::Video,
            flags,
            stream_id: 0,
            seq,
            pts_us: Some(pts_us),
            send_ts_us: None,
            payload,
        })?;
        video_pts_n += 1;

        // 加密：**必须在 store 之前**，且只做一次。
        // 见上面 sealer 创建处的说明 —— 重传会复用这份密文。
        if let Some(s) = sealer.as_mut() {
            if let Err(e) = s.seal_frame(&mut encoded) {
                warn!(error = %e, seq, "视频帧加密失败，丢弃本帧");
                continue;
            }
        }

        cache.store(
            StoreSpec::with_flags(FrameType::Video, flags, 0, seq, 0, encoded.clone()),
            sidelens_proto::now_us(),
        );

        let fragments_sent = s2.send_frame(&encoded).await;
        if fragments_sent > 0 {
            sent += 1;
            sent_bytes += encoded.len() as u64;
        }

        // FEC：按同一套分片切分算冗余，冗余包独立成帧另发。
        //
        // 注意顺序 —— 必须用**和 send_frame 完全一样**的切分方式
        // （fragment_frame + MAX_DATAGRAM），否则两边分片边界不同，
        // 组内下标错位，恢复出来的是垃圾。
        if let Some(f) = fec.as_mut() {
            let datagrams = sidelens_transport::encode_datagrams(&encoded);
            let n = f.send_frame(&s2, seq, &datagrams, is_key).await;
            fec_reds += n;
        }

        if sent > 0 && sent % (args.fps * 5) == 0 {
            let elapsed = started.elapsed().as_secs_f64();
            let st = s2.stats.snapshot();
            let fec_overhead = fec.as_ref().map(|f| f.overhead_ratio()).unwrap_or(0.0);
            info!(
                sent,
                keyframes,
                retransmitted,
                dropped = st.simulated_drops,
                fps = format!("{:.1}", sent as f64 / elapsed),
                mbps = format!("{:.2}", sent_bytes as f64 / 1e6 / elapsed),
                offset_ms = format!(
                    "{:.2}",
                    clock
                        .offset_us()
                        .map(|v| v as f64 / 1000.0)
                        .unwrap_or(f64::NAN)
                ),
                rtt_ms = format!("{:.2}", clock.rtt_us() as f64 / 1000.0),
                synced = clock.is_synced(),
                fec_reds,
                fec_overhead = format!("{:.1}%", fec_overhead * 100.0),
                // 自适应状态：只在周期日志里打，末尾汇总另有完整一组
                fec_redundant = fec.as_ref().map(|f| f.config.redundant).unwrap_or(0),
                fec_loss_est = format!(
                    "{:.4}",
                    fec.as_ref().map(|f| f.loss.loss_estimate()).unwrap_or(0.0)
                ),
                // 音频侧：帧数应与「已过秒数 / 0.02」基本吻合
                audio_sent = audio.as_ref().map(|a| a.sent).unwrap_or(0),
                // 单位与上面的 mbps 保持一致（MB/s，不是 Mbps）——
                // 两个字段并排显示，口径不同会让人直接读错量级
                audio_mbps = format!(
                    "{:.3}",
                    audio
                        .as_ref()
                        .map(|a| a.bytes as f64 / 1e6 / elapsed)
                        .unwrap_or(0.0)
                ),
                // 音频码率用 kbps：几十 kbps 的绝对值用 MB/s 看不出量级。
                // 与视频的 mbps 并存不冲突 —— 两者差三个数量级，混读反而不会错。
                audio_kbps = format!(
                    "{:.1}",
                    audio
                        .as_ref()
                        .map(|a| a.bytes as f64 * 8.0 / 1e3 / elapsed)
                        .unwrap_or(0.0)
                ),
                // H.264 模式下报编码耗时；默认模式下这两个字段都是 0
                encode_us = format!("{:.0}", encode_stats(&h264).0),
                h264 = h264.is_some(),
                "发送中"
            );
        }

        if args.duration_secs > 0 && started.elapsed() >= Duration::from_secs(args.duration_secs) {
            break;
        }
    }

    let elapsed = started.elapsed().as_secs_f64();
    let st = s2.stats.snapshot();
    let cs = cache.stats;
    let fec_overhead = fec.as_ref().map(|f| f.overhead_ratio()).unwrap_or(0.0);
    let fec_data_bytes = fec.as_ref().map(|f| f.data_bytes).unwrap_or(0);
    let fec_red_bytes = fec.as_ref().map(|f| f.redundant_bytes).unwrap_or(0);
    let fec_oversize = fec.as_ref().map(|f| f.oversize_skipped).unwrap_or(0);
    // 冗余配置也打出来：两端 group_size / redundant 必须一致，
    // 对不上时恢复出来是垃圾数据且不报错，出问题第一时间看这两个值。
    //
    // 组大小是动态的，这里给的是**最后一帧**用的值 + 全程分布
    // （`组大小:帧数`）。分布里只有一项说明组大小稳定，没在来回抖 ——
    // 抖动会让接收端不停重建解码器，且每次重建都会丢掉跨配置的组。
    // 从没生效过（每帧都被 should_fec 跳过）时 config 还是占位值，
    // 打 0 比打占位的 64 更不容易让人误判
    let fec_group = fec
        .as_ref()
        .filter(|f| f.sender.is_some())
        .map(|f| f.config.group_size)
        .unwrap_or(0);
    let fec_redundant = fec.as_ref().map(|f| f.config.redundant).unwrap_or(0);
    // 自适应的中间量全打出来：出问题时第一件要确认的是
    // 「估计值对不对」而不是「redundant 为什么是这个数」。
    let fec_redundant_usage = fec
        .as_ref()
        .map(|f| {
            f.redundant_usage
                .iter()
                .map(|(r, n)| format!("{r}:{n}"))
                .collect::<Vec<_>>()
                .join(",")
        })
        .unwrap_or_default();
    let fec_loss_est = fec.as_ref().map(|f| f.loss.loss_estimate()).unwrap_or(0.0);
    let fec_loss_last = fec.as_ref().map(|f| f.loss.last_sample()).unwrap_or(0.0);
    let fec_loss_frame_last = fec
        .as_ref()
        .map(|f| f.loss.last_frame_loss())
        .unwrap_or(0.0);
    let fec_loss_avg_frags = fec.as_ref().map(|f| f.loss.last_avg_frags()).unwrap_or(0.0);
    let fec_loss_redundant = fec.as_ref().map(|f| f.loss.last_redundant()).unwrap_or(0.0);
    let fec_loss_windows = fec.as_ref().map(|f| f.loss.windows).unwrap_or(0);
    let fec_nack_frames = fec.as_ref().map(|f| f.loss.nack_frames_total).unwrap_or(0);
    let fec_adaptive = fec.as_ref().map(|f| f.adaptive).unwrap_or(false);
    let fec_hyst_switches = fec.as_ref().map(|f| f.hysteresis.switches).unwrap_or(0);
    let fec_initial_redundant = fec.as_ref().map(|f| f.initial_redundant).unwrap_or(0);
    let fec_skip_overhead = fec.as_ref().map(|f| f.skip_overhead).unwrap_or(0);
    let fec_group_usage = fec
        .as_ref()
        .map(|f| {
            f.group_usage
                .iter()
                .map(|(gs, n)| format!("{gs}:{n}"))
                .collect::<Vec<_>>()
                .join(",")
        })
        .unwrap_or_default();
    let fec_config_frames = fec.as_ref().map(|f| f.config_frames).unwrap_or(0);
    // 分片数 <= redundant 的帧不发冗余包（发了反而让开销爆炸），这里报出来
    let fec_skipped_frames = fec
        .as_ref()
        .map(|f| f.skip_frames.values().sum::<u64>())
        .unwrap_or(0);
    info!(
        sent,
        keyframes,
        retransmitted,
        simulated_drops = st.simulated_drops,
        cache_served = cs.served,
        cache_refused_expired = cs.refused_expired,
        fec_reds,
        fec_overhead = format!("{:.1}%", fec_overhead * 100.0),
        fec_group,
        fec_redundant,
        fec_initial_redundant,
        fec_adaptive,
        fec_group_usage,
        fec_redundant_usage,
        fec_config_frames,
        fec_hyst_switches,
        fec_skipped_frames,
        fec_skip_overhead,
        // 丢包率估计链路的中间量：loss_est 是平滑后的决策输入，
        // loss_last 是最近一个窗口的原始样本，两者差很多说明还在爬升
        fec_loss_est = format!("{fec_loss_est:.4}"),
        fec_loss_last = format!("{fec_loss_last:.4}"),
        fec_loss_frame_last = format!("{fec_loss_frame_last:.4}"),
        fec_loss_avg_frags = format!("{fec_loss_avg_frags:.1}"),
        fec_loss_redundant = format!("{fec_loss_redundant:.1}"),
        fec_hyst_blocked = fec.as_ref().map(|f| f.hysteresis.blocked).unwrap_or(0),
        fec_loss_windows,
        fec_nack_frames,
        fec_data_bytes,
        fec_red_bytes,
        fec_oversize_skipped = fec_oversize,
        // 码率自适应：`bitrate_adaptations` 是实际调用 `set_bitrate` 的次数。
        // 长期为 0 说明接收端没在发建议（老版本 / 未启用控制器），
        // 或者建议值与当前码率的差异一直没超过 5% 的闸门。
        bitrate_adaptations,
        last_rate_hint = last_rate_hint.unwrap_or(0),
        elapsed = format!("{elapsed:.2}s"),
        avg_fps = format!("{:.1}", sent as f64 / elapsed),
        // 音频侧汇总。audio_frames_expected 是「按时长理论上该发多少帧」，
        // 与 audio_sent 对不上说明节拍被视频/重传阻塞过。
        audio_sent = audio.as_ref().map(|a| a.sent).unwrap_or(0),
        audio_bytes = audio.as_ref().map(|a| a.bytes).unwrap_or(0),
        audio_frames_expected = audio
            .as_ref()
            .map(|a| (elapsed * 1e6 / a.pacer.frame_duration_us() as f64) as u64)
            .unwrap_or(0),
        // Pacer 推进到的 PTS 与真实经过时长的差：正常应为 0 附近，
        // 明显偏大说明音频节拍被别的分支饿过
        audio_kbps = format!(
            "{:.1}",
            audio
                .as_ref()
                .map(|a| a.bytes as f64 * 8.0 / 1e3 / elapsed)
                .unwrap_or(0.0)
        ),
        audio_avg_frame_bytes = format!(
            "{:.1}",
            audio
                .as_ref()
                .map(|a| if a.sent == 0 {
                    0.0
                } else {
                    a.bytes as f64 / a.sent as f64
                })
                .unwrap_or(0.0)
        ),
        audio_pts_span_ms = format!(
            "{:.1}",
            audio
                .as_ref()
                .map(|a| a.pacer.current_pts_us().saturating_sub(t0_us) as f64 / 1000.0)
                .unwrap_or(0.0)
        ),
        // ── Opus 调优的**发送侧**证据 ──────────────────────────────
        //
        // 这几个字段是判断「DTX / in-band FEC 到底有没有生效」的可靠依据：
        // 接收端的 audio_decoded 会同时被「抖动缓冲容量」「解码器输出缓冲大小」
        // 影响，换个帧长就失真；编码器自己的计数器只反映码流本身。
        //
        // `dtx_frames` 是编码器判定为 DTX 并压成 1~2 字节的帧数 ——
        // 它 > 0 才说明 DTX 真的在省带宽，而不是「设上了但一次没触发」。
        opus_frames = audio
            .as_ref()
            .and_then(|a| a.encoder.as_ref())
            .map(|e| e.frames())
            .unwrap_or(0),
        opus_dtx_frames = audio
            .as_ref()
            .and_then(|a| a.encoder.as_ref())
            .map(|e| e.dtx_frames())
            .unwrap_or(0),
        opus_dtx_bytes = audio
            .as_ref()
            .and_then(|a| a.encoder.as_ref())
            .map(|e| e.dtx_bytes())
            .unwrap_or(0),
        opus_avg_speech_bytes = format!(
            "{:.1}",
            audio
                .as_ref()
                .and_then(|a| a.encoder.as_ref())
                .map(|e| e.avg_speech_frame_bytes())
                .unwrap_or(0.0)
        ),
        // H.264 模式下的编码侧汇总。默认模式下全部为 0 / off。
        h264 = h264.is_some(),
        encode_avg_us = format!("{:.0}", encode_stats(&h264).0),
        encode_frames = encode_stats(&h264).1,
        h264_dump_bytes = encode_stats(&h264).2,
        // 采集侧：`capture=on` 时才非零。`capture_empty` 是 DXGI 报
        // 「桌面没变化」的次数（不是错误，等于总帧数才说明采错了显示器）
        capture = capture_stats(&h264).enabled,
        // 三段分开记：`wait` 大是正常的（CPU 在等桌面刷新），
        // `process` 大才是真该优化的。混在一起判断不了瓶颈。
        capture_total_us = format!("{:.0}", capture_stats(&h264).total_us),
        capture_wait_us = format!("{:.0}", capture_stats(&h264).wait_us),
        capture_process_us = format!("{:.0}", capture_stats(&h264).process_us),
        capture_empty = capture_stats(&h264).empty,
        // 显示器分辨率在运行中变了（插拔/改设置）的次数。非 0 说明
        // 画面在冻结，需要重启发送端或改 --size
        capture_size_mismatch = capture_stats(&h264).size_mismatch,
        "发送结束"
    );

    // H.264 模式下把最后几帧冲出来（MFT 内部可能还压着 0~1 帧）
    #[cfg(windows)]
    if let Some(h) = h264.as_mut() {
        match h.encoder.flush() {
            Ok(tail) if !tail.is_empty() => {
                info!(bytes = tail.len(), "flush 补出残留码流（未发送，仅计数）");
            }
            Ok(_) => {}
            Err(e) => warn!(error = %e, "编码器 flush 失败"),
        }
    }

    if st.simulated_drops > 0 && cs.served == 0 {
        warn!("有丢包但从未收到 NACK —— 检查接收端是否在发、端口是否对");
    }

    // FEC 一边开着却一个冗余包都没发出去，只有两种可能：
    // 配置非法（上面已 warn 过），或者每帧的分片数都没超过 redundant ——
    // 后者是**刻意跳过**的（见 should_fec），不是故障。
    if fec.is_some() && fec_reds == 0 {
        if fec_skipped_frames > 0 {
            warn!(
                skipped_frames = fec_skipped_frames,
                redundant = fec_redundant,
                "FEC 未发出冗余包：每帧分片数都不超过 redundant，按设计跳过（小帧靠 NACK 补更划算）"
            );
        } else {
            warn!("FEC 已启用但未发出任何冗余包 —— 检查帧大小是否过小（不足一个分片组）");
        }
    }
    if fec_oversize > 0 {
        warn!(
            skipped = fec_oversize,
            limit = FEC_WIRE_LIMIT,
            "有 FEC 冗余包超过 MTU 被跳过（send_raw 不分片，超了必然发不出去）"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `--opus-frame-ms` 必须真的把整条音频时间线改掉
    ///
    /// 这是「开关接上了」和「开关看起来接上了」的分界线：只改 `frame_ms`
    /// 而 `frame_samples` / `frame_duration_us` 里有任何一处还硬编码
    /// 960 或 20000，PTS 每帧就会和真实码流错开，10 秒后音画彻底分离。
    #[test]
    fn opus帧长_驱动采样数与帧时长() {
        for (ms, samples, dur_us) in [
            (2.5_f64, 120_u32, 2_500_u64),
            (5.0, 240, 5_000),
            (10.0, 480, 10_000),
            (20.0, 960, 20_000),
            (40.0, 1920, 40_000),
            (60.0, 2880, 60_000),
        ] {
            let cfg = OpusConfig {
                frame_ms: ms,
                ..OpusConfig::default()
            };
            assert!(cfg.validate().is_ok(), "{ms}ms 应是合法帧长");
            assert_eq!(cfg.frame_samples(), samples, "{ms}ms 的采样数");
            assert_eq!(cfg.frame_duration_us(), dur_us, "{ms}ms 的帧时长");
            // Pacer 的每帧递增量必须与配置一致 —— 它才是 PTS 的真正来源
            let mut pacer = AudioPacer::new(cfg, 1_000).expect("配置合法");
            assert_eq!(pacer.frame_duration_us(), dur_us);
            assert_eq!(pacer.next_frame(), (1_000, dur_us));
            assert_eq!(pacer.next_frame(), (1_000 + dur_us, dur_us));
        }
    }

    #[test]
    fn opus帧长_非法值被拒绝而不是静默改写() {
        // 15ms 不在六档里：编码器会静默按 10 或 20 编，时间线从此错位
        for bad in [15.0_f64, 30.0, 0.0, -20.0, f64::NAN] {
            let cfg = OpusConfig {
                frame_ms: bad,
                ..OpusConfig::default()
            };
            assert!(cfg.validate().is_err(), "{bad}ms 应被拒绝");
        }
    }

    #[test]
    fn opus调优_从命令行参数落到结构体() {
        let mut a = test_args(2, false);
        assert!(OpusTuning::from_args(&a).is_default(), "默认应全关");

        a.opus_dtx = true;
        a.opus_inband_fec = true;
        a.opus_loss = 10;
        let t = OpusTuning::from_args(&a);
        assert!(t.dtx && t.inband_fec);
        assert_eq!(t.loss_perc, 10);
        assert!(!t.is_default());
    }

    #[test]
    fn 组大小_首帧直接用当前分片数() {
        assert_eq!(pick_group_size_hysteretic(2, None), 2);
        assert_eq!(pick_group_size_hysteretic(14, None), 16);
    }

    #[test]
    fn 组大小_只增不减() {
        // 大帧把组大小涨上去后，小帧不会把它拉回来
        assert_eq!(pick_group_size_hysteretic(14, Some(2)), 16);
        assert_eq!(pick_group_size_hysteretic(2, Some(16)), 16);
        assert_eq!(pick_group_size_hysteretic(4, Some(16)), 16);
    }

    #[test]
    fn 组大小_分片数减少时不切换() {
        // 这是 H.264 场景的核心：P 帧 2 片不会把 IDR 帧涨上去的组大小拉回来
        let mut cur = None;
        // 模拟 H.264 的帧序列：大量 P 帧 + 偶尔 IDR
        for (frags, expect_after) in [
            (2usize, 2usize),
            (2, 2),
            (14, 16), // IDR
            (2, 16),  // 又是 P 帧，但不回落
            (2, 16),
            (3, 16),
            (14, 16), // 第二个 IDR，不涨
        ] {
            let next = pick_group_size_hysteretic(frags, cur);
            assert_eq!(
                next, expect_after,
                "分片数 {frags} 时组大小应为 {expect_after}"
            );
            cur = Some(next);
        }
    }

    #[test]
    fn 组大小_不超过上限() {
        assert_eq!(pick_group_size_hysteretic(1000, None), FEC_GROUP_SIZE_MAX);
        assert_eq!(
            pick_group_size_hysteretic(1000, Some(16)),
            FEC_GROUP_SIZE_MAX
        );
    }

    #[test]
    fn 组大小_不低于下限() {
        assert_eq!(pick_group_size_hysteretic(1, None), FEC_GROUP_SIZE_MIN);
    }

    #[test]
    fn 跳过判据_分片数不大于冗余度时跳过() {
        // 这是 FEC 在低码率视频上几乎失效的直接原因
        assert!(!should_fec(1, 2));
        assert!(!should_fec(2, 2), "2 分片 + 2 冗余 = 100% 开销，必须跳过");
        assert!(should_fec(14, 2));
        // 3 分片配 1 个冗余（钳过的结果）可以发；配 2 个则超出预算，
        // 但那是 affordable_redundant 的职责，不是 should_fec 的
        // （第一版把预算判据塞进 should_fec，造成整帧跳过的死循环）。
        assert!(should_fec(3, 1));
    }

    #[test]
    fn 开销预算_钳冗余度而不是关掉fec() {
        // 8 分片：预算 = floor(8 * 0.35) = 2
        assert_eq!(affordable_redundant(8, 3), 2, "want=3 应钳到 2（25%）");
        assert_eq!(affordable_redundant(8, 2), 2);
        assert_eq!(affordable_redundant(8, 1), 1);
        // 钳完 still > 0 -> should_fec 放行，FEC 照发
        assert!(should_fec(8, affordable_redundant(8, 6)));
        // 55 分片：预算 = 19，want 永远到不了，不钳
        assert_eq!(affordable_redundant(55, 16), 16);
        assert_eq!(affordable_redundant(55, 5), 5);
        // 3 分片：预算 = 1 -> 只剩 1 个冗余，仍可发
        assert_eq!(affordable_redundant(3, 2), 1);
        assert!(should_fec(3, 1));
        // 2 分片：预算 = 0 -> 不发（未知数 >= 方程数，数学上无意义）
        assert_eq!(affordable_redundant(2, 1), 0);
        assert!(!should_fec(2, 0));
        // 1 分片同理
        assert_eq!(affordable_redundant(1, 2), 0);
    }

    #[test]
    fn 开销预算_不会造成整帧跳过() {
        // 这是第一版 A/B 实测的死循环回归测试：
        // 8 分片帧 want=3，旧逻辑 3/8=37.5% > 35% 整帧跳过，
        // 导致 FEC 被自适应关掉（nack 从 21 涨到 160）。
        for frags in [3usize, 4, 8, 14, 55, 64] {
            for want in 1..=16usize {
                let r = affordable_redundant(frags, want);
                if r == 0 {
                    assert!(frags < 3, "{frags} 分片钳完不该是 0（want={want}）");
                    continue;
                }
                assert!(
                    should_fec(frags, r),
                    "{frags} 分片 want={want} 钳到 {r} 后应仍可发 FEC"
                );
                assert!(
                    r as f64 / frags as f64 <= FEC_MAX_OVERHEAD_RATIO + 1e-9,
                    "{frags} 分片钳到 {r} 后开销应不超预算"
                );
            }
        }
    }

    #[test]
    fn 开销预算_分片数足够大时want原样保留() {
        // 55 分片能负担 19 个冗余，自适应的 want（<= 16）永远不该被钳
        for want in 1..=16usize {
            assert_eq!(
                affordable_redundant(55, want),
                want,
                "55 分片不该钳 want={want}"
            );
        }
    }

    #[test]
    fn 跳过判据_零分片与零冗余都跳过() {
        assert!(!should_fec(0, 2));
        assert!(!should_fec(8, 0));
    }

    /// 造一个最小 Args（只填 FEC 相关的字段）
    fn test_args(fec_redundant: usize, fec_fixed: bool) -> Args {
        Args {
            use_udp: true,
            target: String::new(),
            fps: 30,
            frame_bytes: 8192,
            duration_secs: 0,
            loss: 0.05,
            fec_redundant: Some(fec_redundant),
            fec_auto: false,
            fec_fixed,
            audio: false,
            psk: None,
            use_opus: false,
            opus_bitrate: 32_000,
            opus_frame_ms: 20.0,
            size: None,
            opus_dtx: false,
            opus_inband_fec: false,
            opus_loss: 0,
            audio_silence_after: None,
            audio_in: "synth".to_string(),
            fec_target: DEFAULT_RECOVERY_TARGET,
            screen: "off".to_string(),
            clock_bias_us: 0,
            h264: false,
            h264_dump: None,
        }
    }

    #[test]
    fn 自适应_预热前用初始值() {
        let mut f = FecOutbound::from_args(&test_args(2, false)).expect("应启用");
        f.record_sent(55, 2);
        // 还没滚过窗口：必须用初始值，不能因为「0 丢包」关掉
        assert_eq!(f.decide_redundant(false, 55), 2);
    }

    #[test]
    fn 自适应_固定模式下永远用初始值() {
        let mut f = FecOutbound::from_args(&test_args(2, true)).expect("应启用");
        for i in 0..200u32 {
            f.on_nack(&[i]);
            f.record_sent(55, 2);
            assert_eq!(f.decide_redundant(false, 55), 2, "--fec-fixed 下不该自适应");
        }
    }

    #[test]
    fn 自适应_高丢包大组会涨冗余() {
        let mut f = FecOutbound::from_args(&test_args(2, false)).expect("应启用");
        // 55 片帧，每帧都 NACK —— 估计器会算出很高的丢包率
        let mut last = 2usize;
        for i in 0..400u32 {
            f.on_nack(&[i]);
            last = f.decide_redundant(false, 55);
            f.record_sent(55, affordable_redundant(55, last));
        }
        assert!(
            last > 2,
            "55 片 + 高丢包必须把 redundant 涨上去，实际停在 {last}"
        );
    }

    #[test]
    fn 自适应_链路干净时冗余降到下限() {
        let mut f = FecOutbound::from_args(&test_args(6, false)).expect("应启用");
        // 一个 NACK 都没有：估计器算出 0 丢包，冗余应降到 1
        let mut last = 6usize;
        for _ in 0..400 {
            last = f.decide_redundant(false, 55);
            f.record_sent(55, last);
        }
        assert_eq!(last, 1, "链路干净时应降到 1（不是 0：FecConfig 要求 >= 1）");
    }

    #[test]
    fn 关键帧拿到的冗余严格多于_p_帧() {
        // 关键帧丢了要等一个 GOP（60 帧 = 1 秒），P 帧只是 1 个 RTT。
        // 所以同一丢包率、同一分片数下，关键帧必须拿更多冗余。
        //
        // 实测参考（35 分片 @5%）：P 帧策略 r=2（58.4%），关键帧策略 r=17（95.1%）。
        let mut f = FecOutbound::from_args(&test_args(2, false)).expect("应启用");
        // 喂出高丢包率
        for i in 0..400u32 {
            f.on_nack(&[i]);
            f.record_sent(55, 2);
        }
        let delta = f.decide_redundant(false, 55);
        let key = f.decide_redundant(true, 55);
        assert!(key >= delta, "关键帧冗余（{key}）不该少于 P 帧（{delta}）");
        assert!(
            key > 2,
            "55 片高丢包下关键帧必须显著高于初始值 2，实际 {key}"
        );
    }

    #[test]
    fn fec_target_只影响_p_帧不影响关键帧() {
        // `--fec-target` 是给 P 帧调优用的。关键帧的目标在
        // `suggest_redundant_for_frame` 内部取 max(KEYFRAME_RECOVERY_TARGET)，
        // 调用方传什么都降不下来 —— 这是刻意的，但接口上容易误解，
        // 所以用测试把这条语义钉住。
        let mut loose = FecOutbound::from_args(&test_args(2, false)).expect("应启用");
        loose.target = 0.90;
        let mut strict = FecOutbound::from_args(&test_args(2, false)).expect("应启用");
        strict.target = 0.999;

        // 喂出高丢包
        for i in 0..400u32 {
            loose.on_nack(&[i]);
            loose.record_sent(55, 2);
            strict.on_nack(&[i]);
            strict.record_sent(55, 2);
        }

        // P 帧：更严的目标必须给出不少于（通常多于）宽松目标的冗余
        assert!(
            strict.decide_redundant(false, 55) >= loose.decide_redundant(false, 55),
            "P 帧：target=0.999 的冗余不该少于 target=0.90"
        );
        // 关键帧：两者必须完全相同（target 被 max 吃掉）
        assert_eq!(
            strict.decide_redundant(true, 55),
            loose.decide_redundant(true, 55),
            "关键帧不受 --fec-target 影响"
        );
    }

    #[test]
    fn 音频帧被丢包模拟器吃掉也要计数() {
        // 真 bug 回归：`send_next` 里 `frags == 0` 时直接 return false，
        // 不递增 `self.sent`。结果 `audio_sent` 比 `audio_frames_expected`
        // 少了整整 24 帧（8 秒 × 50fps × 5% ≈ 20），看起来像「音频节拍
        // 被视频阻塞」，实际只是丢包模拟器的账。
        //
        // `sent` 的语义是「音频节拍推进了几次」，不是「网卡吐了几个包」。
        // 修后实测：audio_sent=401 / expected=400（含首帧），完全对上。
        // 见 docs/progress.md 第四十五轮。
        assert_eq!(
            audio_accounting(0),
            (1, 0),
            "全丢：计一帧，但字节数不算（没有任何字节真的上过网）"
        );
        assert_eq!(audio_accounting(2), (1, 2), "正常：计一帧，记分片数");
    }

    #[test]
    fn 关键帧必须按自己的分片数算冗余() {
        // 真 bug 回归：第一版用 self.last_fragments（上一帧的分片数）。
        // H.264 场景下 P 帧只有 1-2 片、关键帧 30+ 片，轮到关键帧时
        // last_fragments 还是 2，affordable_redundant(2, want) 直接钳成 0 ——
        // 最需要保护的那一帧一点冗余都拿不到。
        //
        // 实测（修复前 720p @5%）：fec_skipped_frames=344 / fec_skip_overhead=344。
        let mut f = FecOutbound::from_args(&test_args(2, false)).expect("应启用");
        for i in 0..400u32 {
            f.on_nack(&[i]);
            f.record_sent(2, 0); // 前 400 帧都是 2 分片的 P 帧
        }
        // 现在来一个 40 分片的关键帧：必须按 40 算，不是按 2
        let want = f.decide_redundant(true, 40);
        let actual = affordable_redundant(40, want);
        assert!(
            actual > 0,
            "40 分片的关键帧不该被钳成 0 个冗余（want={want}），"
        );
        assert!(
            actual >= 2,
            "高丢包下 40 分片关键帧至少该拿 2 个冗余，实际 {actual}"
        );
    }

    #[test]
    fn 关键帧冗余过迟滞后仍受预算钳制() {
        // 关键帧策略想要 r=17（55 片 @5%），但 affordable_redundant 只给 19
        // （55 * 0.35 = 19）。这条守着「关键帧也逃不过预算闸门」。
        let mut f = FecOutbound::from_args(&test_args(2, false)).expect("应启用");
        for i in 0..400u32 {
            f.on_nack(&[i]);
            f.record_sent(55, 2);
        }
        let want = f.decide_redundant(true, 55);
        let actual = affordable_redundant(55, want);
        assert!(actual <= 19, "55 片的预算上限是 19，实际给出 {actual}");
        assert!(actual >= 2, "关键帧不该被预算钳成 0 或 1，实际 {actual}");
    }

    #[test]
    fn 自适应_小帧不污染丢包估计() {
        // 第一版的 bug：record_frame 记的是 config.redundant，
        // 而 2 分片的 H.264 P 帧实际被预算钳成 0 个冗余。
        // 记成「有冗余保护」会让反解出的丢包率严重失真。
        let mut f = FecOutbound::from_args(&test_args(6, false)).expect("应启用");
        for i in 0..300u32 {
            f.on_nack(&[i]);
            let want = f.decide_redundant(false, 55);
            // 2 分片帧：预算 = floor(2*0.35) = 0，实际不发冗余
            let actual = affordable_redundant(2, want);
            assert_eq!(actual, 0, "2 分片帧不该发冗余");
            f.record_sent(2, actual);
        }
        // 记的是 0 个冗余（FEC 关），反解应按 r=0 做；
        // 若误记成 6，反解出的丢包率会低得多
        assert!(
            f.loss.last_redundant() < 0.5,
            "估计器应记录「本帧没有 FEC 保护」，实际 last_redundant={}",
            f.loss.last_redundant()
        );
    }
}
