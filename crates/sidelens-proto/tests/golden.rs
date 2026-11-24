//! 黄金向量：JS 实现与 Rust 实现必须产生**逐字节相同**的输出
//!
//! 这些 hex 由 JS 版 `packages/proto/src/index.js` 的 `encodeFrame()` 生成
//! （见 `tools/gen-golden.mjs`）。任何一方改了协议布局，这里会立刻失败。
//!
//! 这是 [ADR 0002] 里「Rust 重写必须让 JS 版的行为成为规范」的具体落地：
//! 不是两份文档互相对照，而是字节级对齐。

use sidelens_proto::{encode_frame_owned, FrameSpec, FrameType};

/// (名称, 期望的十六进制, 输入)
fn cases() -> Vec<(&'static str, &'static str, FrameSpec<'static>)> {
    static AUDIO_PAYLOAD: [u8; 50] = [9u8; 50];

    vec![
        (
            "video_empty",
            "534c4e3002000000000000000000000000000000000000000000000000000000",
            FrameSpec {
                ty: FrameType::Video,
                flags: 0,
                stream_id: 0,
                seq: 0,
                pts_us: Some(0),
                send_ts_us: Some(0),
                payload: &[],
            },
        ),
        (
            "video_abc",
            "534c4e300200000001000000e803000000000000d00700000000000003000000616263",
            FrameSpec {
                ty: FrameType::Video,
                flags: 0,
                stream_id: 0,
                seq: 1,
                pts_us: Some(1000),
                send_ts_us: Some(2000),
                payload: b"abc",
            },
        ),
        (
            "video_key",
            "534c4e3002010700393000000068e5cf8b010000f469e5cf8b0100000400000001020304",
            FrameSpec {
                ty: FrameType::Video,
                flags: 1,
                stream_id: 7,
                seq: 12345,
                pts_us: Some(1_700_000_000_000),
                send_ts_us: Some(1_700_000_000_500),
                payload: &[1, 2, 3, 4],
            },
        ),
        (
            "audio_50",
            "534c4e30030001002a000000404b4c0000000000a44b4c0000000000320000000909090909090909090909090909090909090909090909090909090909090909090909090909090909090909090909090909",
            FrameSpec {
                ty: FrameType::Audio,
                flags: 0,
                stream_id: 1,
                seq: 42,
                pts_us: Some(5_000_000),
                send_ts_us: Some(5_000_100),
                payload: &AUDIO_PAYLOAD,
            },
        ),
        (
            "hello_json",
            "534c4e30000000000000000000000000000000000000000000000000150000007b2276223a302c226e616d65223a2273696465227d",
            FrameSpec {
                ty: FrameType::Hello,
                flags: 0,
                stream_id: 0,
                seq: 0,
                pts_us: Some(0),
                send_ts_us: Some(0),
                payload: br#"{"v":0,"name":"side"}"#,
            },
        ),
        (
            "stats_json",
            "534c4e300500000009000000581b000000000000581b000000000000090000007b22727474223a337d",
            FrameSpec {
                ty: FrameType::Stats,
                flags: 0,
                stream_id: 0,
                seq: 9,
                pts_us: Some(7000),
                send_ts_us: Some(7000),
                payload: br#"{"rtt":3}"#,
            },
        ),
        (
            "max_seq",
            "534c4e300400ffffffffffff010000000000000002000000000000000100000078",
            FrameSpec {
                ty: FrameType::KeyframeReq,
                flags: 0,
                stream_id: 65535,
                seq: 4_294_967_295,
                pts_us: Some(1),
                send_ts_us: Some(2),
                payload: b"x",
            },
        ),
    ]
}

#[test]
fn encoded_bytes_match_js_implementation() {
    for (name, expected_hex, spec) in cases() {
        let got = encode_frame_owned(&spec).unwrap_or_else(|e| panic!("{name} 编码失败: {e}"));
        let got_hex: String = got.iter().map(|b| format!("{b:02x}")).collect();

        assert_eq!(
            got_hex, expected_hex,
            "\n用例 {name} 与 JS 实现不一致\n  JS  : {expected_hex}\n  Rust: {got_hex}"
        );
        assert_eq!(got.len(), expected_hex.len() / 2, "{name} 长度不符");
    }
}

#[test]
fn max_values_are_accepted() {
    // stream_id 与 seq 的上界必须能正常编码，不能溢出
    let spec = FrameSpec {
        ty: FrameType::Video,
        stream_id: u16::MAX,
        seq: u32::MAX,
        ..FrameSpec::new(FrameType::Video, &[])
    };
    assert!(encode_frame_owned(&spec).is_ok());
}

#[test]
fn payload_length_field_is_little_endian() {
    // 载荷 256 字节 → 头里 offset 28 的值应为 0x00000100，小端写作 00 01 00 00
    let payload = vec![0u8; 256];
    let buf = encode_frame_owned(&FrameSpec::new(FrameType::Video, &payload)).unwrap();
    assert_eq!(&buf[28..32], &[0x00, 0x01, 0x00, 0x00]);
}
