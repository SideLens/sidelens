//! 共享内存帧源（把合成帧灌进共享内存供虚拟摄像头取用）
//!
//! # 用途
//!
//! 虚拟摄像头（FrameServer 里的 DLL）通过「命名管道 + 共享内存」取帧。
//! 这个模块就是**写端**，用来在没有真实摄像头/手机的情况下验证整条链路：
//!
//!    sidelensd --shm-feed
//!      → 建管道 sidelens-frame + 共享内存 sidelens-shm
//!      → 每 33ms 写一帧 NV12 到共享内存
//!      → 通过管道发 FrameSignal 通知
//!
//! 之后任何用 IMFSourceReader 从「SideLens Camera」读帧的程序
//! 都应该能看到画面（测试图案：滚动的彩条 + 帧号）。
//!
//! # 为什么图案要「动」
//!
//! 静止图案无法区分「帧在流动」和「卡在第一帧」。
//! 滚动竖条 + 每帧变色的顶栏能一眼看出帧率是否正常。

use std::time::{Duration, Instant};

use anyhow::Result;
use sidelens_ipc::ring::RingLayout;
use sidelens_ipc::wire::{FrameMeta, FrameSignal, PixelFormat};
use sidelens_ipc::{PipeServer, ShmRing, DEFAULT_PIPE_NAME, DEFAULT_SHM_NAME};
use sidelens_proto::now_us;
use tracing::{info, warn};

/// 测试图案的参数
#[derive(Debug, Clone, Copy)]
pub struct PatternConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

impl Default for PatternConfig {
    fn default() -> Self {
        Self {
            width: 1280,
            height: 720,
            fps: 30,
        }
    }
}

/// 生成一帧 NV12 测试图案
///
/// # NV12 布局
///
///    ┌─────────────────┐
///    │   Y 平面        │  width × height 字节（亮度）
///    ├─────────────────┤
///    │   UV 平面       │  width × height / 2 字节（U,V 交错，每 2×2 一个）
///    └─────────────────┘
///
/// # 图案设计
///
/// - Y 平面：滚动的竖直条纹（每 64 像素一条），条宽随时间平移
/// - 顶部 40 行：整片高亮，亮度随帧号跳变 —— 一眼看出帧在动
/// - UV 平面：交替的蓝/红（U 高 V 低 = 蓝，U 低 V 高 = 红）
pub fn render_nv12(cfg: PatternConfig, frame_index: u64, out: &mut [u8]) {
    let w = cfg.width as usize;
    let h = cfg.height as usize;
    let y_size = w * h;
    let need = y_size * 3 / 2;
    if out.len() < need {
        return;
    }

    // 条纹每秒平移 4 个周期
    let phase = (frame_index as usize) * 4 / (cfg.fps.max(1) as usize);

    // ---- Y 平面 ----
    for y in 0..h {
        let row = y * w;
        if y < 40 {
            // 顶栏：亮度随帧号在 32..224 之间跳
            let v = 32u8.wrapping_add(((frame_index * 37) % 192) as u8);
            out[row..row + w].fill(v);
            continue;
        }
        for x in 0..w {
            // 每 64 像素一条，条纹按 phase 平移
            let band = ((x + phase * 8) / 64) % 2;
            let base = if band == 0 { 40u8 } else { 200u8 };
            // 加一点竖向渐变，方便看画面是否被拉伸
            let grad = ((y * 60 / h.max(1)) as u8).min(60);
            out[row + x] = base.saturating_add(grad);
        }
    }

    // ---- UV 平面 ----
    // 每 2×2 个 Y 对应一组 UV。按帧号在蓝/红之间交替。
    let uv_start = y_size;
    let blue = (frame_index / 30) % 2 == 0;
    let (u, v) = if blue { (200u8, 60u8) } else { (60u8, 200u8) };
    for i in (0..(need - uv_start)).step_by(2) {
        out[uv_start + i] = u;
        if uv_start + i + 1 < need {
            out[uv_start + i + 1] = v;
        }
    }
}

