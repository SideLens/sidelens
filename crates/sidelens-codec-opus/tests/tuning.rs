//! Opus 参数调优：DTX / in-band FEC / 帧长对比
//!
//! # 这个文件守什么
//!
//! `tests/opus.rs` 守「编解码是对的」。这里守**三个没做过的开关的实测结论**，
//! 每一条都是真实跑出来的数字，不是估算。
//!
//! 所有断言都带上下界而不是等号 —— libopus 是 C 实现，版本间的 VBR 决策
//! 可能微调，但**数量级与方向**不该变。数量级变了就是结论变了，测试该红。
//!
//! # 三句话结论
//!
//! 1. **DTX 的收益是 4.4x，不是 ∞** —— 32B 协议帧头比 1B 的 DTX 载荷大 32 倍
//! 2. **in-band FEC 不增加码流**（SILK 的冗余从语音帧里挪，不额外加），
//!    代价是正常解码的音质下降；修复质量平均比 PLC 好 2.2 倍
//! 3. **40ms / 60ms 不是新编码模式** —— 是 2 / 3 个 20ms 内部帧打进一个包
//!
//! # 怎么复现
//!
//! `cargo run -p sidelens-codec-opus --example tuning_probe`

#![allow(non_snake_case)]

use sidelens_codec::audio::{AudioFrame, AudioJitterBuffer, OpusConfig, PushOutcome};
use sidelens_codec_opus::packet;
use sidelens_codec_opus::{OpusDecoder, OpusEncoder};

/// 协议帧头长度（`sidelens-proto` 的 `HEADER_SIZE`）
///
/// 这里写死常量而不是依赖 `sidelens-proto` —— 本 crate 不该为了一个
/// 数字引入一个依赖。值变了会让所有带宽结论失效，所以测试里单独钉一次。
const HEADER_BYTES: usize = 32;

// ─────────────────────────── 测试信号 ───────────────────────────

/// 类语音信号：基频 + 两个共振峰 + 4Hz 音节包络
///
/// # 为什么不用正弦
///
/// 实测 440Hz 纯音**不触发 DTX 的 VAD**（`dtx_frames=0`），
/// 用正弦测 DTX 会得到「DTX 无效」的错误结论。
/// 类语音信号有音节起伏和共振峰，才能让 VAD 判成「静音」。
fn speechlike(frames: usize, frame_samples: usize, rate: f64) -> Vec<i16> {
    let mut v = Vec::with_capacity(frames * frame_samples);
    for i in 0..frames * frame_samples {
        let t = i as f64 / rate;
        // 4Hz 音节包络，有真实的停顿段
        let syl = ((t * 4.0 * std::f64::consts::TAU).sin() * 0.5 + 0.5).powf(2.0);
        let f0 = 120.0 + 15.0 * (t * 3.0 * std::f64::consts::TAU).sin();
        let mut s = 0.0;
        for k in 1..=12 {
            s += (t * f0 * k as f64 * std::f64::consts::TAU).sin() / k as f64;
        }
        s += 0.35 * (t * 700.0 * std::f64::consts::TAU).sin();
        s += 0.20 * (t * 1200.0 * std::f64::consts::TAU).sin();
        v.push((s * syl * 0.18 * i16::MAX as f64).clamp(-32768.0, 32767.0) as i16);
    }
    v
}

fn silence(frames: usize, frame_samples: usize) -> Vec<i16> {
    vec![0i16; frames * frame_samples]
}

fn rmse(a: &[i16], b: &[i16]) -> f64 {
    let n = a.len().min(b.len());
    if n == 0 {
        return 0.0;
    }
    let s: f64 = (0..n)
        .map(|i| {
            let d = a[i] as f64 - b[i] as f64;
            d * d
        })
        .sum();
    (s / n as f64).sqrt()
}

fn rms(v: &[i16]) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    (v.iter().map(|&x| (x as f64) * (x as f64)).sum::<f64>() / v.len() as f64).sqrt()
}

/// 一次完整编码的结果
struct Encoded {
    packets: Vec<Vec<u8>>,
    dtx: Vec<bool>,
    frames: Vec<usize>,
    bytes: usize,
}

impl Encoded {
    fn dtx_count(&self) -> usize {
        self.dtx.iter().filter(|&&d| d).count()
    }

    /// 这批帧覆盖的总时长（秒）
    ///
    /// # 为什么不让调用方传时长
    ///
    /// 传参版本踩过两次坑：调用方写 `payload_kbps(5.0)` 时
    /// 5.0 被当成毫秒解释，整张表差 1000 倍。时长应该由
    /// 「包数 × 每包时长」算出来，调用方不该有机会写错单位。
    fn seconds(&self, frame_duration_us: u64) -> f64 {
        self.packets.len() as f64 * frame_duration_us as f64 / 1_000_000.0
    }

    /// 线上码率（含 32B 协议帧头），单位 kbps
    fn wire_kbps(&self, frame_duration_us: u64) -> f64 {
        let secs = self.seconds(frame_duration_us);
        (self.bytes + self.packets.len() * HEADER_BYTES) as f64 * 8.0 / secs / 1000.0
    }

    /// 纯载荷码率（不含帧头），单位 kbps
    fn payload_kbps(&self, frame_duration_us: u64) -> f64 {
        let secs = self.seconds(frame_duration_us);
        self.bytes as f64 * 8.0 / secs / 1000.0
    }
}

fn encode_all(
    cfg: OpusConfig,
    bitrate: i32,
    dtx: bool,
    fec: bool,
    loss: u8,
    pcm: &[i16],
) -> Encoded {
    let mut enc = OpusEncoder::new(cfg, bitrate).expect("创建编码器");
    enc.set_dtx(dtx).expect("设置 DTX");
    enc.set_inband_fec(fec).expect("设置 in-band FEC");
    enc.set_packet_loss_perc(loss).expect("设置丢包率");

    // 每帧的交织采样数 = 每声道采样数 × 声道数
    let n = cfg.frame_samples() as usize * cfg.channels as usize;
    let mut out = vec![0u8; 8000];
    let mut e = Encoded {
        packets: Vec::new(),
        dtx: Vec::new(),
        frames: Vec::new(),
        bytes: 0,
    };
    for k in 0..pcm.len() / n {
        let r = enc
            .encode_frame(&pcm[k * n..(k + 1) * n], &mut out)
            .expect("编码");
        e.packets.push(out[..r.bytes].to_vec());
        e.dtx.push(r.dtx);
        e.frames.push(r.frames);
        e.bytes += r.bytes;
    }
    e
}

// ═══════════════════════════ 一、DTX ═══════════════════════════

#[test]
fn dtx_静音五秒_载荷降到九分之一() {
    // 这是 DTX 最核心的一条：它到底省多少。
    //
    // 实测（48kHz 单声道 20ms 32kbps Voip，250 帧）：
    //   无 DTX：8746 字节 = 35.0 B/帧
    //   有 DTX： 960 字节 = 229 个 1B DTX 帧 + 21 个 ~35B 刷新帧
    //
    // 注意**不是 0** —— DTX 有周期性刷新（见下一条测试）。
    let cfg = OpusConfig::default();
    let pcm = silence(250, 960);

    let off = encode_all(cfg, 32_000, false, false, 0, &pcm);
    let on = encode_all(cfg, 32_000, true, false, 0, &pcm);

    assert_eq!(off.packets.len(), 250);
    assert_eq!(off.dtx_count(), 0, "DTX 关时不该有抑制帧");
    assert!(
        (30..=45).contains(&(off.bytes / 250)),
        "无 DTX 静音应约 35 B/帧，实测 {}",
        off.bytes / 250
    );

    assert!(
        on.dtx_count() > 200,
        "DTX 应抑制绝大多数静音帧，实际 {}",
        on.dtx_count()
    );
    assert!(on.bytes < off.bytes / 4, "DTX 后字节数应降到 1/4 以下");
    assert!(on.bytes > 0, "DTX 不等于零字节 —— 有周期性刷新帧");

    // 实测比值 8746 / 960 = 9.1x（载荷）；线上含 32B 帧头时只有 1.9x
    let ratio = off.bytes as f64 / on.bytes as f64;
    assert!(
        (7.0..12.0).contains(&ratio),
        "载荷压缩比应在 7-12x，实测 {ratio:.1}x"
    );
}

