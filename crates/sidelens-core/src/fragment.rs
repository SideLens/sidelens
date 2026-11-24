//! 分片与重组
//!
//! # 为什么需要
//!
//! UDP 单包上限受 MTU 限制（以太网 1500，扣除 IP/UDP 头后约 1472；
//! WiFi 与隧道环境下实际安全值约 1200）。一个 8KB 的 1080p 帧必须切成 7 片。
//!
//! # 为什么不用 IP 层分片
//!
//! IP 分片的问题是**丢任何一片，整个 IP 包都作废**，而且接收方要等所有片到齐
//! 才能开始重组，重传粒度是整个 IP 包（8KB）。应用层分片可以做到：
//!
//! - 只重传丢的那一片（1.2KB），不是整个帧
//! - 单独判断哪一片缺，精确 NACK
//! - 可以只交出一部分片给解码器（某些解码器支持部分帧）
//!
//! # 分片头格式（8 字节，小端）
//!
//! | 偏移 | 长度 | 字段 |
//! |------|------|------|
//! | 0 | 2 | fragment_index（从 0 开始） |
//! | 2 | 2 | fragment_count |
//! | 4 | 4 | total_size（原始帧总字节数，用于预分配） |
//!
//! 放在帧头（32B）之后、原 payload 之前。`FLAG_FRAGMENT` 置位表示存在这个头。

use std::collections::HashMap;

use sidelens_proto::{FrameType, FLAG_FRAGMENT};

/// 分片头长度
pub const FRAGMENT_HEADER_SIZE: usize = 8;

/// 安全的 UDP 载荷上限（留出余量应对隧道/PPPoE 等场景）
pub const SAFE_MTU_PAYLOAD: usize = 1200;

/// 分片头的元数据
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FragmentHeader {
    pub index: u16,
    pub count: u16,
    pub total_size: u32,
}

impl FragmentHeader {
    pub fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.index.to_le_bytes());
        out.extend_from_slice(&self.count.to_le_bytes());
        out.extend_from_slice(&self.total_size.to_le_bytes());
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < FRAGMENT_HEADER_SIZE {
            return None;
        }
        Some(Self {
            index: u16::from_le_bytes([bytes[0], bytes[1]]),
            count: u16::from_le_bytes([bytes[2], bytes[3]]),
            total_size: u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
        })
    }
}

/// 把一帧切成若干分片
///
/// 返回每个分片的完整字节（含帧头 + 分片头 + 数据），可直接 sendto。
///
/// # 参数
///
/// - `encoded`：已经编码好的完整帧（32B 头 + payload）
/// - `max_datagram`：单个 UDP 包的载荷上限（含帧头与分片头）
///
/// # 注意
///
/// 帧头里的 `payload_len` 会被改写成"这一片的数据长度"，并把 `FLAG_FRAGMENT` 置位。
/// 重组端靠 `total_size`（分片头里）还原完整帧长度。
pub fn fragment_frame(encoded: &[u8], max_datagram: usize) -> Vec<Vec<u8>> {
    use sidelens_proto::HEADER_SIZE;

    if encoded.len() <= max_datagram {
        // 不需要分片：原样返回
        return vec![encoded.to_vec()];
    }

    let frame_header = &encoded[..HEADER_SIZE];
    let payload = &encoded[HEADER_SIZE..];
    let total_size = payload.len();

    let per_frag = max_datagram
        .saturating_sub(HEADER_SIZE)
        .saturating_sub(FRAGMENT_HEADER_SIZE)
        .max(1);

    let count = total_size.div_ceil(per_frag);
    if count > u16::MAX as usize {
        // 帧太大，放弃分片（上层应当限制单帧大小）
        return vec![encoded.to_vec()];
    }

    let mut out = Vec::with_capacity(count);
    for (index, chunk) in payload.chunks(per_frag).enumerate() {
        let mut buf = Vec::with_capacity(HEADER_SIZE + FRAGMENT_HEADER_SIZE + chunk.len());

        // 复制原帧头，改 flags 与 payload_len
        buf.extend_from_slice(frame_header);
        buf[5] |= FLAG_FRAGMENT;

        // 分片头
        FragmentHeader {
            index: index as u16,
            count: count as u16,
            total_size: total_size as u32,
        }
        .encode(&mut buf);

        buf.extend_from_slice(chunk);

        // 改写 payload_len = 分片头 + 数据
        let new_len = (FRAGMENT_HEADER_SIZE + chunk.len()) as u32;
        buf[28..32].copy_from_slice(&new_len.to_le_bytes());

        out.push(buf);
    }
    out
}

