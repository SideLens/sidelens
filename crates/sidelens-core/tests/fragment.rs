//! 分片与重组测试
//!
//! 核心验证点：任意丢一片都能被检测到并精确报出，不静默丢帧。

use sidelens_core::{fragment_frame, DatagramPiece, Reassembler, SAFE_MTU_PAYLOAD};
use sidelens_proto::{
    encode_frame_owned, FrameSpec, FrameType, FLAG_FRAGMENT, FLAG_KEYFRAME, HEADER_SIZE,
};

fn build_frame(payload_len: usize) -> Vec<u8> {
    let payload: Vec<u8> = (0..payload_len).map(|i| (i % 251) as u8).collect();
    encode_frame_owned(&FrameSpec {
        ty: FrameType::Video,
        flags: 0,
        stream_id: 0,
        seq: 1,
        pts_us: Some(1_000_000),
        send_ts_us: Some(1_000_000),
        payload: &payload,
    })
    .unwrap()
}

/// 把分片喂进重组器，每片都用独立调用（模拟真实 UDP 到达）
fn feed_all(
    asm: &mut Reassembler,
    frags: &[Vec<u8>],
    now_us: u64,
) -> Option<sidelens_core::ReassembledFrame> {
    let mut result = None;
    for f in frags {
        let p = parse_datagram(f);
        let piece = DatagramPiece {
            stream_id: p.stream_id,
            seq: p.seq,
            ty: p.ty,
            flags: p.flags,
            pts_us: p.pts_us,
            send_ts_us: p.send_ts_us,
            payload: &p.payload,
        };
        if let Some(r) = asm.push(piece, now_us) {
            result = Some(r);
        }
    }
    result
}

struct Parsed {
    stream_id: u16,
    seq: u32,
    ty: FrameType,
    flags: u8,
    pts_us: u64,
    send_ts_us: u64,
    payload: Vec<u8>,
}

fn parse_datagram(bytes: &[u8]) -> Parsed {
    assert!(bytes.len() >= HEADER_SIZE);
    let ty = FrameType::from_u8(bytes[4]).expect("类型应合法");
    let payload_len = u32::from_le_bytes([bytes[28], bytes[29], bytes[30], bytes[31]]) as usize;
    Parsed {
        stream_id: u16::from_le_bytes([bytes[6], bytes[7]]),
        seq: u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]),
        ty,
        flags: bytes[5],
        pts_us: u64::from_le_bytes(bytes[12..20].try_into().unwrap()),
        send_ts_us: u64::from_le_bytes(bytes[20..28].try_into().unwrap()),
        payload: bytes[HEADER_SIZE..HEADER_SIZE + payload_len].to_vec(),
    }
}

#[test]
fn small_frame_is_not_fragmented() {
    let frame = build_frame(100);
    let frags = fragment_frame(&frame, SAFE_MTU_PAYLOAD);
    assert_eq!(frags.len(), 1);
    assert_eq!(frags[0], frame, "小于 MTU 的帧应原样返回");
}

#[test]
fn large_frame_is_split_and_reassembled() {
    let frame = build_frame(8000);
    let frags = fragment_frame(&frame, SAFE_MTU_PAYLOAD);
    assert!(frags.len() > 1, "8KB 帧必须被切分，实际 {} 片", frags.len());

    // 每片都不超过 max_datagram
    for f in &frags {
        assert!(f.len() <= SAFE_MTU_PAYLOAD, "分片 {} 字节超限", f.len());
    }

    let mut asm = Reassembler::new(100.0, 64);
    let out = feed_all(&mut asm, &frags, 0).expect("应能重组");
    assert_eq!(out.payload.len(), 8000);
    // 逐字节与原 payload 对比
    let expected: Vec<u8> = (0..8000).map(|i| (i % 251) as u8).collect();
    assert_eq!(out.payload, expected);
}

#[test]
fn fragment_flag_is_set_on_pieces() {
    let frame = build_frame(5000);
    let frags = fragment_frame(&frame, SAFE_MTU_PAYLOAD);
    for f in &frags {
        assert_ne!(f[5] & FLAG_FRAGMENT, 0, "分片必须置位 FLAG_FRAGMENT");
    }
}

#[test]
fn reassembled_frame_clears_fragment_flag() {
    let frame = build_frame(5000);
    let frags = fragment_frame(&frame, SAFE_MTU_PAYLOAD);
    let mut asm = Reassembler::new(100.0, 64);
    let out = feed_all(&mut asm, &frags, 0).expect("应能重组");
    assert_eq!(out.flags & FLAG_FRAGMENT, 0, "重组后应清除分片标志");
}

#[test]
fn missing_fragment_blocks_reassembly() {
    let frame = build_frame(8000);
    let mut frags = fragment_frame(&frame, SAFE_MTU_PAYLOAD);
    // 丢掉中间一片
    frags.remove(2);

    let mut asm = Reassembler::new(100.0, 64);
    let out = feed_all(&mut asm, &frags, 0);
    assert!(out.is_none(), "缺片时不应吐出完整帧");
    assert_eq!(asm.pending_count(), 1, "半成品应留在重组器里");
}