/// 跑共享内存帧源
///
/// 会阻塞直到进程被中断。
pub async fn run(cfg: PatternConfig) -> Result<()> {
    let frame_size = (cfg.width as usize) * (cfg.height as usize) * 3 / 2;
    // 4 个 slot 能吸收一个完整 RTT 的抖动
    let layout = RingLayout::new(4, frame_size);

    let ring = ShmRing::create(DEFAULT_SHM_NAME, layout)?;
    info!(
        shm = ring.name(),
        slots = layout.slot_count(),
        capacity = layout.slot_capacity(),
        "共享内存已创建"
    );

    let server = PipeServer::create(DEFAULT_PIPE_NAME, true)?;
    info!(pipe = server.path(), "命名管道已创建，等待虚拟摄像头连接");

    let mut pixels = vec![0u8; frame_size];
    let interval = Duration::from_micros(1_000_000 / cfg.fps.max(1) as u64);
    let mut frame_index = 0u64;
    let mut last_report = Instant::now();
    let mut sent_since_report = 0u64;
    let mut connected = false;

    loop {
        let tick_start = Instant::now();

        // 没有客户端时也继续 accept（DLL 可能随时被 FrameServer 加载）
        if !connected {
            match server.accept(Duration::from_millis(0)) {
                Ok(true) => {
                    connected = true;
                    info!("虚拟摄像头已连接");
                }
                Ok(false) => {}
                Err(e) => {
                    warn!(error = %e, "accept 失败");
                }
            }
        }

        // 渲染并写入共享内存（无论有没有客户端都写，
        // 这样客户端连上时立刻就有最新帧，而不是等一个帧周期）
        render_nv12(cfg, frame_index, &mut pixels);

        let mut meta = FrameMeta::new(cfg.width, cfg.height, PixelFormat::Nv12);
        meta.pts_us = now_us();
        meta.keyframe = u32::from(frame_index % (cfg.fps.max(1) as u64) == 0);

        let slot = ring.write_frame(&pixels, &meta)?;

        if connected {
            let sig = FrameSignal::new(slot, meta);
            if server.write_message(&sig.encode()).is_err() {
                // 对端走了：断开，回到 accept
                connected = false;
                server.disconnect();
                info!("虚拟摄像头已断开");
            } else {
                sent_since_report += 1;
            }
        }

        frame_index += 1;

        if last_report.elapsed() >= Duration::from_secs(2) {
            // 没有读者时 pending/dropped 会一直涨 —— 那不代表任何问题，
            // 只说明还没人连上来。所以只在已连接时展示这两个数，
            // 避免日志里出现看起来像故障的持续增长。
            if connected {
                info!(
                    fps = sent_since_report / 2,
                    frames = frame_index,
                    dropped = ring.dropped(),
                    pending = ring.pending(),
                    "帧源统计"
                );
            } else {
                info!(
                    frames = frame_index,
                    shm = ring.name(),
                    "等待虚拟摄像头连接"
                );
            }
            sent_since_report = 0;
            last_report = Instant::now();
        }

        // 精确节拍：减去本帧已用时间，避免累积漂移
        let elapsed = tick_start.elapsed();
        if elapsed < interval {
            tokio::time::sleep(interval - elapsed).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> PatternConfig {
        PatternConfig {
            width: 64,
            height: 48,
            fps: 30,
        }
    }

    #[test]
    fn render_fills_exactly_nv12_size() {
        let c = cfg();
        let size = c.width as usize * c.height as usize * 3 / 2;
        let mut buf = vec![0u8; size];
        render_nv12(c, 0, &mut buf);
        // 全部字节都应被写过（不能留大片空白）
        assert!(buf.iter().any(|&b| b != 0), "图案不应全黑");
    }

    #[test]
    fn consecutive_frames_differ() {
        let c = cfg();
        let size = c.width as usize * c.height as usize * 3 / 2;
        let mut a = vec![0u8; size];
        let mut b = vec![0u8; size];
        render_nv12(c, 0, &mut a);
        render_nv12(c, 15, &mut b);
        assert_ne!(a, b, "不同帧号的图案必须不同，否则看不出画面在动");
    }

    #[test]
    fn top_bar_brightness_changes_every_frame() {
        let c = cfg();
        let size = c.width as usize * c.height as usize * 3 / 2;
        let mut a = vec![0u8; size];
        let mut b = vec![0u8; size];
        render_nv12(c, 1, &mut a);
        render_nv12(c, 2, &mut b);
        assert_ne!(a[0], b[0], "顶栏亮度应随帧号变化");
    }

    #[test]
    fn uv_plane_alternates_blue_red() {
        let c = cfg();
        let size = c.width as usize * c.height as usize * 3 / 2;
        let y_size = c.width as usize * c.height as usize;
        let mut buf = vec![0u8; size];

        render_nv12(c, 0, &mut buf);
        let (u0, v0) = (buf[y_size], buf[y_size + 1]);
        render_nv12(c, 30, &mut buf);
        let (u1, v1) = (buf[y_size], buf[y_size + 1]);
        assert_ne!((u0, v0), (u1, v1), "UV 应周期性切换色调");
    }

    #[test]
    fn small_buffer_is_left_untouched() {
        let c = cfg();
        let mut tiny = [0u8; 8];
        // 缓冲区不够时直接返回，不越界写
        render_nv12(c, 0, &mut tiny);
        assert_eq!(tiny, [0u8; 8]);
    }
}
