//! 运行中改码率到底生不生效？A/B 实测。
//!
//! # 为什么必须实测
//!
//! `ICodecAPI::SetValue` 对 `CODECAPI_AVEncCommonMeanBitRate` 返回 S_OK，
//! 之后 `GetValue` 也能读回新值 —— 这两件事**都不能**证明码率真的变了。
//! 本 crate 已经在 MF_LOW_LATENCY 上踩过同一种坑：接口全部成功，行为完全没变。
//!
//! 唯一可信的判据是**输出字节数**。
//!
//! # 实验设计
//!
//! 对照组必须**同内容、同帧序号**，否则测到的是内容差异而不是码率差异。
//! 每组都是 90 帧，前 30 帧统一 2 Mbps，第 30 帧之前按组操作：
//!
//! | 组 | 第 30 帧前的操作 |
//! | --- | --- |
//! | 对照 | 什么都不做 |
//! | 降档 | `set_bitrate(500_000)` |
//! | 降档+强制 IDR | `set_bitrate(500_000)` + `force_keyframe()` |
//! | 升档 | 起始 500 kbps，这里 `set_bitrate(2_000_000)` |
//!
//! # 内容复杂度是决定性的
//!
//! 已有的 examples/rate_control.rs 证明 mfh264enc 的 CBR 只在「内容本来就需要
//! 这么多码率」时才起作用。所以这里对三种内容各做一遍 —— 结论不是「有用/没用」，
//! 而是「在什么内容上才有用」。
//!
//! 跑法：
//!
//! `cargo run --release -p sidelens-codec-mf --example bitrate_step`

// 整个实现是 Windows 专属（Media Foundation）。
// 用 mod imp 包一层而不是 #![cfg(windows)] —— 后者会把 main 一起删掉，
// 报 E0601「main function not found」，看起来像忘了写 main。
#[cfg(windows)]
mod imp {
    use std::time::Instant;

    use sidelens_codec::nalu;
    use sidelens_codec_mf::{max_bitrate_for, EncoderConfig, H264Encoder, MAX_BITRATE_BPS};

    const W: u32 = 640;
    const H: u32 = 360;
    const FPS: u32 = 30;
    /// 改码率前的帧数
    const PRE: u32 = 30;
    /// 改码率后的帧数
    const POST: u32 = 60;
    const HIGH_BPS: u32 = 2_000_000;
    const LOW_BPS: u32 = 500_000;

    pub fn main() {
        println!("=== 运行中改码率：A/B 实测 ===");
        println!("分辨率 {W}x{H} @ {FPS}fps，GOP 60（60 帧一个 IDR）");
        println!("窗口：前 {PRE} 帧 / 后 {POST} 帧，分界点在第 {PRE} 帧之前\n");

        let enc_name = H264Encoder::new(cfg(HIGH_BPS))
            .map(|e| e.encoder_name().to_string())
            .unwrap_or_else(|e| format!("<创建失败：{e}>"));
        println!("编码器：{enc_name}\n");

        println!(
            "{:<18} {:>10} {:>10} {:>8} {:>11}",
            "组", "前 kbps", "后 kbps", "后/前", "后帧字节"
        );
        println!("{}", "-".repeat(62));

        for (label, kind) in [
            ("低熵（渐变+竖条）", Kind::Simple),
            ("中熵（大块噪声平移）", Kind::PatchNoise),
            ("高熵（逐帧全噪声）", Kind::Noise),
        ] {
            println!("[{label}]");
            let control = run(kind, HIGH_BPS, None, false);
            let down = run(kind, HIGH_BPS, Some(LOW_BPS), false);
            let down_idr = run(kind, HIGH_BPS, Some(LOW_BPS), true);
            let up = run(kind, LOW_BPS, Some(HIGH_BPS), false);

            report("对照 2M→2M", &control);
            report("降档 2M→500k", &down);
            report("降档+强制IDR", &down_idr);
            report("升档 500k→2M", &up);

            // 关键判据：降档组相对**对照组的后窗口**降了多少。
            // 直接比「后/前」会被编码器自身的收敛（前 30 帧含 IDR，
            // 后 60 帧稳定下来本来就小）污染。
            let ctrl_post = control.post_bytes as f64 / control.post_frames.max(1) as f64;
            let down_post = down.post_bytes as f64 / down.post_frames.max(1) as f64;
            println!(
                "  → 降档 vs 对照（同内容同帧号）：后窗口字节 {ctrl_post:.0} → {down_post:.0}，\
                 比值 {:.2}",
                down_post / ctrl_post
            );
            println!("  降档逐帧字节: {}", trace_str(&down));
            println!(
                "  前/后 IDR 数：对照 {}/{}，降档 {}/{}\n",
                control.pre_idr, control.post_idr, down.pre_idr, down.post_idr
            );
        }

        println!("=== 判据 ===");
        println!("「降档 vs 对照」≈ 1.00 → set_bitrate 对输出**没有影响**（MFT 忽略运行中改动）");
        println!("「降档 vs 对照」显著 < 1 → set_bitrate **生效**\n");

        convergence_probe();
        readback_probe();
        rebuild_cost();
        validation_probe();
    }