#[test]
fn missing_fragments_are_reported() {
    let frame = build_frame(8000);
    let mut frags = fragment_frame(&frame, SAFE_MTU_PAYLOAD);
    let dropped_index = 2;
    frags.remove(dropped_index);

    let mut asm = Reassembler::new(100.0, 64);
    feed_all(&mut asm, &frags, 0);

    let missing = asm.missing_fragments(0, 1);
    assert_eq!(missing, vec![dropped_index as u16], "应精确报出缺失的下标");
}

#[test]
fn expired_partial_frame_is_reported() {
    let frame = build_frame(8000);
    let mut frags = fragment_frame(&frame, SAFE_MTU_PAYLOAD);
    frags.remove(1);

    let mut asm = Reassembler::new(100.0, 64);
    feed_all(&mut asm, &frags, 0);

    // 50ms 内还没到期
    assert!(asm.expire(50_000).is_empty());
    assert_eq!(asm.pending_count(), 1);

    // 150ms 后到期
    let expired = asm.expire(150_000);
    assert_eq!(expired, vec![(0, 1)], "应报出超时的 (stream, seq)");
    assert_eq!(asm.pending_count(), 0);
    assert_eq!(asm.stats.expired, 1);
}

#[test]
fn duplicate_fragment_is_ignored() {
    let frame = build_frame(5000);
    let frags = fragment_frame(&frame, SAFE_MTU_PAYLOAD);
    let mut asm = Reassembler::new(100.0, 64);

    // 重复投递第一片
    let p = parse_datagram(&frags[0]);
    let piece = DatagramPiece {
        stream_id: 0,
        seq: 1,
        ty: p.ty,
        flags: p.flags,
        pts_us: p.pts_us,
        send_ts_us: p.send_ts_us,
        payload: &p.payload,
    };
    asm.push(piece, 0);
    asm.push(piece, 0);

    assert_eq!(asm.stats.duplicates, 1);

    // 其余片正常投递，仍应能重组
    let out = feed_all(&mut asm, &frags[1..], 0);
    assert!(out.is_some(), "重复分片不应妨碍重组");
}

#[test]
fn keyframe_flag_survives_fragmentation() {
    let payload = vec![7u8; 6000];
    let frame = encode_frame_owned(&FrameSpec {
        ty: FrameType::Video,
        flags: FLAG_KEYFRAME,
        stream_id: 0,
        seq: 42,
        pts_us: Some(5000),
        send_ts_us: Some(6000),
        payload: &payload,
    })
    .unwrap();

    let frags = fragment_frame(&frame, SAFE_MTU_PAYLOAD);
    let mut asm = Reassembler::new(100.0, 64);
    let out = feed_all(&mut asm, &frags, 0).expect("应能重组");

    assert!(out.is_keyframe(), "关键帧标志必须保留");
    assert_eq!(out.seq, 42);
    assert_eq!(out.pts_us, 5000);
    assert_eq!(out.send_ts_us, 6000);
}

#[test]
fn pending_limit_evicts_oldest() {
    let mut asm = Reassembler::new(100.0, 2);

    // 造 3 个都不完整的帧
    for seq in 1..=3u32 {
        let payload = vec![0u8; 5000];
        let frame = encode_frame_owned(&FrameSpec {
            ty: FrameType::Video,
            flags: 0,
            stream_id: 0,
            seq,
            pts_us: Some(seq as u64 * 1000),
            send_ts_us: Some(0),
            payload: &payload,
        })
        .unwrap();
        let mut frags = fragment_frame(&frame, SAFE_MTU_PAYLOAD);
        frags.remove(0); // 缺首片，永远不完整
        for f in &frags {
            let p = parse_datagram(f);
            let piece = DatagramPiece {
                stream_id: 0,
                seq,
                ty: p.ty,
                flags: p.flags,
                pts_us: p.pts_us,
                send_ts_us: p.send_ts_us,
                payload: &p.payload,
            };
            asm.push(piece, seq as u64 * 1000);
        }
    }

    assert!(asm.pending_count() <= 2, "应受 max_pending 限制");
    assert!(asm.stats.frames_dropped > 0);
}

#[test]
fn multiple_streams_do_not_interfere() {
    let mut asm = Reassembler::new(100.0, 64);

    // 同一 seq 但不同流
    for stream in 0..2u16 {
        let payload = vec![stream as u8; 4000];
        let frame = encode_frame_owned(&FrameSpec {
            ty: if stream == 0 {
                FrameType::Video
            } else {
                FrameType::Audio
            },
            flags: 0,
            stream_id: stream,
            seq: 7,
            pts_us: Some(1000),
            send_ts_us: Some(0),
            payload: &payload,
        })
        .unwrap();
        let frags = fragment_frame(&frame, SAFE_MTU_PAYLOAD);
        let out = feed_all(&mut asm, &frags, 0).expect("两路都该成功");
        assert_eq!(out.stream_id, stream);
        assert_eq!(out.payload[0], stream as u8);
    }
}
