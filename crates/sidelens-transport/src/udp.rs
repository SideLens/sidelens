//! UDP 传输层
//!
//! # 与 WebSocket 的关键差异
//!
//! | 维度 | WebSocket（TCP） | UDP |
//! | --- | --- | --- |
//! | 可靠性 | 内核重传 | 应用层自己做（NACK） |
//! | 有序性 | 保证 | 不保证（抖动缓冲处理） |
//! | 消息边界 | 需自己拼帧 | 天然一包一消息 |
//! | 大帧 | 自动分片 | 需应用层分片（`sidelens_core::fragment`） |
//!
//! # 为什么这是低延迟的正解
//!
//! TCP 的队头阻塞：丢一个包，后续所有包都要等它重传。
//! 对实时视频这是灾难 —— 一个 1500 字节的包丢了，整个 8KB 帧都卡在缓冲区。
//! UDP 下丢包只影响那一片，其余片立刻可用，缺的一片由 NACK 单独补。
//!
//! # 丢包模拟
//!
//! [`PacketLossSimulator`] 用于在回环上验证 NACK 逻辑。真实环境要等跨设备测试。

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tokio::net::UdpSocket;
use tracing::{debug, info};

use sidelens_core::fragment::{fragment_frame, ReassembledFrame, Reassembler, SAFE_MTU_PAYLOAD};
use sidelens_proto::{FrameDecoder, FrameType, HEADER_SIZE};

/// UDP 默认端口（与 WebSocket 分开，便于同时跑两套对比）
pub const DEFAULT_UDP_PORT: u16 = 27200;

/// 单个 UDP 包的安全上限
pub const MAX_DATAGRAM: usize = SAFE_MTU_PAYLOAD;

/// UDP 统计
#[derive(Debug, Default)]
pub struct UdpStats {
    pub datagrams_out: AtomicU64,
    pub bytes_out: AtomicU64,
    pub datagrams_in: AtomicU64,
    pub bytes_in: AtomicU64,
    /// 发送时被丢包模拟器拦下的
    pub simulated_drops: AtomicU64,
    /// 接收时因缓冲满被内核丢掉的（本次无法统计，留作占位）
    pub recv_errors: AtomicU64,
}

impl UdpStats {
    pub fn snapshot(&self) -> UdpStatsSnapshot {
        UdpStatsSnapshot {
            datagrams_out: self.datagrams_out.load(Ordering::Relaxed),
            bytes_out: self.bytes_out.load(Ordering::Relaxed),
            datagrams_in: self.datagrams_in.load(Ordering::Relaxed),
            bytes_in: self.bytes_in.load(Ordering::Relaxed),
            simulated_drops: self.simulated_drops.load(Ordering::Relaxed),
            recv_errors: self.recv_errors.load(Ordering::Relaxed),
        }
    }
}

/// 统计快照
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UdpStatsSnapshot {
    pub datagrams_out: u64,
    pub bytes_out: u64,
    pub datagrams_in: u64,
    pub bytes_in: u64,
    pub simulated_drops: u64,
    pub recv_errors: u64,
}

/// 丢包模拟器
///
/// 用确定性的 xorshift 而不是真随机：同样种子产生同样序列，
/// 测试可复现。这点很重要 —— 丢包测试如果不可复现，失败时无法调试。
#[derive(Debug)]
pub struct PacketLossSimulator {
    /// 丢包率（0.0 = 不丢，1.0 = 全丢）
    rate: f64,
    state: AtomicU64,
    enabled: bool,
}

impl PacketLossSimulator {
    /// `rate` 为丢包率；`seed` 用于复现
    pub fn new(rate: f64, seed: u64) -> Self {
        Self {
            rate: rate.clamp(0.0, 1.0),
            state: AtomicU64::new(seed | 1),
            enabled: rate > 0.0,
        }
    }

    pub fn disabled() -> Self {
        Self {
            rate: 0.0,
            state: AtomicU64::new(1),
            enabled: false,
        }
    }

    pub fn rate(&self) -> f64 {
        self.rate
    }

    /// 决定这一包是否丢弃
    pub fn should_drop(&self) -> bool {
        if !self.enabled {
            return false;
        }
        // xorshift64
        let mut x = self.state.load(Ordering::Relaxed);
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state.store(x, Ordering::Relaxed);

        // 把 x 归一化到 [0,1)
        let normalized = (x >> 11) as f64 / (1u64 << 53) as f64;
        normalized < self.rate
    }
}

