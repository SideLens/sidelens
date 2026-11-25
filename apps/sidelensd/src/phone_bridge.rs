//! 手机摄像头 → 虚拟摄像头（完整链路）
//!
//! ```text
//! 手机 Camera2 → MediaCodec → TCP → 本模块
//!                                      ↓ H.264 解码（MF）
//!                                    NV12
//!                                      ↓ ShmRing
//!                                   共享内存
//!                                      ↓ FrameSource
//!                                   DirectShow 滤镜
//!                                      ↓
//!                                   Zoom / OBS / 浏览器
//! ```
//!
//! # 与 `phone_recv` 的区别
//!
//! | 模式 | 输出 | 用途 |
//! | --- | --- | --- |
//! | `--phone-recv` | .h264 文件 | 验证链路、离线分析 |
//! | `--phone-to-vcam` | 共享内存 | **让应用真的用上** |
//!
//! # 为什么在这里解码而不是让消费端解
//!
//! DirectShow 滤镜输出的是**原始 NV12**（摄像头类设备的约定）。
//! 消费端（Zoom）拿到的是解码后的像素，它不知道 H.264 的存在。
//! 所以解码必须在滤镜之前 —— 也就是这一层。

#![allow(unsafe_code)]

use std::io::Read;
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use sidelens_ipc::wire::{FrameMeta, FrameSignal, PixelFormat};
use sidelens_ipc::{PipeServer, ShmRing, DEFAULT_PIPE_NAME, DEFAULT_SHM_NAME};

/// 帧头：4 + 8 + 1
const HEADER_SIZE: usize = 13;