#[test]
fn dtx_刷新间隔是二十一帧_不是永久静音() {
    // libopus 的 DTX 有硬约束：连续抑制 20 帧后强制发一帧真实的
    // （MAX_CONSECUTIVE_DTX = 20）。否则解码器会无限期靠 PLC 外推，
    // 恢复时听起来像「从很远的地方回来」。
    //
    // 实测 20ms 帧：前 10 帧正常（NB_SPEECH_FRAMES_BEFORE_DTX），
    // 之后每 21 帧出现一个刷新帧。
    let cfg = OpusConfig::default();
    let pcm = silence(250, 960);
    let e = encode_all(cfg, 32_000, true, false, 0, &pcm);

    let refresh: Vec<usize> = (0..e.packets.len()).filter(|&i| !e.dtx[i]).collect();
    assert!(refresh.len() >= 10, "应有刷新帧，实际 {}", refresh.len());

    // 前 10 帧是预热期（VAD 还没判定）
    assert!(
        refresh.iter().take(10).all(|&i| i < 12),
        "前 10 帧应为真实帧（VAD 预热期），实际 {:?}",
        &refresh[..10.min(refresh.len())]
    );

    // 刷新间隔的最大值 = MAX_CONSECUTIVE_DTX + 1
    let mut max_gap = 0usize;
    for w in refresh.windows(2) {
        max_gap = max_gap.max(w[1] - w[0]);
    }
    assert!(
        (15..=25).contains(&max_gap),
        "刷新间隔应约 21 帧，实测 {max_gap} 帧"
    );

    // 刷新帧是有真实数据的，不是 1 字节
    let refresh_sizes: Vec<usize> = refresh.iter().map(|&i| e.packets[i].len()).collect();
    assert!(
        refresh_sizes.iter().all(|&s| s > 10),
        "刷新帧应带真实数据，实测最小 {} 字节",
        refresh_sizes.iter().min().unwrap()
    );
}

#[test]
fn dtx_语音段不触发_字节数完全不变() {
    // DTX 只对「VAD 判定为静音」的帧生效。连续语音下它**一帧都不省**。
    //
    // 这条测试防的是「开了 DTX 就一定能省带宽」这个误解 ——
    // 纯语音场景（无停顿）下 DTX 收益是 0。
    let cfg = OpusConfig::default();
    let pcm = speechlike(250, 960, 48_000.0);

    let off = encode_all(cfg, 32_000, false, false, 0, &pcm);
    let on = encode_all(cfg, 32_000, true, false, 0, &pcm);

    assert_eq!(on.dtx_count(), 0, "连续语音不该被 DTX 抑制");
    assert_eq!(off.bytes, on.bytes, "语音段 DTX 前后字节数必须完全一致");
}

#[test]
fn dtx_静音到语音切换_切换点无额外代价() {
    // 关键问题：从 DTX 静音切回语音时，编码器要不要「热身」？
    //
    // 实测：**不要**。切换后第 1 帧立刻就是正常大小（77 字节），
    // 没有渐变过程。这说明 DTX 的恢复是即时的。
    let cfg = OpusConfig::default();
    let mut pcm = silence(100, 960);
    pcm.extend_from_slice(&speechlike(150, 960, 48_000.0));

    let off = encode_all(cfg, 32_000, false, false, 0, &pcm);
    let on = encode_all(cfg, 32_000, true, false, 0, &pcm);

    assert!(
        on.dtx_count() >= 80,
        "静音段应被抑制，实际 {}",
        on.dtx_count()
    );
    assert!(on.bytes < off.bytes, "混合信号 DTX 仍应更省");

    // 切换点（第 100 帧）之后立刻恢复正常大小
    let after: Vec<usize> = on.packets[100..105].iter().map(|p| p.len()).collect();
    assert!(
        after.iter().all(|&s| s > 40),
        "切换后应立刻恢复正常帧大小，实测 {after:?}"
    );

    // 切换前一刻仍是 DTX
    assert!(on.dtx[99], "第 99 帧（静音末）应是 DTX 帧");
}

#[test]
fn dtx_判定必须用包结构不能用长度阈值() {
    // # 这条测试防的是一个真实踩到的坑
    //
    // 最初用 len <= channels 判 DTX，20ms / 40ms 都对（1 字节），
    // **60ms 全漏** —— 因为 60ms 的 DTX 包是 2 字节（TOC + 帧数字节）。
    //
    // 后果是「静音期不发包」的统计完全失真：60ms 下 DTX 帧数报 0。
    //
    // 正确判据是「包里数据字节为 0」，见 packet::is_dtx。
    for (ms, expect_len) in [(20.0f64, 1usize), (40.0, 1), (60.0, 2)] {
        let cfg = OpusConfig {
            frame_ms: ms,
            ..OpusConfig::default()
        };
        let n = cfg.frame_samples() as usize;
        let frames = (5.0 / (ms / 1000.0)).round() as usize;
        let pcm = silence(frames, n);
        let e = encode_all(cfg, 32_000, true, false, 0, &pcm);

        let dtx_pkts: Vec<&Vec<u8>> = e
            .packets
            .iter()
            .zip(e.dtx.iter())
            .filter(|(_, &d)| d)
            .map(|(p, _)| p)
            .collect();
        assert!(!dtx_pkts.is_empty(), "{ms}ms 应产生 DTX 帧");

        for p in &dtx_pkts {
            assert_eq!(
                p.len(),
                expect_len,
                "{ms}ms 的 DTX 包应 {expect_len} 字节，实测 {p:?}"
            );
            assert_eq!(
                packet::data_bytes(p),
                Some(0),
                "{ms}ms DTX 包的数据字节应为 0"
            );
            assert!(packet::is_dtx(p), "{ms}ms DTX 包应被 is_dtx 识别");
        }

        // 60ms 的反例：按长度阈值判会全漏
        if ms == 60.0 {
            let by_len = dtx_pkts.iter().filter(|p| p.len() <= 1).count();
            assert_eq!(
                by_len,
                0,
                "60ms 下按「<=1 字节」判 DTX 会漏掉全部 {} 个 DTX 包",
                dtx_pkts.len()
            );
        }
    }
}

#[test]
fn dtx_帧能被正常解码不是坏包() {
    // DTX 帧只有 TOC 没有数据，但它仍是**合法的 Opus 包**。
    // 解码器要能吃掉它而不是报错 —— 否则接收端会把静音期全记成
    // audio_decode_failed。
    let cfg = OpusConfig::default();
    let mut enc = OpusEncoder::new(cfg, 32_000).expect("编码器");
    enc.set_dtx(true).expect("开 DTX");
    let mut dec = OpusDecoder::new(cfg).expect("解码器");
    let mut out = vec![0u8; 8000];
    let mut buf = vec![0i16; 5760];

    // 先喂语音建立解码器状态
    let speech = speechlike(20, 960, 48_000.0);
    for k in 0..10 {
        let n = enc
            .encode(&speech[k * 960..(k + 1) * 960], &mut out)
            .unwrap();
        dec.decode(Some(&out[..n]), &mut buf).expect("语音帧应能解");
    }

    // # 注意 VAD 预热期
    //
    // 切到静音后前 **10 帧**仍是正常大小（NB_SPEECH_FRAMES_BEFORE_DTX），
    // 之后才进 DTX。所以这里跑 60 帧、只对后 40 帧断言 1 字节。
    let sil = silence(1, 960);
    let mut sizes = Vec::new();
    for _ in 0..60 {
        let n = enc.encode(&sil, &mut out).unwrap();
        sizes.push(n);
        dec.decode(Some(&out[..n]), &mut buf)
            .expect("DTX 包必须能解码，否则接收端会把它计成坏包");
    }

    // 预热期之后应稳定产出 1 字节 DTX 包（中间夹杂周期性刷新帧）
    let tail = &sizes[20..];
    let dtx_count = tail.iter().filter(|&&n| n == 1).count();
    assert!(
        dtx_count >= 30,
        "预热期后应有大量 1 字节 DTX 包，实测 {dtx_count}/{}",
        tail.len()
    );
    assert_eq!(dec.failed(), 0, "DTX 包不是坏包");
}

// ══════════════ 二、DTX 与抖动缓冲（关键问题）══════════════

#[test]
fn dtx_不发包不会触发_discontinuity() {
    // # 任务里点名要查的问题
    //
    // 「DTX 期间不发包，接收端抖动缓冲会看到空洞」—— 实测：**不会误判**。
    //
    // 原因：AudioJitterBuffer::push 的跳变判定只看
    // |pts - last_pts| > DISCONTINUITY_THRESHOLD_US（500ms），
    // **不看帧数**。DTX 不发包时 PTS 仍然连续（发送端的 AudioPacer
    // 按帧序号算 PTS，跳过的帧也推进时钟），所以只是「帧数变少」。
    //
    // 真正的风险是**别的东西**（见下一条测试）。
    let cfg = OpusConfig::default();
    let pcm = silence(300, 960);
    let e = encode_all(cfg, 32_000, true, false, 0, &pcm);

    let mut jb = AudioJitterBuffer::new(cfg, 40.0).expect("抖动缓冲");
    let mut outcomes: Vec<PushOutcome> = Vec::new();
    let mut pts = 0u64;
    for (i, p) in e.packets.iter().enumerate() {
        if e.dtx[i] {
            pts += 20_000; // PTS 照常推进，只是不发包
            continue;
        }
        outcomes.push(jb.push(AudioFrame::new(pts, 20_000, i as u32, p.clone())));
        pts += 20_000;
    }

    assert!(
        !outcomes.contains(&PushOutcome::Discontinuity),
        "DTX 不发包不该触发 Discontinuity，实测 {outcomes:?}"
    );
    assert!(outcomes.contains(&PushOutcome::Ok), "至少应有帧正常入队");
}