/// 重组中的一帧
#[derive(Debug)]
struct PartialFrame {
    ty: FrameType,
    flags: u8,
    stream_id: u16,
    seq: u32,
    pts_us: u64,
    send_ts_us: u64,
    total_size: usize,
    received: usize,
    /// 已收到的片（按下标放）
    chunks: Vec<Option<Vec<u8>>>,
    /// 首片的帧头（用于还原完整帧的头）
    first_seen_us: u64,
}

impl PartialFrame {
    fn is_complete(&self) -> bool {
        self.received == self.chunks.len() && self.chunks.iter().all(|c| c.is_some())
    }

    /// 拼接成完整 payload
    fn assemble(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.total_size);
        for v in self.chunks.iter().flatten() {
            out.extend_from_slice(v);
        }
        out
    }
}

/// 重组结果
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReassembledFrame {
    pub ty: FrameType,
    pub flags: u8,
    pub stream_id: u16,
    pub seq: u32,
    pub pts_us: u64,
    pub send_ts_us: u64,
    pub payload: Vec<u8>,
}

impl ReassembledFrame {
    pub fn is_keyframe(&self) -> bool {
        self.flags & sidelens_proto::FLAG_KEYFRAME != 0
    }
}

/// 一个待重组的数据报片段
///
/// 打包成结构体而不是散列成 8 个形参：UDP 解包处字段来源不同
/// （头里几个、载荷一个），散传极易顺序传错。
#[derive(Debug, Clone, Copy)]
pub struct DatagramPiece<'a> {
    pub stream_id: u16,
    pub seq: u32,
    pub ty: FrameType,
    pub flags: u8,
    pub pts_us: u64,
    pub send_ts_us: u64,
    /// 帧头之后的载荷（若 `FLAG_FRAGMENT` 置位，开头是 8 字节分片头）
    pub payload: &'a [u8],
}

/// 重组统计
#[derive(Debug, Clone, Copy, Default)]
pub struct ReassemblerStats {
    /// 收到的分片总数
    pub fragments_in: u64,
    /// 完整还原的帧数
    pub frames_out: u64,
    /// 因缺片而丢弃的帧数
    pub frames_dropped: u64,
    /// 因超时丢弃的半成品数
    pub expired: u64,
    /// 重复分片（同一帧同一片收到两次）
    pub duplicates: u64,
    /// 收到分片但对应帧已被清理（乱序太严重）
    pub orphans: u64,
}

/// 分片重组器
///
/// 按 `(stream_id, seq)` 聚合同一帧的分片；齐了就吐出完整帧。
///
/// # 缺片检测
///
/// 每当一帧齐了或者超时被丢弃，都会通过 [`Reassembler::missing`] 报告
/// 那一帧缺了哪些分片下标 —— 上层据此发出精确的 NACK。
///
/// 注意 NACK 是按**帧 seq** 请求重传整个帧的（不按分片），
/// 因为发送端重传时是整帧重新分片，比"只补某一片"实现简单、也够用。
#[derive(Debug)]
pub struct Reassembler {
    partial: HashMap<(u16, u32), PartialFrame>,
    /// 半成品超时（毫秒）
    max_age_ms: f64,
    /// 同时在重组中的帧数上限
    max_pending: usize,
    pub stats: ReassemblerStats,
}

impl Default for Reassembler {
    fn default() -> Self {
        Self::new(100.0, 64)
    }
}

impl Reassembler {
    pub fn new(max_age_ms: f64, max_pending: usize) -> Self {
        Self {
            partial: HashMap::new(),
            max_age_ms,
            max_pending: max_pending.max(1),
            stats: ReassemblerStats::default(),
        }
    }

    pub fn pending_count(&self) -> usize {
        self.partial.len()
    }

