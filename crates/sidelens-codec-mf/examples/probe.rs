//! 编码器实测探针
//!
//! 跑法：
//!
//! `text
//! cargo run -p sidelens-codec-mf --release --example probe
//! cargo run -p sidelens-codec-mf --release --example probe -- 1280 720 30 2000000 60 60 out.h264
//! `
//!
//! 参数顺序：width height fps bitrate gop frames [dump_path]
//!
//! 输出：
//! - 每个 MFT 的候选清单（含硬件/异步标记）
//! - 实际输出格式（Annex-B / AVCC，**实测**）
//! - 总字节、IDR 数、平均帧大小、实际码率
//! - 单帧编码耗时（min/avg/p50/p95/max）与占帧间隔的百分比
//! - 用 nalu::split_annexb 复验 dump 出来的文件

// 整个实现是 Windows 专属（Media Foundation）。
// 用 mod imp 包一层而不是 #![cfg(windows)] —— 后者会把 main 一起删掉，
// 报 E0601「main function not found」，看起来像忘了写 main。
#[cfg(windows)]
mod imp {
    use std::time::Instant;

    use sidelens_codec::nalu;
    use sidelens_codec_mf::{EncoderConfig, H264Encoder};

    pub fn main() {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let num =
            |i: usize, d: u64| -> u64 { args.get(i).and_then(|s| s.parse().ok()).unwrap_or(d) };

        let config = EncoderConfig {
            width: num(0, 1280) as u32,
            height: num(1, 720) as u32,
            fps: num(2, 30) as u32,
            bitrate: num(3, 2_000_000) as u32,
            gop: num(4, 60) as u32,
        };
        let frames = num(5, 60) as usize;
        let dump_path = args.get(6).cloned();

        println!("=== 候选 MFT ===");
        match H264Encoder::list_encoders() {
            Ok(list) => {
                for (i, info) in list.iter().enumerate() {
                    println!(
                        "  [{i}] {:<52} hardware={:<5} async={}",
                        info.name, info.hardware, info.is_async
                    );
                }
                if list.is_empty() {
                    println!("  （空）");
                }
            }
            Err(e) => println!("  枚举失败：{e}"),
        }

        println!("\n=== 配置 ===");
        println!(
            "  {}x{} @{}fps, bitrate={} bps, gop={} ({}s), nv12_len={}",
            config.width,
            config.height,
            config.fps,
            config.bitrate,
            config.gop,
            config.gop / config.fps,
            config.nv12_len()
        );

        let mut enc = match H264Encoder::new(config) {
            Ok(e) => e,
            Err(e) => {
                println!("创建失败：{e}");
                std::process::exit(1);
            }
        };
        println!("  实际编码器：{}", enc.encoder_name());
        println!(
            "  参数集（来自 MF_MT_MPEG_SEQUENCE_HEADER）：{} 字节",
            enc.parameter_sets().len()
        );
        if !enc.parameter_sets().is_empty() {
            println!("    {}", hex(enc.parameter_sets()));
            for n in nalu::split_annexb(enc.parameter_sets()) {
                println!("    - {} ({} 字节)", n.ty, n.len());
            }
        }
        println!(
            "  MFT 自己提供输出 sample：{}",
            enc.output_provides_samples()
        );

        let mut nv12 = vec![0u8; config.nv12_len()];
        let mut total = 0u64;
        let mut idr = 0u32;
        let mut idr_slices = 0u32;
        let mut frames_with_output = 0u32;
        let mut durations_us: Vec<f64> = Vec::with_capacity(frames);
        let mut sizes: Vec<usize> = Vec::with_capacity(frames);
        let mut dump: Vec<u8> = Vec::new();
        let mut first_frame: Option<Vec<u8>> = None;

        println!("\n=== 编码 {frames} 帧 ===");
        for i in 0..frames {
            // 合成 NV12：Y 平面画移动的竖条（保证帧间有真实差异，
            // 否则编码器会把 P 帧压到几十字节，码率完全测不准）
            fill_nv12(&mut nv12, config.width, config.height, i as u32);

            let pts_us = (i as i64) * config.frame_interval_us();
            let t0 = Instant::now();
            let out = match enc.encode_nv12(&nv12, pts_us) {
                Ok(o) => o,
                Err(e) => {
                    println!("第 {i} 帧编码失败：{e}");
                    std::process::exit(1);
                }
            };
            let dt = t0.elapsed().as_secs_f64() * 1e6;
            durations_us.push(dt);

            if out.is_empty() {
                println!("  第 {i} 帧：无输出（MFT 内部缓冲）");
                continue;
            }
            frames_with_output += 1;
            if first_frame.is_none() {
                first_frame = Some(out.clone());
            }

            let nalus = nalu::split_annexb(&out);
            let types: Vec<String> = nalus.iter().map(|n| n.ty.to_string()).collect();
            let is_key = nalus.iter().any(|n| n.is_keyframe());
            if is_key {
                idr += 1;
            }
            // 一帧可能被切成多个 slice（720p 实测 7 个 IDR 切片），
            // 帧级与切片级两个数必须分开报 —— 混着看会以为 GOP 错了。
            idr_slices += nalus.iter().filter(|n| n.is_keyframe()).count() as u32;
            total += out.len() as u64;
            sizes.push(out.len());
            dump.extend_from_slice(&out);

            if i < 3 || is_key {
                println!(
                    "  第 {i:>3} 帧：{:>7} 字节  key={:<5} 耗时 {:>7.1}µs  [{}]",
                    out.len(),
                    is_key,
                    dt,
                    types.join(",")
                );
            }
        }

        // 冲刷
        let tail = enc.flush().unwrap_or_default();
        if !tail.is_empty() {
            println!("\n  flush 输出：{} 字节", tail.len());
            for n in nalu::split_annexb(&tail) {
                println!("    - {} ({} 字节)", n.ty, n.len());
            }
            total += tail.len() as u64;
            dump.extend_from_slice(&tail);
        } else {
            println!("\n  flush 输出：空（没有积压帧）");
        }

        // ---- 结论 ----
        println!("\n=== 输出格式（实测） ===");
        println!(
            "  {}",
            enc.output_format()
                .map(|f| f.as_str())
                .unwrap_or("（未产出任何帧）")
        );
        if let Some(first) = &first_frame {
            println!("  首帧前 16 字节：{}", hex(&first[..first.len().min(16)]));
        }

        let secs = frames as f64 / config.fps as f64;
        println!("\n=== 实测数据 ===");
        println!("  帧数（喂入 / 有输出）：{frames} / {frames_with_output}");
        println!("  总字节：{total}");
        println!(
            "  IDR 帧数（含 IDR 切片的帧）：{idr}    IDR 切片数（一帧可含多片）：{idr_slices}"
        );
        if frames_with_output > 0 {
            println!(
                "  平均帧大小：{:.0} 字节",
                total as f64 / frames_with_output as f64
            );
        }
        if !sizes.is_empty() {
            let mut sorted = sizes.clone();
            sorted.sort_unstable();
            println!(
                "  帧大小 min/p50/p95/max：{}/{}/{}/{}",
                sorted[0],
                sorted[sorted.len() / 2],
                sorted[(sorted.len() * 95) / 100],
                sorted[sorted.len() - 1]
            );
        }
        println!(
            "  实际码率：{:.0} bps（目标 {} bps，偏差 {:+.1}%）",
            total as f64 * 8.0 / secs,
            config.bitrate,
            (total as f64 * 8.0 / secs - config.bitrate as f64) / config.bitrate as f64 * 100.0
        );

        if !durations_us.is_empty() {
            let mut d = durations_us.clone();
            d.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let sum: f64 = d.iter().sum();
            let avg = sum / d.len() as f64;
            let interval_us = 1_000_000.0 / config.fps as f64;
            println!("\n=== 单帧编码耗时 ===");
            println!(
                "  min/p50/p95/max：{:.1}/{:.1}/{:.1}/{:.1} µs",
                d[0],
                d[d.len() / 2],
                d[(d.len() * 95) / 100],
                d[d.len() - 1]
            );
            println!(
                "  平均：{:.1} µs = 帧间隔（{:.0} µs）的 {:.2}%",
                avg,
                interval_us,
                avg / interval_us * 100.0
            );
        }

        // ---- dump 与复验 ----
        if let Some(path) = dump_path {
            if let Err(e) = std::fs::write(&path, &dump) {
                println!("\n写 dump 失败：{e}");
            } else {
                println!("\n=== dump ===");
                println!("  已写入 {path}（{} 字节）", dump.len());

                // 复验：把文件读回来，按 Annex-B 切分
                let bytes = std::fs::read(&path).unwrap_or_default();
                let views = nalu::split_annexb(&bytes);
                let mut counts: std::collections::BTreeMap<String, usize> =
                    std::collections::BTreeMap::new();
                for v in &views {
                    *counts.entry(v.ty.to_string()).or_default() += 1;
                }
                println!("  split_annexb 切出 {} 个 NALU", views.len());
                for (k, v) in counts {
                    println!("    {k}: {v}");
                }
                let reconstructed: usize = views.iter().map(|v| v.len()).sum();
                println!(
                    "  NALU 数据总长 {} / 文件 {} 字节（起始码占 {} 字节）",
                    reconstructed,
                    bytes.len(),
                    bytes.len() - reconstructed
                );
                println!(
                    "  文件能被 is_keyframe 识别出关键帧：{}",
                    views.iter().any(|v| v.is_keyframe())
                );
            }
        }
    }

