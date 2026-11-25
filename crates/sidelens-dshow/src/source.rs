//! 从 SideLens 的管道和共享内存读取 NV12 最新帧。

#![allow(unsafe_code)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sidelens_ipc::wire::{FrameSignal, PixelFormat};
use sidelens_ipc::{PipeClient, ShmRing, DEFAULT_PIPE_NAME, DEFAULT_SHM_NAME};

/// 一帧的像素数据（NV12）
#[derive(Debug, Clone)]
pub struct Frame {
    /// NV12 字节
    pub data: Vec<u8>,
    /// 宽（像素）
    pub width: u32,
    /// 高（像素）
    pub height: u32,
    /// 显示时间戳（100ns 单位，DirectShow 的 REFERENCE_TIME）
    pub timestamp: i64,
}

impl Frame {
    /// 构造一帧全黑 NV12
    ///
    /// # 为什么 Y=16 而不是 0
    ///
    /// Y=0 在 limited range 下是非法值，UV=0 是错色度（应 128）。
    /// 这个坑在第三十轮踩过（见 docs/progress.md「已知坑」第 30 条）。
    pub fn black(width: u32, height: u32) -> Self {
        let y_size = (width as usize) * (height as usize);
        let uv_size = y_size / 2;
        let mut data = vec![16u8; y_size];
        data.extend(std::iter::repeat(128u8).take(uv_size));
        Self {
            data,
            width,
            height,
            timestamp: 0,
        }
    }

    /// NV12 的字节数
    pub fn nv12_len(width: u32, height: u32) -> usize {
        (width as usize) * (height as usize) * 3 / 2
    }
}

/// 统计计数器
#[derive(Debug, Default)]
pub struct SourceStats {
    /// poll 次数
    pub polls: AtomicU64,
    /// 成功取到新帧
    pub frames: AtomicU64,
    /// 重复上一帧
    pub repeats: AtomicU64,
    /// 连接失败
    pub connect_failures: AtomicU64,
    /// 共享内存未命中
    pub shm_misses: AtomicU64,
    /// 尺寸不匹配
    pub size_mismatch: AtomicU64,
    /// 是否已经记录过首次非黑帧
    live_frame_logged: std::sync::atomic::AtomicBool,
}

/// 连接
#[derive(Debug)]
struct Connection {
    pipe: PipeClient,
    ring: ShmRing,
}

/// 帧来源
///
/// 懒连接：第一次 `poll` 时才去连管道和共享内存。
/// `sidelensd` 可能比消费端晚启动，也可能中途重启 —— 连不上不是错误，
/// 是「现在没有帧」。
#[derive(Debug)]
pub struct FrameSource {
    pipe_name: String,
    shm_name: String,
    timeout: Duration,
    conn: Mutex<Option<Connection>>,
    last: Mutex<Option<Frame>>,
    stats: Arc<SourceStats>,
    width: u32,
    height: u32,
}

impl FrameSource {
    /// 用默认管道名创建
    pub fn new(width: u32, height: u32) -> Self {
        Self::with_names(width, height, DEFAULT_PIPE_NAME, DEFAULT_SHM_NAME)
    }

    /// 指定管道与共享内存名（测试与多实例场景用）
    pub fn with_names(
        width: u32,
        height: u32,
        pipe_name: impl Into<String>,
        shm_name: impl Into<String>,
    ) -> Self {
        Self {
            pipe_name: pipe_name.into(),
            shm_name: shm_name.into(),
            timeout: Duration::from_millis(8),
            conn: Mutex::new(None),
            last: Mutex::new(None),
            stats: Arc::new(SourceStats::default()),
            width,
            height,
        }
    }

    /// 协商的尺寸
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// 统计
    pub fn stats(&self) -> &Arc<SourceStats> {
        &self.stats
    }

    /// 是否已连上
    pub fn is_connected(&self) -> bool {
        self.conn.lock().map(|g| g.is_some()).unwrap_or(false)
    }

    /// 断开（sidelensd 重启后重连）
    pub fn disconnect(&self) {
        if let Ok(mut g) = self.conn.lock() {
            *g = None;
        }
    }

    fn ensure_connected(&self) -> bool {
        if self.conn.lock().map(|g| g.is_some()).unwrap_or(false) {
            return true;
        }

        // 先连管道：sidelensd 必须先建好管道，共享内存才有意义
        let mut pipe = match PipeClient::connect(&self.pipe_name, 1, Duration::from_millis(5)) {
            Ok(p) => p,
            Err(e) => {
                self.log_connect_failure(format!("命名管道连接失败: {e}"));
                return false;
            }
        };
        pipe.set_timeout(self.timeout);
        if let Err(e) = pipe.set_message_mode() {
            self.log_connect_failure(format!("切换命名管道消息模式失败: {e}"));
            return false;
        }

        let ring = match ShmRing::open(&self.shm_name) {
            Ok(r) => r,
            Err(e) => {
                self.log_connect_failure(format!("共享内存打开失败: {e}"));
                return false;
            }
        };

        // 刚连上时丢掉积压，从最新帧开始播
        ring.skip_to_latest();

        if let Ok(mut g) = self.conn.lock() {
            *g = Some(Connection { pipe, ring });
        }
        true
    }

    fn log_connect_failure(&self, reason: String) {
        let failures = self.stats.connect_failures.fetch_add(1, Ordering::Relaxed) + 1;
        if failures == 1 || failures % 300 == 0 {
            crate::log_to_file(&format!(
                "FrameSource 连接失败 {failures} 次，pipe={} shm={}：{reason}",
                self.pipe_name, self.shm_name
            ));
        }
    }