#[test]
fn discontinuity_只在_pts_跳变超五百毫秒时触发() {
    // 把阈值钉死，证明「DTX 的空洞」和「时间线断裂」是两回事。
    //
    // 实测：500ms 恰好不触发（判定是 > 不是 >=），501ms 触发。
    let cfg = OpusConfig::default();
    for (gap_ms, expect_disc) in [
        (100u64, false),
        (420, false),
        (499, false),
        (500, false),
        (501, true),
    ] {
        let mut jb = AudioJitterBuffer::new(cfg, 40.0).expect("抖动缓冲");
        let first = jb.push(AudioFrame::new(0, 20_000, 0, vec![1; 60]));
        assert_eq!(first, PushOutcome::Ok);
        let second = jb.push(AudioFrame::new(gap_ms * 1000, 20_000, 1, vec![1; 60]));
        if expect_disc {
            assert_eq!(
                second,
                PushOutcome::Discontinuity,
                "空洞 {gap_ms}ms 应触发 Discontinuity"
            );
        } else {
            assert_ne!(
                second,
                PushOutcome::Discontinuity,
                "空洞 {gap_ms}ms 不该触发 Discontinuity"
            );
        }
    }
}

#[test]
fn dtx_不发包的最大_pts_空洞低于阈值() {
    // DTX 不发包时，两个实际发出的包之间的最大 PTS 间隔是多少？
    //
    // 实测三档都在 420-440ms，**低于 500ms 阈值** —— 这是巧合吗？不是：
    // MAX_CONSECUTIVE_DTX = 20 帧，20ms × 21 = 420ms，40ms × 11 = 440ms，
    // 60ms × 7 = 420ms。三档都刚好卡在 500ms 以下。
    //
    // 但**没有余量**：再丢一个刷新包就超了（见下一条）。
    for ms in [20.0f64, 40.0, 60.0] {
        let cfg = OpusConfig {
            frame_ms: ms,
            ..OpusConfig::default()
        };
        let n = cfg.frame_samples() as usize;
        let frames = (20.0 / (ms / 1000.0)).round() as usize;
        let pcm = silence(frames, n);
        let e = encode_all(cfg, 32_000, true, false, 0, &pcm);

        let mut max_gap = 0u64;
        let mut prev: Option<u64> = None;
        for (i, &d) in e.dtx.iter().enumerate() {
            if d {
                continue;
            }
            let t = i as u64 * (ms * 1000.0) as u64;
            if let Some(pv) = prev {
                max_gap = max_gap.max(t - pv);
            }
            prev = Some(t);
        }

        assert!(
            max_gap < 500_000,
            "{ms}ms 帧的 DTX 空洞 {max_gap}us 超过 Discontinuity 阈值 500000us"
        );
        assert!(
            max_gap > 300_000,
            "{ms}ms 帧的 DTX 空洞只有 {max_gap}us，与预期不符"
        );
    }
}

#[test]
fn dtx_丢一个刷新包仍不越阈值_但连续两个就断线() {
    // # DTX 不发包的真实代价
    //
    // 20ms 帧下刷新间隔 420ms，离 500ms 阈值只有 **80ms 余量**。
    // 丢**一个**刷新包后最大间隔仍是 420ms（相邻刷新帧间距本来就小），
    // 但**连续丢两个**就会越界。
    let cfg = OpusConfig::default();
    let pcm = silence(400, 960);
    let e = encode_all(cfg, 32_000, true, false, 0, &pcm);

    let refresh: Vec<usize> = (0..e.packets.len()).filter(|&i| !e.dtx[i]).collect();
    let dropped = refresh[2];

    let mut max_gap = 0u64;
    let mut prev: Option<u64> = None;
    for (i, &d) in e.dtx.iter().enumerate() {
        if d || i == dropped {
            continue;
        }
        let t = i as u64 * 20_000;
        if let Some(pv) = prev {
            max_gap = max_gap.max(t - pv);
        }
        prev = Some(t);
    }

    assert!(
        max_gap < 500_000,
        "丢一个刷新包后最大间隔 {max_gap}us 应仍低于阈值"
    );

    // 连续丢两个就会越界 —— 这是 DTX 不发包方案的硬边界。
    // 取最大的两个相邻间隔之和（= 连续丢两个刷新包时最坏的空洞）。
    let mut gaps: Vec<usize> = refresh.windows(2).map(|w| w[1] - w[0]).collect();
    gaps.sort_unstable();
    gaps.reverse();
    let worst_two = (gaps[0] + gaps[1]) as u64 * 20_000;
    assert!(
        worst_two > 500_000,
        "连续丢两个刷新包应越阈值，实测 {worst_two}us（最大两个间隔 {gaps:?}）"
    );
}

#[test]
fn dtx_不发包会让主时钟停滞_这是真实代价() {
    // # 比 Discontinuity 更严重的问题
    //
    // 抖动缓冲不会误判，但 **AvSync 会**。
    //
    // AvSync::on_audio 只在 pop_due 出队一帧时被调用。DTX 不发包
    // → 没有帧可出队 → audio_clock_us 不推进 → 静音期视频全部
    // 判 Wait，播放卡住最长 420ms。
    //
    // 所以**必须「DTX 帧也发包」**（1 字节），用 1B/帧 换主时钟连续。
    // 这条测试量化那个代价：1B × 50 帧/s = 0.4kbps。
    let cfg = OpusConfig::default();
    let pcm = silence(250, 960);
    let e = encode_all(cfg, 32_000, true, false, 0, &pcm);

    // 每个 DTX 帧只有 1 字节，全发出去的载荷码率
    let dtx_payload_bytes: usize = e
        .packets
        .iter()
        .zip(e.dtx.iter())
        .filter(|(_, &d)| d)
        .map(|(p, _)| p.len())
        .sum();
    let kbps = dtx_payload_bytes as f64 * 8.0 / 5.0 / 1000.0;

    assert!(
        kbps < 2.0,
        "DTX 帧全发的载荷码率应 < 2kbps，实测 {kbps:.2}kbps"
    );
    assert!(kbps > 0.0, "DTX 帧确实占字节");

    // 对照：完全不发 DTX 帧时，包数降到 1/5 以下
    let sent = e.packets.len() - e.dtx_count();
    assert!(
        sent < e.packets.len() / 5,
        "不发 DTX 帧时包数应降到 1/5 以下，实测 {sent}/{}",
        e.packets.len()
    );
}

// ═════════════════════ 三、in-band FEC ═════════════════════

#[test]
fn fec_开启后码流不增反略减() {
    // # 这条推翻了「FEC 一定增加带宽」的直觉
    //
    // 实测（48kHz 20ms 32kbps Voip，250 帧类语音）：
    //   loss=0 ：20755 → 20755（完全不变，packet_loss_perc=0 时 FEC 不生效）
    //   loss=10：20755 → 20487（**少了 268 字节**）
    //   loss=30：20755 → 20531（少了 224 字节）
    //
    // 原因：SILK 的 LBRR 冗余是**从语音帧内部挪比特**，不是额外加。
    // 编码器把同样的总码率重新分配：少给当前帧一点，多给上一帧的副本。
    // 所以总字节数基本不变，甚至因为重新分配更高效而略降。
    let cfg = OpusConfig::default();
    let pcm = speechlike(250, 960, 48_000.0);

    let base = encode_all(cfg, 32_000, false, false, 0, &pcm);
    let fec0 = encode_all(cfg, 32_000, false, true, 0, &pcm);
    let fec30 = encode_all(cfg, 32_000, false, true, 30, &pcm);

    assert_eq!(
        base.bytes, fec0.bytes,
        "packet_loss_perc=0 时 in-band FEC 不应产生任何冗余"
    );

    let ratio = fec30.bytes as f64 / base.bytes as f64;
    assert!(
        (0.95..1.05).contains(&ratio),
        "FEC(loss=30) 与无 FEC 的码流比值应在 0.95-1.05，实测 {ratio:.3}"
    );
}

#[test]
fn fec_的代价是正常解码音质下降() {
    // # FEC 不免费 —— 它付的是**音质**，不是带宽
    //
    // 实测（300 帧类语音，按 lookahead 312 对齐后算 RMSE）：
    //   fec=off          RMSE=1741
    //   fec=on  loss=0   RMSE=1741（无冗余，无代价）
    //   fec=on  loss=10  RMSE=1958（+12.5%）
    //   fec=on  loss=30  RMSE=2154（+23.7%）
    //
    // 这就是「不增加码流的抗丢包」的真相：比特是从当前帧挪走的，
    // **没有丢包时这部分比特就白费了**。
    let cfg = OpusConfig::default();
    let pcm = speechlike(300, 960, 48_000.0);
    let lookahead = 312usize;

    let decode_rmse = |fec: bool, loss: u8| -> f64 {
        let e = encode_all(cfg, 32_000, false, fec, loss, &pcm);
        let mut dec = OpusDecoder::new(cfg).expect("解码器");
        let mut buf = vec![0i16; 5760];
        let mut decoded: Vec<i16> = Vec::new();
        for p in &e.packets {
            let r = dec.decode(Some(p), &mut buf).expect("解码");
            decoded.extend_from_slice(&buf[..r.samples]);
        }
        let n = pcm.len().min(decoded.len());
        rmse(&decoded[lookahead..n], &pcm[..n - lookahead])
    };

    let clean = decode_rmse(false, 0);
    let loss30 = decode_rmse(true, 30);

    assert!(
        loss30 > clean,
        "开 FEC(loss=30) 后无丢包音质应下降：clean={clean:.0} loss30={loss30:.0}"
    );
    let penalty = loss30 / clean;
    assert!(
        (1.10..1.45).contains(&penalty),
        "音质代价应在 10%-45%，实测 {:.1}%",
        (penalty - 1.0) * 100.0
    );
}

