//! 手机帧接收端（验证用）
//!
//! # 为什么单独一个模式
//!
//! 现有 `sidelensd` 走的是 `sidelens-proto` 的 UDP 协议（分片 + NACK + FEC）。
//! 但手机端第一步只需要验证「摄像头 → 编码 → 网络 → 电脑」这条链，
//! 用的是裸 TCP（见 `FrameSender.kt` 里的说明）。
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
//! ```text
//! [4B 长度 BE][8B ptsUs BE][1B flags][N B H.264 数据]
//! ```
//!
//! flags: bit0 = 关键帧。

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;

use anyhow::{Context, Result};

/// 帧头：4 + 8 + 1
const HEADER_SIZE: usize = 13;

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

    let mut header = [0u8; HEADER_SIZE];
    let mut frames = 0u64;
    let mut bytes = 0u64;
    let mut keyframes = 0u64;
    let mut first_pts = None::<i64>;
    let started = std::time::Instant::now();

    loop {
        // 读帧头：要么读满 13 字节，要么对端关闭
        match stream.read_exact(&mut header) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                println!("对端关闭连接");
                break;
            }
            Err(e) => {
                eprintln!("读帧头失败: {e}");
                break;
            }
        }

        let len = u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as usize;
        let pts = i64::from_be_bytes([
            header[4], header[5], header[6], header[7], header[8], header[9], header[10],
            header[11],
        ]);
        let flags = header[12];
        let is_key = flags & 1 != 0;

        if len == 0 || len > 8 * 1024 * 1024 {
            eprintln!("非法帧长度 {len}，断开");
            break;
        }

        let mut data = vec![0u8; len];
        if let Err(e) = stream.read_exact(&mut data) {
            eprintln!("读帧数据失败: {e}");
            break;
        }

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
        bytes += len as u64;
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
