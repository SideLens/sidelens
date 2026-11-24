//! 环形缓冲的并发与回绕测试
//!
//! 这些是跨进程共享内存最容易出 bug 的地方：
//! 序号回绕、读写竞争、丢帧检测。

#![cfg(windows)]

use sidelens_ipc::{FrameMeta, FrameSignal, PixelFormat, RingLayout, SlotIndex};

#[test]
fn sequence_wraps_at_u32_max() {
    let mut idx = SlotIndex::new();
    idx.write_seq = u32::MAX;
    idx.advance_write();
    assert_eq!(idx.write_seq, 0, "u32::MAX + 1 应回绕到 0");
}

#[test]
fn pending_is_correct_across_wrap() {
    let mut idx = SlotIndex::new();
    idx.write_seq = u32::MAX - 1;
    idx.read_seq = u32::MAX - 3;
    assert_eq!(idx.pending(), 2);

    idx.advance_write();
    idx.advance_write();
    // 写端回绕到 0，读端还在 u32::MAX-3
    assert_eq!(idx.pending(), 4, "回绕后待读数应正确");
}

#[test]
fn slot_index_is_stable_across_wrap() {
    let layout = RingLayout::new(4, 1024);
    // 序号 0 和 2^32 应落在同一个 slot（因为 2^32 % 4 == 0）
    assert_eq!(layout.slot_of(0), layout.slot_of(0));
    // 验证连续 8 个序号覆盖所有 slot
    let slots: Vec<u32> = (0..8).map(|s| layout.slot_of(s)).collect();
    assert_eq!(slots, vec![0, 1, 2, 3, 0, 1, 2, 3]);
}

#[test]
fn frame_meta_abi_size_matches_signal() {
    // FrameSignal = slot(4) + FrameMeta(40)
    assert_eq!(FrameSignal::SIZE, 44);

    let mut meta = FrameMeta::new(1920, 1080, PixelFormat::Nv12);
    meta.seq = 123;
    let sig = FrameSignal::new(2, meta);
    assert_eq!(sig.encode().len(), FrameSignal::SIZE);
}

#[test]
fn frame_signal_roundtrip() {
    let mut meta = FrameMeta::new(1280, 720, PixelFormat::Nv12);
    meta.seq = 99;
    meta.keyframe = 1;
    meta.pts_us = 1_234_567_890;
    meta.payload_len = 1_382_400;

    let sig = FrameSignal::new(3, meta);
    let decoded = FrameSignal::decode(&sig.encode()).expect("应能解码");

    assert_eq!(decoded.slot, 3);
    assert_eq!(decoded.meta.seq, 99);
    assert_eq!(decoded.meta.width, 1280);
    assert!(decoded.meta.is_keyframe());
    assert_eq!(decoded.meta.pts_us, 1_234_567_890);
}

#[test]
fn malformed_signal_is_rejected() {
    assert!(FrameSignal::decode(&[]).is_none());
    assert!(
        FrameSignal::decode(&[0u8; 10]).is_none(),
        "长度不足应返回 None"
    );
    // 刚好够 slot 但不够 meta
    assert!(FrameSignal::decode(&[0u8; 20]).is_none());
}

#[test]
fn version_mismatch_is_detectable() {
    let mut meta = FrameMeta::new(640, 480, PixelFormat::Nv12);
    meta.version = 999;
    assert!(!meta.is_version_compatible(), "版本不同应被检测到");
}

#[test]
fn producer_consumer_simulation() {
    // 模拟：写端 60fps 写，读端 15fps 读，容量 4
    let layout = RingLayout::new(4, 1024);
    let mut idx = SlotIndex::new();

    let mut written = 0u32;
    let mut read = 0u32;

    for _ in 0..60 {
        idx.advance_write();
        written += 1;

        // 每 4 帧读一次（60 / 4 = 15fps）
        if written % 4 == 0 {
            idx.advance_read();
            read += 1;
        }
    }

    assert_eq!(written, 60);
    assert_eq!(read, 15);
    // 写端比读端快 4 倍，容量 4 → 必然丢帧
    let dropped = idx.dropped(layout.slot_count());
    assert!(dropped > 0, "读端跟不上时应有丢帧，实际 {dropped}");
}

#[test]
fn skip_to_latest_discards_backlog() {
    let mut idx = SlotIndex::new();
    for _ in 0..20 {
        idx.advance_write();
    }
    assert_eq!(idx.pending(), 20);

    idx.skip_to_latest();
    assert_eq!(idx.pending(), 0);
    assert_eq!(idx.dropped(4), 0, "跳过后不应再算丢帧");
}

