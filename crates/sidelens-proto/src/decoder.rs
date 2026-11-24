use crate::frame::{FrameType, FLAG_KEYFRAME, HEADER_SIZE, MAGIC};

/// 已解析帧的元数据 + 载荷在解码器缓冲区中的位置
///
/// 为什么不直接返回 `&[u8]` 切片：那样会让解码器在帧的生命周期内被永久借用，
/// 后续 `push` 全都编译不过。拆成「元数据 + 区间」就能先解析一批、
/// 再在不可变借用下逐个取载荷，全程零拷贝。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameMeta {
    pub ty: FrameType,
    pub flags: u8,
    pub stream_id: u16,
    pub seq: u32,
    pub pts_us: u64,
    pub send_ts_us: u64,
    /// 载荷在解码器缓冲区中的绝对区间 `[start, end)`
    pub range: (usize, usize),
}

impl FrameMeta {
    pub fn is_keyframe(&self) -> bool {
        self.flags & FLAG_KEYFRAME != 0
    }

    pub fn payload_len(&self) -> usize {
        self.range.1 - self.range.0
    }
}

/// 增量解码器
///
/// 喂任意切分的字节块，吐出完整帧。UDP 场景下每包就是完整帧，
/// 但 TCP / WebSocket 会分片或合并，所以必须做流式拼装。
///
/// # 用法
///
/// ```
/// use sidelens_proto::{encode_frame_owned, FrameDecoder, FrameSpec, FrameType};
///
/// let bytes = encode_frame_owned(&FrameSpec::new(FrameType::Video, b"abc")).unwrap();
/// let mut dec = FrameDecoder::new();
/// assert_eq!(dec.push(&bytes), 1);
/// for meta in dec.frames() {
///     let payload = dec.payload(*meta);
///     assert_eq!(payload, b"abc");
/// }
/// ```
#[derive(Debug, Default)]
pub struct FrameDecoder {
    buf: Vec<u8>,
    /// buf 中已消费的字节数
    start: usize,
    /// 已解析出的帧（元数据）
    frames: Vec<FrameMeta>,
    /// 累计丢弃的垃圾字节（用于诊断）
    pub resync_skipped: u64,
}

/// 压缩阈值：已消费超过这么多字节才搬移剩余部分，避免每帧 memmove
const COMPACT_THRESHOLD: usize = 64 * 1024;
/// magic 长度
const MAGIC_LEN: usize = 4;

impl FrameDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// 尚未组成完整帧的字节数
    pub fn pending_bytes(&self) -> usize {
        self.buf.len() - self.start
    }

    /// 本轮解析出的帧（不可变借用）
    pub fn frames(&self) -> &[FrameMeta] {
        &self.frames
    }

    /// 取某帧的载荷（不可变借用，零拷贝）
    pub fn payload(&self, meta: FrameMeta) -> &[u8] {
        &self.buf[meta.range.0..meta.range.1]
    }

    /// 控制帧的载荷解析为 JSON
    pub fn json(&self, meta: FrameMeta) -> Option<serde_json::Value> {
        if !meta.ty.is_control() || meta.payload_len() == 0 {
            return None;
        }
        serde_json::from_slice(self.payload(meta)).ok()
    }

    /// 清空本轮已解析的帧（下一轮 push 前调用，或让 push 自动清）
    pub fn clear_frames(&mut self) {
        self.frames.clear();
    }

    /// 喂入一块数据并解析出所有完整帧
    ///
    /// 返回本轮新增的帧数。上一轮的帧元数据会被清空 —— 若需保留，
    /// 请在 push 前把 `frames()` 里的内容读走。
    pub fn push(&mut self, chunk: &[u8]) -> usize {
        self.frames.clear();

        if !chunk.is_empty() {
            self.compact_if_needed();
            self.buf.extend_from_slice(chunk);
        }

        let mut parsed = 0;
        loop {
            let avail_start = self.start;
            let avail_len = self.buf.len() - avail_start;

            if avail_len < HEADER_SIZE {
                break;
            }

            let head = &self.buf[avail_start..];

            if head[..MAGIC_LEN] != MAGIC {
                // 流被污染：向后找下一个 magic，避免整条连接作废
                match find_magic(&head[1..]) {
                    Some(pos) => {
                        let skip = pos + 1;
                        self.resync_skipped += skip as u64;
                        self.start += skip;
                        continue;
                    }
                    None => {
                        // 只保留末尾 3 字节（可能是被切开的 magic 前缀）
                        let keep = avail_len.saturating_sub(MAGIC_LEN - 1);
                        self.resync_skipped += keep as u64;
                        self.start += keep;
                        break;
                    }
                }
            }

            let payload_len = u32::from_le_bytes([head[28], head[29], head[30], head[31]]) as usize;
            let total = HEADER_SIZE + payload_len;
            if avail_len < total {
                break;
            }

            let Some(ty) = FrameType::from_u8(head[4]) else {
                // 类型非法：这个 magic 是巧合，跳过它继续找
                self.resync_skipped += 1;
                self.start += 1;
                continue;
            };

            self.frames.push(FrameMeta {
                ty,
                flags: head[5],
                stream_id: u16::from_le_bytes([head[6], head[7]]),
                seq: u32::from_le_bytes([head[8], head[9], head[10], head[11]]),
                pts_us: u64::from_le_bytes(head[12..20].try_into().unwrap()),
                send_ts_us: u64::from_le_bytes(head[20..28].try_into().unwrap()),
                range: (avail_start + HEADER_SIZE, avail_start + total),
            });
            self.start += total;
            parsed += 1;
        }

        parsed
    }

    fn compact_if_needed(&mut self) {
        if self.start >= COMPACT_THRESHOLD {
            self.buf.drain(..self.start);
            // 已解析帧的 range 是绝对偏移，压缩后必须同步平移
            for f in &mut self.frames {
                f.range.0 -= self.start;
                f.range.1 -= self.start;
            }
            self.start = 0;
        }
    }
}

fn find_magic(hay: &[u8]) -> Option<usize> {
    if hay.len() < MAGIC_LEN {
        return None;
    }
    hay.windows(MAGIC_LEN).position(|w| w == MAGIC)
}