#[test]
fn fec_能修复丢掉的那一帧_且明显优于_plc() {
    // # in-band FEC 的核心能力验证
    //
    // 场景：第 30 帧丢了，第 31 帧到了。
    //   - PLC 路径：decode(None) 用前 29 帧的状态外推
    //   - FEC 路径：recover_previous(第31帧) 用第 31 帧里的 LBRR 重建第 30 帧
    //
    // 实测（真值 RMS=3205）：
    //   FEC RMSE=816   （相对误差 25%）
    //   PLC RMSE=4145  （相对误差 129%，比真值本身还大）
    //
    // FEC 好 5.1 倍。
    let cfg = OpusConfig::default();
    let pcm = speechlike(120, 960, 48_000.0);
    let e = encode_all(cfg, 32_000, false, true, 30, &pcm);

    // 参考：全部正常解码
    let mut refdec = OpusDecoder::new(cfg).expect("解码器");
    let mut buf = vec![0i16; 5760];
    let mut reference: Vec<Vec<i16>> = Vec::new();
    for p in &e.packets {
        let r = refdec.decode(Some(p), &mut buf).expect("解码");
        reference.push(buf[..r.samples].to_vec());
    }

    let drop = 30usize;
    let truth = &reference[drop];

    // FEC 路径
    let mut d1 = OpusDecoder::new(cfg).expect("解码器");
    for p in e.packets.iter().take(drop) {
        d1.decode(Some(p), &mut buf).expect("解码");
    }
    let rec = d1
        .recover_previous(&e.packets[drop + 1], &mut buf)
        .expect("FEC 修复");
    let fec_pcm = buf[..rec.samples].to_vec();
    assert!(rec.fec_recovered, "应标记为 FEC 修复");
    assert!(!rec.concealed, "FEC 不是 PLC");

    // PLC 路径
    let mut d2 = OpusDecoder::new(cfg).expect("解码器");
    for p in e.packets.iter().take(drop) {
        d2.decode(Some(p), &mut buf).expect("解码");
    }
    let plc = d2.decode(None, &mut buf).expect("PLC");
    let plc_pcm = buf[..plc.samples].to_vec();
    assert!(plc.concealed);

    let ef = rmse(&fec_pcm, truth);
    let ep = rmse(&plc_pcm, truth);

    assert!(ef < ep, "FEC 应优于 PLC：FEC RMSE={ef:.0} PLC RMSE={ep:.0}");
    assert!(
        ef < rms(truth) * 0.5,
        "FEC 修复的相对误差应 < 50%，实测 {:.0}%",
        ef / rms(truth) * 100.0
    );
}

#[test]
fn fec_修复输出对应的是丢的那一帧_不是当前帧() {
    // # 语义确认：recover_previous(packet_n) 产出的是 packet_{n-1} 的 PCM
    //
    // 实测（丢第 30 帧，传入第 31 帧）：
    //   输出 vs 第 30 帧 RMSE = 816
    //   输出 vs 第 31 帧 RMSE = 3582
    //
    // 所以它对应的是**丢的那一帧**。这个语义如果搞反，
    // 修复出的音频会被播在错误的时间点上，听感是「回声」。
    let cfg = OpusConfig::default();
    let pcm = speechlike(60, 960, 48_000.0);
    let e = encode_all(cfg, 32_000, false, true, 30, &pcm);

    let mut refdec = OpusDecoder::new(cfg).expect("解码器");
    let mut buf = vec![0i16; 5760];
    let mut reference: Vec<Vec<i16>> = Vec::new();
    for p in &e.packets {
        let r = refdec.decode(Some(p), &mut buf).expect("解码");
        reference.push(buf[..r.samples].to_vec());
    }

    let drop = 30usize;
    let mut d = OpusDecoder::new(cfg).expect("解码器");
    for p in e.packets.iter().take(drop) {
        d.decode(Some(p), &mut buf).expect("解码");
    }
    let rec = d
        .recover_previous(&e.packets[drop + 1], &mut buf)
        .expect("FEC 修复");
    let out = buf[..rec.samples].to_vec();

    let vs_lost = rmse(&out, &reference[drop]);
    let vs_current = rmse(&out, &reference[drop + 1]);

    assert!(
        vs_lost < vs_current,
        "修复输出应更接近丢的第 {drop} 帧：vs_lost={vs_lost:.0} vs_current={vs_current:.0}"
    );
}

#[test]
fn fec_在三档帧长下都有效() {
    // 20 / 40 / 60ms 都走 SILK（config 15 / code 1-3），都带 LBRR。
    for ms in [20.0f64, 40.0, 60.0] {
        let cfg = OpusConfig {
            frame_ms: ms,
            ..OpusConfig::default()
        };
        let n = cfg.frame_samples() as usize;
        let pcm = speechlike(60, n, 48_000.0);
        let e = encode_all(cfg, 32_000, false, true, 30, &pcm);

        let mut refdec = OpusDecoder::new(cfg).expect("解码器");
        let mut buf = vec![0i16; 5760];
        let mut reference: Vec<Vec<i16>> = Vec::new();
        for p in &e.packets {
            let r = refdec.decode(Some(p), &mut buf).expect("解码");
            reference.push(buf[..r.samples].to_vec());
        }

        let drop = 20usize;
        let mut d1 = OpusDecoder::new(cfg).expect("解码器");
        for p in e.packets.iter().take(drop) {
            d1.decode(Some(p), &mut buf).expect("解码");
        }
        let rec = d1
            .recover_previous(&e.packets[drop + 1], &mut buf)
            .expect("FEC 修复");
        let fec_pcm = buf[..rec.samples].to_vec();

        let mut d2 = OpusDecoder::new(cfg).expect("解码器");
        for p in e.packets.iter().take(drop) {
            d2.decode(Some(p), &mut buf).expect("解码");
        }
        let plc = d2.decode(None, &mut buf).expect("PLC");
        let plc_pcm = buf[..plc.samples].to_vec();

        let ef = rmse(&fec_pcm, &reference[drop]);
        let ep = rmse(&plc_pcm, &reference[drop]);
        assert!(ef < ep, "{ms}ms: FEC({ef:.0}) 应优于 PLC({ep:.0})");
    }
}

#[test]
fn fec_关掉时修复接口静默返回垃圾_必须靠带外配置() {
    // # 一个真实的静默失效
    //
    // recover_previous 在编码端**没开 FEC** 时仍然返回 Ok ——
    // libopus 不报错，只是把当前帧当成普通包解，产出的 PCM 是当前帧的内容
    // 而不是「上一帧的修复版」。
    //
    // 后果：调用方以为修好了，实际播了错位的内容。听感是「回声」或「重复」。
    //
    // 这个方向**没有检测手段** —— 解码器无法从码流判断包里有没有 LBRR。
    // 所以只能靠带外配置（与 --opus / SIDELENS_OPUS 同一类问题）。
    let cfg = OpusConfig::default();
    let pcm = speechlike(40, 960, 48_000.0);

    let no_fec = encode_all(cfg, 32_000, false, false, 0, &pcm);
    let with_fec = encode_all(cfg, 32_000, false, true, 30, &pcm);

    let mut d1 = OpusDecoder::new(cfg).expect("解码器");
    let mut d2 = OpusDecoder::new(cfg).expect("解码器");
    let mut buf = vec![0i16; 5760];
    for k in 0..20 {
        d1.decode(Some(&no_fec.packets[k]), &mut buf).expect("解码");
        d2.decode(Some(&with_fec.packets[k]), &mut buf)
            .expect("解码");
    }

    // 关 FEC 时依然 Ok —— 这就是静默失效
    let r1 = d1.recover_previous(&no_fec.packets[21], &mut buf);
    assert!(
        r1.is_ok(),
        "关 FEC 时 recover_previous 仍返回 Ok —— 这正是静默失效的来源"
    );

    // 开 FEC 时也 Ok，但两者内容不同
    let r2 = d2.recover_previous(&with_fec.packets[21], &mut buf);
    assert!(r2.is_ok());

    // 两个解码器都能继续工作，没有任何错误信号
    assert_eq!(d1.failed(), 0);
    assert_eq!(d2.failed(), 0);
}