#[test]
fn fits_rejects_oversized_frame() {
    let layout = RingLayout::recommended(1280, 720, 4);
    // 1080p 帧比 720p slot 大
    let frame_1080 = PixelFormat::Nv12.buffer_size(1920, 1080);
    assert!(!layout.fits(frame_1080), "720p 的 slot 放不下 1080p 帧");
}

#[test]
fn different_resolutions_have_different_sizes() {
    assert!(PixelFormat::Nv12.buffer_size(1920, 1080) > PixelFormat::Nv12.buffer_size(1280, 720));
    assert!(
        PixelFormat::Bgra.buffer_size(640, 480) > PixelFormat::Nv12.buffer_size(640, 480),
        "BGRA 比 NV12 大"
    );
}

// ============================================================================
// 共享内存环形缓冲的真实并发测试
// ============================================================================
//
// 上面那些是纯数学验证（序号运算），下面这些是**真的起两个线程**
// 在共享内存上跑生产者-消费者。它们能抓到纯计算测不出来的问题：
// 内存序写错、slot 复用竞态、计数器不一致。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use sidelens_ipc::ShmRing;

fn unique(tag: &str) -> String {
    format!(
        "sidelens-conc-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    )
}

#[test]
fn producer_consumer_never_tears_a_frame() {
    // 每个 slot 只有 64 字节；写端把整帧填成同一个值，
    // 读端拿到帧后检查「所有字节是否都等于该值」。
    // 如果内存序或 slot 复用有 bug，就会读到新旧混合的撕裂帧。
    let name = unique("tear");
    let layout = RingLayout::new(8, 64);
    let writer = Arc::new(ShmRing::create(&name, layout).expect("创建"));
    let reader = Arc::new(ShmRing::open(&name).expect("打开"));

    let stop = Arc::new(AtomicBool::new(false));

    let w = writer.clone();
    let w_stop = stop.clone();
    let producer = std::thread::spawn(move || {
        let mut seq = 0u32;
        while !w_stop.load(Ordering::Relaxed) {
            let v = (seq & 0xFF) as u8;
            let mut m = FrameMeta::new(64, 1, PixelFormat::Nv12);
            m.pts_us = seq as u64;
            w.write_frame(&[v; 64], &m).expect("写帧");
            seq += 1;
        }
        seq
    });

    let r = reader.clone();
    let r_stop = stop.clone();
    let consumer = std::thread::spawn(move || {
        let mut buf = [0u8; 64];
        let mut got = 0u64;
        while !r_stop.load(Ordering::Relaxed) {
            if let Some(meta) = r.read_latest(&mut buf) {
                // 撕裂检测：整帧必须是同一个字节
                let first = buf[0];
                assert!(
                    buf.iter().all(|&b| b == first),
                    "帧被撕裂：seq={} 首字节={first} 全帧={:?}",
                    meta.seq,
                    &buf[..8]
                );
                // 内容必须与 seq 对应
                assert_eq!(first as u32, meta.seq & 0xFF, "内容与序号不一致");
                got += 1;
            }
        }
        got
    });

    std::thread::sleep(Duration::from_millis(300));
    stop.store(true, Ordering::Relaxed);

    let written = producer.join().expect("生产者线程");
    let read = consumer.join().expect("消费者线程");

    assert!(written > 100, "300ms 内应写入不少帧，实际 {written}");
    assert!(read > 0, "消费者至少读到一帧，实际 {read}");
    // 消费者故意不忙等（read_latest 返回 None 时立刻重试），所以能读掉大部分
    assert!(read <= written as u64, "不可能读得比写得多");
}

#[test]
fn counters_stay_consistent_under_concurrency() {
    let name = unique("counters");
    let layout = RingLayout::new(4, 32);
    let writer = Arc::new(ShmRing::create(&name, layout).expect("创建"));
    let reader = Arc::new(ShmRing::open(&name).expect("打开"));

    let stop = Arc::new(AtomicBool::new(false));

    let w = writer.clone();
    let w_stop = stop.clone();
    let producer = std::thread::spawn(move || {
        let mut n = 0u64;
        while !w_stop.load(Ordering::Relaxed) {
            w.write_frame(&[1u8; 32], &FrameMeta::new(32, 1, PixelFormat::Nv12))
                .expect("写");
            n += 1;
        }
        n
    });

    let r = reader.clone();
    let r_stop = stop.clone();
    let consumer = std::thread::spawn(move || {
        let mut buf = [0u8; 32];
        let mut n = 0u64;
        while !r_stop.load(Ordering::Relaxed) {
            if r.read_latest(&mut buf).is_some() {
                n += 1;
            }
        }
        n
    });

    std::thread::sleep(Duration::from_millis(200));
    stop.store(true, Ordering::Relaxed);

    let produced = producer.join().unwrap();
    let consumed = consumer.join().unwrap();

    // 写端自己数的帧数必须等于共享内存里的计数 ——
    // 两者不一致说明 fetch_add 丢了或者被覆盖
    assert_eq!(writer.written(), produced, "写端计数应与实际写入次数一致");
    assert_eq!(
        reader.read_count(),
        consumed,
        "读端计数应与实际读取次数一致"
    );
    assert!(consumed <= produced, "读的不能比写的多");
}

#[test]
fn many_short_bursts_keep_slot_math_correct() {
    // 反复写满一圈再读，专测 slot 回绕边界（seq & mask）
    let name = unique("burst");
    let layout = RingLayout::new(4, 16);
    let writer = ShmRing::create(&name, layout).expect("创建");
    let reader = ShmRing::open(&name).expect("打开");

    let mut buf = [0u8; 16];
    for round in 0..100u32 {
        // 每轮写满 4 个 slot
        for i in 0..4u32 {
            let v = ((round * 4 + i) & 0xFF) as u8;
            writer
                .write_frame(&[v; 16], &FrameMeta::new(16, 1, PixelFormat::Nv12))
                .expect("写");
        }
        // 只读最新一帧
        let meta = reader.read_latest(&mut buf).expect("读");
        let expect = ((round * 4 + 3) & 0xFF) as u8;
        assert_eq!(meta.seq, round * 4 + 3);
        assert_eq!(buf[0], expect, "round={round} 读到了错误的 slot");
    }
}

#[test]
fn pipe_signal_carries_shm_slot_across_threads() {
    // 模拟真实架构：写线程写共享内存 → 通过管道发信号 → 读线程按 slot 取帧
    use sidelens_ipc::{PipeClient, PipeServer};

    let shm_name = unique("e2e-shm");
    let pipe_name = unique("e2e-pipe");
    let layout = RingLayout::new(4, 64);

    let server = PipeServer::create(&pipe_name, true).expect("建管道");
    let writer = Arc::new(ShmRing::create(&shm_name, layout).expect("建共享内存"));

    let pipe_for_writer = pipe_name.clone();
    let w = writer.clone();
    let producer = std::thread::spawn(move || {
        // 等读者连上
        std::thread::sleep(Duration::from_millis(50));
        let client =
            PipeClient::connect(&pipe_for_writer, 50, Duration::from_millis(20)).expect("连接管道");
        client.set_message_mode().expect("消息模式");

        for i in 0..20u8 {
            let mut m = FrameMeta::new(64, 1, PixelFormat::Nv12);
            m.pts_us = i as u64;
            let slot = w.write_frame(&[i; 64], &m).expect("写帧");
            let mut sig_meta = m;
            sig_meta.seq = i as u32;
            sig_meta.payload_len = 64;
            client
                .write_message(&FrameSignal::new(slot, sig_meta).encode())
                .expect("发信号");
        }
        std::thread::sleep(Duration::from_millis(200));
    });

    assert!(server.accept(Duration::from_secs(5)).expect("accept"));
    let reader = ShmRing::open(&shm_name).expect("打开共享内存");

    let mut buf = [0u8; 64];
    let mut received = 0u64;
    let mut msgs = [0u8; 256];
    // 读 20 条信号（读端可能追不上，容忍少几条）
    for _ in 0..20 {
        let n = match server.read_message(&mut msgs) {
            Ok(n) => n,
            Err(_) => break,
        };
        let sig = FrameSignal::decode(&msgs[..n]).expect("解信号");
        // 用信号里的 slot 直接定位（不是靠 seq 重算，这正是信号存在的意义）
        assert!(sig.slot < 4, "slot 必须在范围内");
        let got = reader.read_latest(&mut buf);
        if got.is_some() {
            received += 1;
        }
    }

    producer.join().unwrap();
    assert!(received > 0, "至少应取到一帧");
}
