//! 帧来源：从命名管道 + 共享内存取帧
//!
//! # 在整条链路里的位置
//!
//!    sidelensd.exe                      FrameServer (session 0)
//!    ─────────────                      ─────────────────────
//!    ShmRing::write_frame(pixels)   ──→ ShmRing::read_latest(buf)
//!    PipeServer::write_message(sig) ──→ PipeClient::read_message()
//!                                             │
//!                                             └─→ IMFSample → MEMediaSample
//!
//! # 为什么要在 DLL 里做「懒连接 + 失败可重试」
//!
//! DLL 由 FrameServer 加载，加载时机完全不受我们控制 ——
//! 用户可能在 sidelensd 还没启动时就打开了 Zoom。
//! 所以连接不能放在 DllMain 或构造里（那会让「没启动服务」变成
//! 「摄像头不可用」），必须放在第一次 RequestSample 时，
//! 且失败要能优雅降级（投递空样本或返回错误让消费端重试）。
//!
//! # 为什么不用「读不到就阻塞等」
//!
//! RequestSample 跑在 FrameServer 的线程池里。阻塞会让整个取帧管线卡住，
//! 包括其他摄像头。所以这里用**带超时的读**：超时就返回 None，
//! 让调用方决定是重试还是投递空帧。

#![allow(unsafe_code)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use sidelens_ipc::wire::FrameSignal;
use sidelens_ipc::{PipeClient, ShmRing};

use crate::source::SourceConfig;

/// 默认管道名
pub const DEFAULT_PIPE: &str = sidelens_ipc::DEFAULT_PIPE_NAME;
/// 默认共享内存名
pub const DEFAULT_SHM: &str = sidelens_ipc::DEFAULT_SHM_NAME;

/// 取帧来源的统计
#[derive(Debug, Default)]
pub struct SourceStats {
    /// 成功取到帧的次数
    pub frames_ok: AtomicU64,
    /// 管道超时次数（没有新帧）
    pub timeouts: AtomicU64,
    /// 连接失败次数
    pub connect_failures: AtomicU64,
    /// 共享内存读失败次数（seqlock 重试耗尽）
    pub shm_misses: AtomicU64,
    /// 尺寸不匹配丢弃的帧数
    pub size_mismatch: AtomicU64,
}

