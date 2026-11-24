//! SideLens 传输层
//!
//! # 为什么先做 WebSocket 而不是直接 QUIC
//!
//! 阶段 0 的目标是「拿到 Rust 版的真实端到端延迟」。QUIC 需要
//! QUIC 库 + UDP 打洞 + 自签证书 + 多路复用调度，这些会淹没核心问题。
//!
//! 但接口按 QUIC 的语义设计：
//!
//! - `send()` 是**尽力发出**，不承诺有序可靠
//! - 上层（抖动缓冲）已经能处理乱序与丢帧
//! - 换 QUIC 时只需替换 `WsServer` / `WsClient`，上层不动
//!
//! # 分层
//!
//! ```text
//!   [客户端帧编码 FrameSpec] → [Transport::send] → 网络
//!   网络 → [Transport::recv] → [FrameDecoder] → [JitterBuffer] → 呈现
//! ```

#![forbid(unsafe_code)]
#![warn(missing_debug_implementations)]

pub mod client;
pub mod server;
pub mod udp;

pub use client::{connect_with_retry, ClientStats, DownlinkFrame, WsClient};
pub use server::{control_frame, InboundFrame, ServerStats, StatsSnapshot, WsServer};
pub use udp::{
    decode_stream, encode_datagrams, PacketLossSimulator, UdpDatagram, UdpInbound, UdpReceiver,
    UdpSender, UdpStats, UdpStatsSnapshot, DEFAULT_UDP_PORT, MAX_DATAGRAM,
};

/// 默认推流端口
pub const DEFAULT_PORT: u16 = 27100;