    /// 拉一帧
    ///
    /// 保证返回一帧 —— 没有新帧时重复上一帧，从来没有过就全黑。
    /// 调用方只需把 `data` 拷进自己的缓冲区。
    pub fn poll(&self, timestamp: i64) -> Frame {
        self.stats.polls.fetch_add(1, Ordering::Relaxed);

        if !self.ensure_connected() {
            return self.fallback(timestamp);
        }

        let mut guard = match self.conn.lock() {
            Ok(g) => g,
            Err(_) => return self.fallback(timestamp),
        };
        let Some(conn) = guard.as_mut() else {
            return self.fallback(timestamp);
        };

        // 1. 等管道信号（带超时）
        let mut msg = [0u8; FrameSignal::SIZE];
        match conn.pipe.read_message(&mut msg) {
            Ok(n) if n >= FrameSignal::SIZE => {
                if FrameSignal::decode(&msg[..n]).is_none() {
                    drop(guard);
                    self.disconnect();
                    return self.fallback(timestamp);
                }
            }
            Ok(_) => {
                drop(guard);
                return self.fallback(timestamp);
            }
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                drop(guard);
                return self.fallback(timestamp);
            }
            Err(_) => {
                // 管道断开：sidelensd 退出了，下次重连
                drop(guard);
                self.disconnect();
                return self.fallback(timestamp);
            }
        }

        // 2. 从共享内存取最新帧
        let need = Frame::nv12_len(self.width, self.height);
        let mut buf = vec![0u8; need];
        let Some(meta) = conn.ring.read_latest(&mut buf) else {
            self.stats.shm_misses.fetch_add(1, Ordering::Relaxed);
            drop(guard);
            return self.fallback(timestamp);
        };

        // 3. 校验尺寸
        if meta.width != self.width
            || meta.height != self.height
            || meta.format != PixelFormat::Nv12 as u32
            || meta.payload_len as usize != need
        {
            self.stats.size_mismatch.fetch_add(1, Ordering::Relaxed);
            drop(guard);
            return self.fallback(timestamp);
        }

        let frame = Frame {
            data: buf,
            width: self.width,
            height: self.height,
            timestamp,
        };
        let prior_frames = self.stats.frames.fetch_add(1, Ordering::Relaxed);
        let luma = frame
            .data
            .len()
            .min((self.width as usize) * (self.height as usize));
        let peak = frame.data[..luma]
            .iter()
            .step_by(97)
            .copied()
            .max()
            .unwrap_or(0);
        if prior_frames == 0 {
            crate::log_to_file(&format!(
                "FrameSource 首次收到共享内存帧: {}x{}，亮度峰值 {peak}",
                self.width, self.height
            ));
        }
        if peak > 16 && !self.stats.live_frame_logged.swap(true, Ordering::AcqRel) {
            crate::log_to_file(&format!(
                "FrameSource 已收到非黑共享内存帧，亮度峰值 {peak}"
            ));
        }
        drop(guard);
        if let Ok(mut l) = self.last.lock() {
            *l = Some(frame.clone());
        }
        frame
    }

    /// 没有新帧时的兜底
    fn fallback(&self, timestamp: i64) -> Frame {
        self.stats.repeats.fetch_add(1, Ordering::Relaxed);
        let mut l = match self.last.lock() {
            Ok(g) => g,
            Err(e) => e.into_inner(),
        };
        if let Some(prev) = l.as_mut() {
            prev.timestamp = timestamp;
            return prev.clone();
        }
        let mut black = Frame::black(self.width, self.height);
        black.timestamp = timestamp;
        *l = Some(black.clone());
        black
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nv12_len_is_1_5_bytes_per_pixel() {
        assert_eq!(Frame::nv12_len(1280, 720), 1280 * 720 * 3 / 2);
        assert_eq!(Frame::nv12_len(640, 480), 640 * 480 * 3 / 2);
    }

    #[test]
    fn black_frame_uses_limited_range_values() {
        let f = Frame::black(4, 4);
        // Y 平面 16 字节全 16
        assert_eq!(f.data.len(), 4 * 4 * 3 / 2);
        assert!(f.data[..16].iter().all(|&b| b == 16), "Y 必须是 16");
        // UV 平面全 128
        assert!(f.data[16..].iter().all(|&b| b == 128), "UV 必须是 128");
    }

    #[test]
    fn black_frame_is_not_all_zero() {
        // 回归测试：曾经用 vec![0u8; n]，Y=0 是非法值
        let f = Frame::black(8, 8);
        assert!(f.data.iter().any(|&b| b != 0), "不能是全零缓冲");
    }

    #[test]
    fn poll_without_producer_returns_black_not_empty() {
        let src = FrameSource::with_names(64, 48, "sidelens-test-none", "sidelens-test-none-shm");
        let f = src.poll(0);
        assert_eq!(f.data.len(), 64 * 48 * 3 / 2, "必须返回完整一帧");
        assert!(f.data[..64 * 48].iter().all(|&b| b == 16));
    }

    #[test]
    fn poll_repeats_last_frame_when_no_new_data() {
        let src = FrameSource::with_names(32, 32, "sidelens-test-none2", "sidelens-test-none2-shm");
        let a = src.poll(100);
        let b = src.poll(200);
        assert_eq!(a.data.len(), b.data.len());
        assert_eq!(b.timestamp, 200, "时间戳必须更新");
        assert!(src.stats().repeats.load(Ordering::Relaxed) >= 2);
    }

    #[test]
    fn stats_start_at_zero() {
        let src = FrameSource::new(16, 16);
        assert_eq!(src.stats().frames.load(Ordering::Relaxed), 0);
        assert_eq!(src.stats().polls.load(Ordering::Relaxed), 0);
    }
}