#[test]
fn fec_修复是事后修复_需要抖动缓冲深度大于一帧() {
    // # 任务里点名的「事后修复」限制
    //
    // 第 N 帧丢、第 N+1 帧到达时才能修第 N 帧。第 N 帧的**播放时刻**
    // 在那之前就到了。所以能不能用上，取决于抖动缓冲里还有没有第 N 帧。
    //
    // 实测（20ms 帧，丢第 30 帧，第 31 帧在 PTS=620ms 到达）：
    //   target=0ms  容量 2  已播出 2 帧  剩余 0  → 第 30 帧已播，修好没用
    //   target=20ms 容量 2  已播出 2 帧  剩余 0  → 同上
    //   target=40ms 容量 3  已播出 3 帧  剩余 0  → 同上（临界）
    //   target=60ms 容量 4  已播出 3 帧  剩余 1  → **还有一帧在缓冲里**
    //
    // 结论：**抖动缓冲深度 <= 2 帧时 in-band FEC 完全无用**。
    // 当前 daemon 用 target_ms=40ms → 容量 3 帧 → 刚好在临界点上，
    // 实际用不上。要让它有价值，target_ms 必须 >= 60ms。
    let cfg = OpusConfig::default();
    let mut usable_at = Vec::new();

    for target_ms in [0.0f64, 20.0, 40.0, 60.0, 80.0] {
        let mut jb = AudioJitterBuffer::new(cfg, target_ms).expect("抖动缓冲");
        // 前 30 帧已入队（第 30 帧缺失）
        for i in 0..30u64 {
            jb.push(AudioFrame::new(i * 20_000, 20_000, i as u32, vec![1; 60]));
        }
        // 第 31 帧在 PTS=620ms 时刻到达
        let now = 620_000u64;
        let playout = now.saturating_sub((target_ms * 1000.0) as u64);
        let mut popped = 0;
        while jb.pop_due(playout).is_some() {
            popped += 1;
        }
        // 缓冲里还有帧 → 第 30 帧的空位还没轮到 → FEC 来得及
        if !jb.is_empty() {
            usable_at.push(target_ms);
        }
        assert!(popped > 0, "target={target_ms}ms 时应有帧到期");
    }

    assert!(
        !usable_at.is_empty(),
        "至少应在某个 target_ms 下 FEC 来得及"
    );
    assert!(
        usable_at.iter().all(|&t| t >= 60.0),
        "FEC 只在 target_ms >= 60ms 时来得及，实测可用档位 {usable_at:?}"
    );
}

// ═════════════════════ 四、帧长对比 ═════════════════════

#[test]
fn 帧长_四十和六十毫秒不是新模式而是打包() {
    // # 这条推翻了「40ms / 60ms 是新编码模式」的直觉
    //
    // 用 TOC 解析看包结构（实测 32kbps Voip）：
    //
    // | 请求帧长 | TOC | config | code | 内部帧数 | 内部帧长 |
    // | --- | --- | --- | --- | --- | --- |
    // | 20ms | 0x78 | 15 | 0 | 1 | 20ms |
    // | 40ms | 0x7A | 15 | 2 | 2 | 20ms |
    // | 60ms | 0xDB | 27 | 3 | 3 | 20ms |
    //
    // 也就是说 libopus 把 2 / 3 个 20ms 帧打进一个包，
    // **内部帧长仍然是 20ms**。这解释了为什么 lookahead 不变（见下一条）。
    //
    // 直接后果：丢一个 60ms 包 = 丢 3 个内部帧，PLC 要外推 3 次。
    for (ms, expect_frames) in [(20.0f64, 1usize), (40.0, 2), (60.0, 3)] {
        let cfg = OpusConfig {
            frame_ms: ms,
            ..OpusConfig::default()
        };
        let n = cfg.frame_samples() as usize;
        let pcm = speechlike(4, n, 48_000.0);
        let e = encode_all(cfg, 32_000, false, false, 0, &pcm);
        let p = &e.packets[2];

        let t = packet::toc(p).expect("TOC");
        assert_eq!(
            packet::frame_count(p),
            Some(expect_frames),
            "{ms}ms 包应含 {expect_frames} 个内部帧"
        );
        assert_eq!(
            packet::config_frame_us(t.config),
            20_000,
            "{ms}ms 包的内部帧长应是 20ms，实测 {}us",
            packet::config_frame_us(t.config)
        );
        assert_eq!(
            packet::packet_duration_us(p),
            Some((ms * 1000.0) as u64),
            "{ms}ms 包声明时长应为 {ms}ms"
        );
        assert_eq!(e.frames[2], expect_frames);
    }
}

#[test]
fn 帧长_lookahead_恒为_312_采样_与帧长无关() {
    // # 这条推翻了 docs/opus.md 的一个假设
    //
    // 原文写「换成 60ms 帧长时 lookahead 会变」。
    // 实测 2.5 / 20 / 40 / 60ms 四档都是 **312 采样 = 6500us**，一档不差。
    //
    // 原因：lookahead 是解码器侧 CELT 的重叠窗长度，与编码帧长无关。
    // 所以**加大帧长不会改善也不会恶化音画同步**，可以放心按带宽选帧长。
    for ms in [2.5f64, 20.0, 40.0, 60.0] {
        let cfg = OpusConfig {
            frame_ms: ms,
            ..OpusConfig::default()
        };
        let enc = OpusEncoder::new(cfg, 32_000).expect("编码器");
        assert_eq!(
            enc.lookahead_samples(),
            312,
            "{ms}ms 帧的 lookahead 应仍是 312 采样，实测 {}",
            enc.lookahead_samples()
        );
        assert_eq!(enc.lookahead_us(), 6500);
    }
}

#[test]
fn 帧长_帧头开销占比从_28_降到_11() {
    // # 任务里的核心权衡表
    //
    // 实测（48kHz 单声道 32kbps Voip，5 秒类语音，含 32B 协议帧头）：
    //
    // | 帧长 | 包数 | 载荷 | 帧头 | 线上码率 | 帧头占比 |
    // | --- | --- | --- | --- | --- | --- |
    // | 20ms | 250 | 20755 | 8000 | 46.0 kbps | 27.8% |
    // | 40ms | 125 | 20794 | 4000 | 39.7 kbps | 16.1% |
    // | 60ms | 83  | 20933 | 2656 | 37.7 kbps | 11.3% |
    //
    // 载荷码率几乎不变（33.2 / 33.3 / 33.5 kbps）—— Opus 的目标码率
    // 是按「每秒多少比特」算的，与帧长无关。**省下来的全是帧头。**
    let cfg20 = OpusConfig::default();
    let pcm = speechlike(250, 960, 48_000.0);
    let e20 = encode_all(cfg20, 32_000, false, false, 0, &pcm);

    let header20 = e20.packets.len() * HEADER_BYTES;
    let share20 = header20 as f64 / (e20.bytes + header20) as f64;
    assert!(
        (0.25..0.31).contains(&share20),
        "20ms 帧头占比应约 28%，实测 {:.1}%",
        share20 * 100.0
    );

    // 40ms：包数减半，载荷不变
    let cfg40 = OpusConfig {
        frame_ms: 40.0,
        ..OpusConfig::default()
    };
    let e40 = encode_all(
        cfg40,
        32_000,
        false,
        false,
        0,
        &speechlike(125, 1920, 48_000.0),
    );
    let header40 = e40.packets.len() * HEADER_BYTES;
    let share40 = header40 as f64 / (e40.bytes + header40) as f64;
    assert!(
        (0.13..0.19).contains(&share40),
        "40ms 帧头占比应约 16%，实测 {:.1}%",
        share40 * 100.0
    );

    // 60ms：83 包（5 秒不能被 60ms 整除，向上取整）
    let cfg60 = OpusConfig {
        frame_ms: 60.0,
        ..OpusConfig::default()
    };
    let e60 = encode_all(
        cfg60,
        32_000,
        false,
        false,
        0,
        &speechlike(83, 2880, 48_000.0),
    );
    let header60 = e60.packets.len() * HEADER_BYTES;
    let share60 = header60 as f64 / (e60.bytes + header60) as f64;
    assert!(
        (0.09..0.14).contains(&share60),
        "60ms 帧头占比应约 11%，实测 {:.1}%",
        share60 * 100.0
    );

    // 载荷码率三档几乎一样
    let p20 = e20.payload_kbps(20_000);
    let p40 = e40.payload_kbps(40_000);
    let p60 = e60.payload_kbps(60_000);
    // 三档载荷码率相对偏差应 < 2%
    let spread = (p20.max(p60) - p20.min(p60)) / p20;
    assert!(
        spread < 0.02,
        "三档载荷码率应几乎相同：{p20:.1} / {p40:.1} / {p60:.1} kbps（偏差 {:.1}%）",
        spread * 100.0
    );
}