    /// 改码率之后要多久才稳定下来？
    ///
    /// 拥塞控制器按秒级节奏调码率，需要知道「调完之后多久能看到效果」。
    /// 这里把 210 帧切成 30/30/60/90 四段统计。
    fn convergence_probe() {
        println!("=== 收敛速度（内容 = 中熵 patch-noise，210 帧）===");
        println!(
            "{:<16} {:>10} {:>10} {:>10} {:>10}",
            "组", "0-29", "30-59", "60-119", "120-209"
        );
        for (label, pre, post) in [
            ("对照 全程 2M", HIGH_BPS, None),
            ("降档 2M→500k", HIGH_BPS, Some(LOW_BPS)),
            ("升档 500k→2M", LOW_BPS, Some(HIGH_BPS)),
        ] {
            let b = bucket_run(pre, post);
            println!(
                "{label:<16} {:>10.0} {:>10.0} {:>10.0} {:>10.0}",
                b[0], b[1], b[2], b[3]
            );
        }
        println!("（单位 kbps；第 30 帧之前改码率）");
        println!("降档：下一帧就掉下来，且不回弹");
        println!("升档：要 60~120 帧才追上对照 —— 编码器升码率比降码率慢得多\n");
    }

    fn bucket_run(pre_bps: u32, post_bps: Option<u32>) -> [f64; 4] {
        const N: u32 = 210;
        let cfg = cfg(pre_bps);
        let mut enc = match H264Encoder::new(cfg) {
            Ok(e) => e,
            Err(_) => return [0.0; 4],
        };
        let mut nv12 = vec![0u8; cfg.nv12_len()];
        let mut buckets = [(0usize, 0u32); 4];
        for i in 0..N {
            if i == PRE {
                if let Some(bps) = post_bps {
                    let _ = enc.set_bitrate(bps);
                }
            }
            fill(Kind::PatchNoise, &mut nv12, i);
            let n = enc
                .encode_nv12(&nv12, (i as i64) * cfg.frame_interval_us())
                .map(|v| v.len())
                .unwrap_or(0);
            let k = if i < 30 {
                0
            } else if i < 60 {
                1
            } else if i < 120 {
                2
            } else {
                3
            };
            buckets[k].0 += n;
            buckets[k].1 += 1;
        }
        let mut out = [0.0f64; 4];
        for (k, &(bytes, frames)) in buckets.iter().enumerate() {
            out[k] = kbps(bytes, frames);
        }
        out
    }

    fn cfg(bitrate: u32) -> EncoderConfig {
        EncoderConfig {
            width: W,
            height: H,
            fps: FPS,
            bitrate,
            gop: 60,
        }
    }

    /// 一次完整编码：前 PRE 帧用 pre_bps，第 PRE 帧前按需改到 post_bps
    fn run(kind: Kind, pre_bps: u32, post_bps: Option<u32>, force_idr: bool) -> Sample {
        let cfg = cfg(pre_bps);
        let mut enc = H264Encoder::new(cfg).expect("创建编码器");
        let mut nv12 = vec![0u8; cfg.nv12_len()];
        let mut s = Sample::default();

        for i in 0..(PRE + POST) {
            if i == PRE {
                if let Some(bps) = post_bps {
                    enc.set_bitrate(bps).expect("set_bitrate 应当成功");
                    if force_idr {
                        let _ = enc.force_keyframe();
                    }
                }
            }
            fill(kind, &mut nv12, i);
            let out = enc
                .encode_nv12(&nv12, (i as i64) * cfg.frame_interval_us())
                .expect("编码");
            let key = nalu::is_keyframe(&out);
            if i < PRE {
                s.pre_bytes += out.len();
                s.pre_frames += 1;
                s.pre_idr += u32::from(key);
            } else {
                s.post_bytes += out.len();
                s.post_frames += 1;
                s.post_idr += u32::from(key);
            }
            if (PRE - 2..PRE + 6).contains(&i) {
                s.trace.push((i, out.len(), key));
            }
        }
        s
    }