/// 打包：把一帧编码字节切成一个或多个 UDP 载荷
pub fn encode_datagrams(encoded: &[u8]) -> Vec<Vec<u8>> {
    fragment_frame(encoded, MAX_DATAGRAM)
}

/// 发送端
#[derive(Debug)]
pub struct UdpSender {
    socket: Arc<UdpSocket>,
    remote: SocketAddr,
    loss: Arc<PacketLossSimulator>,
    pub stats: Arc<UdpStats>,
}

impl UdpSender {
    pub async fn connect(remote: &str, loss: PacketLossSimulator) -> std::io::Result<Self> {
        let socket = UdpSocket::bind("0.0.0.0:0").await?;
        socket.connect(remote).await?;
        let addr: SocketAddr = remote
            .parse()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("{e}")))?;
        info!(%addr, loss_rate = loss.rate(), "UDP 发送端就绪");
        Ok(Self {
            socket: Arc::new(socket),
            remote: addr,
            loss: Arc::new(loss),
            stats: Arc::new(UdpStats::default()),
        })
    }

    pub fn remote(&self) -> SocketAddr {
        self.remote
    }

    /// 本端 socket
    ///
    /// 收发必须用**同一个** socket：UDP 无连接，接收端回 NACK 时是往数据报的
    /// 源地址发。如果收用另一个 socket，NACK 会发到发送 socket 的端口上，
    /// 那个端口没人读，重传链路就是断的（这个坑实测踩过）。
    ///
    /// socket 已 `connect`，`recv` 只会收到来自 `remote` 的包，正是所需。
    pub fn socket(&self) -> Arc<UdpSocket> {
        self.socket.clone()
    }

    /// 收一个数据报（用于接收 NACK 等下行控制帧）
    pub async fn recv_raw(&self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.socket.recv(buf).await
    }

    /// 发送一帧（自动分片）。返回成功发出的分片数。
    pub async fn send_frame(&self, encoded: &[u8]) -> usize {
        let datagrams = encode_datagrams(encoded);
        let mut sent = 0;

        for dg in datagrams {
            if self.loss.should_drop() {
                self.stats.simulated_drops.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            match self.socket.send(&dg).await {
                Ok(n) => {
                    sent += 1;
                    self.stats.datagrams_out.fetch_add(1, Ordering::Relaxed);
                    self.stats.bytes_out.fetch_add(n as u64, Ordering::Relaxed);
                }
                Err(e) => {
                    // UDP 发送失败不重试：实时视频宁可丢帧也不要积压
                    debug!(error = %e, "发送失败");
                }
            }
        }
        sent
    }

    /// 发送一个原始数据报（用于 NACK 等控制帧，不分片）
    pub async fn send_raw(&self, bytes: &[u8]) -> bool {
        if self.loss.should_drop() {
            self.stats.simulated_drops.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        match self.socket.send(bytes).await {
            Ok(n) => {
                self.stats.datagrams_out.fetch_add(1, Ordering::Relaxed);
                self.stats.bytes_out.fetch_add(n as u64, Ordering::Relaxed);
                true
            }
            Err(_) => false,
        }
    }
}

/// 接收端收到的一帧
#[derive(Debug, Clone)]
pub struct UdpInbound {
    pub from: SocketAddr,
    pub frame: ReassembledFrame,
}

/// 一个到达的数据报（重组前）
///
/// # 为什么需要它
///
/// [`UdpInbound`] 只在**重组完成**时产生，此时分片头早已被吃掉。
/// 而 FEC 需要在**每个分片到达时**拿到它的数据段（冗余包保护的是分片级数据）。
///
/// 所以提供这个结构：它在重组前就把分片信息交出来，
/// 同时附带「这个分片是否让某帧变完整」的结果。
#[derive(Debug, Clone)]
pub struct UdpDatagram {
    pub from: SocketAddr,
    /// 帧类型
    pub ty: FrameType,
    /// 帧序号
    pub seq: u32,
    /// 帧头 flags（bit0 = 关键帧，bit2 = 分片）
    ///
    /// FEC 恢复路径要用它还原帧属性 —— 丢了关键帧位，恢复出的关键帧
    /// 会被当成普通帧，`needs_keyframe` 逻辑失效（见 `FragmentInfo::flags`）。
    pub flags: u8,
    /// 呈现时间戳（微秒）
    pub pts_us: u64,
    /// 发送端时间戳（微秒），延迟统计与时钟同步用
    pub send_ts_us: u64,
    /// 是否带分片头
    pub fragmented: bool,
    /// 分片下标（未分片时为 0）
    pub fragment_index: u16,
    /// 分片总数（未分片时为 1）
    pub fragment_count: u16,
    /// 帧载荷总长（未分片时等于数据段长度）
    pub total_size: u32,
    /// 数据段（已去掉帧头与分片头）
    pub data: Vec<u8>,
    /// 原始载荷（含分片头，未分片时等于 data）
    pub payload: Vec<u8>,
    /// 这个分片让某帧重组完成时给出
    pub completed: Option<ReassembledFrame>,
}

/// 接收端
///
/// 内部维护一个 [`Reassembler`]，自动处理分片与超时。
/// 超时的帧通过 [`UdpReceiver::expire`] 报给上层，由上层发 NACK。
#[derive(Debug)]
pub struct UdpReceiver {
    socket: Arc<UdpSocket>,
    pub stats: Arc<UdpStats>,
}

impl UdpReceiver {
    pub async fn bind(port: u16) -> std::io::Result<Self> {
        // 端口占用自动上移，与 WsServer 行为一致
        let mut last_err = None;
        for offset in 0..20u16 {
            let try_port = port.saturating_add(offset);
            match UdpSocket::bind(("0.0.0.0", try_port)).await {
                Ok(socket) => {
                    let actual = socket.local_addr()?.port();
                    info!(port = actual, "UDP 接收端就绪");
                    return Ok(Self {
                        socket: Arc::new(socket),
                        stats: Arc::new(UdpStats::default()),
                    });
                }
                Err(e) => {
                    if e.kind() != std::io::ErrorKind::AddrInUse {
                        return Err(e);
                    }
                    last_err = Some(e);
                }
            }
        }
        Err(last_err.unwrap_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::AddrInUse, "找不到可用端口")
        }))
    }

    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    pub fn port(&self) -> std::io::Result<u16> {
        Ok(self.local_addr()?.port())
    }

    pub fn socket(&self) -> Arc<UdpSocket> {
        self.socket.clone()
    }

    /// 收一个数据报并尝试重组。返回 `Some` 表示这一帧已完整。
    /// 收一个数据报并尝试重组
    ///
    /// # 关于 FEC 的输入点（重要）
    ///
    /// 返回的是**重组完成**的帧。但 FEC 需要的是**每个分片到达时**的数据段 ——
    /// 因为冗余包保护的是分片级数据，不是整帧。
    ///
    /// 所以另提供 [`Self::recv_datagram`]，它在重组前就把分片信息交出来。
    pub async fn recv_frame(&self, asm: &mut Reassembler, buf: &mut [u8]) -> Option<UdpInbound> {
        let (n, from) = match self.socket.recv_from(buf).await {
            Ok(v) => v,
            Err(e) => {
                self.stats.recv_errors.fetch_add(1, Ordering::Relaxed);
                debug!(error = %e, "接收失败");
                return None;
            }
        };

        self.stats.datagrams_in.fetch_add(1, Ordering::Relaxed);
        self.stats.bytes_in.fetch_add(n as u64, Ordering::Relaxed);

        let datagram = &buf[..n];
        if datagram.len() < HEADER_SIZE {
            return None;
        }

        let ty = FrameType::from_u8(datagram[4])?;
        let flags = datagram[5];
        let stream_id = u16::from_le_bytes([datagram[6], datagram[7]]);
        let seq = u32::from_le_bytes([datagram[8], datagram[9], datagram[10], datagram[11]]);
        let pts_us = u64::from_le_bytes(datagram[12..20].try_into().ok()?);
        let send_ts_us = u64::from_le_bytes(datagram[20..28].try_into().ok()?);
        let payload_len =
            u32::from_le_bytes([datagram[28], datagram[29], datagram[30], datagram[31]]) as usize;

        if datagram.len() < HEADER_SIZE + payload_len {
            return None;
        }
        let payload = &datagram[HEADER_SIZE..HEADER_SIZE + payload_len];
        let now = sidelens_proto::now_us();

        // 控制帧（FEC / NACK / RTT 等）不走重组器。
        //
        // # 这里踩过一个隐蔽的坑
        //
        // 早期实现无条件 `asm.push(piece, now)?` —— 它对**未完成重组**的帧
        // 返回 `None`，于是整个函数返回 `None`。
        //
        // 后果：`FrameType::Fec` 的包被喂进重组器（它们不是分片帧，
        // 永远凑不齐），`asm.push` 返回 None，函数返回 None，
        // **FEC 分支在调用方永远走不到**。实测 `fec_reds=0` 而发送端明明发了
        // 347 个冗余包 —— 全部被静默吞掉。
        //
        // 修法：非分片帧直接包成 `ReassembledFrame` 返回，不经过重组器。
        let is_fragment = flags & sidelens_proto::FLAG_FRAGMENT != 0;
        if !is_fragment && !ty.is_media() {
            let frame = sidelens_core::fragment::ReassembledFrame {
                ty,
                flags,
                stream_id,
                seq,
                pts_us,
                send_ts_us,
                payload: payload.to_vec(),
            };
            return Some(UdpInbound { from, frame });
        }

        let piece = sidelens_core::fragment::DatagramPiece {
            stream_id,
            seq,
            ty,
            flags,
            pts_us,
            send_ts_us,
            payload,
        };
        let frame = asm.push(piece, now)?;
        Some(UdpInbound { from, frame })
    }

    /// 收一个数据报，返回**重组前**的分片信息 + 可选的重组结果
    ///
    /// 与 [`Self::recv_frame`] 的区别：这个函数对**每个到达的分片**都返回，
    /// 而不是只在重组完成时返回。FEC 必须用这个 —— 冗余包保护的是分片级数据，
    /// 而重组后的帧里分片头早已被吃掉。
    pub async fn recv_datagram(
        &self,
        asm: &mut Reassembler,
        buf: &mut [u8],
    ) -> Option<UdpDatagram> {
        let (n, from) = match self.socket.recv_from(buf).await {
            Ok(v) => v,
            Err(e) => {
                self.stats.recv_errors.fetch_add(1, Ordering::Relaxed);
                debug!(error = %e, "接收失败");
                return None;
            }
        };

        self.stats.datagrams_in.fetch_add(1, Ordering::Relaxed);
        self.stats.bytes_in.fetch_add(n as u64, Ordering::Relaxed);

        let datagram = &buf[..n];
        if datagram.len() < HEADER_SIZE {
            return None;
        }

        let ty = FrameType::from_u8(datagram[4])?;
        let flags = datagram[5];
        let stream_id = u16::from_le_bytes([datagram[6], datagram[7]]);
        let seq = u32::from_le_bytes([datagram[8], datagram[9], datagram[10], datagram[11]]);
        let pts_us = u64::from_le_bytes(datagram[12..20].try_into().ok()?);
        let send_ts_us = u64::from_le_bytes(datagram[20..28].try_into().ok()?);
        let payload_len =
            u32::from_le_bytes([datagram[28], datagram[29], datagram[30], datagram[31]]) as usize;

        if datagram.len() < HEADER_SIZE + payload_len {
            return None;
        }
        let payload = &datagram[HEADER_SIZE..HEADER_SIZE + payload_len];

        let fragmented = flags & sidelens_proto::FLAG_FRAGMENT != 0;
        let (fragment_index, fragment_count, total_size, data) = if fragmented {
            match sidelens_core::fragment::FragmentHeader::decode(payload) {
                Some(hdr) => (
                    hdr.index,
                    hdr.count,
                    hdr.total_size,
                    payload[sidelens_core::fragment::FRAGMENT_HEADER_SIZE..].to_vec(),
                ),
                None => (0, 1, payload.len() as u32, payload.to_vec()),
            }
        } else {
            (0, 1, payload.len() as u32, payload.to_vec())
        };

        // 非媒体帧（FEC/NACK/RTT）不进重组器，直接算「完成」
        let completed = if !fragmented && !ty.is_media() {
            Some(sidelens_core::fragment::ReassembledFrame {
                ty,
                flags,
                stream_id,
                seq,
                pts_us,
                send_ts_us,
                payload: payload.to_vec(),
            })
        } else {
            let piece = sidelens_core::fragment::DatagramPiece {
                stream_id,
                seq,
                ty,
                flags,
                pts_us,
                send_ts_us,
                payload,
            };
            asm.push(piece, sidelens_proto::now_us())
        };

        Some(UdpDatagram {
            from,
            ty,
            seq,
            flags,
            pts_us,
            send_ts_us,
            fragmented,
            fragment_index,
            fragment_count,
            total_size,
            data,
            payload: payload.to_vec(),
            completed,
        })
    }
}

/// 从一段字节流里切出完整帧（用于测试与诊断）
pub fn decode_stream(bytes: &[u8]) -> Vec<(FrameType, u32, Vec<u8>)> {
    let mut dec = FrameDecoder::new();
    dec.push(bytes);
    dec.frames()
        .iter()
        .map(|m| (m.ty, m.seq, dec.payload(*m).to_vec()))
        .collect()
}
