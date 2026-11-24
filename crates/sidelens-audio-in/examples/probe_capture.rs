//! 探测默认采集设备与 loopback 的 WASAPI 参数（诊断用）
//!
//! 打印：设备混音格式、引擎缓冲、设备周期、采集延迟，
//! 以及「连续 read 时返回 0 的比例」—— loopback 在静音时这个比例是 100%，
//! 那是最容易被误判成「设备坏了」的现象。
//!
//! ~~~text
//! cargo run --release -p sidelens-audio-in --example probe_capture
//! ~~~
//!
//! 打不开设备时**明确打印原因**（含 HRESULT 文本），不 panic。

// 平台门控：示例主体依赖 WASAPI，只在 Windows 上编译。
// 不能用 `#![cfg(windows)]` —— 那会把 `main` 一起删掉，触发 E0601。
#[cfg(windows)]
mod imp {
    use std::time::{Duration, Instant};

    use sidelens_audio_in::{AudioCapture, CaptureMode, PcmFormat};

    pub fn main() {
        println!("=== SideLens 音频采集探测 ===");
        println!("请求格式  {}", PcmFormat::default());
        println!();

        probe_one(
            "麦克风（默认采集端点）",
            CaptureMode::Microphone,
            AudioCapture::new(PcmFormat::default()),
            sidelens_audio_in::default_capture_device_id(),
        );
        println!();
        probe_one(
            "系统回环（默认渲染端点的镜像）",
            CaptureMode::Loopback,
            AudioCapture::loopback(PcmFormat::default()),
            sidelens_audio_in::default_render_device_id(),
        );
        println!();

        // headless 永远可用，作为对照组
        let mut headless = AudioCapture::headless(PcmFormat::default());
        let mut out = Vec::new();
        let _ = headless.read(&mut out);
        std::thread::sleep(Duration::from_millis(30));
        out.clear();
        let n = headless.read(&mut out).unwrap_or(0);
        println!("=== headless（对照组） ===");
        println!("模式      {}", headless.mode().as_str());
        println!(
            "设备格式  {:?}（None = 没有设备）",
            headless.device_format()
        );
        println!(
            "30ms 后 read 拿到 {n} 采样，采集延迟 {:.3}ms",
            headless.latency_ms()
        );
    }

    fn probe_one(
        title: &str,
        mode: CaptureMode,
        opened: Result<AudioCapture, sidelens_audio_in::CaptureError>,
        device_id: Option<String>,
    ) {
        println!("=== {title} ===");
        let mut cap = match opened {
            Ok(c) => c,
            Err(e) => {
                // 打不开就把原因完整打出来：HRESULT 文本比错误码有用得多
                println!("打开失败：{e}");
                println!("（模式 {} 不可用，调用方应降级为 headless）", mode.as_str());
                return;
            }
        };

        println!("模式      {}", cap.mode().as_str());
        match device_id {
            Some(id) => println!("端点 id   {id}"),
            None => println!("端点 id   （拿不到）"),
        }
        let dev = cap.device_format();
        match dev {
            Some(d) => {
                println!("设备格式  {d}");
                println!(
                    "          {} 帧 = {:.3}ms",
                    cap.buffer_frames(),
                    cap.buffer_frames() as f64 * 1000.0 / d.sample_rate as f64
                );
            }
            None => println!("设备格式  （无）"),
        }
        let (def_ms, min_ms) = cap.device_period_ms();
        println!("设备周期  默认 {def_ms:.3}ms / 最小 {min_ms:.3}ms（GetDevicePeriod）");
        println!(
            "采集延迟  {:.3}ms（流延迟与设备周期取 max + 积压帧）",
            cap.latency_ms()
        );
        println!();

        // 连续 read 500ms，统计「返回 0」的比例与采样速率
        println!("--- 连续 read 500ms ---");
        let mut out = Vec::new();
        let mut total = 0usize;
        let mut zeros = 0usize;
        let mut max_batch = 0usize;
        let mut read_ns_max = 0u128;
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(500) {
            let t0 = Instant::now();
            let n = match cap.read(&mut out) {
                Ok(n) => n,
                Err(e) => {
                    println!("read 失败：{e}");
                    return;
                }
            };
            read_ns_max = read_ns_max.max(t0.elapsed().as_nanos());
            if n == 0 {
                zeros += 1;
            }
            total += n;
            max_batch = max_batch.max(n);
            out.clear();
            std::thread::sleep(Duration::from_millis(1));
        }
        let elapsed = start.elapsed().as_secs_f64();
        let rate = total as f64 / elapsed;
        println!(
            "read 调用 {} 次，其中 {} 次返回 0（{:.1}%）",
            cap.read_calls(),
            zeros,
            if cap.read_calls() == 0 {
                0.0
            } else {
                zeros as f64 * 100.0 / cap.read_calls() as f64
            }
        );
        println!(
            "共 {total} 采样（{:.1} 采样/秒，期望 ≈ {}），单批最大 {max_batch}",
            rate,
            cap.format().sample_rate
        );
        println!("单次 read 最慢 {:.3}µs", read_ns_max as f64 / 1000.0);
        println!(
            "累计采集 {:.1}ms，丢弃 {} 采样",
            cap.captured_ms(),
            cap.samples_dropped()
        );
        if zeros * 2 > cap.read_calls() as usize {
            println!(
                "注意：超过一半的 read 返回 0。{}",
                if cap.mode() == CaptureMode::Loopback {
                    "loopback 在系统没放声音时**根本不推进**，这是正常状态，不是设备故障。"
                } else {
                    "麦克风路径下这通常意味着设备没在送数据（被独占？采样率不匹配？）。"
                }
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
    println!("本示例探测 WASAPI 采集设备（Windows）。其它平台没有对应 API。");
}
