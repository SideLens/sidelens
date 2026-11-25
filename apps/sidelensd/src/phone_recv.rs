//! 手机帧接收端（验证用）
//!
//! # 为什么单独一个模式
//!
//! 现有 `sidelensd` 走的是 `sidelens-proto` 的 UDP 协议（分片 + NACK + FEC）。
//! 但手机端第一步只需要验证「摄像头 → 编码 → 网络 → 电脑」这条链，
//! 使用 `sidelens-phone-proto` 定义的版本化 TCP 消息。
//!
//! 这个模式收的就是那条裸 TCP 流，把 H.264 存成文件，再用现有工具验证：
//!
//! ```text
//! sidelensd --phone-recv 27300
//!   → target/phone-capture.h264
//!   → tools/h264-inspect.mjs 可以解析
//! ```
//!
//! # 线格式
//!
//! `u8 类型 | u32 长度 (LE) | payload`。CONFIG 与 FRAME 载荷见协议 crate。

use std::io::Write;
use std::net::TcpListener;
use std::path::PathBuf;

use anyhow::{Context, Result};
use sidelens_phone_proto::{read_message, MessageType, VideoCodec, VideoConfig, VideoFrame};

pub fn run(port: u16, out_path: Option<PathBuf>) -> Result<()> {
    let out = out_path.unwrap_or_else(|| PathBuf::from("target/phone-capture.h264"));

    println!("=== 手机帧接收端 ===");
    println!("监听 0.0.0.0:{port}");
    println!("输出 {out:?}");
    println!();
    println!("手机端准备：");
    println!("  1. adb reverse tcp:{port} tcp:{port}");
    println!("  2. 打开 SideLens app → 开始共享");
    println!();

    let listener =
        TcpListener::bind(("0.0.0.0", port)).with_context(|| format!("无法监听端口 {port}"))?;
    println!("等待手机连接…");

    let (mut stream, peer) = listener.accept().context("accept 失败")?;
    stream.set_nodelay(true).ok();
    println!("✅ 已连接: {peer}");

    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let mut file =
        std::fs::File::create(&out).with_context(|| format!("无法创建输出文件 {out:?}"))?;

    let mut frames = 0u64;
    let mut bytes = 0u64;
    let mut keyframes = 0u64;
    let mut first_pts = None::<i64>;
    let started = std::time::Instant::now();

    loop {
        let message = match read_message(&mut stream) {
            Ok(Some(message)) => message,
            Ok(None) => {
                println!("对端关闭连接");
                break;
            }
            Err(e) => {
                eprintln!("读取协议消息失败: {e}");
                break;
            }
        };

        let (pts, is_key, data) = match message.kind {
            MessageType::Hello => {
                println!("HELLO {}", String::from_utf8_lossy(&message.payload));
                continue;
            }
            MessageType::Config => match VideoConfig::parse(&message.payload) {
                Ok((config, codec_data)) if config.codec == VideoCodec::H264 => {
                    println!("CONFIG H.264 {}x{}", config.width, config.height);
                    if !codec_data.is_empty() {
                        file.write_all(codec_data)
                            .context("写入 H.264 参数集失败")?;
                    }
                    continue;
                }
                Ok((config, _)) => {
                    eprintln!("不支持视频编码 {:?}", config.codec);
                    continue;
                }
                Err(e) => {
                    eprintln!("CONFIG 无效: {e}");
                    continue;
                }
            },
            MessageType::Frame => match VideoFrame::parse(&message.payload) {
                Ok(frame) => (
                    frame.pts_us as i64,
                    frame.is_keyframe(),
                    frame.data.to_vec(),
                ),
                Err(e) => {
                    eprintln!("FRAME 无效: {e}");
                    continue;
                }
            },
            MessageType::State => {
                println!("STATE {}", String::from_utf8_lossy(&message.payload));
                continue;
            }
            MessageType::AudioConfig | MessageType::Audio => {
                eprintln!("收到音频消息，当前接收模式只存 H.264 视频");
                continue;
            }
            MessageType::Control | MessageType::Ping => continue,
        };

        if first_pts.is_none() {
            first_pts = Some(pts);
            println!("首帧到达，开始写文件");
        }

        // 直接写 Annex-B 原始流，tools/h264-inspect.mjs 能解析
        if let Err(e) = file.write_all(&data) {
            eprintln!("写文件失败: {e}");
            break;
        }

        frames += 1;
        bytes += data.len() as u64;
        if is_key {
            keyframes += 1;
        }

        if frames % 30 == 0 {
            let elapsed = started.elapsed().as_secs_f64();
            println!(
                "  帧 {frames}  关键帧 {keyframes}  {:.1} KB  {:.1} fps  {:.2} Mbps",
                bytes as f64 / 1024.0,
                frames as f64 / elapsed,
                (bytes as f64 * 8.0) / elapsed / 1_000_000.0
            );
        }
    }

    file.flush().ok();

    println!();
    println!("=== 统计 ===");
    println!("帧数    {frames}");
    println!("关键帧  {keyframes}");
    println!("字节    {} ({:.1} KB)", bytes, bytes as f64 / 1024.0);
    if let Some(p) = first_pts {
        println!("首帧 PTS {p} µs");
    }
    println!("输出    {out:?}");
    println!();
    if frames > 0 {
        println!("验证码流：");
        println!("  node tools/h264-inspect.mjs {out:?} --summary");
    }
    Ok(())
}
