//! `write` 单次调用的耗时实测
//!
//! # 为什么这条基准很关键
//!
//! `AudioSink::write` 是在 **1ms tick** 里被调用的。一次 20ms 帧（960 采样）
//! 如果耗掉 500µs，tick 就被吃掉一半，视频出队、消息泵、抖动缓冲统计
//! 全部往后推 —— 表现为「音频一响画面就开始抖」。
//!
//! 预算：**单次 < 100µs**（tick 的 10%）。
//!
//! # 两种路径要分开测
//!
//! `write` 有两条完全不同的执行路径，混在一起测会得出骗人的数字：
//!
//! | 路径 | 触发条件 | 做什么 | 期望耗时 |
//! | --- | --- | --- | --- |
//! | **有空间** | 缓冲区没满 | GetCurrentPadding → i16→f32 转换 → GetBuffer → memcpy → ReleaseBuffer | 几 µs |
//! | **缓冲满** | 缓冲区满 | GetCurrentPadding → 记丢弃 → 返回 Ok(0) | 亚 µs |
//!
//! 裸循环猛写 2000 次，99% 都走「满」路径，测出来的 0.1µs 毫无意义。
//! 所以下面**按 20ms 节拍喂**，让每次 write 都真的写进引擎缓冲区。
//!
//! # 跑法
//!
//! ```text
//! cargo run --release -p sidelens-audio-out --example bench_write
//! ```
//!
//! 真设备打不开就只跑 headless（headless 测的是外层开销 + 计数逻辑）。

// 平台门控：示例主体依赖 WASAPI，只在 Windows 上编译。
// 不能用 `#![cfg(windows)]` —— 那会把 `main` 一起删掉，触发 E0601。
#[cfg(windows)]
mod imp {
    use std::time::{Duration, Instant};

    use sidelens_audio_out::{AudioSink, PcmFormat};

    /// 一轮测量：返回 (p50, p95, p99, max) 微秒
    fn stats(mut v: Vec<f64>) -> (f64, f64, f64, f64) {
        if v.is_empty() {
            return (0.0, 0.0, 0.0, 0.0);
        }
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let pick = |q: f64| -> f64 {
            let idx = ((v.len() as f64 - 1.0) * q).round() as usize;
            v[idx.min(v.len() - 1)]
        };
        (pick(0.50), pick(0.95), pick(0.99), *v.last().unwrap())
    }

    fn line(label: &str, s: (f64, f64, f64, f64)) {
        println!(
            "  {label:<18} p50 {:7.3}µs   p95 {:7.3}µs   p99 {:7.3}µs   max {:7.3}µs",
            s.0, s.1, s.2, s.3
        );
    }

    /// 按节拍写入，测量「缓冲区有空间」那条路径
    fn bench_paced(
        sink: &mut AudioSink,
        frame: &[i16],
        frame_ms: f64,
        iters: usize,
    ) -> (Vec<f64>, u64, u64) {
        let mut costs = Vec::with_capacity(iters);
        let mut ok = 0u64;
        let mut stalled = 0u64;
        let start = Instant::now();
        for i in 0..iters {
            let t0 = Instant::now();
            let n = sink.write(frame).unwrap_or(0);
            costs.push(t0.elapsed().as_secs_f64() * 1e6);
            if n > 0 {
                ok += 1;
            } else {
                stalled += 1;
            }
            // 按帧时长推进，让引擎有时间把上一帧取走
            let target = start + Duration::from_secs_f64((i + 1) as f64 * frame_ms / 1000.0);
            let now = Instant::now();
            if target > now {
                std::thread::sleep(target - now);
            }
        }
        (costs, ok, stalled)
    }

    /// 不节拍、猛写，测量「缓冲区满」那条路径
    fn bench_saturated(sink: &mut AudioSink, frame: &[i16], iters: usize) -> Vec<f64> {
        let mut costs = Vec::with_capacity(iters);
        for _ in 0..iters {
            let t0 = Instant::now();
            let _ = sink.write(frame);
            costs.push(t0.elapsed().as_secs_f64() * 1e6);
        }
        costs
    }

    pub fn main() {
        let format = PcmFormat::default(); // 48kHz 单声道
        let frame_20ms = vec![0i16; 960];
        let frame_60ms = vec![0i16; 2_880];

        println!("=== sidelens-audio-out write 基准 ===");
        println!("格式：{format}（Opus 解码器默认输出）");
        println!("预算：单次 < 100µs（1ms tick 的 10%）");
        println!();

        // ---------- 真设备 ----------
        match AudioSink::new(format) {
            Ok(mut sink) => {
                let (def_ms, min_ms) = sink.device_period_ms();
                println!("[wasapi] 共享模式");
                println!(
                    "  设备格式 {:?}，缓冲 {} 帧",
                    sink.device_format(),
                    sink.buffer_frames()
                );
                println!("  设备周期 默认 {def_ms:.2}ms / 最小 {min_ms:.2}ms");
                println!("  流固有延迟 {:.2}ms", sink.stream_latency_ms());
                println!();

                // 预热
                for _ in 0..20 {
                    let _ = sink.write(&frame_20ms);
                    std::thread::sleep(Duration::from_millis(20));
                }

                println!("  按 20ms 节拍写入（缓冲区有空间 → 真写路径）：");
                let (c20, ok20, st20) = bench_paced(&mut sink, &frame_20ms, 20.0, 100);
                let s20 = stats(c20);
                line("960 采样 (20ms)", s20);
                println!("    → 成功写入 {ok20}/100 次，卡顿 {st20} 次");

                let (c60, ok60, st60) = bench_paced(&mut sink, &frame_60ms, 60.0, 40);
                let s60 = stats(c60);
                line("2880 采样 (60ms)", s60);
                println!("    → 成功写入 {ok60}/40 次，卡顿 {st60} 次");

                let sat = bench_saturated(&mut sink, &frame_20ms, 2_000);
                let ssat = stats(sat);
                line("缓冲满 (Ok(0))", ssat);
                println!();

                println!(
                    "  累计写入 {} 采样（{:.0}ms），丢弃 {} 采样，卡顿 {} 次",
                    sink.samples_written(),
                    sink.played_ms(),
                    sink.samples_dropped(),
                    sink.write_stalls()
                );
                println!();

                let worst = s20.0.max(s60.0);
                let verdict = if worst < 100.0 { "PASS" } else { "FAIL" };
                println!("预算 100µs → 真写路径 p50 最大 {worst:.3}µs → {verdict}");
            }
            Err(e) => {
                println!("[wasapi] 打不开真设备，跳过：{e}");
            }
        }

        println!();

        // ---------- headless ----------
        {
            let mut sink = AudioSink::headless(format);
            println!("[headless] 纯计数（不碰平台 API）");
            for _ in 0..20 {
                let _ = sink.write(&frame_20ms);
            }
            let (c, _, _) = bench_paced(&mut sink, &frame_20ms, 20.0, 100);
            line("960 采样 (20ms)", stats(c));
            println!(
                "  平均 {:.3}µs（{} 次调用）",
                sink.avg_write_ns() as f64 / 1000.0,
                sink.write_calls()
            );
        }
    }
}

#[cfg(windows)]
pub fn main() {
    imp::main();
}

#[cfg(not(windows))]
fn main() {
    println!("本示例依赖 WASAPI（Windows）。其它平台上 write 基准无意义。");
}