impl SourceStats {
    pub fn snapshot(&self) -> SourceStatsSnapshot {
        SourceStatsSnapshot {
            frames_ok: self.frames_ok.load(Ordering::Relaxed),
            timeouts: self.timeouts.load(Ordering::Relaxed),
            connect_failures: self.connect_failures.load(Ordering::Relaxed),
            shm_misses: self.shm_misses.load(Ordering::Relaxed),
            size_mismatch: self.size_mismatch.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SourceStatsSnapshot {
    pub frames_ok: u64,
    pub timeouts: u64,
    pub connect_failures: u64,
    pub shm_misses: u64,
    pub size_mismatch: u64,
}

/// 一次取帧的结果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FramePoll {
    /// 拿到一帧，像素已写入调用方的缓冲区
    Got {
        /// 实际字节数
        len: usize,
        /// 呈现时间戳（微秒）
        pts_us: i64,
        /// 是否关键帧
        keyframe: bool,
    },
    /// 没有新帧（管道超时）
    NoFrame,
    /// 尚未连上帧来源
    NotConnected,
    /// 共享内存的 slot 容量装不下目标分辨率
    ///
    /// 与 SizeMismatch 的区别：这是**连接阶段**就发现的布局不符
    /// （sidelensd 按 720p 建的缓冲，DLL 却按 1080p 要帧），
    /// 后者是**运行期**某一帧的元数据与配置不符。
    LayoutMismatch {
        /// slot 实际容量
        capacity: usize,
        /// 目标分辨率需要的大小
        needed: usize,
    },
    /// 帧尺寸与目标格式不符，已丢弃
    SizeMismatch,
}

/// 帧来源
///
/// 内部状态是「懒连接」：第一次 poll 时才去连管道和共享内存。
#[derive(Debug)]
pub struct FrameSource {
    config: SourceConfig,
    pipe_name: String,
    shm_name: String,
    /// 读超时
    timeout: Duration,
    /// 连接状态（None = 还没连上）
    conn: Mutex<Option<Connection>>,
    pub stats: SourceStats,
}

#[derive(Debug)]
struct Connection {
    pipe: PipeClient,
    ring: ShmRing,
}

impl FrameSource {
    /// 用默认名字创建
    pub fn new(config: SourceConfig) -> Self {
        Self {
            config,
            pipe_name: DEFAULT_PIPE.to_string(),
            shm_name: DEFAULT_SHM.to_string(),
            timeout: Duration::from_millis(16),
            conn: Mutex::new(None),
            stats: SourceStats::default(),
        }
    }

    /// 指定管道与共享内存名（测试与多实例场景用）
    pub fn with_names(
        config: SourceConfig,
        pipe_name: impl Into<String>,
        shm_name: impl Into<String>,
    ) -> Self {
        let mut s = Self::new(config);
        s.pipe_name = pipe_name.into();
        s.shm_name = shm_name.into();
        s
    }

    /// 目标配置
    pub fn config(&self) -> SourceConfig {
        self.config
    }

    /// 设置读超时
    pub fn set_timeout(&mut self, t: Duration) {
        self.timeout = t;
    }

    /// 是否已连上
    pub fn is_connected(&self) -> bool {
        self.conn.lock().map(|g| g.is_some()).unwrap_or(false)
    }

    /// 主动断开（sidelensd 重启后需要重连）
    pub fn disconnect(&self) {
        if let Ok(mut g) = self.conn.lock() {
            *g = None;
        }
    }

    /// 尝试建立连接
    ///
    /// 成功返回 Ok(())；失败返回具体原因，让上层能给出有用的诊断。
    fn ensure_connected(&self) -> Result<(), FramePoll> {
        if self.conn.lock().map(|g| g.is_some()).unwrap_or(false) {
            return Ok(());
        }

        // 先连管道（sidelensd 必须先建好管道才能打开共享内存）
        let pipe = match PipeClient::connect(&self.pipe_name, 1, Duration::from_millis(5)) {
            Ok(p) => p,
            Err(_) => {
                self.stats.connect_failures.fetch_add(1, Ordering::Relaxed);
                return Err(FramePoll::NotConnected);
            }
        };
        if pipe.set_message_mode().is_err() {
            self.stats.connect_failures.fetch_add(1, Ordering::Relaxed);
            return Err(FramePoll::NotConnected);
        }

        let ring = match ShmRing::open(&self.shm_name) {
            Ok(r) => r,
            Err(_) => {
                self.stats.connect_failures.fetch_add(1, Ordering::Relaxed);
                return Err(FramePoll::NotConnected);
            }
        };

        // 校验共享内存的布局与目标格式一致
        //
        // 这一步是「快速失败」的关键：sidelensd 按 720p 建的缓冲，
        // 而 DLL 按 1080p 要帧时，如果继续往下走就会读到越界内存。
        // 明确报出容量差，比让用户看到花屏有用得多。
        let need = self.config.resolution.nv12_size();
        if !ring.layout().fits(need) {
            self.stats.connect_failures.fetch_add(1, Ordering::Relaxed);
            return Err(FramePoll::LayoutMismatch {
                capacity: ring.layout().slot_capacity(),
                needed: need,
            });
        }

        // 刚连上时丢掉积压，从最新帧开始播
        ring.skip_to_latest();

        if let Ok(mut g) = self.conn.lock() {
            *g = Some(Connection { pipe, ring });
        }
        Ok(())
    }

    /// 取一帧到 buf
    ///
    /// buf 至少要能放下 width * height * 3 / 2 字节（NV12）。
    pub fn poll(&self, buf: &mut [u8]) -> FramePoll {
        if let Err(reason) = self.ensure_connected() {
            return reason;
        }

        let mut guard = match self.conn.lock() {
            Ok(g) => g,
            Err(_) => return FramePoll::NotConnected,
        };
        let Some(conn) = guard.as_mut() else {
            return FramePoll::NotConnected;
        };

        // 1. 等管道信号（带超时）
        let mut msg = [0u8; FrameSignal::SIZE];
        match conn.pipe.read_message(&mut msg) {
            Ok(n) if n >= FrameSignal::SIZE => {
                if FrameSignal::decode(&msg[..n]).is_none() {
                    // 信号格式不对，说明对端版本不兼容
                    drop(guard);
                    self.disconnect();
                    return FramePoll::NotConnected;
                }
            }
            Ok(_) => {
                // 短消息：不可能是有效信号
                return FramePoll::NoFrame;
            }
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                self.stats.timeouts.fetch_add(1, Ordering::Relaxed);
                return FramePoll::NoFrame;
            }
            Err(_) => {
                // 管道断开：sidelensd 退出了，下次重连
                drop(guard);
                self.disconnect();
                return FramePoll::NotConnected;
            }
        }

        // 2. 从共享内存取最新帧
        let Some(meta) = conn.ring.read_latest(buf) else {
            self.stats.shm_misses.fetch_add(1, Ordering::Relaxed);
            return FramePoll::NoFrame;
        };

        // 3. 校验格式与尺寸
        if meta.width != self.config.resolution.width
            || meta.height != self.config.resolution.height
        {
            self.stats.size_mismatch.fetch_add(1, Ordering::Relaxed);
            return FramePoll::SizeMismatch;
        }

        self.stats.frames_ok.fetch_add(1, Ordering::Relaxed);
        FramePoll::Got {
            len: meta.payload_len as usize,
            pts_us: meta.pts_us as i64,
            keyframe: meta.is_keyframe(),
        }
    }
}

/// 把长度夹到 u32 范围（IMFSample 的缓冲长度是 u32）
pub fn nv12_sample_bytes(len: usize) -> u32 {
    len.min(u32::MAX as usize) as u32
}

/// 微秒 → MFTIME（100ns）
///
/// # 为什么必须换算
///
/// Media Foundation 的时间单位是 100 纳秒（MFTIME）。
/// 我们内部用微秒，差 10 倍。忘了换算会让播放速度差一个数量级 ——
/// 而且症状是「视频像快进」这种一眼看不出根因的表现。
pub fn us_to_mftime(us: i64) -> i64 {
    us.saturating_mul(10)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn us_to_mftime_is_ten_times() {
        assert_eq!(us_to_mftime(1_000_000), 10_000_000, "1 秒 = 10^7 个 100ns");
        assert_eq!(us_to_mftime(0), 0);
        assert_eq!(us_to_mftime(16_667), 166_670);
    }

    #[test]
    fn us_to_mftime_saturates_instead_of_overflowing() {
        // 极端值不能 panic，也不能回绕成负数
        assert_eq!(us_to_mftime(i64::MAX), i64::MAX);
    }

    #[test]
    fn nv12_sample_bytes_clamps() {
        assert_eq!(nv12_sample_bytes(1024), 1024);
        // 超过 u32 的值被夹住而不是回绕
        assert_eq!(nv12_sample_bytes(usize::MAX), u32::MAX);
    }

    #[test]
    fn source_starts_disconnected() {
        let s = FrameSource::new(SourceConfig::default());
        assert!(!s.is_connected());
        assert_eq!(s.stats.snapshot(), SourceStatsSnapshot::default());
    }

    #[test]
    fn poll_without_server_reports_not_connected() {
        let s = FrameSource::with_names(
            SourceConfig::default(),
            "sidelens-test-nonexistent-pipe",
            "sidelens-test-nonexistent-shm",
        );
        let mut buf = vec![0u8; 1280 * 720 * 3 / 2];
        assert_eq!(s.poll(&mut buf), FramePoll::NotConnected);
        assert!(s.stats.snapshot().connect_failures > 0);
    }

    #[test]
    fn disconnect_resets_state() {
        let s = FrameSource::new(SourceConfig::default());
        s.disconnect();
        assert!(!s.is_connected());
    }

    #[test]
    fn default_names_match_ipc_constants() {
        assert_eq!(DEFAULT_PIPE, sidelens_ipc::DEFAULT_PIPE_NAME);
        assert_eq!(DEFAULT_SHM, sidelens_ipc::DEFAULT_SHM_NAME);
    }
}
