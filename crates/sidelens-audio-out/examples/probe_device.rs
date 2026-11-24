//! 探测默认播放设备的 WASAPI 参数（诊断用）
//!
//! 打印：设备混音格式、引擎缓冲区大小、流固有延迟、以及连续写入时的
//! padding 轨迹。这些数字决定了 ~latency_ms()~ 的下限。
//!
//! ~~~text
//! cargo run --release -p sidelens-audio-out --example probe_device
//! ~~~

// 平台门控：示例主体依赖 WASAPI，只在 Windows 上编译。
// 不能用 `#![cfg(windows)]` —— 那会把 `main` 一起删掉，触发 E0601。
#[cfg(windows)]
mod imp {
    use std::time::{Duration, Instant};

    use sidelens_audio_out::{AudioSink, PcmFormat};

    pub fn main() {
        let format = PcmFormat::default();
        let mut sink = match AudioSink::new(format) {
            Ok(s) => s,
            Err(e) => {
                println!("没有可用音频设备：{e}");
                return;
            }
        };

        println!("=== WASAPI 共享模式设备探测 ===");
        match sidelens_audio_out::default_device_id() {
            Some(id) => println!("设备 id    {id}"),
            None => println!("设备 id    （拿不到）"),
        }
        println!("请求格式   {format}");
        println!("设备格式   {:?}", sink.device_format());
        println!(
            "缓冲帧数   {} 帧（{:.2}ms @ 设备采样率）",
            sink.buffer_frames(),
            sink.buffer_frames() as f64 * 1000.0 / sink.device_format().sample_rate as f64
        );
        let (def_ms, min_ms) = sink.device_period_ms();
        println!(
            "设备周期   默认 {:.3}ms / 最小 {:.3}ms（GetDevicePeriod）",
            def_ms, min_ms
        );
        println!(
            "流固有延迟 {:.3}ms（GetStreamLatency）",
            sink.stream_latency_ms()
        );
        println!();

        // 空转 100ms，看 padding 是否真的在下降（引擎在消费）
        println!("--- 空转：观察引擎是否在消费 ---");
        let frame = vec![0i16; 960];
        let _ = sink.write(&frame);
        println!("写入 960 帧后 padding = {}", sink.padding_frames());
        for i in 0..5 {
            std::thread::sleep(Duration::from_millis(5));
            println!(
                "  +{}ms 后 padding 仍是 {}（这里不变是正常的：padding 只在 write 时刷新）",
                (i + 1) * 5,
                sink.padding_frames()
            );
        }
        println!();

        // 连续写入，观察 padding 稳态
        println!("--- 连续写入 40 帧 × 20ms，观察 padding 稳态 ---");
        let start = Instant::now();
        let mut paddings = Vec::new();
        for i in 0..40 {
            let written = sink.write(&frame).unwrap_or(0);
            paddings.push(sink.padding_frames());
            if i < 12 || i % 8 == 0 {
                println!(
                    "  帧 {i:2}  写入 {written:4}  padding {:4}  latency {:6.2}ms",
                    sink.padding_frames(),
                    sink.latency_ms()
                );
            }
            let target = start + Duration::from_millis((i + 1) * 20);
            let now = Instant::now();
            if target > now {
                std::thread::sleep(target - now);
            }
        }

        paddings.sort_unstable();
        println!();
        println!(
            "padding 分布：min {}  median {}  max {}",
            paddings[0],
            paddings[paddings.len() / 2],
            paddings[paddings.len() - 1]
        );
        println!(
            "累计写入 {} 采样，丢弃 {} 采样，卡顿 {} 次",
            sink.samples_written(),
            sink.samples_dropped(),
            sink.write_stalls()
        );
        println!();
        println!(
            "结论：latency_ms() 的下限 = 流固有延迟 {:.2}ms；",
            sink.stream_latency_ms()
        );
        println!("      实际输出延迟 = 固有延迟 + padding/采样率。");
    }
}

#[cfg(windows)]
pub fn main() {
    imp::main();
}

#[cfg(not(windows))]
fn main() {
    println!("本示例探测 WASAPI 播放设备（Windows）。其它平台没有对应 API。");
}
