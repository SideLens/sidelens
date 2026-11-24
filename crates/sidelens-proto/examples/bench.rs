//! 编码 / 解码开销基准（按载荷大小扫描）
//!
//! 用法：`cargo run --release -p sidelens-proto --example bench`
//!
//! # 为什么按大小扫描而不是只测一个尺寸
//!
//! 固定测 8KB 会看出 130+ GB/s 这种"超过物理内存带宽"的数字 —— 那不是测错了，
//! 而是 8KB 完全装进 L1/L2，测到的是缓存带宽。按大小扫描能看清两件事：
//!
//! 1. **协议自身的固定开销**（构造 32 字节头 + 边界检查），与载荷无关；
//! 2. **memcpy 的边际成本**，随载荷线性增长。
//!
//! # 关于 `black_box`
//!
//! 每轮都要真实读取产出（累加到 `sink`），否则 LTO 会证明写出的字节从未被读、
//! 把整个拷贝删掉。

use std::hint::black_box;
use std::time::Instant;

use sidelens_proto::{encode_frame, FrameDecoder, FrameSpec, FrameType};

/// 每个尺寸跑的轮数（小尺寸多跑，大尺寸少跑，总字节量大致相当）
fn rounds_for(bytes: usize) -> usize {
    match bytes {
        0 => 2_000_000,
        64 => 1_000_000,
        1024 => 500_000,
        8192 => 100_000,
        65_536 => 20_000,
        _ => 5_000,
    }
}

fn main() {
    println!(
        "{:>10}  {:>12}  {:>12}  {:>12}  {:>12}",
        "载荷", "编码 ns/帧", "解码 ns/帧", "编码 MB/s", "解码 MB/s"
    );
    println!("{}", "-".repeat(66));

    let sizes = [0usize, 64, 1024, 8192, 65_536, 262_144];

    for &size in &sizes {
        let mut payload = vec![0u8; size];
        let mut x: u32 = 0x1234_5678;
        for b in payload.iter_mut() {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            *b = (x & 0xff) as u8;
        }

        let spec = FrameSpec {
            ty: FrameType::Video,
            flags: 0,
            stream_id: 0,
            seq: 1,
            pts_us: Some(1_000_000),
            send_ts_us: Some(1_000_000),
            payload: &payload,
        };

        let rounds = rounds_for(size);
        let mut buf = Vec::new();
        for _ in 0..100 {
            encode_frame(&spec, &mut buf).unwrap();
        }

        // ---- 编码 ----
        let mut sink: u64 = 0;
        let t = Instant::now();
        for _ in 0..rounds {
            encode_frame(&spec, &mut buf).unwrap();
            sink = sink.wrapping_add(*buf.last().unwrap() as u64);
        }
        let enc = t.elapsed();

        // ---- 解码 ----
        let mut dec = FrameDecoder::new();
        let t = Instant::now();
        for _ in 0..rounds {
            dec.push(black_box(&buf));
            for m in dec.frames() {
                sink = sink.wrapping_add(dec.payload(*m).len() as u64);
            }
        }
        let de = t.elapsed();
        black_box(sink);

        let enc_ns = enc.as_nanos() as f64 / rounds as f64;
        let de_ns = de.as_nanos() as f64 / rounds as f64;
        let enc_mb = (size as f64 * rounds as f64) / 1e6 / enc.as_secs_f64();
        let de_mb = (size as f64 * rounds as f64) / 1e6 / de.as_secs_f64();

        let label = if size >= 1024 {
            format!("{} KB", size / 1024)
        } else {
            format!("{size} B")
        };

        println!("{label:>10}  {enc_ns:>12.0}  {de_ns:>12.0}  {enc_mb:>12.0}  {de_mb:>12.0}");
    }

    println!();
    println!("说明：");
    println!("  小载荷（0 B）的数字就是**协议固定开销** —— 构造 32 字节头本身。");
    println!("  大载荷的 MB/s 受内存带宽限制，不代表真实帧率上限。");
    println!("  真实场景每帧 8–16 KB，看 8 KB 那一行即可。");
}