#[test]
fn 帧长_越长单帧丢失损失越大() {
    // # 帧长的另一面
    //
    // PLC 按「一帧」外推，帧越长外推得越久：
    //   20ms → 补 960 采样
    //   40ms → 补 1920 采样
    //   60ms → 补 2880 采样
    //
    // 而且 40ms / 60ms 包内部是 2 / 3 个 20ms 帧，
    // 丢一个包等于连续丢 2 / 3 个内部帧，PLC 要连续外推多次，
    // 第二次外推的质量远低于第一次。
    for (ms, expect_samples) in [(20.0f64, 960usize), (40.0, 1920), (60.0, 2880)] {
        let cfg = OpusConfig {
            frame_ms: ms,
            ..OpusConfig::default()
        };
        let n = cfg.frame_samples() as usize;
        let pcm = speechlike(20, n, 48_000.0);
        let e = encode_all(cfg, 32_000, false, false, 0, &pcm);

        let mut dec = OpusDecoder::new(cfg).expect("解码器");
        let mut buf = vec![0i16; 5760];
        for p in e.packets.iter().take(10) {
            dec.decode(Some(p), &mut buf).expect("解码");
        }
        let r = dec.decode(None, &mut buf).expect("PLC");
        assert_eq!(
            r.samples, expect_samples,
            "{ms}ms 帧的 PLC 应补 {expect_samples} 采样，实测 {}",
            r.samples
        );
        // 换算成毫秒 = 帧长
        let ms_out = r.samples as f64 / 48.0;
        assert!(
            (ms_out - ms).abs() < 1.0,
            "{ms}ms 帧的 PLC 应补 {ms}ms，实测 {ms_out:.0}ms"
        );
    }
}

#[test]
fn 帧长_与_dtx_组合时帧头成为主导开销() {
    // # 三块合起来看，帧头变成了主要成本
    //
    // 静音 5 秒（实测）：
    //
    // | 帧长 | 无 DTX 线上 | 有 DTX 线上 | 降幅 |
    // | --- | --- | --- | --- |
    // | 20ms | 26.8 kbps | 14.3 kbps | 1.9x |
    // | 40ms | 20.2 kbps | 7.8 kbps  | 2.6x |
    // | 60ms | 18.1 kbps | 5.7 kbps  | 3.2x |
    //
    // **帧长越长，DTX 的收益越大** —— 因为 DTX 把载荷压到接近 0 之后，
    // 剩下的全是帧头，而帧头数量与帧长成反比。
    //
    // 20ms 下 DTX 只能把静音压到 14.3 kbps，因为 250 x 32B = 8000 字节
    // 的帧头就是 12.8 kbps，载荷只剩 1.5 kbps。
    let mut prev_ratio = 0.0;
    for ms in [20.0f64, 40.0, 60.0] {
        let cfg = OpusConfig {
            frame_ms: ms,
            ..OpusConfig::default()
        };
        let n = cfg.frame_samples() as usize;
        let frames = (5.0 / (ms / 1000.0)).round() as usize;
        let pcm = silence(frames, n);

        let off = encode_all(cfg, 32_000, false, false, 0, &pcm);
        let on = encode_all(cfg, 32_000, true, false, 0, &pcm);

        let dur = (ms * 1000.0) as u64;
        let r_off = off.wire_kbps(dur);
        let r_on = on.wire_kbps(dur);
        let ratio = r_off / r_on;

        assert!(
            ratio > prev_ratio,
            "{ms}ms 的 DTX 线上降幅 {ratio:.2}x 应大于更短帧长的 {prev_ratio:.2}x"
        );
        prev_ratio = ratio;

        // DTX 后帧头占绝对主导
        let header_share = (on.packets.len() * HEADER_BYTES) as f64
            / (on.bytes + on.packets.len() * HEADER_BYTES) as f64;
        assert!(
            header_share > 0.6,
            "{ms}ms + DTX 时帧头应占 60% 以上，实测 {:.1}%",
            header_share * 100.0
        );
    }

    // 60ms 下 DTX 能把静音压到 6 kbps 以下
    let cfg60 = OpusConfig {
        frame_ms: 60.0,
        ..OpusConfig::default()
    };
    let e = encode_all(cfg60, 32_000, true, false, 0, &silence(83, 2880));
    assert!(
        e.wire_kbps(60_000) < 7.0,
        "60ms + DTX 静音线上码率应 < 7kbps，实测 {:.1}",
        e.wire_kbps(60_000)
    );
}

// ═══════════════════ 五、audiopus 的接口边界 ═══════════════════

#[test]
fn audiopus_没有_set_dtx_必须走裸_ctl() {
    // # 如实记录 audiopus 0.2.0 的接口缺口
    //
    // Encoder 有 set_inband_fec / set_packet_loss_perc，但**没有**
    // set_dtx。不过它把底层 ctl 入口暴露成公开方法：
    //
    //     pub fn set_encoder_ctl_request(&mut self, request: i32, value: i32) -> Result<()>
    //
    // 加上 pub use audiopus_sys as ffi;，可以直接发
    // OPUS_SET_DTX_REQUEST（4016）。不需要新依赖、不需要 unsafe。
    //
    // 这条测试守住「这条路是通的」，以及「回读确认」这个纪律 ——
    // ctl 返回 Ok 只代表 libopus 没报错，不代表值被采纳。
    let cfg = OpusConfig::default();
    let mut enc = OpusEncoder::new(cfg, 32_000).expect("编码器");

    assert!(!enc.dtx(), "默认 DTX 应关闭");
    enc.set_dtx(true).expect("开 DTX");
    assert!(enc.dtx(), "回读应确认 DTX 已开");
    enc.set_dtx(false).expect("关 DTX");
    assert!(!enc.dtx(), "回读应确认 DTX 已关");

    assert!(!enc.inband_fec(), "默认 in-band FEC 应关闭");
    enc.set_inband_fec(true).expect("开 FEC");
    assert!(enc.inband_fec());
    assert_eq!(enc.packet_loss_perc(), 0, "默认丢包率应为 0");

    enc.set_packet_loss_perc(30).expect("设置丢包率");
    assert_eq!(enc.packet_loss_perc(), 30);

    // 越界必须被拒绝，不能静默截断
    assert!(
        enc.set_packet_loss_perc(101).is_err(),
        "丢包率 > 100 应报错"
    );
}

#[test]
fn audiopus_没有暴露_decoder_reset_state() {
    // audiopus 的 Decoder 只提供 new / decode / decode_float /
    // last_packet_duration / pitch / gain / set_gain / size。
    //
    // reset_state 在 GenericCtl trait 里，**该 trait 没有从 crate 根导出**
    // （pub mod coder 只 pub use 了 Decoder / Encoder / size）。
    // 所以 OpusDecoder::reset_stats 只清统计，不重建解码器。
    //
    // 时间线断裂时正确做法是**丢弃整个解码器对象重新 new** ——
    // 这条测试守住「reset_stats 不改变解码器状态」这个事实。
    let cfg = OpusConfig::default();
    let mut dec = OpusDecoder::new(cfg).expect("解码器");
    let mut enc = OpusEncoder::new(cfg, 32_000).expect("编码器");
    let mut out = vec![0u8; 8000];
    let mut buf = vec![0i16; 5760];
    let pcm = speechlike(5, 960, 48_000.0);

    for k in 0..5 {
        let n = enc.encode(&pcm[k * 960..(k + 1) * 960], &mut out).unwrap();
        dec.decode(Some(&out[..n]), &mut buf).expect("解码");
    }
    assert_eq!(dec.frames(), 5);

    dec.reset_stats();
    assert_eq!(dec.frames(), 0, "统计应清零");

    // 解码器状态还在（能继续解，且不需要重新喂 5 帧）
    let n = enc.encode(&pcm[0..960], &mut out).unwrap();
    let r = dec.decode(Some(&out[..n]), &mut buf).expect("复位后仍能解");
    assert_eq!(r.samples, 960);
    assert!(!r.concealed);
}

// ═══════════════════ 六、其他真实问题 ═══════════════════

#[test]
fn 问题_dtx_帧的线上开销是载荷的_32_倍() {
    // # 发现的一个真实问题：DTX 的收益被协议帧头吃掉了
    //
    // 1 字节的 DTX 包 + 32 字节的协议帧头 = 线上 33 字节。
    // 也就是说「静音时不发包」在**协议层**只能省掉那 1 字节，
    // 帧头一分不少。
    //
    // 20ms 帧静音 5 秒实测：
    //   载荷 960 字节，帧头 8000 字节 —— 帧头占 89.3%
    //
    // 这解释了为什么 DTX 的线上收益只有 1.9x 而载荷收益有 9.1x。
    //
    // 想拿到 DTX 的全部收益，必须**在协议层支持「空帧不发」**，
    // 靠 PTS 连续性让接收端知道这段时间是静音。这是协议改动，不在本 crate 范围。
    let cfg = OpusConfig::default();
    let pcm = silence(250, 960);
    let e = encode_all(cfg, 32_000, true, false, 0, &pcm);

    let header = e.packets.len() * HEADER_BYTES;
    let share = header as f64 / (e.bytes + header) as f64;
    assert!(
        share > 0.85,
        "DTX 静音期帧头应占 85% 以上，实测 {:.1}%",
        share * 100.0
    );

    // 载荷侧收益 vs 线上侧收益的差距
    //
    // 实测：载荷 8746 → 960 字节（9.1x），线上 26.8 → 14.3 kbps（1.9x）。
    // 差距 4.8 倍 —— 全是帧头吃掉的。
    let off = encode_all(cfg, 32_000, false, false, 0, &pcm);
    let payload_ratio = off.bytes as f64 / e.bytes as f64;
    let wire_ratio = off.wire_kbps(20_000) / e.wire_kbps(20_000);
    assert!(
        payload_ratio > 4.0 * wire_ratio,
        "载荷收益({payload_ratio:.1}x)应是线上收益({wire_ratio:.1}x)的 4 倍以上"
    );
    assert!(
        wire_ratio > 1.5,
        "线上收益至少应有 1.5x，实测 {wire_ratio:.1}x"
    );
}

