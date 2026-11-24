//! 帧来源端到端测试：sidelensd 侧写 → 虚拟摄像头侧读
//!
//! 这是整条 IPC 链路的真实验证：
//!
//!     ShmRing::create  +  PipeServer::create        （模拟 sidelensd）
//!     FrameSource::poll                             （虚拟摄像头侧）
//!
//! 两个线程真的跑起来，管道与共享内存都是真实的 Windows 内核对象。

#![cfg(windows)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use sidelens_ipc::ring::RingLayout;
use sidelens_ipc::wire::{FrameMeta, FrameSignal, PixelFormat};
use sidelens_ipc::{PipeServer, ShmRing};
use sidelens_vcam::frame_source::{FramePoll, FrameSource};
use sidelens_vcam::source::{Resolution, SourceConfig};

fn unique(tag: &str) -> String {
    format!(
        "sidelens-e2e-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    )
}

fn config() -> SourceConfig {
    SourceConfig {
        // 用小分辨率让测试快：160x120 NV12 = 28800 字节
        resolution: Resolution::new(160, 120),
        ..SourceConfig::default()
    }
}

/// 起一个「假 sidelensd」：建管道 + 共享内存，持续写帧
struct FakeDaemon {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<u64>>,
}

impl FakeDaemon {
    fn start(pipe_name: String, shm_name: String, cfg: SourceConfig) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let pn = pipe_name.clone();
        let sn = shm_name.clone();

        let handle = std::thread::spawn(move || {
            let frame_size = cfg.resolution.nv12_size();
            let layout = RingLayout::new(4, frame_size);
            let ring = ShmRing::create(&sn, layout).expect("建共享内存");
            let server = PipeServer::create(&pn, true).expect("建管道");

            let mut seq = 0u64;
            let mut buf = vec![0u8; frame_size];

            // 外层循环：一个客户端断开后重新 accept。
            // 真实 sidelensd 也必须这么做 —— 否则 DLL 重连时会永远等不到。
            while !stop2.load(Ordering::Relaxed) {
                match server.accept(Duration::from_millis(200)) {
                    Ok(true) => {}
                    Ok(false) => continue,
                    Err(_) => break,
                }

                // 内层循环：给这个客户端发帧，直到它断开
                while !stop2.load(Ordering::Relaxed) {
                    let v = (seq & 0xFF) as u8;
                    buf.iter_mut().for_each(|b| *b = v);

                    let mut meta = FrameMeta::new(
                        cfg.resolution.width,
                        cfg.resolution.height,
                        PixelFormat::Nv12,
                    );
                    meta.pts_us = seq * 33_333;
                    meta.keyframe = u32::from(seq % 30 == 0);

                    let slot = ring.write_frame(&buf, &meta).expect("写帧");
                    if server
                        .write_message(&FrameSignal::new(slot, meta).encode())
                        .is_err()
                    {
                        break;
                    }
                    seq += 1;
                    std::thread::sleep(Duration::from_millis(2));
                }

                // 客户端走了，拆掉这条连接，准备接受下一个
                server.disconnect();
            }
            seq
        });

        Self {
            stop,
            handle: Some(handle),
        }
    }
}

