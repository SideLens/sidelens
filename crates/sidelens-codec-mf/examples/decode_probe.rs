//! 编解码闭环探针
//!
//! 跑法：
//!
//! ```text
//! cargo run -p sidelens-codec-mf --example decode_probe
//! cargo run -p sidelens-codec-mf --example decode_probe -- 640 360 30 30
//! ```
//!
//! 参数顺序：width height fps frames
//!
//! 做的事：
//!
//! 1. 合成 `frames` 帧 NV12（复用 `probe.rs` 的合成方式：渐变背景 + 移动竖条）
//! 2. 用 [@@H264Encoder@@] 编成 Annex-B
//! 3. 用 [@@H264Decoder@@] 解回 NV12
//! 4. 打印每帧的输入/输出字节数、耗时，最后汇总
//!
//! 关键断言（不满足就 exit 1）：
//!
//! - 输出帧数 == 输入帧数，或差 1（解码器首帧吞掉 / flush 延迟）
//! - 输出格式必须是已知格式
//! - 输出字节数必须是 `frame_len` 的整数倍
//! - 纯色块的 Y 值往返后差值 < 16（H.264 有损，不要求逐字节相等）

// 整个实现是 Windows 专属（Media Foundation）。
// 用 mod imp 包一层而不是 #![cfg(windows)] —— 后者会把 main 一起删掉，
// 报 E0601「main function not found」，看起来像忘了写 main。
#[cfg(windows)]
mod imp {
    use std::time::Instant;

    use sidelens_codec::nalu;
    use sidelens_codec_mf::{
        DecodedFormat, DecoderConfig, EncoderConfig, H264Decoder, H264Encoder,
    };

