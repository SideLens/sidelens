//! 快速缺片检测
//!
//! 核心验证：后续帧已到时，缺片的帧应立刻被报出，而不是白等超时。

use sidelens_core::{fragment_frame, DatagramPiece, Reassembler, SAFE_MTU_PAYLOAD};
use sidelens_proto::{encode_frame_owned, FrameSpec, FrameType, HEADER_SIZE};

fn build(seq: u32, payload_len: usize) -> Vec<u8> {
    let payload: Vec<u8> = (0..payload_len).map(|i| (i % 251) as u8).collect();
    encode_frame_owned(&FrameSpec {
        ty: FrameType::Video,
        flags: 0,
        stream_id: 0,
        seq,
        pts_us: Some(seq as u64 * 16_667),
        send_ts_us: Some(0),
        payload: &payload,
    })
    .unwrap()
}

fn feed(asm: &mut Reassembler, datagram: &[u8], now: u64) -> bool {
    let ty = FrameType::from_u8(datagram[4]).unwrap();
    let payload_len =
        u32::from_le_bytes([datagram[28], datagram[29], datagram[30], datagram[31]]) as usize;
    let piece = DatagramPiece {
        stream_id: u16::from_le_bytes([datagram[6], datagram[7]]),
        seq: u32::from_le_bytes([datagram[8], datagram[9], datagram[10], datagram[11]]),
        ty,
        flags: datagram[5],
        pts_us: u64::from_le_bytes(datagram[12..20].try_into().unwrap()),
        send_ts_us: u64::from_le_bytes(datagram[20..28].try_into().unwrap()),
        payload: &datagram[HEADER_SIZE..HEADER_SIZE + payload_len],
    };
    asm.push(piece, now).is_some()
}

#[test]
fn stale_detection_fires_before_timeout() {
    let mut asm = Reassembler::new(1000.0, 64); // 超时设成 1 秒，确保不是超时触发的

    // seq 1 缺一片
    let f1 = build(1, 8000);
    let mut frags1 = fragment_frame(&f1, SAFE_MTU_PAYLOAD);
    frags1.remove(2);
    for f in &frags1 {
        feed(&mut asm, f, 0);
    }
    assert_eq!(asm.pending_count(), 1);

    // seq 10 完整到达（超过 gap = 3）
    let f10 = build(10, 4000);
    for f in fragment_frame(&f10, SAFE_MTU_PAYLOAD) {
        feed(&mut asm, &f, 1_000);
    }

    // 此刻 seq 1 应该被判为"不可能补齐"
    let stale = asm.poll_stale(10, 3);
    assert_eq!(stale, vec![(0, 1)], "应立刻报出缺片的 seq 1");
    assert_eq!(asm.pending_count(), 0);
}

#[test]
fn stale_detection_respects_gap() {
    let mut asm = Reassembler::new(1000.0, 64);

    let f5 = build(5, 8000);
    let mut frags5 = fragment_frame(&f5, SAFE_MTU_PAYLOAD);
    frags5.remove(1);
    for f in &frags5 {
        feed(&mut asm, f, 0);
    }

    // 只到 seq 6，gap=3 时需要 highest > 5+3 = 8
    assert!(asm.poll_stale(6, 3).is_empty(), "间隔不足时不应误判");
    assert_eq!(asm.pending_count(), 1);

    // 到 seq 9 才够
    assert_eq!(asm.poll_stale(9, 3).len(), 1);
}

#[test]
fn complete_frames_are_never_stale() {
    let mut asm = Reassembler::new(1000.0, 64);

    // 4000 字节帧会被切成多片，只有最后一片才返回 true
    let f1 = build(1, 4000);
    let frags = fragment_frame(&f1, SAFE_MTU_PAYLOAD);
    assert!(frags.len() > 1, "测试前提：该帧应被分片");
    let mut completed = 0;
    for (i, f) in frags.iter().enumerate() {
        if feed(&mut asm, f, 0) {
            completed += 1;
            assert_eq!(i, frags.len() - 1, "只应在最后一片时完成");
        }
    }
    assert_eq!(completed, 1, "整帧应恰好完成一次");

    // 已完成的帧不在重建表里，poll_stale 不应报它
    assert!(asm.poll_stale(100, 3).is_empty());
    assert_eq!(asm.pending_count(), 0);
}

#[test]
fn stale_detection_handles_multiple_pending() {
    let mut asm = Reassembler::new(1000.0, 64);

    // seq 1..=3 都缺片
    for seq in 1..=3u32 {
        let f = build(seq, 8000);
        let mut frags = fragment_frame(&f, SAFE_MTU_PAYLOAD);
        frags.remove(0);
        for fr in &frags {
            feed(&mut asm, fr, 0);
        }
    }
    assert_eq!(asm.pending_count(), 3);

    let stale = asm.poll_stale(10, 3);
    assert_eq!(stale.len(), 3, "三个缺片帧都应被报出");
    assert_eq!(asm.pending_count(), 0);
}
