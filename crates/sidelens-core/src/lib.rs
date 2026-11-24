//! SideLens 共享核心
//!
//! # 分层
//!
//! - `sidelens-proto`：纯字节格式（帧编解码、magic 重同步），无会话状态
//! - `sidelens-core`（本 crate）：会话层状态机
//!   - [`jitter`]：抖动缓冲与乱序重排
//!   - [`clock`]：RTT 估计与音画同步
//!   - [`nack`]：丢包检测与重传请求
//!   - [`retransmit`]：发送端重传缓存
//!   - [`congestion`]：拥塞控制与码率自适应（延迟梯度 + AIMD）
//!   - [`timer`]：系统定时器分辨率（Windows 专项）

#![deny(unsafe_code)]
#![warn(missing_debug_implementations)]

pub mod clock;
pub mod congestion;
pub mod fec;
pub mod fec_link;
pub mod fragment;
pub mod jitter;
pub mod nack;
pub mod platform;
pub mod retransmit;
pub mod sync;
pub mod timer;

pub use clock::{AvSync, RttEstimator, SyncVerdict};
pub use congestion::{
    CongestionConfig, CongestionController, CongestionSignal, Decision, DelayTrend, Feedback,
    RateState, Reason,
};
pub use fec_link::{evaluate_fec, FecVerdict, FragmentInfo};
pub use fragment::{
    fragment_frame, DatagramPiece, FragmentHeader, ReassembledFrame, Reassembler, ReassemblerStats,
    FRAGMENT_HEADER_SIZE, SAFE_MTU_PAYLOAD,
};
pub use jitter::{JitterBuffer, JitterConfig, JitterOutcome, JitterStats};
pub use nack::{NackGenerator, NackRequest};
pub use retransmit::{CachedFrame, RetransmitCache, RetransmitConfig, RetransmitStats, StoreSpec};
pub use sync::{ClockSync, ProbeState, RttProbe, RttReply, SyncSample};
pub use timer::HighResTimer;