    /// 投递一个分片（或完整帧）
    ///
    /// 返回 `Some(完整帧)` 表示这一帧已齐；`None` 表示还在等其余分片，
    /// 或者这个分片属于一个已被清理的帧。
    pub fn push(&mut self, piece: DatagramPiece<'_>, now_us: u64) -> Option<ReassembledFrame> {
        let DatagramPiece {
            stream_id,
            seq,
            ty,
            flags,
            pts_us,
            send_ts_us,
            payload: payload_with_frag_header,
        } = piece;

        self.stats.fragments_in += 1;

        // 不分片的帧：直接吐出
        if flags & FLAG_FRAGMENT == 0 {
            self.stats.frames_out += 1;
            return Some(ReassembledFrame {
                ty,
                flags,
                stream_id,
                seq,
                pts_us,
                send_ts_us,
                payload: payload_with_frag_header.to_vec(),
            });
        }

        let hdr = FragmentHeader::decode(payload_with_frag_header)?;
        let data = &payload_with_frag_header[FRAGMENT_HEADER_SIZE..];

        if hdr.count == 0 || hdr.index >= hdr.count {
            return None;
        }

        let key = (stream_id, seq);
        let total = hdr.total_size as usize;

        // 容量保护：先清理过期的，再判断是否仍超限
        if !self.partial.contains_key(&key) && self.partial.len() >= self.max_pending {
            self.expire(now_us);
            if self.partial.len() >= self.max_pending {
                // 仍然满：淘汰最老的一个
                if let Some((&oldest_key, _)) =
                    self.partial.iter().min_by_key(|(_, f)| f.first_seen_us)
                {
                    self.partial.remove(&oldest_key);
                    self.stats.frames_dropped += 1;
                }
            }
        }

        let entry = self.partial.entry(key).or_insert_with(|| PartialFrame {
            ty,
            flags,
            stream_id,
            seq,
            pts_us,
            send_ts_us,
            total_size: total,
            received: 0,
            chunks: vec![None; hdr.count as usize],
            first_seen_us: now_us,
        });

        let idx = hdr.index as usize;
        if idx >= entry.chunks.len() {
            return None;
        }
        if entry.chunks[idx].is_some() {
            self.stats.duplicates += 1;
            return None;
        }

        entry.chunks[idx] = Some(data.to_vec());
        entry.received += 1;

        if !entry.is_complete() {
            return None;
        }

        let done = self.partial.remove(&key)?;
        self.stats.frames_out += 1;
        Some(ReassembledFrame {
            ty: done.ty,
            flags: done.flags & !FLAG_FRAGMENT,
            stream_id: done.stream_id,
            seq: done.seq,
            pts_us: done.pts_us,
            send_ts_us: done.send_ts_us,
            payload: done.assemble(),
        })
    }

    /// 清理超时的半成品，返回被丢弃的 `(stream_id, seq)` 列表
    ///
    /// 上层拿到这个列表后应当对每个 seq 发 NACK。
    pub fn expire(&mut self, now_us: u64) -> Vec<(u16, u32)> {
        let max_age_us = (self.max_age_ms * 1000.0) as u64;
        let expired: Vec<(u16, u32)> = self
            .partial
            .iter()
            .filter(|(_, f)| now_us.saturating_sub(f.first_seen_us) > max_age_us)
            .map(|(k, _)| *k)
            .collect();

        for key in &expired {
            self.partial.remove(key);
            self.stats.frames_dropped += 1;
            self.stats.expired += 1;
        }
        expired
    }

    /// 快速缺片检测：某帧的半成品已经"不可能再补齐"时立即报出
    ///
    /// # 为什么需要
    ///
    /// 只靠超时（`max_age_ms`）会让缺片的帧白等整整 30ms 才发 NACK，
    /// 重传再花 1 个 RTT，整体延迟可能翻倍。
    ///
    /// 但 UDP 有个有用的性质：**同一 socket 上，后来的包到了说明前面的包
    /// 已经过了网络层的最长可能路径**。所以当发现「seq + N 的完整帧已收下」，
    /// 而 seq 还缺片，就可以断定缺的片不会再来了，立刻报缺。
    ///
    /// 这里用 `gap` 参数：后续已完成的帧序号超过当前半成品 `gap` 个，
    /// 就认为它没救了。默认 `gap = 3`（约 50ms @60fps，比超时快）。
    pub fn poll_stale(&mut self, highest_completed_seq: u32, gap: u32) -> Vec<(u16, u32)> {
        let stale: Vec<(u16, u32)> = self
            .partial
            .iter()
            .filter(|((_, seq), _)| highest_completed_seq > seq.saturating_add(gap))
            .map(|(k, _)| *k)
            .collect();

        for key in &stale {
            self.partial.remove(key);
            self.stats.frames_dropped += 1;
            self.stats.expired += 1;
        }
        stale
    }

    /// 查询某帧还缺哪些分片下标（用于诊断）
    pub fn missing_fragments(&self, stream_id: u16, seq: u32) -> Vec<u16> {
        let Some(f) = self.partial.get(&(stream_id, seq)) else {
            return Vec::new();
        };
        f.chunks
            .iter()
            .enumerate()
            .filter(|(_, c)| c.is_none())
            .map(|(i, _)| i as u16)
            .collect()
    }

    /// 丢掉某帧（收到重传后重新组装时用）
    pub fn forget(&mut self, stream_id: u16, seq: u32) {
        self.partial.remove(&(stream_id, seq));
    }

    pub fn clear(&mut self) {
        self.partial.clear();
    }
}
