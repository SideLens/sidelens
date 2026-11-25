//! 共享内存帧源探针（读端验证）
//!
//! 用 [sidelens_vcam::frame_source::FrameSource] 从正在运行的
//! sidelensd --shm-feed 里读帧，验证整条 IPC 链路：
//!
//!    sidelensd --shm-feed          sidelensd --shm-probe
//!    ─────────────────────         ─────────────────────
//!    ShmRing::write_frame      ──→ FrameSource::poll
//!    PipeServer::write_message ──→
//!
//! 这是**跨进程**的真实验证 —— 和虚拟摄像头 DLL 走的是完全相同的代码路径。

use std::time::{Duration, Instant};

use anyhow::Result;
use sidelens_vcam::frame_source::{FramePoll, FrameSource};
use sidelens_vcam::source::{Resolution, SourceConfig};
use tracing::{info, warn};

/// 跑探针。
///
/// frames 是目标读取帧数，width/height 必须与写端一致。
/// `dump` 非空时把**最后一帧**的 NV12 原样写盘。
///
/// 为什么要这个：`distinct_frames` 只能证明"字节在变"，证明不了"画面不是黑的"。
/// 排查"手机在推流但虚拟摄像头里一片黑"这类问题时，必须能把像素拿出来看。
/// 写的是原始 NV12（不经转换），避免转换层的问题掩盖上游的问题；
/// 需要看图时用 `tools/nv12-to-png.mjs` 转。
pub async fn run_with_dump(
    width: u32,
    height: u32,
    frames: u64,
    dump: Option<&std::path::Path>,
) -> Result<()> {
    let cfg = SourceConfig {
        resolution: Resolution::new(width, height),
        ..SourceConfig::default()
    };
    let need = cfg.resolution.nv12_size();

    let src = FrameSource::new(cfg);
    let mut buf = vec![0u8; need];

    info!(size = format!("{width}x{height}"), need, frames, "开始读帧");

    let start = Instant::now();
    let mut got = 0u64;
    let mut no_frame = 0u64;
    let mut not_connected = 0u64;
    let mut last_pts: i64 = -1;
    let mut first_pts: Option<i64> = None;
    let mut distinct_frames = 0u64;
    // 只在需要导出时保留最后一帧，避免常态下多占 1.3 MB
    let mut last_frame: Vec<u8> = Vec::new();

    while got < frames {
        match src.poll(&mut buf) {
            FramePoll::Got { len, pts_us, .. } => {
                if len != need {
                    warn!(len, need, "帧长不符");
                    continue;
                }
                got += 1;
                if dump.is_some() {
                    last_frame.clear();
                    last_frame.extend_from_slice(&buf[..len]);
                }
                if first_pts.is_none() {
                    first_pts = Some(pts_us);
                }
                if pts_us != last_pts {
                    distinct_frames += 1;
                    last_pts = pts_us;
                }
            }
            FramePoll::NoFrame => {
                no_frame += 1;
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
            FramePoll::NotConnected => {
                not_connected += 1;
                if not_connected == 1 {
                    info!("等待 sidelensd --shm-feed 启动…");
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            FramePoll::LayoutMismatch { capacity, needed } => {
                warn!(capacity, needed, "共享内存布局不符：写端分辨率与本端不一致");
                return Ok(());
            }
            FramePoll::SizeMismatch => {
                warn!("单帧尺寸不符，已跳过");
            }
        }

        // 兜底：太久没连上就退出，避免测试脚本挂死
        if start.elapsed() > Duration::from_secs(10) && got == 0 {
            warn!("10 秒内没读到任何帧");
            return Ok(());
        }
    }

    let elapsed = start.elapsed();
    let fps = got as f64 / elapsed.as_secs_f64();
    let span_us = last_pts - first_pts.unwrap_or(0);

    info!(
        frames = got,
        distinct_frames,
        elapsed_ms = format!("{:.1}", elapsed.as_secs_f64() * 1000.0),
        fps = format!("{fps:.1}"),
        pts_span_ms = format!("{:.1}", span_us as f64 / 1000.0),
        no_frame,
        not_connected,
        stats = ?src.stats.snapshot(),
        "读帧完成"
    );

    if let (Some(path), false) = (dump, last_frame.is_empty()) {
        // Y 平面统计：纯黑是 16（NV12 的 limited-range 黑电平），
        // 全 0 说明写端根本没填过数据。
        let y_len = (width as usize) * (height as usize).min(last_frame.len() / width as usize);
        let y = &last_frame[..y_len.min(last_frame.len())];
        let min = y.iter().copied().min().unwrap_or(0);
        let max = y.iter().copied().max().unwrap_or(0);
        let mean = if y.is_empty() {
            0.0
        } else {
            y.iter().map(|v| *v as f64).sum::<f64>() / y.len() as f64
        };
        std::fs::write(path, &last_frame)
            .map_err(|e| anyhow::anyhow!("写 {:?} 失败: {e}", path))?;
        info!(
            path = %path.display(),
            bytes = last_frame.len(),
            y_min = min, y_max = max, y_mean = format!("{mean:.1}"),
            "已导出最后一帧的 NV12"
        );
    }

    Ok(())
}
