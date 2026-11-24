//! 播 440Hz 正弦 1 秒，并**用设备时钟验证样本真的到了声卡**
//!
//! # 跑法
//!
//! ```text
//! cargo run --release -p sidelens-audio-out --example play_tone
//! ```
//!
//! # 为什么用 440Hz
//!
//! A4 是最容易用耳朵确认「响了、音高对不对」的频率。如果听到的是
//! 断续的「嘟—嘟—」而不是连续的「嘀——」，说明写入节奏与设备消费
//! 速率对不上（缓冲太大或 tick 太慢）。
//!
//! # 怎么证明声音真的出去了
//!
//! 计数器只能证明「我们调过 write」。真正能证明样本被设备消费的是
//! `IAudioClock::GetPosition` —— 它由驱动维护，给出「已经播到第几帧」。
//!
//! 所以本示例做三件事：
//!
//! 1. 播放前记下 `device_position_frames()`
//! 2. 播放后再读一次，看它前进了多少
//! 3. 断言前进量 ≈ 提交量 —— 差距就是「还在缓冲区里没播完」的部分
//!
//! 这是端到端延迟最接近的软件测量：写入时刻 → 设备时钟前进到该位置。
//!
//! 真·耳膜延迟需要回环线或麦克风，本示例不覆盖。

// 平台门控：示例主体依赖 WASAPI，只在 Windows 上编译。
// 不能用 `#![cfg(windows)]` —— 那会把 `main` 一起删掉，触发 E0601。
#[cfg(windows)]
mod imp {
    use std::f64::consts::TAU;
    use std::time::{Duration, Instant};

    use sidelens_audio_out::{AudioSink, PcmFormat, SinkMode};

    const FREQ_HZ: f64 = 440.0;
    const DURATION_SECS: f64 = 1.0;
    const FRAME_MS: f64 = 20.0;
    const AMPLITUDE: f64 = 0.25; // -12dBFS，别把音量拉满

