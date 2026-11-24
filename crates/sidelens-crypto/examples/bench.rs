//! 加密层的性能基准（不走 cargo bench，直接跑二进制）
//!
//! # 为什么要测
//!
//! ChaCha20-Poly1305 是纯软件实现（没有 AES-NI 那样的硬件加速指令，
//! 但有 AVX2/SSE 的向量化路径）。720p30 的码率约 240 KB/s，
//! 看起来完全不是瓶颈 —— 但**单帧延迟**才是关键：加密发生在
//! 「采集完成 → 发送」之间，这段耗时会直接加进端到端延迟预算。
//!
//! 所以这里测两件事：
//! 1. 吞吐（MB/s）：链路码率上限能到多少
//! 2. 单帧耗时（µs）：8KB 帧加解密各要多久，占 20ms 延迟预算的百分比

use std::time::Instant;

use sidelens_crypto::{handshake::Role, FrameSealer, Handshake};
use sidelens_proto::{encode_frame_owned, FrameSpec, FrameType};

fn paired() -> (FrameSealer, FrameSealer) {
    let a = Handshake::new_ephemeral();
    let b = Handshake::new_ephemeral();
    let ka = a.derive(&b.peer_public(), Role::Initiator).unwrap();
    let kb = b.derive(&a.peer_public(), Role::Responder).unwrap();
    (
        FrameSealer::new(ka.traffic_keys(Role::Initiator), Role::Initiator),
        FrameSealer::new(kb.traffic_keys(Role::Responder), Role::Responder),
    )
}

/// 测一组帧的加密耗时，返回 (总微秒, 总字节)
fn bench_seal(frame_bytes: usize, iterations: u32) -> (f64, usize) {
    let (mut tx, _) = paired();
    let payload = vec![0x5Au8; frame_bytes];
    // 预先把帧都编好，避免把编码时间算进加密
    let mut frames: Vec<Vec<u8>> = (0..iterations)
        .map(|i| {
            encode_frame_owned(&FrameSpec {
                ty: FrameType::Video,
                flags: 0,
                stream_id: 0,
                seq: i,
                pts_us: Some(0),
                send_ts_us: Some(0),
                payload: &payload,
            })
            .unwrap()
        })
        .collect();

    let start = Instant::now();
    for f in frames.iter_mut() {
        tx.seal_frame(f).unwrap();
    }
    let elapsed = start.elapsed();
    (
        elapsed.as_secs_f64() * 1e6,
        frames.iter().map(|f| f.len()).sum(),
    )
}

fn bench_open(frame_bytes: usize, iterations: u32) -> (f64, usize) {
    let (mut tx, mut rx) = paired();
    let payload = vec![0x5Au8; frame_bytes];
    let mut frames: Vec<Vec<u8>> = (0..iterations)
        .map(|i| {
            let mut f = encode_frame_owned(&FrameSpec {
                ty: FrameType::Video,
                flags: 0,
                stream_id: 0,
                seq: i,
                pts_us: Some(0),
                send_ts_us: Some(0),
                payload: &payload,
            })
            .unwrap();
            tx.seal_frame(&mut f).unwrap();
            f
        })
        .collect();

    let start = Instant::now();
    for f in frames.iter_mut() {
        rx.open_frame(f).unwrap();
    }
    let elapsed = start.elapsed();
    (
        elapsed.as_secs_f64() * 1e6,
        frames.iter().map(|f| f.len()).sum(),
    )
}

fn main() {
    println!("=== SideLens 加密层基准 ===");
    println!();

    // 720p 一帧 8KB 是最典型的场景
    for &(label, bytes, iters) in &[
        ("2 KB (低码率 480p)", 2048usize, 5000u32),
        ("8 KB (720p 典型)", 8192, 3000),
        ("16 KB (1080p)", 16384, 2000),
        ("64 KB (关键帧)", 65536, 500),
    ] {
        let (seal_us, seal_bytes) = bench_seal(bytes, iters);
        let (open_us, open_bytes) = bench_open(bytes, iters);

        let seal_per = seal_us / iters as f64;
        let open_per = open_us / iters as f64;
        let seal_mbps = seal_bytes as f64 / seal_us; // bytes/us = MB/s
        let open_mbps = open_bytes as f64 / open_us;

        println!("{label}  x{iters}");
        println!(
            "  加密 {seal_per:7.2} µs/帧  {seal_mbps:6.1} MB/s   → 占 20ms 预算 {:.3}%",
            seal_per / 20000.0 * 100.0
        );
        println!(
            "  解密 {open_per:7.2} µs/帧  {open_mbps:6.1} MB/s   → 占 20ms 预算 {:.3}%",
            open_per / 20000.0 * 100.0
        );
        println!();
    }

    // 分片级别的额外开销：加密后每片多 16 字节
    //
    // 分片公式（见 sidelens_core::fragment）：
    //   每片数据 = MAX_DATAGRAM(1200) - HEADER(32) - FRAG_HEADER(8) = 1160
    //   分片数   = ceil(payload_len / 1160)
    const PER_FRAG: usize = 1200 - 32 - 8;
    println!("=== 线格式开销 ===");
    println!("  每帧 +16 字节标签（AEAD tag）");
    for &plain in &[2048usize, 8192, 16384, 65536] {
        let sealed = plain + 16;
        let n_plain = plain.div_ceil(PER_FRAG);
        let n_sealed = sealed.div_ceil(PER_FRAG);
        let mark = if n_plain == n_sealed {
            "不变"
        } else {
            "**增加**"
        };
        println!("  明文 {plain:6}B → 密文 {sealed:6}B ：分片 {n_plain} → {n_sealed}（{mark}）");
    }
    println!("  结论：加密只在「明文长度正好落在分片边界前 16 字节内」时才会多一片，");
    println!("        概率约 16/1160 ≈ 1.4%，对带宽影响可忽略。");
}
