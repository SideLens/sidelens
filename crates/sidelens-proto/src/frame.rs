use std::fmt;

use crate::clock::now_us;

/// 帧头 magic：`SLN0`
pub const MAGIC: [u8; 4] = [0x53, 0x4c, 0x4e, 0x30];
/// 帧头长度（字节）
pub const HEADER_SIZE: usize = 32;
/// 协议版本
pub const PROTOCOL_VERSION: u8 = 0;
/// flags 的 bit0：关键帧
pub const FLAG_KEYFRAME: u8 = 0x01;
/// flags 的 bit1：这一帧是重传的（接收端据此区分"首传"与"重传"，避免重复计入统计）
pub const FLAG_RETRANSMIT: u8 = 0x02;
/// flags 的 bit2：分片（UDP 传输时一帧被切成多个包）
pub const FLAG_FRAGMENT: u8 = 0x04;

/// 帧类型。控制帧的 payload 是 UTF-8 JSON，媒体帧是裸码流。
///
/// # 版本演进
///
/// - v0：0-6（HELLO / CONFIG / VIDEO / AUDIO / KEYFRAME_REQ / STATS / BYE）
/// - v1：新增 7-11（NACK / RETRANSMIT / FEC / RTT_PROBE / RTT_REPLY）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum FrameType {
    Hello = 0,
    Config = 1,
    Video = 2,
    Audio = 3,
    KeyframeReq = 4,
    Stats = 5,
    Bye = 6,
    /// 接收端 → 发送端：请求重传（JSON: `{"stream":0,"seqs":[12,13]}`）
    Nack = 7,
    /// 发送端 → 接收端：重传的帧（flags 里的 `FLAG_RETRANSMIT` 被置位）
    Retransmit = 8,
    /// 前向纠错包（预留，当前未实现）
    Fec = 9,
    /// 时钟探测：T1（发送端时刻）
    RttProbe = 10,
    /// 时钟回复：T1 回显 + T2（接收端收到）+ T3（接收端发出）
    RttReply = 11,
}

impl FrameType {
    /// 全部合法取值，用于穷举校验
    pub const ALL: [FrameType; 12] = [
        FrameType::Hello,
        FrameType::Config,
        FrameType::Video,
        FrameType::Audio,
        FrameType::KeyframeReq,
        FrameType::Stats,
        FrameType::Bye,
        FrameType::Nack,
        FrameType::Retransmit,
        FrameType::Fec,
        FrameType::RttProbe,
        FrameType::RttReply,
    ];

    /// 从 u8 解析；非法值返回 None
    pub fn from_u8(v: u8) -> Option<Self> {
        Self::ALL.into_iter().find(|t| *t as u8 == v)
    }

    /// 是否为控制帧（payload 是 JSON）
    pub fn is_control(self) -> bool {
        !matches!(
            self,
            FrameType::Video | FrameType::Audio | FrameType::Retransmit | FrameType::Fec
        )
    }

    /// 是否为媒体帧（需要进抖动缓冲）
    pub fn is_media(self) -> bool {
        matches!(
            self,
            FrameType::Video | FrameType::Audio | FrameType::Retransmit
        )
    }
}

impl fmt::Display for FrameType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(type_name(*self))
    }
}

/// 帧类型名，未知值返回 `"UNKNOWN"`
pub fn type_name(t: FrameType) -> &'static str {
    match t {
        FrameType::Hello => "HELLO",
        FrameType::Config => "CONFIG",
        FrameType::Video => "VIDEO",
        FrameType::Audio => "AUDIO",
        FrameType::KeyframeReq => "KEYFRAME_REQ",
        FrameType::Stats => "STATS",
        FrameType::Bye => "BYE",
        FrameType::Nack => "NACK",
        FrameType::Retransmit => "RETRANSMIT",
        FrameType::Fec => "FEC",
        FrameType::RttProbe => "RTT_PROBE",
        FrameType::RttReply => "RTT_REPLY",
    }
}

/// 编码参数。`pts_us` / `send_ts_us` 为 None 时取当前时钟。
#[derive(Debug, Clone)]
pub struct FrameSpec<'a> {
    pub ty: FrameType,
    pub flags: u8,
    pub stream_id: u16,
    pub seq: u32,
    pub pts_us: Option<u64>,
    pub send_ts_us: Option<u64>,
    pub payload: &'a [u8],
}

impl<'a> Default for FrameSpec<'a> {
    fn default() -> Self {
        Self {
            ty: FrameType::Video,
            flags: 0,
            stream_id: 0,
            seq: 0,
            pts_us: None,
            send_ts_us: None,
            payload: &[],
        }
    }
}

impl<'a> FrameSpec<'a> {
    /// 最简构造：只给类型和载荷
    pub fn new(ty: FrameType, payload: &'a [u8]) -> Self {
        Self {
            ty,
            payload,
            ..Default::default()
        }
    }
}

/// 编码错误
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EncodeError {
    /// 未知帧类型（u8 不在合法枚举内）
    UnknownType(u8),
    /// 载荷超过 u32 上限
    PayloadTooLarge(usize),
}

impl fmt::Display for EncodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EncodeError::UnknownType(v) => write!(f, "未知帧类型 {v}"),
            EncodeError::PayloadTooLarge(n) => write!(f, "载荷过大：{n} 字节"),
        }
    }
}

impl std::error::Error for EncodeError {}

/// 编码一帧到 `out`。复用缓冲区，避免每帧分配。
pub fn encode_frame(spec: &FrameSpec<'_>, out: &mut Vec<u8>) -> Result<(), EncodeError> {
    let payload_len = spec.payload.len();
    if payload_len > u32::MAX as usize {
        return Err(EncodeError::PayloadTooLarge(payload_len));
    }

    out.clear();
    out.reserve(HEADER_SIZE + payload_len);

    out.extend_from_slice(&MAGIC);
    out.push(spec.ty as u8);
    out.push(spec.flags);
    out.extend_from_slice(&spec.stream_id.to_le_bytes());
    out.extend_from_slice(&spec.seq.to_le_bytes());
    out.extend_from_slice(&spec.pts_us.unwrap_or_else(now_us).to_le_bytes());
    out.extend_from_slice(&spec.send_ts_us.unwrap_or_else(now_us).to_le_bytes());
    out.extend_from_slice(&(payload_len as u32).to_le_bytes());
    out.extend_from_slice(spec.payload);
    Ok(())
}

/// 便捷版：返回新分配的 Vec
pub fn encode_frame_owned(spec: &FrameSpec<'_>) -> Result<Vec<u8>, EncodeError> {
    let mut out = Vec::with_capacity(HEADER_SIZE + spec.payload.len());
    encode_frame(spec, &mut out)?;
    Ok(out)
}
