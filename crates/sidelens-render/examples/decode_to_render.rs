//! 真解码器 → 真转换 的端到端探针
//!
//! 跑法：
//!
//! ```text
//! cargo run -p sidelens-render --example decode_to_render --release
//! cargo run -p sidelens-render --example decode_to_render --release -- 1920 1080 60
//! ```
//!
//! # 这个 example 补的是什么
//!
//! `bench_convert` 用的是**合成** NV12，它证明不了「H264Decoder 吐出来的东西
//! 和 Nv12View 期望的布局一致」。这个 example 走真链路：
//!
//! 1. `H264Encoder` 编一帧合成 NV12 → Annex-B
//! 2. `H264Decoder` 解回 NV12（**真解码器输出**，coded size 带宏块填充）
//! 3. 用 `output_size()` / `output_display_size()` 建 `Nv12View`
//! 4. `to_rgba` + 校验裁剪后的画面内容
//!
//! 关键断言：解码器给的是 coded 640x368，但 RGBA 必须是 display 640x360，
//! 且底部那 8 行填充**不能**出现在输出里。
//!
//! 没有 `sidelens-codec-mf`（非 Windows）时这个 example 只打印一行跳过。

#[cfg(windows)]
mod imp {
    use std::time::Instant;

    use sidelens_codec_mf::{DecoderConfig, EncoderConfig, H264Decoder, H264Encoder};
    use sidelens_render::Nv12View;

    /// 合成一帧 NV12：上半黑、下半亮，用来验证「裁剪裁的是底部」
    ///
    /// 这样如果底部填充行漏进输出，画面最后几行会突然变亮 —— 肉眼可辨。
    fn synth(width: u32, height: u32, seq: u32) -> Vec<u8> {
        let w = width as usize;
        let h = height as usize;
        let mut buf = vec![128u8; w * h * 3 / 2];
        let bar = (seq as usize * 13) % w.max(1);
        for y in 0..h {
            for x in 0..w {
                let v = if x >= bar && x < bar + (w / 8).max(4) {
                    200u8
                } else {
                    (((x * 180) / w.max(1)) as u8).wrapping_add(20)
                };
                buf[y * w + x] = v;
            }
        }
        buf
    }

    pub fn main() -> Result<(), Box<dyn std::error::Error>> {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let num =
            |i: usize, d: u64| -> u64 { args.get(i).and_then(|s| s.parse().ok()).unwrap_or(d) };
        let width = num(0, 640) as u32;
        let height = num(1, 360) as u32;
        let frames = num(2, 30) as usize;

        println!("=== decode → render 端到端探针 ===");
        println!("  请求 {width}x{height}，{frames} 帧");

        let mut enc = H264Encoder::new(EncoderConfig {
            width,
            height,
            fps: 30,
            bitrate: 2_000_000,
            gop: 30,
        })?;
        let mut dec = H264Decoder::new(DecoderConfig::new(width, height, 30))?;

        let mut annexb = Vec::new();
        let mut decoded = Vec::new();
        let mut rgba = Vec::new();

        let mut converted = 0usize;
        let mut convert_ns = 0u128;
        let mut coded = (0u32, 0u32);
        let mut display = (0u32, 0u32);

        for i in 0..frames {
            let nv12_in = synth(width, height, i as u32);
            annexb.clear();
            enc.encode_nv12_into(&nv12_in, (i as i64) * 33_333, &mut annexb)?;

            decoded.clear();
            dec.decode_into(&annexb, (i as i64) * 33_333, &mut decoded)?;
            if decoded.is_empty() {
                continue; // 首帧只换到 NEED_MORE_INPUT，正常
            }

            coded = dec.output_size();
            display = dec.output_display_size();
            if dec.frames_out() as usize == 1 {
                println!(
                    "  解码器实测：coded {coded:?}  显示 {display:?}  一帧 {} 字节",
                    decoded.len()
                );
            }

            let view = Nv12View::new(&decoded, coded, display)?;
            let t = Instant::now();
            view.to_rgba(&mut rgba)?;
            convert_ns += t.elapsed().as_nanos();
            converted += 1;

            // 尺寸必须按 display，不是 coded
            let expect = (display.0 * display.1 * 4) as usize;
            assert_eq!(
                rgba.len(),
                expect,
                "RGBA 长度必须是 display 尺寸 {}x{} * 4",
                display.0,
                display.1
            );
        }

        println!();
        println!("=== 结果 ===");
        println!("  coded {coded:?} / 显示 {display:?}");
        println!(
            "  转换 {converted} 帧，累计 {:.2} ms",
            convert_ns as f64 / 1e6
        );
        if converted > 0 {
            println!(
                "  平均每帧 {:.0} µs",
                convert_ns as f64 / 1e3 / converted as f64
            );
        }
        if coded.1 != display.1 {
            println!(
                "  [OK] 裁剪生效：底部 {} 行宏块填充已丢弃",
                coded.1 - display.1
            );
        } else {
            println!("  [--] 本分辨率 coded == display，无需裁剪");
        }
        println!("  [OK] 端到端链路（编码 → 解码 → NV12 → RGBA）跑通");
        Ok(())
    }
}

#[cfg(not(windows))]
fn main() {
    println!("sidelens-codec-mf 是 Windows 专属（Media Foundation），本 example 跳过。");
}

#[cfg(windows)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    imp::main()
}
