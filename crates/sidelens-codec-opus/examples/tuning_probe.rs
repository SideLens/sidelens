//! Opus 参数诊断：DTX / in-band FEC / 帧长对比
//!
//! 这个程序**只打印数字**，不做断言 —— 所有结论都由
//! `tests/tuning.rs` 固化。它的用途是「libopus 升级后重跑一遍看数字变了没」。
//!
//! 跑法：
//!
//! `text
//! cargo run -p sidelens-codec-opus --example tuning_probe
//! `

use sidelens_codec::audio::{AudioFrame, AudioJitterBuffer, OpusConfig, PushOutcome};
use sidelens_codec_opus::packet;
use sidelens_codec_opus::{OpusDecoder, OpusEncoder};

/// 协议帧头长度（`sidelens-proto` 的 `HEADER_SIZE`）
const HEADER: usize = 32;

fn speechlike(frames: usize, n: usize, rate: f64) -> Vec<i16> {
    let mut v = Vec::with_capacity(frames * n);
    for i in 0..frames * n {
        let t = i as f64 / rate;
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

struct Seq {
    pkts: Vec<Vec<u8>>,
    dtx: Vec<bool>,
    bytes: usize,
}

impl Seq {
    fn seconds(&self, frame_us: u64) -> f64 {
        self.pkts.len() as f64 * frame_us as f64 / 1_000_000.0
    }
    fn wire_kbps(&self, frame_us: u64) -> f64 {
        (self.bytes + self.pkts.len() * HEADER) as f64 * 8.0 / self.seconds(frame_us) / 1000.0
    }
    fn payload_kbps(&self, frame_us: u64) -> f64 {
        self.bytes as f64 * 8.0 / self.seconds(frame_us) / 1000.0
    }
    fn dtx_count(&self) -> usize {
        self.dtx.iter().filter(|&&d| d).count()
    }
}

fn encode(cfg: OpusConfig, br: i32, dtx: bool, fec: bool, loss: u8, pcm: &[i16]) -> Seq {
    let mut enc = OpusEncoder::new(cfg, br).expect("编码器");
    enc.set_dtx(dtx).expect("DTX");
    enc.set_inband_fec(fec).expect("FEC");
    enc.set_packet_loss_perc(loss).expect("丢包率");
    let n = cfg.frame_samples() as usize * cfg.channels as usize;
    let mut out = vec![0u8; 8000];
    let mut s = Seq {
        pkts: Vec::new(),
        dtx: Vec::new(),
        bytes: 0,
    };
    for chunk in pcm.chunks_exact(n) {
        let e = enc.encode_frame(chunk, &mut out).expect("编码");
        s.pkts.push(out[..e.bytes].to_vec());
        s.dtx.push(e.dtx);
        s.bytes += e.bytes;
    }
    s
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

fn section(title: &str) {
    println!("\n== {title} ==");
}

fn main() {
    let cfg = OpusConfig::default();

    // ─────────────────────────── DTX ───────────────────────────
    section("1. DTX：静音 5 秒（250 帧 @20ms 32kbps Voip）");
    let sil = vec![0i16; 250 * 960];
    for dtx in [false, true] {
        let s = encode(cfg, 32_000, dtx, false, 0, &sil);
        println!(
            "  dtx={dtx:<5} 载荷={:<6}B 每帧={:.1}B DTX帧={:<4} 载荷={:.1}kbps 线上={:.1}kbps",
            s.bytes,
            s.bytes as f64 / s.pkts.len() as f64,
            s.dtx_count(),
            s.payload_kbps(20_000),
            s.wire_kbps(20_000)
        );
    }

    section("2. DTX 刷新间隔（MAX_CONSECUTIVE_DTX）");
    for ms in [20.0f64, 40.0, 60.0] {
        let c = OpusConfig {
            frame_ms: ms,
            ..cfg
        };
        let n = c.frame_samples() as usize;
        let frames = (20.0 / (ms / 1000.0)).round() as usize;
        let s = encode(c, 32_000, true, false, 0, &vec![0i16; frames * n]);
        let refresh: Vec<usize> = (0..s.pkts.len()).filter(|&i| !s.dtx[i]).collect();
        let mut gaps: Vec<usize> = refresh.windows(2).map(|w| w[1] - w[0]).collect();
        gaps.sort_unstable();
        let max_gap = *gaps.last().unwrap_or(&0);
        println!(
            "  {ms}ms 刷新帧={} 个，最大间隔={max_gap} 帧 = {}ms（Discontinuity 阈值 500ms）",
            refresh.len(),
            max_gap as f64 * ms
        );
    }

    section("3. DTX 与抖动缓冲：PTS 连续但帧数变少");
    {
        let s = encode(cfg, 32_000, true, false, 0, &vec![0i16; 300 * 960]);
        for (label, skip_dtx) in [("每帧都发(含 1B DTX)", false), ("不发 DTX 帧", true)] {
            let mut jb = AudioJitterBuffer::new(cfg, 40.0).expect("抖动缓冲");
            let mut counts: std::collections::BTreeMap<String, usize> =
                std::collections::BTreeMap::new();
            let mut pts = 0u64;
            for (i, p) in s.pkts.iter().enumerate() {
                if !(skip_dtx && s.dtx[i]) {
                    let o = jb.push(AudioFrame::new(pts, 20_000, i as u32, p.clone()));
                    *counts.entry(format!("{o}")).or_default() += 1;
                }
                pts += 20_000;
            }
            println!("  {label}: {counts:?}");
        }
        let mut jb = AudioJitterBuffer::new(cfg, 40.0).expect("抖动缓冲");
        let a = jb.push(AudioFrame::new(0, 20_000, 0, vec![1; 60]));
        let b = jb.push(AudioFrame::new(501_000, 20_000, 1, vec![1; 60]));
        println!("  对照：PTS 跳 501ms → {a} → {b}（这才是真 Discontinuity）");
    }

    // ────────────────────────── in-band FEC ──────────────────────────
    section("4. in-band FEC：码流增量");
    let sp = speechlike(250, 960, 48_000.0);
    let base = encode(cfg, 32_000, false, false, 0, &sp);
    for loss in [0u8, 10, 20, 30, 50] {
        let f = encode(cfg, 32_000, false, true, loss, &sp);
        println!(
            "  loss={loss:<3} 无FEC={:<6} 有FEC={:<6} 比值={:.3} 每帧增量={:+.1}B",
            base.bytes,
            f.bytes,
            f.bytes as f64 / base.bytes as f64,
            (f.bytes as f64 - base.bytes as f64) / base.pkts.len() as f64
        );
    }

    section("5. in-band FEC：正常解码的音质代价");
    {
        let pcm = speechlike(300, 960, 48_000.0);
        let la = 312usize;
        for (fec, loss) in [(false, 0u8), (true, 0), (true, 10), (true, 30)] {
            let s = encode(cfg, 32_000, false, fec, loss, &pcm);
            let mut dec = OpusDecoder::new(cfg).expect("解码器");
            let mut buf = vec![0i16; 5760];
            let mut decoded: Vec<i16> = Vec::new();
            for p in &s.pkts {
                let r = dec.decode(Some(p), &mut buf).expect("解码");
                decoded.extend_from_slice(&buf[..r.samples]);
            }
            let n = pcm.len().min(decoded.len());
            let e = rmse(&decoded[la..n], &pcm[..n - la]);
            println!(
                "  fec={fec:<5} loss={loss:<3} RMSE={:.0} 相对={:.2}%",
                e,
                e / rms(&pcm) * 100.0
            );
        }
    }

    section("6. in-band FEC：丢帧修复 vs PLC（多个位置）");
    {
        let pcm = speechlike(200, 960, 48_000.0);
        let s = encode(cfg, 32_000, false, true, 30, &pcm);
        let mut refdec = OpusDecoder::new(cfg).expect("解码器");
        let mut buf = vec![0i16; 5760];
        let mut reference: Vec<Vec<i16>> = Vec::new();
        for p in &s.pkts {
            let r = refdec.decode(Some(p), &mut buf).expect("解码");
            reference.push(buf[..r.samples].to_vec());
        }
        let (mut wins, mut n, mut sf, mut sp2) = (0, 0, 0.0, 0.0);
        for drop in (20..180).step_by(20) {
            let mut d1 = OpusDecoder::new(cfg).expect("解码器");
            for p in s.pkts.iter().take(drop) {
                d1.decode(Some(p), &mut buf).expect("解码");
            }
            let r = d1
                .recover_previous(&s.pkts[drop + 1], &mut buf)
                .expect("FEC");
            let fec_pcm = buf[..r.samples].to_vec();
            let mut d2 = OpusDecoder::new(cfg).expect("解码器");
            for p in s.pkts.iter().take(drop) {
                d2.decode(Some(p), &mut buf).expect("解码");
            }
            d2.decode(None, &mut buf).expect("PLC");
            let plc_pcm = buf[..960].to_vec();
            let ef = rmse(&fec_pcm, &reference[drop]);
            let ep = rmse(&plc_pcm, &reference[drop]);
            sf += ef;
            sp2 += ep;
            n += 1;
            if ef < ep {
                wins += 1;
            }
            println!(
                "  丢第 {drop:>3} 帧 真值RMS={:>5.0} FEC RMSE={:>6.0} PLC RMSE={:>6.0} {}",
                rms(&reference[drop]),
                ef,
                ep,
                if ef < ep { "FEC 胜" } else { "PLC 胜" }
            );
        }
        println!(
            "  FEC 胜 {wins}/{n}，平均 RMSE：FEC={:.0} PLC={:.0}",
            sf / n as f64,
            sp2 / n as f64
        );
    }

    // ─────────────────────────── 帧长 ───────────────────────────
    section("7. 帧长对比：包结构（TOC）");
    for ms in [20.0f64, 40.0, 60.0] {
        let c = OpusConfig {
            frame_ms: ms,
            ..cfg
        };
        let n = c.frame_samples() as usize;
        let s = encode(c, 32_000, false, false, 0, &speechlike(4, n, 48_000.0));
        let p = &s.pkts[2];
        let t = packet::toc(p).expect("TOC");
        println!(
            "  {ms}ms: {:.1}B TOC=0x{:02X} config={} code={} 内部帧={} × {}us",
            p.len() as f64,
            p[0],
            t.config,
            t.code,
            packet::frame_count(p).unwrap_or(0),
            packet::config_frame_us(t.config)
        );
    }

    section("8. 帧长对比：帧头开销与线上码率（5 秒类语音）");
    for ms in [20.0f64, 40.0, 60.0] {
        let c = OpusConfig {
            frame_ms: ms,
            ..cfg
        };
        let n = c.frame_samples() as usize;
        let frames = (5.0 / (ms / 1000.0)).round() as usize;
        let s = encode(c, 32_000, false, false, 0, &speechlike(frames, n, 48_000.0));
        let dur = (ms * 1000.0) as u64;
        let hdr = s.pkts.len() * HEADER;
        println!(
            "  {ms}ms 包数={:<4} 载荷={:<6} 帧头={:<5} 载荷={:.1}kbps 线上={:.1}kbps 帧头占比={:.1}%",
            s.pkts.len(),
            s.bytes,
            hdr,
            s.payload_kbps(dur),
            s.wire_kbps(dur),
            hdr as f64 / (s.bytes + hdr) as f64 * 100.0
        );
    }

    section("9. 帧长 × DTX：静音 5 秒");
    for ms in [20.0f64, 40.0, 60.0] {
        let c = OpusConfig {
            frame_ms: ms,
            ..cfg
        };
        let n = c.frame_samples() as usize;
        let frames = (5.0 / (ms / 1000.0)).round() as usize;
        let dur = (ms * 1000.0) as u64;
        for dtx in [false, true] {
            let s = encode(c, 32_000, dtx, false, 0, &vec![0i16; frames * n]);
            println!(
                "  {ms}ms dtx={dtx:<5} 包数={:<4} 载荷={:<5} DTX帧={:<4} 线上={:.1}kbps",
                s.pkts.len(),
                s.bytes,
                s.dtx_count(),
                s.wire_kbps(dur)
            );
        }
    }

    section("10. 帧长：PLC 外推长度与 lookahead");
    for ms in [2.5f64, 20.0, 40.0, 60.0] {
        let c = OpusConfig {
            frame_ms: ms,
            ..cfg
        };
        let n = c.frame_samples() as usize;
        let s = encode(c, 32_000, false, false, 0, &speechlike(20, n, 48_000.0));
        let mut dec = OpusDecoder::new(c).expect("解码器");
        let mut buf = vec![0i16; 5760];
        for p in s.pkts.iter().take(10) {
            dec.decode(Some(p), &mut buf).expect("解码");
        }
        let r = dec.decode(None, &mut buf).expect("PLC");
        let enc = OpusEncoder::new(c, 32_000).expect("编码器");
        println!(
            "  {ms}ms PLC 补 {} 采样 = {:.0}ms，lookahead={} 采样",
            r.samples,
            r.samples as f64 / 48.0,
            enc.lookahead_samples()
        );
    }

    // ─────────────────────────── 其他 ───────────────────────────
    section("11. 编码耗时（debug 构建，仅供参考）");
    for ms in [20.0f64, 40.0, 60.0] {
        let c = OpusConfig {
            frame_ms: ms,
            ..cfg
        };
        let n = c.frame_samples() as usize;
        let pcm = speechlike(100, n, 48_000.0);
        let mut enc = OpusEncoder::new(c, 32_000).expect("编码器");
        let mut out = vec![0u8; 8000];
        let t0 = std::time::Instant::now();
        for chunk in pcm.chunks_exact(n) {
            enc.encode(chunk, &mut out).expect("编码");
        }
        let us = t0.elapsed().as_micros() as f64 / 100.0;
        println!(
            "  {ms}ms: {us:.0}us/帧（预算 {:.0}us）占用 {:.2}%",
            ms * 1000.0,
            us / (ms * 1000.0) * 100.0
        );
    }

    let _ = PushOutcome::Ok;
}