    pub fn main() {
        let format = PcmFormat::default();
        let frame_samples = format.frames_for_ms(FRAME_MS);
        let total_frames =
            (format.sample_rate as f64 * DURATION_SECS / frame_samples as f64).ceil() as usize;

        println!("=== SideLens play_tone ===");
        println!("格式 {format}，{FREQ_HZ}Hz 正弦，{DURATION_SECS}s");
        println!("帧长 {FRAME_MS}ms = {frame_samples} 采样，共 {total_frames} 帧");
        println!();

        let mut sink = match AudioSink::new(format) {
            Ok(s) => {
                let (def_ms, min_ms) = s.device_period_ms();
                println!("设备已打开：{:?}", s.device_format());
                println!(
                    "  缓冲区 {} 帧，设备周期 默认 {def_ms:.2}ms / 最小 {min_ms:.2}ms",
                    s.buffer_frames()
                );
                println!(
                    "  流固有延迟 {:.2}ms，估算输出延迟 {:.2}ms",
                    s.stream_latency_ms(),
                    s.latency_ms()
                );
                s
            }
            Err(e) => {
                println!("打不开音频设备：{e}");
                println!("降级 headless（听不到声音，但计时与计数仍然有效）");
                AudioSink::headless(format)
            }
        };

        let is_device = matches!(sink.mode(), SinkMode::Device);
        let pos_before = sink.device_position_frames();
        println!();
        if let Some(p) = pos_before {
            println!("播放前设备时钟位置：{p} 帧");
        } else if is_device {
            println!("设备不提供 IAudioClock，无法用时钟验证");
        }
        println!();

        let mut phase = 0.0f64;
        let mut buf = vec![0i16; frame_samples];
        let start = Instant::now();
        let mut write_intervals = Vec::with_capacity(total_frames);
        let mut latency_samples = Vec::with_capacity(total_frames);
        let mut write_us_samples = Vec::with_capacity(total_frames);
        let mut last_write: Option<Instant> = None;

        for i in 0..total_frames {
            for s in buf.iter_mut() {
                *s = (phase.sin() * AMPLITUDE * 32767.0) as i16;
                phase += TAU * FREQ_HZ / format.sample_rate as f64;
                if phase >= TAU {
                    phase -= TAU;
                }
            }

            let t0 = Instant::now();
            let written = sink.write(&buf).unwrap_or(0);
            let write_us = t0.elapsed().as_secs_f64() * 1e6;
            write_us_samples.push(write_us);

            if let Some(prev) = last_write {
                write_intervals.push(t0.duration_since(prev).as_secs_f64() * 1000.0);
            }
            last_write = Some(t0);
            latency_samples.push(sink.latency_ms());

            if i % 10 == 0 {
                let pos = sink
                    .device_position_frames()
                    .map(|p| p.to_string())
                    .unwrap_or_else(|| "-".into());
                println!(
                    "帧 {i:3}/{total_frames}  写入 {written:4}  本次 {write_us:6.2}µs  设备位置 {pos:>7} 帧"
                );
            }

            let target = start + Duration::from_secs_f64((i + 1) as f64 * FRAME_MS / 1000.0);
            let now = Instant::now();
            if target > now {
                std::thread::sleep(target - now);
            }
        }

        let elapsed = start.elapsed().as_secs_f64();
        println!();
        println!("=== 结果 ===");
        println!("实际耗时 {elapsed:.3}s（理想 {DURATION_SECS:.3}s）");
        println!(
            "提交 {} 采样 = {:.0}ms 音频；丢弃 {} 采样，卡顿 {} 次",
            sink.samples_written(),
            sink.played_ms(),
            sink.samples_dropped(),
            sink.write_stalls()
        );

        if !write_intervals.is_empty() {
            let mut v = write_intervals.clone();
            v.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let p = |q: f64| v[((v.len() as f64 - 1.0) * q).round() as usize];
            println!(
                "write 间隔：p50 {:.3}ms  p95 {:.3}ms  max {:.3}ms（理想 {FRAME_MS}ms）",
                p(0.5),
                p(0.95),
                *v.last().unwrap()
            );
        }

        if !write_us_samples.is_empty() {
            let mut v = write_us_samples.clone();
            v.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let p = |q: f64| v[((v.len() as f64 - 1.0) * q).round() as usize];
            println!(
                "write 耗时：p50 {:.2}µs  p95 {:.2}µs  max {:.2}µs",
                p(0.5),
                p(0.95),
                *v.last().unwrap()
            );
        }

        if !latency_samples.is_empty() {
            let mut v = latency_samples.clone();
            v.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let p = |q: f64| v[((v.len() as f64 - 1.0) * q).round() as usize];
            println!(
                "latency_ms()：min {:.2}  p50 {:.2}  p95 {:.2}  max {:.2}",
                v[0],
                p(0.5),
                p(0.95),
                *v.last().unwrap()
            );
        }

        // --- 设备时钟验证 ---
        if let Some(before) = pos_before {
            // 等缓冲区里剩下的播完（缓冲区 22ms，等 100ms 足够）
            std::thread::sleep(Duration::from_millis(100));
            if let Some(after) = sink.device_position_frames() {
                let advanced = after.saturating_sub(before);
                println!();
                println!("=== 设备时钟验证 ===");
                println!("播放前 {before} 帧 → 播放后 {after} 帧，前进 {advanced} 帧");
                println!("提交 {} 帧", sink.samples_written());
                let ratio = if sink.samples_written() > 0 {
                    advanced as f64 / sink.samples_written() as f64
                } else {
                    0.0
                };
                println!("设备消费 / 提交 = {:.1}%", ratio * 100.0);
                if ratio > 0.9 {
                    println!("✓ 样本确实被设备消费了（不是只调了 write）");
                } else {
                    println!("✗ 设备时钟前进不足 —— 样本可能没真的到声卡");
                }
            }
        }

        println!();
        if is_device {
            println!("如果你听到了 1 秒的 {FREQ_HZ}Hz 纯音，说明整条链路通了。");
        } else {
            println!("headless 模式：没有声音是预期的。");
        }
    }
}

#[cfg(windows)]
pub fn main() {
    imp::main();
}

#[cfg(not(windows))]
fn main() {
    println!("本示例依赖 WASAPI 设备时钟（Windows）。其它平台无法验证样本到达声卡。");
}
