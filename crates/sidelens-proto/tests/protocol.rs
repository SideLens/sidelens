//! 逐条对应 JS 版的 7 个协议测试

use sidelens_proto::{
    encode_frame_owned, FrameDecoder, FrameSpec, FrameType, HEADER_SIZE, MAGIC, PROTOCOL_VERSION,
};

#[test]
fn magic_and_protocol_version() {
    assert_eq!(&MAGIC, b"SLN0");
    assert_eq!(PROTOCOL_VERSION, 0);
}

#[test]
fn header_is_32_bytes() {
    let buf = encode_frame_owned(&FrameSpec::new(FrameType::Video, b"abc")).unwrap();
    assert_eq!(buf.len(), HEADER_SIZE + 3);
    assert_eq!(buf[4], FrameType::Video as u8);
    assert_eq!(u32::from_le_bytes(buf[28..32].try_into().unwrap()), 3);
}

#[test]
fn roundtrip_preserves_fields() {
    let spec = FrameSpec {
        ty: FrameType::Video,
        flags: 1,
        stream_id: 7,
        seq: 12345,
        pts_us: Some(1_700_000_000_000),
        send_ts_us: Some(1_700_000_000_500),
        payload: &[1, 2, 3, 4],
    };
    let buf = encode_frame_owned(&spec).unwrap();

    let mut dec = FrameDecoder::new();
    assert_eq!(dec.push(&buf), 1);

    let m = dec.frames()[0];
    assert_eq!(m.ty, FrameType::Video);
    assert_eq!(m.flags, 1);
    assert_eq!(m.stream_id, 7);
    assert_eq!(m.seq, 12345);
    assert_eq!(m.pts_us, 1_700_000_000_000);
    assert_eq!(m.send_ts_us, 1_700_000_000_500);
    assert_eq!(dec.payload(m), &[1, 2, 3, 4]);
}

#[test]
fn control_frame_payload_parses_as_json() {
    let json = br#"{"v":0,"name":"side"}"#;
    let buf = encode_frame_owned(&FrameSpec::new(FrameType::Hello, json)).unwrap();

    let mut dec = FrameDecoder::new();
    dec.push(&buf);
    let m = dec.frames()[0];
    let parsed = dec.json(m).expect("控制帧应能解析为 JSON");
    assert_eq!(parsed["v"], 0);
    assert_eq!(parsed["name"], "side");
}

#[test]
fn fragmented_input_reassembles() {
    let a = encode_frame_owned(&FrameSpec::new(FrameType::Video, &[7u8; 100])).unwrap();
    let b = encode_frame_owned(&FrameSpec::new(FrameType::Audio, &[9u8; 50])).unwrap();
    let all: Vec<u8> = a.iter().chain(b.iter()).copied().collect();

    let mut dec = FrameDecoder::new();
    let mut seen = Vec::new();
    for chunk in all.chunks(7) {
        dec.push(chunk);
        for m in dec.frames() {
            seen.push((m.ty, dec.payload(*m).to_vec()));
        }
    }

    assert_eq!(
        seen.len(),
        2,
        "分成 {} 块喂入应仍解出 2 帧",
        all.len().div_ceil(7)
    );
    assert_eq!(seen[0].0, FrameType::Video);
    assert_eq!(seen[0].1.len(), 100);
    assert_eq!(seen[1].0, FrameType::Audio);
    assert_eq!(seen[1].1.len(), 50);
    assert_eq!(dec.pending_bytes(), 0);
}

#[test]
fn resyncs_after_garbage() {
    let mut dec = FrameDecoder::new();
    let mut junk = b"\xe8\xbf\x99\xe6\x98\xaf\xe5\x99\xaa\xe5\xa3\xb0".to_vec();
    junk.push(0x00);
    let good = encode_frame_owned(&FrameSpec::new(FrameType::Stats, br#"{"rtt":3}"#)).unwrap();

    let mut all = junk;
    all.extend_from_slice(&good);

    let n = dec.push(&all);
    assert_eq!(n, 1, "噪声之后应恢复同步");
    assert_eq!(dec.frames()[0].ty, FrameType::Stats);
    assert!(dec.resync_skipped > 0);
}

#[test]
fn unknown_type_is_rejected_by_encoder() {
    // FrameType 是封闭枚举，无法构造非法值；改为验证枚举全覆盖
    assert_eq!(FrameType::ALL.len(), 12, "v1 协议共 12 种帧类型");
    assert_eq!(FrameType::from_u8(99), None);
    assert_eq!(FrameType::from_u8(12), None, "12 及以上应为非法值");
    for (i, t) in FrameType::ALL.into_iter().enumerate() {
        assert_eq!(t as u8, i as u8, "枚举序号必须连续且从 0 开始");
        assert_eq!(FrameType::from_u8(i as u8), Some(t));
    }
}

#[test]
fn media_and_control_classification() {
    // 媒体帧进抖动缓冲；控制帧解析 JSON
    assert!(FrameType::Video.is_media());
    assert!(FrameType::Audio.is_media());
    assert!(FrameType::Retransmit.is_media(), "重传帧也是媒体帧");

    assert!(FrameType::Hello.is_control());
    assert!(FrameType::Nack.is_control());
    assert!(FrameType::RttProbe.is_control());

    assert!(!FrameType::Video.is_control());
    assert!(!FrameType::Hello.is_media());
}