impl Drop for FakeDaemon {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

#[test]
fn frame_source_reads_frames_from_daemon() {
    let pipe = unique("pipe");
    let shm = unique("shm");
    let cfg = config();
    let _daemon = FakeDaemon::start(pipe.clone(), shm.clone(), cfg);

    // 给假 daemon 一点时间建好对象
    std::thread::sleep(Duration::from_millis(120));

    let src = FrameSource::with_names(cfg, pipe, shm);
    let mut buf = vec![0u8; cfg.resolution.nv12_size()];

    let mut got = 0;
    let mut seen_pts: Vec<i64> = Vec::new();
    for _ in 0..400 {
        match src.poll(&mut buf) {
            FramePoll::Got { len, pts_us, .. } => {
                assert_eq!(len, cfg.resolution.nv12_size(), "帧长应匹配分辨率");
                // 整帧必须是同一个字节（无撕裂）
                let first = buf[0];
                assert!(buf.iter().all(|&b| b == first), "帧被撕裂：首字节 {first}");
                seen_pts.push(pts_us);
                got += 1;
                if got >= 5 {
                    break;
                }
            }
            FramePoll::NoFrame
            | FramePoll::NotConnected
            | FramePoll::SizeMismatch
            | FramePoll::LayoutMismatch { .. } => {
                std::thread::sleep(Duration::from_millis(2));
            }
        }
    }

    assert!(got >= 5, "至少应读到 5 帧，实际 {got}");
    // 时间戳必须单调递增（允许相等，因为可能同一帧被读两次）
    for w in seen_pts.windows(2) {
        assert!(w[1] >= w[0], "pts 应单调不减：{:?}", seen_pts);
    }
    assert!(src.is_connected(), "读完后应保持连接");
    assert!(src.stats.snapshot().frames_ok >= 5);
}

#[test]
fn frame_source_reports_not_connected_without_daemon() {
    let cfg = config();
    let src = FrameSource::with_names(cfg, unique("nopipe"), unique("nosh"));
    let mut buf = vec![0u8; cfg.resolution.nv12_size()];

    for _ in 0..3 {
        assert_eq!(src.poll(&mut buf), FramePoll::NotConnected);
    }
    assert!(!src.is_connected());
    assert!(src.stats.snapshot().connect_failures >= 3);
}

#[test]
fn frame_source_rejects_wrong_resolution() {
    let pipe = unique("wrongpipe");
    let shm = unique("wrongshm");
    // daemon 写 160x120
    let daemon_cfg = config();
    let _daemon = FakeDaemon::start(pipe.clone(), shm.clone(), daemon_cfg);
    std::thread::sleep(Duration::from_millis(120));

    // 但读端以为是 320x240 —— 尺寸校验应该拦住
    let reader_cfg = SourceConfig {
        resolution: Resolution::new(320, 240),
        ..SourceConfig::default()
    };
    let src = FrameSource::with_names(reader_cfg, pipe, shm);
    let mut buf = vec![0u8; reader_cfg.resolution.nv12_size()];

    let mut saw_layout_mismatch = false;
    for _ in 0..200 {
        match src.poll(&mut buf) {
            FramePoll::LayoutMismatch { capacity, needed } => {
                assert_eq!(capacity, daemon_cfg.resolution.nv12_size());
                assert_eq!(needed, reader_cfg.resolution.nv12_size());
                assert!(needed > capacity, "读端需要的比写端提供的大");
                saw_layout_mismatch = true;
                break;
            }
            FramePoll::SizeMismatch => {
                saw_layout_mismatch = true;
                break;
            }
            _ => std::thread::sleep(Duration::from_millis(2)),
        }
    }
    assert!(
        saw_layout_mismatch,
        "布局不符时必须在连接阶段就拒绝，而不是读到越界内存"
    );
    assert!(!src.is_connected(), "布局不符不应建立连接");
}

#[test]
fn disconnect_then_reconnect_works() {
    let pipe = unique("recpipe");
    let shm = unique("recshm");
    let cfg = config();
    let _daemon = FakeDaemon::start(pipe.clone(), shm.clone(), cfg);
    std::thread::sleep(Duration::from_millis(120));

    let src = FrameSource::with_names(cfg, pipe, shm);
    let mut buf = vec![0u8; cfg.resolution.nv12_size()];

    // 先读通
    let mut ok = false;
    for _ in 0..300 {
        if matches!(src.poll(&mut buf), FramePoll::Got { .. }) {
            ok = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(ok, "首次应能读到帧");

    // 主动断开
    src.disconnect();
    assert!(!src.is_connected());

    // 应能自动重连（daemon 还在跑）
    let mut reconnected = false;
    for _ in 0..300 {
        if matches!(src.poll(&mut buf), FramePoll::Got { .. }) {
            reconnected = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(reconnected, "断开后应能自动重连并读到帧");
}
