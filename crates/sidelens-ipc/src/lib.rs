//! 进程间帧传输
//!
//! # 为什么必须有这一层
//!
//! 虚拟摄像头的架构决定了**帧必须跨进程传递**：
//!
//! ```text
//! ┌─────────────────────┐         ┌──────────────────────┐
//! │  sidelensd.exe      │  管道   │  FrameServer (svchost)│
//! │  ─ 接收手机推流      │ ──────→ │  ─ 加载 sidelens_vcam_dll │
//! │  ─ 解码              │  共享   │  ─ 调 IMFMediaStream   │
//! │  ─ 写入共享内存      │  内存   │  ─ RequestSample 取帧  │
//! └─────────────────────┘         └──────────────────────┘
//! ```
//!
//! 官方示例里的 `CreateNamedPipeForStart` 就是这个 —— 不是可选优化，是架构必需。
//!
//! # 为什么用「管道 + 共享内存」而不是「管道传帧数据」
//!
//! 一帧 1080p NV12 是 3MB。走管道要拷贝两次（写端一次、读端一次），
//! 60fps 下就是 360MB/s 的拷贝开销。
//!
//! 正确做法：
//!
//! 1. **共享内存**放帧数据（写端直接写，读端直接读，零拷贝）
//! 2. **管道**只传元数据（帧序号、尺寸、时间戳、就绪信号）
//!
//! 元数据只有几十字节，管道开销可忽略。
//!
//! # 分层
//!
//! ```text
//! wire     跨进程 ABI（FrameMeta / FrameSignal），纯计算、可测
//! ring     环形缓冲布局与索引数学，纯计算、可测
//! shm      共享内存映射（Windows API 薄封装）
//! pipe     命名管道（Windows API 薄封装）
//! ```
//!
//! 前两层完全平台无关且 `#![forbid(unsafe_code)]`；
//! 后两层是 Windows API 的薄封装，unsafe 集中在极小的范围里。
//!
//! # 当前实现范围
//!
//! - [x] 帧元数据格式（跨进程 ABI）
//! - [x] 环形缓冲的索引计算（无锁读写的数学基础）
//! - [x] 管道消息编解码
//! - [x] 共享内存映射
//! - [x] 命名管道（消息模式）
//! - [ ] 共享环形缓冲（头 + slot 的无锁读写）

#![warn(missing_debug_implementations)]

pub mod ring;
pub mod wire;

#[cfg(windows)]
pub mod pipe;
#[cfg(windows)]
pub mod ring_shm;
#[cfg(windows)]
pub mod shm;

pub use ring::{RingLayout, SlotIndex};
pub use wire::{FrameMeta, FrameSignal, PixelFormat};

#[cfg(windows)]
pub use pipe::{PipeClient, PipeServer};
#[cfg(windows)]
pub use ring_shm::ShmRing;
#[cfg(windows)]
pub use shm::ShmMapping;

/// 管道名（不含 `\\.\pipe\` 前缀）
pub const DEFAULT_PIPE_NAME: &str = "sidelens-frame";

/// 共享内存名（不含 `Local\` 前缀）
pub const DEFAULT_SHM_NAME: &str = "sidelens-shm";
