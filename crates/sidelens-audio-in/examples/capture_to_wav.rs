//! 采集 3 秒写入 WAV 文件，打印 RMS 证明真的有信号
//!
//! 为什么用 RMS 而不是「文件大小」：文件大小只能证明**缓冲区被填过**，
//! 全 0 的缓冲也是那个大小。RMS 为 0 说明链路是死的（设备没数据、
//! 或者转换把信号吃掉了）；loopback 下系统在放音乐时 RMS 必然 > 0。
//!
//! ~~~text
//! cargo run --release -p sidelens-audio-in --example capture_to_wav
//! ~~~
//!
//! 输出文件默认 §capture_mic.wav§ / §capture_loopback.wav§（当前目录）。
//! WAV 头自己写 44 字节，不引第三方 crate。

// 平台门控：主体依赖 WASAPI。不能用 `#![cfg(windows)]`（会连 main 一起删掉）。
#[cfg(windows)]
mod imp {
    use std::fs::File;
    use std::io::{BufWriter, Write};
    use std::time::{Duration, Instant};

    use sidelens_audio_in::{AudioCapture, CaptureMode, PcmFormat};

    /// 采集时长
    const SECONDS: f64 = 3.0;

    pub fn main() {
        let format = PcmFormat::default();
        println!("=== 采集 {SECONDS:.0} 秒到 WAV（{}） ===", format);

        let mic = capture_one("capture_mic.wav", CaptureMode::Microphone, || {
            AudioCapture::new(format)
        });
        println!();
        let loopback = capture_one("capture_loopback.wav", CaptureMode::Loopback, || {
            AudioCapture::loopback(format)
        });

        if mic.is_none() && loopback.is_none() {
            println!();
            println!("两个设备都打不开 —— 本机没有可用的采集端点。");
            println!("这时发送端应降级为 AudioCapture::headless()（只发静音）。");
        }
    }

    /// 采集一段并写 WAV，返回 (采样数, RMS)
    fn capture_one<F>(file_name: &str, mode: CaptureMode, open: F) -> Option<(usize, f64)>
    where
        F: FnOnce() -> Result<AudioCapture, sidelens_audio_in::CaptureError>,
    {
        println!("--- {} ---", mode.as_str());
        let mut cap = match open() {
            Ok(c) => c,
            Err(e) => {
                println!("打开失败：{e}");
                return None;
            }
        };
        if let Some(dev) = cap.device_format() {
            println!("设备格式 {dev}，缓冲 {} 帧", cap.buffer_frames());
        }
        println!("采集延迟 {:.3}ms", cap.latency_ms());

        let mut pcm: Vec<i16> = Vec::with_capacity(format_samples());
        let mut out: Vec<i16> = Vec::with_capacity(8192);
        let target = (SECONDS * cap.format().sample_rate as f64) as usize;
        let deadline = Instant::now() + Duration::from_secs_f64(SECONDS + 0.5);
        let mut zeros = 0u64;
        let mut calls = 0u64;
        let wall_start = Instant::now();

        while pcm.len() < target && Instant::now() < deadline {
            match cap.read(&mut out) {
                Ok(0) => zeros += 1,
                Ok(_) => {
                    pcm.extend_from_slice(&out);
                    out.clear();
                }
                Err(e) => {
                    println!("read 失败：{e}");
                    return None;
                }
            }
            calls += 1;
            std::thread::sleep(Duration::from_millis(1));
        }

        if pcm.is_empty() {
            println!(
                "{} 内没采到任何数据（{calls} 次 read 全部返回 0）",
                SECONDS + 0.5
            );
            if mode == CaptureMode::Loopback {
                println!("loopback 在系统没放声音时**不产生数据**，这是正常状态。");
                println!("放一段音乐再跑本示例，RMS 会变成 > 0。");
            }
            return None;
        }

        let rms = rms_of(&pcm);
        let peak = peak_of(&pcm);
        let clipped = pcm
            .iter()
            .filter(|&&s| s == i16::MAX || s == i16::MIN)
            .count();
        let wall = wall_start.elapsed().as_secs_f64();
        println!(
            "采到 {} 采样（音频时长 {:.2}s / 墙上时间 {:.2}s），RMS {rms:.1}，peak {peak}，削顶 {clipped}",
            pcm.len(),
            cap.captured_ms() / 1000.0,
            wall
        );
        // loopback 下「音频时长 < 墙上时间」是正常现象：系统没在放声音时
        // 引擎根本不推进，那段时间没有数据可采。差值就是「静音占比」。
        if wall > 0.0 && cap.captured_ms() / 1000.0 < wall * 0.9 {
            println!(
                "注意：音频时长只占墙上时间的 {:.0}% —— 其余时间系统没有声音在播放。",
                cap.captured_ms() / 1000.0 / wall * 100.0
            );
        }
        println!(
            "read {} 次，返回 0 的有 {zeros} 次，丢弃 {} 采样",
            cap.read_calls(),
            cap.samples_dropped()
        );

        match write_wav(file_name, &cap.format(), &pcm) {
            Ok(bytes) => println!("已写入 {file_name}（{bytes} 字节）"),
            Err(e) => println!("写文件失败：{e}"),
        }
        Some((pcm.len(), rms))
    }

    fn format_samples() -> usize {
        (SECONDS * 48_000.0) as usize + 4_800
    }

    fn rms_of(pcm: &[i16]) -> f64 {
        if pcm.is_empty() {
            return 0.0;
        }
        let sum: f64 = pcm.iter().map(|&s| (s as f64) * (s as f64)).sum();
        (sum / pcm.len() as f64).sqrt()
    }

    fn peak_of(pcm: &[i16]) -> u16 {
        pcm.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0)
    }

    /// 写 44 字节标准 WAV 头 + PCM 数据
    ///
    /// 头是 §RIFF / fmt / data§ 三段，全部小端。
    /// 采样类型固定 16bit int（本 crate 的输出格式），所以
    /// §bitsPerSample = 16§、§blockAlign = channels * 2§。
    fn write_wav(path: &str, format: &PcmFormat, pcm: &[i16]) -> std::io::Result<u64> {
        let data_bytes = (pcm.len() * 2) as u32;
        let block_align = format.channels * 2;
        let byte_rate = format.sample_rate * block_align as u32;

        let mut w = BufWriter::new(File::create(path)?);
        // RIFF 头
        w.write_all(b"RIFF")?;
        // 整个文件大小 - 8
        w.write_all(&(36 + data_bytes).to_le_bytes())?;
        w.write_all(b"WAVE")?;
        // fmt 块
        w.write_all(b"fmt ")?;
        w.write_all(&16u32.to_le_bytes())?; // fmt 块大小
        w.write_all(&1u16.to_le_bytes())?; // PCM = 1（非 IEEE float）
        w.write_all(&format.channels.to_le_bytes())?;
        w.write_all(&format.sample_rate.to_le_bytes())?;
        w.write_all(&byte_rate.to_le_bytes())?;
        w.write_all(&block_align.to_le_bytes())?;
        w.write_all(&16u16.to_le_bytes())?; // bits per sample
                                            // data 块
        w.write_all(b"data")?;
        w.write_all(&data_bytes.to_le_bytes())?;
        for s in pcm {
            w.write_all(&s.to_le_bytes())?;
        }
        w.flush()?;
        Ok(44 + data_bytes as u64)
    }
}

#[cfg(windows)]
pub fn main() {
    imp::main();
}

#[cfg(not(windows))]
fn main() {
    println!("本示例采集 WASAPI 音频并写 WAV（Windows）。其它平台没有对应 API。");
}