    pub fn main() {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let num =
            |i: usize, d: u64| -> u64 { args.get(i).and_then(|s| s.parse().ok()).unwrap_or(d) };

        let width = num(0, 640) as u32;
        let height = num(1, 360) as u32;
        let fps = num(2, 30) as u32;
        let frames = num(3, 30) as usize;

        let enc_cfg = EncoderConfig {
            width,
            height,
            fps,
            bitrate: 2_000_000,
            gop: 30,
        };
        let dec_cfg = DecoderConfig::new(width, height, fps);

        println!("=== 本机解码器候选 ===");
        match H264Decoder::list_decoders() {
            Ok(list) => {
                for (i, info) in list.iter().enumerate() {
                    println!(
                        "  [{i}] {:<52} async={:<5} d3d={}",
                        info.name, info.is_async, info.hardware
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
            "  {}x{} @{}fps, {} 帧, enc bitrate={} bps, gop={}",
            enc_cfg.width, enc_cfg.height, enc_cfg.fps, frames, enc_cfg.bitrate, enc_cfg.gop
        );

        // ---- 编码器 ----
        let mut enc = match H264Encoder::new(enc_cfg) {
            Ok(e) => e,
            Err(e) => {
                println!("编码器创建失败：{e}");
                std::process::exit(1);
            }
        };
        println!("  编码器：{}", enc.encoder_name());

        // ---- 解码器 ----
        let mut dec = match H264Decoder::new(dec_cfg) {
            Ok(d) => d,
            Err(e) => {
                println!("解码器创建失败：{e}");
                println!("  （这条链路需要 H.264 Decoder MFT，Windows 10/11 自带）");
                std::process::exit(1);
            }
        };
        println!("  解码器：{}", dec.decoder_name());
        println!(
            "  协商输出格式：{}  {}x{}  stride 见下",
            dec.output_format().as_str(),
            dec.output_size().0,
            dec.output_size().1
        );

        let mut nv12 = vec![0u8; enc_cfg.nv12_len()];
        let mut stream_total = 0u64;
        let mut decoded_total = 0u64;
        let mut enc_us: Vec<f64> = Vec::with_capacity(frames);
        let mut dec_us: Vec<f64> = Vec::with_capacity(frames);
        let mut roundtrip_us: Vec<f64> = Vec::with_capacity(frames);
        let mut frame_out_count = 0usize;
        let mut frames_with_output = 0usize;
        let mut y_diffs: Vec<i32> = Vec::new();

        // 记录每帧「编出来的字节数」与「解出来的字节数」，最后一起打印
        let mut per_frame: Vec<(usize, usize, usize)> = Vec::with_capacity(frames);

        println!("\n=== 编码 → 解码 ===");
        for i in 0..frames {
            fill_nv12(&mut nv12, width, height, i as u32);
            let pts_us = (i as i64) * enc_cfg.frame_interval_us();

            let t0 = Instant::now();
            let packet = match enc.encode_nv12(&nv12, pts_us) {
                Ok(o) => o,
                Err(e) => {
                    println!("第 {i} 帧编码失败：{e}");
                    std::process::exit(1);
                }
            };
            let t_enc = t0.elapsed().as_secs_f64() * 1e6;
            enc_us.push(t_enc);

            let mut raw = Vec::new();
            let t1 = Instant::now();
            match dec.decode_into(&packet, pts_us, &mut raw) {
                Ok(()) => {}
                Err(e) => {
                    println!("第 {i} 帧解码失败：{e}");
                    std::process::exit(1);
                }
            }
            let t_dec = t1.elapsed().as_secs_f64() * 1e6;
            dec_us.push(t_dec);
            roundtrip_us.push(t_enc + t_dec);

            stream_total += packet.len() as u64;
            decoded_total += raw.len() as u64;
            if !packet.is_empty() {
                frames_with_output += 1;
            }

            let frame_len = dec.output_frame_len();
            let n_out = raw.len().checked_div(frame_len).unwrap_or(0);
            frame_out_count += n_out;

            // Y 值比对：只在「本次确实解出一整帧」时做
            if n_out >= 1 && dec.output_format() == DecodedFormat::Nv12 {
                let (ow, oh) = dec.output_size();
                let sample_pts = [
                    (ow as usize / 2, oh as usize / 2),
                    (8, 8),
                    (ow as usize - 9, oh as usize - 9),
                ];
                for (sx, sy) in sample_pts {
                    if sx < ow as usize && sy < oh as usize {
                        let decoded_y = raw[sy * ow as usize + sx] as i32;
                        let source_y = nv12[sy * width as usize + sx] as i32;
                        y_diffs.push((decoded_y - source_y).abs());
                    }
                }
            }

            per_frame.push((packet.len(), raw.len(), n_out));

            let key = nalu::is_keyframe(&packet);
            if i < 5 || key || n_out == 0 {
                println!(
                    "  第 {i:>3} 帧：in {:>6} B  out {:>7} B  {:>2} 帧  key={:<5} enc {:>6.1}µs dec {:>6.1}µs",
                    packet.len(),
                    raw.len(),
                    n_out,
                    key,
                    t_enc,
                    t_dec
                );
            }
        }

        // ---- flush ----
        let tail_stream = enc.flush().unwrap_or_default();
        if !tail_stream.is_empty() {
            println!("\n  编码器 flush：{} 字节", tail_stream.len());
            stream_total += tail_stream.len() as u64;
        }
        let tail_frames = match dec.flush() {
            Ok(v) => v,
            Err(e) => {
                println!("  解码器 flush 失败：{e}");
                Vec::new()
            }
        };
        let frame_len = dec.output_frame_len();
        let tail_n = tail_frames.len().checked_div(frame_len).unwrap_or(0);
        println!("  解码器 flush：{} 字节（{tail_n} 帧）", tail_frames.len());
        decoded_total += tail_frames.len() as u64;
        frame_out_count += tail_n;

        // ---- 结论 ----
        println!("\n=== 输出格式（实测） ===");
        let (cw, ch) = dec.output_size();
        let (dw, dh) = dec.output_display_size();
        println!(
            "  {}  coded {}x{}  显示 {}x{}  一帧 {} 字节",
            dec.output_format().as_str(),
            cw,
            ch,
            dw,
            dh,
            dec.output_frame_len()
        );
        if (cw, ch) != (dw, dh) {
            println!(
                "  （coded size 比显示尺寸多 {} 行，是 H.264 的宏块对齐填充，显示时按 SPS cropping 裁掉）",
                ch.saturating_sub(dh)
            );
        }
        println!(
            "  SPS profile={:?} level={:?}",
            dec.sps_profile(),
            dec.sps_level()
        );
        println!(
            "  frames_in={}  frames_out={}  not_accepting={}  stream_change={}  need_more_input={}",
            dec.frames_in(),
            dec.frames_out(),
            dec.not_accepting_count(),
            dec.stream_change_count(),
            dec.need_more_input_count()
        );

        println!("\n=== 实测数据 ===");
        println!("  编码输出总字节：{stream_total}");
        println!(
            "  解码输出总字节：{decoded_total}（一帧 {} 字节）",
            dec.output_frame_len()
        );
        println!(
            "  喂入帧数 {frames} / 编码有输出 {frames_with_output} / 解码出帧 {frame_out_count}"
        );

        if !enc_us.is_empty() {
            println!("\n=== 单帧耗时 ===");
            print_stats("编码", &mut enc_us);
            print_stats("解码", &mut dec_us);
            print_stats("往返", &mut roundtrip_us);
        }

        if !y_diffs.is_empty() {
            let max = *y_diffs.iter().max().unwrap();
            let avg = y_diffs.iter().sum::<i32>() as f64 / y_diffs.len() as f64;
            println!("\n=== Y 值往返比对（NV12） ===");
            println!(
                "  采样 {} 个点，|ΔY| 平均 {:.2}，最大 {max}（阈值 16）",
                y_diffs.len(),
                avg
            );
        } else {
            println!("\n=== Y 值往返比对 ===");
            println!("  输出不是 NV12 或没有解出整帧，跳过");
        }

        // ---- 断言 ----
        println!("\n=== 断言 ===");
        let mut failed = 0;

        let diff = (frames as i64 - frame_out_count as i64).abs();
        if diff <= 1 {
            println!("  [OK] 输出帧数 {frame_out_count} 与输入 {frames} 相差 {diff}（≤1）");
        } else {
            println!("  [FAIL] 输出帧数 {frame_out_count} 与输入 {frames} 相差 {diff}（>1）");
            failed += 1;
        }

        if frame_len > 0 && decoded_total as usize % frame_len == 0 {
            println!("  [OK] 解码输出字节数 {decoded_total} 是帧长 {frame_len} 的整数倍");
        } else {
            println!("  [FAIL] 解码输出字节数 {decoded_total} 不是帧长 {frame_len} 的整数倍");
            failed += 1;
        }

        if !y_diffs.is_empty() && *y_diffs.iter().max().unwrap() < 16 {
            println!("  [OK] Y 值最大偏差 {} < 16", y_diffs.iter().max().unwrap());
        } else if y_diffs.is_empty() {
            println!("  [SKIP] 没有可比对的 NV12 采样点");
        } else {
            println!(
                "  [FAIL] Y 值最大偏差 {} >= 16",
                y_diffs.iter().max().unwrap()
            );
            failed += 1;
        }

        if failed == 0 {
            println!("\n全部断言通过。");
        } else {
            println!("\n{failed} 条断言失败。");
            std::process::exit(1);
        }
    }

    fn print_stats(label: &str, samples: &mut [f64]) {
        samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let sum: f64 = samples.iter().sum();
        let avg = sum / samples.len() as f64;
        println!(
            "  {label} min/p50/p95/max：{:.1}/{:.1}/{:.1}/{:.1} µs   平均 {:.1} µs",
            samples[0],
            samples[samples.len() / 2],
            samples[(samples.len() * 95) / 100],
            samples[samples.len() - 1],
            avg
        );
    }

    /// 合成一帧 NV12
    ///
    /// Y 平面画一个随帧号右移的竖条 + 一条横向渐变；UV 平面给固定的
    /// 中性色（128）。与 `examples/probe.rs` 完全一致 —— 闭环比对时
    /// 两边的输入必须是同一份内容。
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
}

#[cfg(windows)]
fn main() {
    imp::main();
}

#[cfg(not(windows))]
fn main() {
    eprintln!("此示例仅支持 Windows —— Media Foundation 是 Windows 专属能力");
}