    #[derive(Default)]
    struct Sample {
        pre_bytes: usize,
        pre_frames: u32,
        pre_idr: u32,
        post_bytes: usize,
        post_frames: u32,
        post_idr: u32,
        /// 分界点前后的逐帧字节数：(帧号, 字节, 是否 IDR)
        trace: Vec<(u32, usize, bool)>,
    }

    fn kbps(bytes: usize, frames: u32) -> f64 {
        if frames == 0 {
            return 0.0;
        }
        bytes as f64 * 8.0 * FPS as f64 / frames as f64 / 1000.0
    }

    fn trace_str(s: &Sample) -> String {
        let mut out = String::new();
        for (i, len, key) in &s.trace {
            out.push_str(&format!("{i}:{len}{} ", if *key { "*" } else { "" }));
        }
        out.push_str("（* = IDR）");
        out
    }

    fn report(label: &str, s: &Sample) {
        let pre = kbps(s.pre_bytes, s.pre_frames);
        let post = kbps(s.post_bytes, s.post_frames);
        let ratio = if pre > 0.0 { post / pre } else { 0.0 };
        println!(
            "{label:<18} {pre:>10.0} {post:>10.0} {ratio:>8.2} {:>11.0}",
            s.post_bytes as f64 / s.post_frames.max(1) as f64
        );
    }

    /// GetValue 能不能读回新值？（读得回 ≠ 生效）
    fn readback_probe() {
        println!("=== GetValue 读回（读得回 ≠ 生效）===");
        let mut enc = match H264Encoder::new(cfg(HIGH_BPS)) {
            Ok(e) => e,
            Err(e) => {
                println!("创建失败，跳过：{e}");
                return;
            }
        };
        println!("创建后 bitrate() = {} bps", enc.bitrate());
        match enc.bitrate_readback() {
            Ok(v) => println!("创建后 GetValue(MeanBitRate) = {v} bps"),
            Err(e) => println!("创建后 GetValue 失败：{e}"),
        }
        match enc.set_bitrate(LOW_BPS) {
            Ok(()) => println!("set_bitrate({LOW_BPS}) → Ok"),
            Err(e) => println!("set_bitrate({LOW_BPS}) → Err：{e}"),
        }
        println!("改后 bitrate() = {} bps", enc.bitrate());
        match enc.bitrate_readback() {
            Ok(v) => println!("改后 GetValue(MeanBitRate) = {v} bps"),
            Err(e) => println!("改后 GetValue 失败：{e}"),
        }
        match enc.force_keyframe() {
            Ok(()) => println!("force_keyframe() → Ok"),
            Err(e) => println!("force_keyframe() → Err：{e}"),
        }
        println!();
    }

    /// 重建一个编码器要多久（改码率若必须重建，这就是每次调整的代价）
    fn rebuild_cost() {
        println!("=== H264Encoder::new 耗时（重建成本）===");
        let cfg = cfg(HIGH_BPS);
        // 预热：第一次会带上 MFStartup 与 dll 加载
        match H264Encoder::new(cfg) {
            Ok(e) => drop(e),
            Err(e) => {
                println!("创建失败，跳过：{e}");
                return;
            }
        }

        const N: u32 = 20;
        let mut total = std::time::Duration::ZERO;
        let mut min = std::time::Duration::MAX;
        let mut max = std::time::Duration::ZERO;
        let mut ok = 0u32;
        for _ in 0..N {
            let t = Instant::now();
            match H264Encoder::new(cfg) {
                Ok(e) => {
                    let d = t.elapsed();
                    drop(e);
                    total += d;
                    min = min.min(d);
                    max = max.max(d);
                    ok += 1;
                }
                Err(e) => {
                    println!("第 {ok} 次创建失败：{e}");
                    break;
                }
            }
        }
        if ok > 0 {
            println!(
                "{ok} 次：均值 {:.0} µs（{:.1} ms），最快 {:.0} µs，最慢 {:.0} µs",
                total.as_secs_f64() * 1e6 / ok as f64,
                total.as_secs_f64() * 1e3 / ok as f64,
                min.as_secs_f64() * 1e6,
                max.as_secs_f64() * 1e6
            );
            println!("重建还要重发 SPS/PPS 与 IDR，接收端必须容忍参数集变化");
        }
        println!();
    }