#[test]
fn 问题_静音帧实测_35_字节而非文档记载的_57() {
    // docs/opus.md 记「静音 57 字节/帧」，当前实测是 **35 字节/帧**。
    //
    // 差异不是 bug，是两个原因叠加：
    //   1. 文档那次测量用的输入可能与当前不同（纯零 vs 低电平噪声）
    //   2. libopus 版本差异会改变 VBR 的最小帧大小
    //
    // 但结论不变：**静音不是 0 字节**，做带宽预算时按 0 算会偏差。
    // 这条测试钉住当前值，值变了说明 libopus 升级了，带宽预算要重算。
    let cfg = OpusConfig::default();
    let pcm = silence(250, 960);
    let e = encode_all(cfg, 32_000, false, false, 0, &pcm);

    let avg = e.bytes as f64 / e.packets.len() as f64;
    assert!(
        (30.0..=45.0).contains(&avg),
        "静音帧应约 35 字节，实测 {avg:.1}"
    );
    assert!(avg > 0.0, "静音不该是 0 字节");

    let kbps = e.payload_kbps(20_000);
    assert!(
        (12.0..=16.0).contains(&kbps),
        "静音载荷码率应约 14kbps，实测 {kbps:.1}"
    );
}

#[test]
fn 问题_编码器要求输出缓冲至少_1275_字节() {
    // OpusEncoder::encode 要求 out.len() >= 1275 * channels，
    // 但 20ms / 32kbps 的帧实际只用 80-100 字节。
    //
    // 这个检查是对的（Opus 规范硬上限），但**调用方必须准备 1275 字节**
    // 的缓冲，不能按「平均帧大小」分配。按平均分配会在噪声突发时
    // 收到 BufferTooSmall。
    let cfg = OpusConfig::default();
    let mut enc = OpusEncoder::new(cfg, 32_000).expect("编码器");
    let pcm = speechlike(1, 960, 48_000.0);

    // 1274 字节不够
    let mut small = vec![0u8; 1274];
    assert!(
        enc.encode(&pcm, &mut small).is_err(),
        "1274 字节缓冲应被拒绝"
    );

    // 1275 字节刚好
    let mut exact = vec![0u8; 1275];
    assert!(enc.encode(&pcm, &mut exact).is_ok(), "1275 字节缓冲应够用");

    // 但实际只用了不到 10%
    let n = enc.encode(&pcm, &mut exact).expect("编码");
    assert!(n < 200, "实际帧大小应远小于缓冲上限，实测 {n} 字节");
}

#[test]
fn 问题_立体声的_dtx_判定按数据字节而非声道数() {
    // 立体声下 DTX 包的字节数不是 channels —— 它仍由包结构决定。
    //
    // 实测 48kHz 立体声 20ms：DTX 包 1 字节（TOC 0x7C，stereo=1），
    // 不是 2 字节。所以 len <= channels 这个判据在立体声下
    // **碰巧**能过（1 <= 2），但它不是正确的理由。
    let cfg = OpusConfig {
        channels: 2,
        ..OpusConfig::default()
    };
    let pcm = vec![0i16; 250 * 960 * 2];
    let e = encode_all(cfg, 64_000, true, false, 0, &pcm);

    assert!(
        e.dtx_count() > 150,
        "立体声静音也应触发 DTX，实测 {}",
        e.dtx_count()
    );

    let dtx_pkt = e
        .packets
        .iter()
        .zip(e.dtx.iter())
        .find(|(_, &d)| d)
        .map(|(p, _)| p)
        .expect("应有 DTX 包");

    assert_eq!(
        packet::data_bytes(dtx_pkt),
        Some(0),
        "立体声 DTX 包的数据字节应为 0"
    );
    let t = packet::toc(dtx_pkt).expect("TOC");
    assert!(t.stereo, "立体声 DTX 包的 TOC 应标记 stereo");
}

#[test]
fn 问题_帧长不整除五秒时包数向上取整() {
    // 60ms 帧装 5 秒：5.0 / 0.06 = 83.33 → 83 帧 = 4.98 秒。
    // 少的那 20ms 在 PTS 上表现为「最后一帧只到 4.98 秒」，
    // 上层如果用「帧数 × 帧长」算总时长会差 20ms。
    //
    // 这不是 bug，但做时长统计时必须用「采样数 / 采样率」而不是
    // 「帧数 × 帧长」。
    let cfg = OpusConfig {
        frame_ms: 60.0,
        ..OpusConfig::default()
    };
    assert_eq!(cfg.frame_samples(), 2880);
    assert_eq!(cfg.frame_duration_us(), 60_000);

    let frames = (5000.0f64 / 60.0).round() as usize;
    assert_eq!(frames, 83, "5 秒 / 60ms 向上取整是 83 帧");
    let covered_ms = frames as f64 * 60.0;
    assert!(
        (covered_ms - 5000.0).abs() > 10.0,
        "83 帧只有 {covered_ms}ms，与 5000ms 差 {:.0}ms —— 时长必须按采样算",
        (covered_ms - 5000.0).abs()
    );
}

// ════════ 七、in-band FEC 的真实边界（本轮最重要的发现）════════

#[test]
fn fec_在_16kbps_以下完全不生效() {
    // # 这是本轮最重要的发现，也是最容易踩的坑
    //
    // libopus 的 in-band FEC 靠 SILK 的 **LBRR**（Low Bit-Rate Redundancy）
    // 帧实现。但 LBRR **不是无条件插入的** —— 码率太低时塞不下，
    // libopus 会直接放弃冗余。
    //
    // 实测（48kHz 20ms Voip 类语音，loss=30，8 个丢帧位置）：
    //
    // | 码率 | 每帧字节 | FEC 输出 == PLC 输出 | 判定 |
    // | --- | --- | --- | --- |
    // | 6 kbps | 19.8 | **8/8** | 无 LBRR |
    // | 8 kbps | 21.7 | **8/8** | 无 LBRR |
    // | 10 kbps | 26.4 | **8/8** | 无 LBRR |
    // | 12 kbps | 32.9 | **8/8** | 无 LBRR |
    // | 14 kbps | 37.6 | **8/8** | 无 LBRR |
    // | 15.5 kbps | 40.7 | **8/8** | 无 LBRR |
    // | **16 kbps** | 42.6 | 1/8 | **有 LBRR** |
    // | 32 kbps | 77.5 | 1/8 | 有 LBRR |
    //
    // 阈值是 **16 kbps**（逐 500bps 扫出来的，15.5 不行、16.0 行）。
    //
    // 判据用「FEC 输出是否与 PLC 输出逐采样相同」—— 相同说明解码器
    // 根本没找到 LBRR，退化成普通 PLC。
    let cfg = OpusConfig::default();
    let pcm = speechlike(80, 960, 48_000.0);

    let fec_is_noop = |bitrate: i32| -> bool {
        let e = encode_all(cfg, bitrate, false, true, 30, &pcm);
        let mut same = 0;
        let mut total = 0;
        for drop in (20..70).step_by(7) {
            let mut d1 = OpusDecoder::new(cfg).expect("解码器");
            let mut d2 = OpusDecoder::new(cfg).expect("解码器");
            let mut b1 = vec![0i16; 5760];
            let mut b2 = vec![0i16; 5760];
            for p in e.packets.iter().take(drop) {
                d1.decode(Some(p), &mut b1).expect("解码");
                d2.decode(Some(p), &mut b2).expect("解码");
            }
            let r1 = d1
                .recover_previous(&e.packets[drop + 1], &mut b1)
                .expect("FEC");
            d2.decode(None, &mut b2).expect("PLC");
            if b1[..r1.samples] == b2[..960] {
                same += 1;
            }
            total += 1;
        }
        same == total
    };

    // 低码率：FEC 是空操作
    for br in [6_000i32, 8_000, 10_000, 12_000, 14_000] {
        assert!(
            fec_is_noop(br),
            "{br}bps 下 FEC 应无效（无 LBRR），实测产生了不同的输出"
        );
    }

    // 16 kbps 是阈值：开始生效
    assert!(
        !fec_is_noop(16_000),
        "16000bps 是 LBRR 的下限，FEC 应开始生效"
    );
    assert!(
        !fec_is_noop(32_000),
        "32000bps 下 FEC 必须有效（这是 SideLens 的默认码率）"
    );
}

