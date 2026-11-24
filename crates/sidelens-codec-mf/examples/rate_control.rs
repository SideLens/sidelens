//! 码控边界：码控到底在什么条件下生效？
//!
//! 三组对照：
//! 1. 内容复杂度 >> 目标码率（全噪声）→ 码控**完全失效**
//! 2. 内容复杂度 ≈ 目标码率（中等熵）→ 码控**精确跟随**
//! 3. 内容复杂度 << 目标码率（低熵）→ 码控给下限，不会硬凑
//!
//! 结论：mfh264enc 的 CBR 只在「内容本来就需要这么多码率」时起作用，
//! 对超过目标的高熵内容它**不做限流**（不是没设上，是它不裁）。

// 整个实现是 Windows 专属（Media Foundation）。
// 用 mod imp 包一层而不是 #![cfg(windows)] —— 后者会把 main 一起删掉，
// 报 E0601「main function not found」，看起来像忘了写 main。
#[cfg(windows)]
mod imp {
    use sidelens_codec::nalu;
    use sidelens_codec_mf::{EncoderConfig, H264Encoder};

    const FRAMES: u32 = 90;

    pub fn main() {
        let cfg0 = EncoderConfig {
            width: 640,
            height: 360,
            ..EncoderConfig::default()
        };
        println!("=== 内容复杂度 vs 码控 ===");
        println!(
            "{:<22} {:>10} {:>12} {:>10}",
            "内容", "目标 kbps", "实际 kbps", "比值"
        );
        for (label, kind) in [
            ("低熵（渐变+竖条）", Kind::Simple),
            ("中熵（块状平移）", Kind::Medium),
            ("高熵（逐帧噪声）", Kind::Noise),
        ] {
            for target in [250_000u32, 1_000_000, 4_000_000] {
                let cfg = EncoderConfig {
                    bitrate: target,
                    ..cfg0
                };
                let (total, _) = run(cfg, kind);
                let bps = total as f64 * 8.0 / (FRAMES as f64 / cfg.fps as f64);
                println!(
                    "{label:<22} {:>10} {:>12.0} {:>9.2}x",
                    target / 1000,
                    bps / 1000.0,
                    bps / target as f64
                );
            }
        }
    }

    fn run(cfg: EncoderConfig, kind: Kind) -> (usize, u32) {
        let mut enc = H264Encoder::new(cfg).expect("创建编码器");
        let mut nv12 = vec![0u8; cfg.nv12_len()];
        let mut total = 0usize;
        let mut idr = 0u32;
        for i in 0..FRAMES {
            match kind {
                Kind::Simple => fill_simple(&mut nv12, cfg.width, cfg.height, i),
                Kind::Medium => fill_medium(&mut nv12, cfg.width, cfg.height, i),
                Kind::Noise => fill_noise(&mut nv12, i),
            }
            let out = enc
                .encode_nv12(&nv12, (i as i64) * cfg.frame_interval_us())
                .unwrap();
            if nalu::is_keyframe(&out) {
                idr += 1;
            }
            total += out.len();
        }
        total += enc.flush().unwrap().len();
        (total, idr)
    }

    #[derive(Clone, Copy)]
    enum Kind {
        Simple,
        Medium,
        Noise,
    }

    fn fill_simple(buf: &mut [u8], width: u32, height: u32, seq: u32) {
        let w = width as usize;
        let h = height as usize;
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

    fn fill_medium(buf: &mut [u8], width: u32, height: u32, seq: u32) {
        let w = width as usize;
        let h = height as usize;
        let shift = (seq as usize * 3) % w;
        for y in 0..h {
            for x in 0..w {
                let xx = (x + shift) % w;
                buf[y * w + x] = (((xx / 8 + y / 8) % 32) as u8).wrapping_mul(7);
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