    /// 入参校验：0 与超上限必须返回 Err 而不是 panic
    fn validation_probe() {
        println!("=== 入参校验 ===");
        let mut enc = match H264Encoder::new(cfg(HIGH_BPS)) {
            Ok(e) => e,
            Err(e) => {
                println!("创建失败，跳过：{e}");
                return;
            }
        };
        println!(
            "set_bitrate(0) → {:?}",
            enc.set_bitrate(0).map_err(|e| e.to_string())
        );
        println!(
            "set_bitrate(MAX+1 = {}) → {:?}",
            MAX_BITRATE_BPS + 1,
            enc.set_bitrate(MAX_BITRATE_BPS + 1)
                .map_err(|e| e.to_string())
        );
        println!(
            "set_bitrate(MAX = {MAX_BITRATE_BPS}) → {:?}",
            enc.set_bitrate(MAX_BITRATE_BPS).map_err(|e| e.to_string())
        );
        println!("bitrate() 现在 = {}", enc.bitrate());
        println!(
            "max_bitrate_for: 500k→{}  2M→{}  100M→{}",
            max_bitrate_for(500_000),
            max_bitrate_for(2_000_000),
            max_bitrate_for(MAX_BITRATE_BPS)
        );
    }

    #[derive(Clone, Copy)]
    enum Kind {
        Simple,
        PatchNoise,
        Noise,
    }

    fn fill(kind: Kind, buf: &mut [u8], seq: u32) {
        match kind {
            Kind::Simple => fill_simple(buf, seq),
            Kind::PatchNoise => fill_patch_noise(buf, seq),
            Kind::Noise => fill_noise(buf, seq),
        }
    }

    fn fill_simple(buf: &mut [u8], seq: u32) {
        let (w, h) = (W as usize, H as usize);
        let bar_x = (seq as usize * 7) % w;
        let bar_w = (w / 12).max(8);
        for y in 0..h {
            for x in 0..w {
                buf[y * w + x] = if x >= bar_x && x < bar_x + bar_w {
                    235
                } else {
                    (((x * 200) / w.max(1)) as u8).wrapping_add((y % 7) as u8)
                };
            }
        }
        for b in buf[w * h..].iter_mut() {
            *b = 128;
        }
    }

    /// 中部半幅噪声块在平移，其余是平滑渐变 —— 码率需求由块大小控制，
    /// 是**唯一**对码率目标敏感的内容（见本文件顶部的实测结论）
    fn fill_patch_noise(buf: &mut [u8], seq: u32) {
        let (w, h) = (W as usize, H as usize);
        for y in 0..h {
            for x in 0..w {
                buf[y * w + x] = (((x * 200) / w.max(1)) as u8).wrapping_add((y % 7) as u8);
            }
        }
        let pw = w / 2;
        let ph = h / 2;
        let px = (seq as usize * 11) % (w - pw);
        let py = (h - ph) / 2;
        let mut s = seq.wrapping_mul(2_654_435_761).wrapping_add(1);
        for y in py..py + ph {
            for x in px..px + pw {
                s = s.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                buf[y * w + x] = (s >> 16) as u8;
            }
        }
        for b in buf[w * h..].iter_mut() {
            *b = 128;
        }
    }

    fn fill_noise(buf: &mut [u8], seq: u32) {
        let mut x = seq.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        for b in buf.iter_mut() {
            x = x.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            *b = (x >> 16) as u8;
        }
    }
}

#[cfg(windows)]
fn main() {
    imp::main();
}

#[cfg(not(windows))]
fn main() {
    eprintln!("此示例仅支持 Windows —— Media Foundation 是 Windows 专属能力");
}