pub fn run(port: u16, width: u32, height: u32, fps: u32) -> Result<()> {
    println!("=== 手机摄像头 → 虚拟摄像头 ===");
    println!("监听 0.0.0.0:{port}");
    println!("输出 {width}x{height}@{fps} → 共享内存");
    println!();

    // 1. 建共享内存环与管道（DirectShow 滤镜会连上来）
    let frame_size = (width as usize) * (height as usize) * 3 / 2;
    // 4 个 slot 能吸收一个完整 RTT 的抖动（与 shm_feed 一致）
    let layout = sidelens_ipc::RingLayout::new(4, frame_size);
    let ring = Arc::new(ShmRing::create(DEFAULT_SHM_NAME, layout).context("创建共享内存失败")?);
    println!("共享内存已建: {DEFAULT_SHM_NAME}");

    let server = Arc::new(Mutex::new(
        PipeServer::create(DEFAULT_PIPE_NAME, true).context("创建管道失败")?,
    ));
    println!("管道已建: {DEFAULT_PIPE_NAME}");
    println!("（打开 Zoom / OBS，选「SideLens Camera」即可看到画面）");
    println!();

    // 2. 监听手机。手机端可以随时停止并重新开始共享，因此服务器必须
    //    在每个 TCP 会话结束后继续 accept，而不是跟着一起退出。
    let listener =
        TcpListener::bind(("0.0.0.0", port)).with_context(|| format!("无法监听端口 {port}"))?;
    println!("等待手机连接…");

    let connected = Arc::new(AtomicBool::new(false));
    // 命名管道连接独立于解码循环等待，避免消费者连得稍晚时阻塞解码，
    // 也避免零超时 accept 与客户端连接时序发生竞争。
    {
        let accept_server = Arc::clone(&server);
        let accept_connected = Arc::clone(&connected);
        std::thread::Builder::new()
            .name("sidelens-pipe-accept".into())
            .spawn(move || {
                let mut errors = 0u64;
                loop {
                    if accept_connected.load(Ordering::Acquire) {
                        std::thread::sleep(Duration::from_millis(10));
                        continue;
                    }

                    let result = match accept_server.lock() {
                        Ok(server) => server.accept(Duration::from_millis(250)),
                        Err(_) => {
                            eprintln!("命名管道状态锁中毒，停止等待摄像头连接");
                            return;
                        }
                    };

                    match result {
                        Ok(true) => {
                            accept_connected.store(true, Ordering::Release);
                            println!("✅ 滤镜已连接，开始转发手机帧");
                            errors = 0;
                        }
                        Ok(false) => {}
                        Err(e) => {
                            errors += 1;
                            if errors == 1 || errors % 100 == 0 {
                                eprintln!("等待滤镜管道连接失败（第 {errors} 次）: {e}");
                            }
                            std::thread::sleep(Duration::from_millis(10));
                        }
                    }
                }
            })
            .context("启动命名管道连接线程失败")?;
    }

    let frames = Arc::new(AtomicU64::new(0));
    let decoded = Arc::new(AtomicU64::new(0));
    let started = Instant::now();

    loop {
        let (mut stream, peer) = listener.accept().context("accept 失败")?;
        stream.set_nodelay(true).ok();
        println!("✅ 手机已连接: {peer}");

        // 每次共享都是新的 H.264 会话，重建解码器以重新接收 SPS/PPS。
        let cfg = sidelens_codec_mf::DecoderConfig {
            width,
            height,
            fps,
            ..Default::default()
        };
        let mut decoder = match sidelens_codec_mf::H264Decoder::new(cfg) {
            Ok(decoder) => decoder,
            Err(e) => {
                eprintln!("创建解码器失败: {e:#}");
                continue;
            }
        };
        println!("解码器: {}", decoder.decoder_name());

        let mut header = [0u8; HEADER_SIZE];
        let mut nv12 = Vec::new();

        loop {
            // 读帧头
            match stream.read_exact(&mut header) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    println!("手机断开连接，等待重新共享…");
                    break;
                }
                Err(e) => {
                    eprintln!("读帧头失败: {e}，等待重新连接…");
                    break;
                }
            }

            let len = u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as usize;
            let pts = i64::from_be_bytes([
                header[4], header[5], header[6], header[7], header[8], header[9], header[10],
                header[11],
            ]);
            let is_key = header[12] & 1 != 0;

            if len == 0 || len > 8 * 1024 * 1024 {
                eprintln!("非法帧长度 {len}，关闭本次连接");
                break;
            }

            let mut data = vec![0u8; len];
            if let Err(e) = stream.read_exact(&mut data) {
                eprintln!("读帧数据失败: {e}，等待重新连接…");
                break;
            }
            frames.fetch_add(1, Ordering::Relaxed);

            // 解码
            nv12.clear();
            if let Err(e) = decoder.decode_into(&data, pts, &mut nv12) {
                eprintln!("解码失败（帧 {}）: {e}", frames.load(Ordering::Relaxed));
                continue;
            }
            if nv12.is_empty() {
                continue; // 编码器可能只出了参数集，还没出画面
            }

            // 灌进共享内存
            let mut meta = FrameMeta::new(width, height, PixelFormat::Nv12);
            meta.pts_us = pts.max(0) as u64;
            meta.keyframe = u32::from(is_key);

            let slot = match ring.write_frame(&nv12, &meta) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("写共享内存失败: {e}");
                    continue;
                }
            };

            // 通知滤镜（如果已连接）
            if connected.load(Ordering::Relaxed) {
                let sig = FrameSignal::new(slot, meta);
                let result = match server.lock() {
                    Ok(server) => {
                        let result = server.write_message(&sig.encode());
                        if result.is_err() {
                            server.disconnect();
                        }
                        result
                    }
                    Err(_) => Err(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        "命名管道状态锁中毒",
                    )),
                };
                if let Err(e) = result {
                    connected.store(false, Ordering::Relaxed);
                    println!("滤镜管道写入失败，等待重连: {e}");
                }
            }

            decoded.fetch_add(1, Ordering::Relaxed);

            let n = decoded.load(Ordering::Relaxed);
            if n % 30 == 0 {
                let secs = started.elapsed().as_secs_f64();
                println!(
                    "  解码 {n} 帧  {:.1} fps  {:.1} Mbps",
                    n as f64 / secs,
                    (frames.load(Ordering::Relaxed) as f64 * 20_000.0 * 8.0) / secs / 1_000_000.0
                );
            }
        }
    }
}
