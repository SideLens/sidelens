//! SideLens 帧协议 v0
//!
//! 设计目标：足够简单，能在一页内读完；足够完整，能承载真实的低延迟需求。
//!
//! # 帧布局（小端）
//!
//! | 偏移 | 长度 | 字段 |
//! |------|------|------|
//! | 0 | 4 | magic `SLN0` |
//! | 4 | 1 | type |
//! | 5 | 1 | flags（bit0 = keyframe） |
//! | 6 | 2 | stream_id |
//! | 8 | 4 | seq |
//! | 12 | 8 | pts_us |
//! | 20 | 8 | send_ts_us |
//! | 28 | 4 | payload_len |
//! | 32 | N | payload |
//!
//! 头长固定 32 字节。控制帧（HELLO/CONFIG/STATS/BYE）的 payload 是 UTF-8 JSON。
//!
//! # 示例
//!
//! ```
//! use sidelens_proto::{encode_frame_owned, FrameDecoder, FrameSpec, FrameType};
//!
//! let buf = encode_frame_owned(&FrameSpec::new(FrameType::Video, b"abc")).unwrap();
//! let mut dec = FrameDecoder::new();
//! dec.push(&buf);
//! let meta = dec.frames()[0];
//! assert_eq!(dec.payload(meta), b"abc");
//! ```

#![forbid(unsafe_code)]
#![warn(missing_debug_implementations)]

mod clock;
mod decoder;
mod frame;

pub use clock::now_us;
pub use decoder::{FrameDecoder, FrameMeta};
pub use frame::{
    encode_frame, encode_frame_owned, type_name, EncodeError, FrameSpec, FrameType, FLAG_FRAGMENT,
    FLAG_KEYFRAME, FLAG_RETRANSMIT, HEADER_SIZE, MAGIC, PROTOCOL_VERSION,
};