    /// 合成一帧 NV12
    ///
    /// Y 平面画一个随帧号右移的竖条 + 一条横向渐变；UV 平面给固定的
    /// 中性色（128）。这样帧间差异是**局部**的，符合真实摄像头画面，
    /// 码率才有参考价值 —— 全随机噪声会让每个宏块都变成帧内编码，
    /// 码率会是真实场景的好几倍。
    fn fill_nv12(buf: &mut [u8], width: u32, height: u32, seq: u32) {
        let w = width as usize;
        let h = height as usize;
        let bar_x = (seq as usize * 7) % w;
        let bar_w = (w / 12).max(8);

        for y in 0..h {
            for x in 0..w {
                let in_bar = x >= bar_x && x < bar_x + bar_w;
                let v = if in_bar {
                    235u8
                } else {
                    // 静态背景：横向渐变 + 少量行噪声
                    let g = ((x * 200) / w.max(1)) as u8;
                    g.wrapping_add((y % 7) as u8)
                };
                buf[y * w + x] = v;
            }
        }

        // UV 平面：中性 128
        let uv_len = w * h / 2;
        for b in buf[w * h..w * h + uv_len].iter_mut() {
            *b = 128;
        }
    }

    fn hex(bytes: &[u8]) -> String {
        bytes
            .iter()
            .map(|b| format!("{b:02X}"))
            .collect::<Vec<_>>()
            .join(" ")
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
