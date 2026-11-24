//! 实测：帧长如何决定 latency_ms()
//!
//! 本 crate 声称「20ms 帧 → 30ms 延迟；2.5ms 帧 → 12ms 左右」。
//! 这条示例把这句话变成可复现的数字，而不是文档里的断言。
//!
//! # 跑法
//!
//! ```text
//! cargo run --release -p sidelens-audio-out --example frame_latency
//! ```
//!
//! # 怎么算的
//!
//! 共享模式的输出延迟由两部分组成：
//!
//! 1. **设备周期**：引擎多久来取一次数。本机 10ms（`GetDevicePeriod`），
//!    这是驱动决定的下限，改不了。
//! 2. **排队的一帧**：刚 write 进去的样本排在队尾，要等前面播完才轮到。
//!    这一项 ≈ 调用方一次给的帧长。
//!
//! 所以 `latency_ms()` ≈ 设备周期 + 帧长。帧长越小延迟越低 ——
//! 代价是包更多、包头开销占比更高、网络丢包的感知更明显。

// 平台门控：示例主体依赖 WASAPI，只在 Windows 上编译。
// 不能用 `#![cfg(windows)]` —— 那会把 `main` 一起删掉，触发 E0601。
#[cfg(windows)]
mod imp {
    use std::time::{Duration, Instant};

    use sidelens_audio_out::{AudioSink, PcmFormat};

    pub fn main() {
        let format = PcmFormat::default();
        println!("=== 帧长 → 输出延迟 ===");
        println!("格式 {format}");
        println!();

        let mut sink = match AudioSink::new(format) {
            Ok(s) => s,
            Err(e) => {
                println!("没有可用音频设备：{e}");
                return;
            }
        };

        let (def_ms, min_ms) = sink.device_period_ms();
        println!("设备周期 默认 {def_ms:.2}ms / 最小 {min_ms:.2}ms");
        println!();
        println!(
            "{:>8}  {:>8}  {:>12}  {:>10}  {:>8}",
            "帧长", "采样数", "latency_ms", "write p50", "卡顿"
        );
        println!("{}", "-".repeat(56));

        // Opus 允许的帧长：2.5 / 5 / 10 / 20 / 40 / 60 ms
        for frame_ms in [2.5f64, 5.0, 10.0, 20.0, 40.0, 60.0] {
            let samples = format.frames_for_ms(frame_ms);
            let buf = vec![0i16; samples];

            // 先让流进入稳态：喂 200ms 的数据
            let warm_start = Instant::now();
            let mut i = 0usize;
            while warm_start.elapsed() < Duration::from_millis(200) {
                let _ = sink.write(&buf);
                i += 1;
                let target = warm_start + Duration::from_secs_f64((i as f64) * frame_ms / 1000.0);
                let now = Instant::now();
                if target > now {
                    std::thread::sleep(target - now);
                }
            }

            // 稳态下测 50 帧
            let mut costs = Vec::with_capacity(50);
            let mut stalls = 0;
            let start = Instant::now();
            let mut lat_sum = 0.0;
            let mut lat_n = 0;
            for k in 0..50usize {
                let t0 = Instant::now();
                let n = sink.write(&buf).unwrap_or(0);
                costs.push(t0.elapsed().as_secs_f64() * 1e6);
                if n == 0 {
                    stalls += 1;
                }
                lat_sum += sink.latency_ms();
                lat_n += 1;
                let target = start + Duration::from_secs_f64((k + 1) as f64 * frame_ms / 1000.0);
                let now = Instant::now();
                if target > now {
                    std::thread::sleep(target - now);
                }
            }
            costs.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let p50 = costs[costs.len() / 2];
            let lat = if lat_n > 0 {
                lat_sum / lat_n as f64
            } else {
                0.0
            };

            println!(
                "{:>6.1}ms  {:>8}  {:>10.2}ms  {:>8.1}µs  {:>8}",
                frame_ms, samples, lat, p50, stalls
            );
        }

        println!();
        println!("结论：latency_ms ≈ 设备周期({def_ms:.1}ms) + 帧长，");
        println!(
            "      但帧长超过缓冲容量（{}ms）后封顶，不再线性增长。",
            sink.buffer_frames() as f64 * 1000.0 / sink.device_format().sample_rate as f64
        );
        println!("      要压低延迟就缩短 Opus 帧长 —— 这是编码侧与网络分包的");
        println!("      决策，本 crate 只负责不额外加缓冲。");
    }
}

#[cfg(windows)]
pub fn main() {
    imp::main();
}

#[cfg(not(windows))]
fn main() {
    println!("本示例依赖 WASAPI 设备周期（Windows）。其它平台没有对应概念。");
}