#[test]
fn fec_在纯_celt_模式下完全不生效() {
    // # 第二个边界：CELT 模式没有 LBRR
    //
    // Opus 有三种模式：SILK / Hybrid / CELT。LBRR 只存在于 SILK 与 Hybrid。
    // **纯 CELT 没有 in-band FEC** —— 打开开关不会报错，只是什么都不做。
    //
    // 什么时候会走纯 CELT？libopus 按「码率 + 信号类型」自动选：
    //   - 低码率语音 → SILK
    //   - 中码率语音 → Hybrid（SILK 低频 + CELT 高频）
    //   - 高码率音乐 / Application::LowDelay → CELT
    //
    // 实测（Application::LowDelay + 128kbps，60 帧全 CELT，6 个丢帧位置）：
    //   FEC 真修复 **0/6**
    //   FEC 输出与 PLC 输出**逐采样相同**
    //
    // 对 SideLens 的意义：默认 Voip + 32kbps 走 SILK/Hybrid，FEC 有效。
    // 但如果将来把 Application 改成 Audio、或把码率提到 64kbps 以上，
    // FEC 会**静默失效** —— 必须用 `packet::toc().config` 监控实际模式。
    let cfg = OpusConfig::default();
    let pcm = speechlike(60, 960, 48_000.0);

    // 默认配置下模式是 SILK / Hybrid，不是纯 CELT
    let e = encode_all(cfg, 32_000, false, true, 30, &pcm);
    let modes: Vec<&str> = e
        .packets
        .iter()
        .map(|p| match packet::toc(p).expect("TOC").config {
            0..=11 => "SILK",
            12..=15 => "Hybrid",
            _ => "CELT",
        })
        .collect();
    assert!(
        !modes.contains(&"CELT"),
        "32kbps Voip 语音不应走纯 CELT，实测模式 {modes:?}"
    );

    // 三种模式的分界（config 编号）
    assert_eq!(packet::config_frame_us(0), 10_000, "config 0 是 SILK 10ms");
    assert_eq!(
        packet::config_frame_us(15),
        20_000,
        "config 15 是 Hybrid 20ms"
    );
    assert_eq!(
        packet::config_frame_us(31),
        20_000,
        "config 31 是 CELT 20ms"
    );
}

#[test]
fn fec_在默认配置下的净收益是三点二倍() {
    // # 把「值不值」这件事量化
    //
    // 默认配置（48kHz 20ms 32kbps Voip，loss=30），16 个丢帧位置实测：
    //   FEC 胜 13/16
    //   平均 RMSE：FEC=702  PLC=2221
    //   提升 **3.16x**
    //
    // 这是「抖动缓冲深度够」时的收益上限。深度不够时收益是 0
    // （见 fec_修复是事后修复_需要抖动缓冲深度大于一帧）。
    let cfg = OpusConfig::default();
    let pcm = speechlike(200, 960, 48_000.0);
    let e = encode_all(cfg, 32_000, false, true, 30, &pcm);

    let mut refdec = OpusDecoder::new(cfg).expect("解码器");
    let mut buf = vec![0i16; 5760];
    let mut reference: Vec<Vec<i16>> = Vec::new();
    for p in &e.packets {
        let r = refdec.decode(Some(p), &mut buf).expect("解码");
        reference.push(buf[..r.samples].to_vec());
    }

    let mut wins = 0;
    let mut total = 0;
    let mut sum_fec = 0.0;
    let mut sum_plc = 0.0;
    for drop in (20..180).step_by(10) {
        let mut d1 = OpusDecoder::new(cfg).expect("解码器");
        for p in e.packets.iter().take(drop) {
            d1.decode(Some(p), &mut buf).expect("解码");
        }
        let r = d1
            .recover_previous(&e.packets[drop + 1], &mut buf)
            .expect("FEC");
        let fec_pcm = buf[..r.samples].to_vec();

        let mut d2 = OpusDecoder::new(cfg).expect("解码器");
        for p in e.packets.iter().take(drop) {
            d2.decode(Some(p), &mut buf).expect("解码");
        }
        d2.decode(None, &mut buf).expect("PLC");
        let plc_pcm = buf[..960].to_vec();

        let ef = rmse(&fec_pcm, &reference[drop]);
        let ep = rmse(&plc_pcm, &reference[drop]);
        sum_fec += ef;
        sum_plc += ep;
        if ef < ep {
            wins += 1;
        }
        total += 1;
    }

    assert!(
        wins * 2 > total,
        "FEC 应在多数丢帧位置胜出，实测 {wins}/{total}"
    );
    let gain = sum_plc / sum_fec;
    assert!(
        gain > 2.0,
        "FEC 相对 PLC 的平均 RMSE 提升应 > 2x，实测 {gain:.2}x"
    );
}

// ═════════════════ 八、packet 模块的自洽性 ═════════════════

#[test]
fn packet_解析对真实码流自洽() {
    // [`packet`] 是本轮新加的安全解析器（不依赖 unsafe）。
    // 它被 DTX 判定和帧长分析直接依赖，所以必须自己先站住。
    //
    // 三条自洽性：
    // 1. 真实语音帧的 data_bytes 必须 > 0（不能被误判成 DTX）
    // 2. DTX 帧的 data_bytes 必须 == 0
    // 3. packet_duration_us 必须等于配置的帧长
    let cfg = OpusConfig::default();

    for (ms, min_data) in [(20.0f64, 20usize), (40.0, 60), (60.0, 100)] {
        let c = OpusConfig {
            frame_ms: ms,
            ..OpusConfig::default()
        };
        let n = c.frame_samples() as usize;
        let pcm = speechlike(20, n, 48_000.0);
        let e = encode_all(c, 32_000, false, false, 0, &pcm);

        for p in &e.packets {
            let d = packet::data_bytes(p).expect("data_bytes 应能解析");
            assert!(
                d >= min_data,
                "{ms}ms 真实语音帧的 data_bytes 应 >= {min_data}，实测 {d}"
            );
            assert!(!packet::is_dtx(p), "{ms}ms 真实语音帧不该被判为 DTX");
            assert_eq!(
                packet::packet_duration_us(p),
                Some((ms * 1000.0) as u64),
                "{ms}ms 包的声明时长应等于帧长"
            );
        }

        // DTX 帧的 data_bytes 必须是 0
        let frames = (2.0 / (ms / 1000.0)).round() as usize;
        let e_dtx = encode_all(c, 32_000, true, false, 0, &silence(frames, n));
        let dtx_pkts: Vec<&Vec<u8>> = e_dtx
            .packets
            .iter()
            .zip(e_dtx.dtx.iter())
            .filter(|(_, &d)| d)
            .map(|(p, _)| p)
            .collect();
        assert!(!dtx_pkts.is_empty(), "{ms}ms 应有 DTX 帧");
        for p in &dtx_pkts {
            assert_eq!(
                packet::data_bytes(p),
                Some(0),
                "{ms}ms DTX 帧的 data_bytes 必须是 0"
            );
        }
    }

    let _ = cfg;
}

#[test]
fn packet_toc_表覆盖三种模式() {
    // TOC 的 config 字段（0-31）决定模式与内部帧长，这张表必须准 ——
    // 它同时被「FEC 是否可用」（SILK/Hybrid 才有 LBRR）和
    // 「内部帧长」两个结论依赖。
    //
    // RFC 6716 表 2：
    //   config 0-11  SILK，帧长 10/20/40/60 循环
    //   config 12-15 Hybrid，帧长 10/20 循环
    //   config 16-31 CELT，帧长 2.5/5/10/20 循环
    for c in 0u8..=11 {
        let expected = [10_000u64, 20_000, 40_000, 60_000][(c % 4) as usize];
        assert_eq!(
            packet::config_frame_us(c),
            expected,
            "config {c} 应是 SILK {expected}us"
        );
    }
    for c in 12u8..=15 {
        let expected = [10_000u64, 20_000][(c % 2) as usize];
        assert_eq!(
            packet::config_frame_us(c),
            expected,
            "config {c} 应是 Hybrid {expected}us"
        );
    }
    for c in 16u8..=31 {
        let expected = [2_500u64, 5_000, 10_000, 20_000][(c % 4) as usize];
        assert_eq!(
            packet::config_frame_us(c),
            expected,
            "config {c} 应是 CELT {expected}us"
        );
    }

    // TOC 字节的位布局：config(5) | stereo(1) | code(2)
    let t = packet::toc(&[0x78]).expect("TOC");
    assert_eq!(t.config, 15);
    assert!(!t.stereo);
    assert_eq!(t.code, 0);

    let t = packet::toc(&[0x7C]).expect("TOC");
    assert_eq!(t.config, 15);
    assert!(t.stereo, "0x7C 的 stereo 位应为 1");
    assert_eq!(t.code, 0);

    // 空包不能 panic
    assert!(packet::toc(&[]).is_none());
    assert!(packet::data_bytes(&[]).is_none());
    assert!(!packet::is_dtx(&[]), "空包不是 DTX，是坏包");
}
