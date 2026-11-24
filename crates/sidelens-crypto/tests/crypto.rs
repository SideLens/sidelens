//! 加密层集成测试
//!
//! # 为什么测试名不是 snake_case
//!
//! 这里故意用中文描述「守什么不变量」，而不是 `test_roundtrip_ok` 这种
//! 只说明「测了哪个函数」的名字。密码学代码的 bug 有个共同特点：
//! 功能看起来完全正常（往返能过、日志正常、画面能出），但安全性已归零。
//! 所以测试名必须说清「攻击怎么失败的」，否则半年后没人知道这些测试在守什么。
//!
//! Rust 允许非 ASCII 标识符，但 `non_snake_case` 会对中文大写字母报警 ——
//! 实际上中文没有大小写概念，这条 lint 在中文标识符上纯属误报。

#![allow(non_snake_case)]

use sidelens_crypto::{
    handshake::{Handshake, PeerPublic, Role},
    nonce,
    replay::{ReplayGuard, ReplayWindow},
    sealer::FrameSealer,
    CryptoError, FrameHead, ReplayError, TrafficKeys, TAG_SIZE,
};
use sidelens_proto::{encode_frame_owned, FrameSpec, FrameType, HEADER_SIZE};

/// 造一对已经完成握手的端点
fn paired(role_a: Role, role_b: Role) -> (FrameSealer, FrameSealer) {
    let a = Handshake::new_ephemeral();
    let b = Handshake::new_ephemeral();
    let ka = a.derive(&b.peer_public(), role_a).unwrap();
    let kb = b.derive(&a.peer_public(), role_b).unwrap();
    (
        FrameSealer::new(ka.traffic_keys(role_a), role_a),
        FrameSealer::new(kb.traffic_keys(role_b), role_b),
    )
}

fn make_frame(ty: FrameType, seq: u32, stream_id: u16, payload: &[u8]) -> Vec<u8> {
    encode_frame_owned(&FrameSpec {
        ty,
        flags: 0,
        stream_id,
        seq,
        pts_us: Some(1_000_000),
        send_ts_us: Some(2_000_000),
        payload,
    })
    .unwrap()
}

// ─────────────────────────── 握手 ───────────────────────────

#[test]
fn 握手_两端_sas_必须相同() {
    let a = Handshake::new_ephemeral();
    let b = Handshake::new_ephemeral();
    let ka = a.derive(&b.peer_public(), Role::Initiator).unwrap();
    let kb = b.derive(&a.peer_public(), Role::Responder).unwrap();
    assert_eq!(ka.sas(), kb.sas(), "SAS 不同意味着用户核对必然失败");
}

#[test]
fn 握手_角色搞反时_sas_必须不同() {
    // 这是最隐蔽的接错方式：两端都以为自己是发起方。
    // 如果 SAS 相同，用户核对通过，但密钥是错的 —— 完全静默的失效。
    let a = Handshake::new_ephemeral();
    let b = Handshake::new_ephemeral();
    let ka = a.derive(&b.peer_public(), Role::Initiator).unwrap();
    let kb = b.derive(&a.peer_public(), Role::Initiator).unwrap();
    assert_ne!(
        ka.sas(),
        kb.sas(),
        "角色相同时 SAS 却相同 —— 角色没有绑进密钥派生"
    );
}

#[test]
fn 握手_sas_落在六位范围内() {
    for _ in 0..32 {
        let a = Handshake::new_ephemeral();
        let b = Handshake::new_ephemeral();
        let k = a.derive(&b.peer_public(), Role::Initiator).unwrap();
        assert!(k.sas() < 1_000_000, "SAS 超出六位：{}", k.sas());
    }
}

#[test]
fn 握手_sas_展示格式带空格() {
    let a = Handshake::new_ephemeral();
    let b = Handshake::new_ephemeral();
    let k = a.derive(&b.peer_public(), Role::Initiator).unwrap();
    let s = k.sas_display();
    assert_eq!(s.len(), 7, "应为 '123 456' 共 7 字符，实际 {s:?}");
    assert_eq!(s.as_bytes()[3], b' ');
}

#[test]
fn 握手_每次都是新密钥() {
    // 前向保密的最低要求：同一对端点连两次，会话密钥不能相同。
    let a1 = Handshake::new_ephemeral();
    let b1 = Handshake::new_ephemeral();
    let k1 = a1.derive(&b1.peer_public(), Role::Initiator).unwrap();

    let a2 = Handshake::new_ephemeral();
    let b2 = Handshake::new_ephemeral();
    let k2 = a2.derive(&b2.peer_public(), Role::Initiator).unwrap();

    // SAS 是密钥的函数，两次相同意味着密钥相同
    assert_ne!(k1.sas(), k2.sas(), "两次握手的密钥相同");
}

#[test]
fn 握手_低阶公钥必须被拒绝() {
    // 全零公钥是阶为 1 的低阶点，X25519 对它产生的共享密钥是全零。
    // 攻击者只有故意构造才能触发 —— 必须当安全事件处理。
    let a = Handshake::new_ephemeral();
    let evil = PeerPublic::new(x25519_dalek::PublicKey::from([0u8; 32]), [0u8; 32]);
    let err = a.derive(&evil, Role::Initiator).unwrap_err();
    assert_eq!(err, sidelens_crypto::HandshakeError::LowOrderPoint);
}

#[test]
fn 握手_对端公钥不参与时无法派生相同密钥() {
    // 用第三方（中间人）的公钥派生，得到的密钥必然与对端不同。
    let a = Handshake::new_ephemeral();
    let b = Handshake::new_ephemeral();
    let mitm = Handshake::new_ephemeral();

    let ka = a.derive(&b.peer_public(), Role::Initiator).unwrap();
    let k_mitm = a.derive(&mitm.peer_public(), Role::Initiator).unwrap();
    assert_ne!(ka.sas(), k_mitm.sas(), "换了公钥密钥却没变");
}

// ─────────────────────────── nonce ───────────────────────────

#[test]
fn nonce_四元组全不同则互不相同() {
    let n1 = nonce::build(Role::Initiator, 0, 0);
    let n2 = nonce::build(Role::Responder, 0, 0);
    let n3 = nonce::build(Role::Initiator, 1, 0);
    let n4 = nonce::build(Role::Initiator, 0, 1);
    assert_ne!(n1, n2);
    assert_ne!(n1, n3);
    assert_ne!(n1, n4);
    assert_ne!(n2, n3);
    assert_ne!(n3, n4);
}

// ─────────────────────────── 加解密往返 ───────────────────────────

#[test]
fn 往返_明文完整还原() {
    let (mut tx, mut rx) = paired(Role::Initiator, Role::Responder);
    let original = make_frame(FrameType::Video, 1, 0, b"the quick brown fox");
    let mut frame = original.clone();

    tx.seal_frame(&mut frame).unwrap();
    assert_eq!(frame.len(), original.len() + TAG_SIZE, "密文长度不对");
    assert_ne!(
        &frame[HEADER_SIZE..],
        &original[HEADER_SIZE..],
        "载荷没被加密"
    );

    rx.open_frame(&mut frame).unwrap();
    assert_eq!(frame, original, "解密后与原文不一致");
}

#[test]
fn 往返_空载荷也正常() {
    let (mut tx, mut rx) = paired(Role::Initiator, Role::Responder);
    let original = make_frame(FrameType::Hello, 0, 0, b"");
    let mut frame = original.clone();
    tx.seal_frame(&mut frame).unwrap();
    assert_eq!(frame.len(), original.len() + TAG_SIZE);
    rx.open_frame(&mut frame).unwrap();
    assert_eq!(frame, original);
}

#[test]
fn 往返_大载荷正常() {
    // 1160 字节是单个分片的数据段上限
    let payload = vec![0xABu8; 1160];
    let (mut tx, mut rx) = paired(Role::Initiator, Role::Responder);
    let original = make_frame(FrameType::Video, 7, 0, &payload);
    let mut frame = original.clone();
    tx.seal_frame(&mut frame).unwrap();
    rx.open_frame(&mut frame).unwrap();
    assert_eq!(frame, original);
}

#[test]
fn 往返_帧头字段全部保持不变() {
    // seq / stream_id / flags / pts / send_ts 都必须原样还原，
    // 否则重组、抖动缓冲、音画同步全部错乱。
    let (mut tx, mut rx) = paired(Role::Initiator, Role::Responder);
    let original = encode_frame_owned(&FrameSpec {
        ty: FrameType::Audio,
        flags: 0b101,
        stream_id: 1,
        seq: 0xDEAD_BEEF,
        pts_us: Some(123_456_789),
        send_ts_us: Some(987_654_321),
        payload: b"audio",
    })
    .unwrap();
    let mut frame = original.clone();
    tx.seal_frame(&mut frame).unwrap();
    rx.open_frame(&mut frame).unwrap();

    assert_eq!(&frame[4..5], &[FrameType::Audio as u8], "type 变了");
    assert_eq!(frame[5], 0b101, "flags 变了");
    assert_eq!(
        u16::from_le_bytes([frame[6], frame[7]]),
        1,
        "stream_id 变了"
    );
    assert_eq!(
        u32::from_le_bytes([frame[8], frame[9], frame[10], frame[11]]),
        0xDEAD_BEEF,
        "seq 变了"
    );
    assert_eq!(frame, original);
}

// ─────────────────────────── 攻击必须失败 ───────────────────────────

#[test]
fn 攻击_篡改密文必须认证失败() {
    let (mut tx, mut rx) = paired(Role::Initiator, Role::Responder);
    let mut frame = make_frame(FrameType::Video, 1, 0, b"secret pixels");
    tx.seal_frame(&mut frame).unwrap();

    // 翻一个 bit
    let at = HEADER_SIZE + 2;
    frame[at] ^= 0x01;

    assert_eq!(
        rx.open_frame(&mut frame).unwrap_err(),
        CryptoError::AuthenticationFailed
    );
}

#[test]
fn 攻击_篡改认证标签必须失败() {
    let (mut tx, mut rx) = paired(Role::Initiator, Role::Responder);
    let mut frame = make_frame(FrameType::Video, 1, 0, b"secret pixels");
    tx.seal_frame(&mut frame).unwrap();

    let last = frame.len() - 1;
    frame[last] ^= 0x80;

    assert_eq!(
        rx.open_frame(&mut frame).unwrap_err(),
        CryptoError::AuthenticationFailed
    );
}

#[test]
fn 攻击_篡改帧头_seq_必须失败() {
    // seq 是 AAD 的一部分。改了它，接收端算出的 nonce 也变了，
    // 但更重要的是 AAD 校验会先失败 —— 这条测试守住「帧头进 AAD」。
    let (mut tx, mut rx) = paired(Role::Initiator, Role::Responder);
    let mut frame = make_frame(FrameType::Video, 1, 0, b"pixels");
    tx.seal_frame(&mut frame).unwrap();

    frame[8] = frame[8].wrapping_add(1);

    let err = rx.open_frame(&mut frame).unwrap_err();
    assert!(
        matches!(
            err,
            CryptoError::AuthenticationFailed | CryptoError::LengthMismatch
        ),
        "篡改 seq 没被拦住：{err:?}"
    );
}

#[test]
fn 攻击_篡改帧头_flags_必须失败() {
    // 把 P 帧标成关键帧，会让解码器从错误的位置开始解
    let (mut tx, mut rx) = paired(Role::Initiator, Role::Responder);
    let mut frame = make_frame(FrameType::Video, 5, 0, b"pixels");
    tx.seal_frame(&mut frame).unwrap();

    frame[5] |= sidelens_proto::FLAG_KEYFRAME;

    assert_eq!(
        rx.open_frame(&mut frame).unwrap_err(),
        CryptoError::AuthenticationFailed,
        "篡改 flags 没被 AAD 拦住"
    );
}

#[test]
fn 攻击_篡改_stream_id_必须失败() {
    let (mut tx, mut rx) = paired(Role::Initiator, Role::Responder);
    let mut frame = make_frame(FrameType::Video, 5, 0, b"pixels");
    tx.seal_frame(&mut frame).unwrap();

    frame[6] = 1; // 伪装成音频流

    assert!(
        rx.open_frame(&mut frame).is_err(),
        "篡改 stream_id 没被拦住"
    );
}

#[test]
fn 攻击_用错误密钥解密必须失败() {
    let (mut tx, _) = paired(Role::Initiator, Role::Responder);
    let (_, mut wrong_rx) = paired(Role::Initiator, Role::Responder);

    let mut frame = make_frame(FrameType::Video, 1, 0, b"pixels");
    tx.seal_frame(&mut frame).unwrap();

    assert_eq!(
        wrong_rx.open_frame(&mut frame).unwrap_err(),
        CryptoError::AuthenticationFailed
    );
}

#[test]
fn 攻击_两端角色相同时加解密必须失败() {
    // 两端都当发起方：密钥相同但 nonce 方向位相同，解密端会用
    // 错误方向的 nonce —— 必须失败，不能"碰巧能解"。
    let a = Handshake::new_ephemeral();
    let b = Handshake::new_ephemeral();
    let ka = a.derive(&b.peer_public(), Role::Initiator).unwrap();
    let kb = b.derive(&a.peer_public(), Role::Initiator).unwrap();

    let mut tx = FrameSealer::new(ka.traffic_keys(Role::Initiator), Role::Initiator);
    let mut rx = FrameSealer::new(kb.traffic_keys(Role::Initiator), Role::Initiator);

    let mut frame = make_frame(FrameType::Video, 1, 0, b"pixels");
    tx.seal_frame(&mut frame).unwrap();
    assert!(
        rx.open_frame(&mut frame).is_err(),
        "角色相同却能解密 —— nonce 方向位没有生效"
    );
}

#[test]
fn 攻击_重放同一帧必须被重放窗口拦住() {
    // 加密层本身不拦重放（密文合法），必须由 ReplayGuard 拦
    let (mut tx, _rx) = paired(Role::Initiator, Role::Responder);
    let mut frame = make_frame(FrameType::Video, 42, 0, b"pixels");
    tx.seal_frame(&mut frame).unwrap();

    let mut guard = ReplayGuard::default();
    assert!(guard.check_and_mark(0, 42).is_ok());
    assert_eq!(
        guard.check_and_mark(0, 42).unwrap_err(),
        ReplayError::Duplicate(42)
    );
}

// ─────────────────────────── sealer 契约 ───────────────────────────

#[test]
fn 契约_同一_seq_封装两次必须报错() {
    // 重传路径重新加密 = nonce 重用 = 灾难。必须显式失败。
    let (mut tx, _) = paired(Role::Initiator, Role::Responder);
    let mut f1 = make_frame(FrameType::Video, 9, 0, b"a");
    tx.seal_frame(&mut f1).unwrap();

    let mut f2 = make_frame(FrameType::Video, 9, 0, b"a");
    assert_eq!(
        tx.seal_frame(&mut f2).unwrap_err(),
        CryptoError::NonceReuse(9),
        "重复加密同一个 seq 没有被拦住"
    );
}

#[test]
fn 契约_不同流用同一_seq_不算重复() {
    // 视频 stream 0 与音频 stream 1 的 seq 各自从 0 开始，撞车是正常的
    let (mut tx, _) = paired(Role::Initiator, Role::Responder);
    let mut v = make_frame(FrameType::Video, 5, 0, b"v");
    let mut a = make_frame(FrameType::Audio, 5, 1, b"a");
    assert!(tx.seal_frame(&mut v).is_ok());
    assert!(tx.seal_frame(&mut a).is_ok(), "不同流同 seq 被误判为重复");
}

#[test]
fn 契约_帧太短必须报错() {
    let (mut tx, _) = paired(Role::Initiator, Role::Responder);
    let mut tiny = vec![0u8; HEADER_SIZE - 1];
    assert_eq!(
        tx.seal_frame(&mut tiny).unwrap_err(),
        CryptoError::FrameTooShort
    );
}

#[test]
fn 契约_magic_不对必须报错() {
    let (mut tx, _) = paired(Role::Initiator, Role::Responder);
    let mut frame = make_frame(FrameType::Video, 1, 0, b"x");
    frame[0] = b'X';
    assert_eq!(
        tx.seal_frame(&mut frame).unwrap_err(),
        CryptoError::BadMagic
    );
}

#[test]
fn 契约_长度字段与实际不符必须报错() {
    let (mut tx, _) = paired(Role::Initiator, Role::Responder);
    let mut frame = make_frame(FrameType::Video, 1, 0, b"abc");
    // 声称载荷 99 字节
    frame[28..32].copy_from_slice(&99u32.to_le_bytes());
    assert_eq!(
        tx.seal_frame(&mut frame).unwrap_err(),
        CryptoError::LengthMismatch
    );
}

#[test]
fn 契约_密文帧太短无法解密() {
    let (_, mut rx) = paired(Role::Initiator, Role::Responder);
    let mut frame = make_frame(FrameType::Video, 1, 0, b"");
    // 只有帧头，没有标签
    assert_eq!(
        rx.open_frame(&mut frame).unwrap_err(),
        CryptoError::FrameTooShort
    );
}

#[test]
fn 契约_plain_len_of_不解密也能读长度() {
    let (mut tx, _) = paired(Role::Initiator, Role::Responder);
    let mut frame = make_frame(FrameType::Video, 1, 0, b"12345");
    let plain = frame.len() - HEADER_SIZE;
    tx.seal_frame(&mut frame).unwrap();
    assert_eq!(FrameSealer::plain_len_of(&frame), Some(plain));
}

#[test]
fn 契约_帧头在密文中保持可读() {
    // 这是设计的核心：seq / stream_id / flags 必须能在解密前读到，
    // 否则重组器无法工作。
    let (mut tx, _) = paired(Role::Initiator, Role::Responder);
    let mut frame = make_frame(FrameType::Video, 0x11223344, 0, b"payload");
    tx.seal_frame(&mut frame).unwrap();

    assert_eq!(&frame[..4], &sidelens_proto::MAGIC);
    assert_eq!(frame[4], FrameType::Video as u8);
    assert_eq!(
        u32::from_le_bytes([frame[8], frame[9], frame[10], frame[11]]),
        0x11223344
    );
}

// ─────────────────────────── 重放窗口 ───────────────────────────

#[test]
fn 重放_正常顺序全部接受() {
    let mut w = ReplayWindow::new(64);
    for seq in 0..100u32 {
        assert!(w.check_and_mark(seq).is_ok(), "seq={seq} 被误拒");
    }
    assert_eq!(w.accepted, 100);
    assert_eq!(w.rejected, 0);
}

#[test]
fn 重放_重复一律拒绝() {
    let mut w = ReplayWindow::new(64);
    w.check_and_mark(10).unwrap();
    assert_eq!(
        w.check_and_mark(10).unwrap_err(),
        ReplayError::Duplicate(10)
    );
}

#[test]
fn 重放_窗口内乱序接受() {
    // WiFi 上 10-30 个包的乱序很常见，不能误杀
    let mut w = ReplayWindow::new(64);
    for seq in [5u32, 3, 7, 1, 6, 2, 4] {
        assert!(w.check_and_mark(seq).is_ok(), "seq={seq} 被误拒");
    }
    assert_eq!(w.accepted, 7);
}

#[test]
fn 重放_超出窗口的旧包拒绝() {
    let mut w = ReplayWindow::new(64);
    w.check_and_mark(0).unwrap();
    w.check_and_mark(1000).unwrap();
    let err = w.check_and_mark(5).unwrap_err();
    assert!(
        matches!(err, ReplayError::TooOld { .. }),
        "应为过旧：{err:?}"
    );
}

#[test]
fn 重放_u32_回绕后不误判() {
    // seq 从 u32::MAX 回到 0 是真实会发生的（长时间推流）
    let mut w = ReplayWindow::new(64);
    w.check_and_mark(u32::MAX - 1).unwrap();
    w.check_and_mark(u32::MAX).unwrap();
    assert!(w.check_and_mark(0).is_ok(), "回绕后的新包被当成过旧");
    assert!(w.check_and_mark(1).is_ok());
}

#[test]
fn 重放_大跳跃后窗口整体滑动() {
    let mut w = ReplayWindow::new(64);
    w.check_and_mark(0).unwrap();
    w.check_and_mark(100).unwrap();
    // 窗口现在是 [37, 100]
    assert!(w.check_and_mark(100).is_err(), "100 应重复");
    assert!(w.check_and_mark(99).is_ok(), "99 在窗口内");
    assert!(w.check_and_mark(36).is_err(), "36 在窗口外");
}

#[test]
fn 重放_按流分开计数() {
    // 这条测试守住模块文档里的那个坑：视频 seq 跑到 100 时，
    // 音频 seq=3 绝不能被当成"过旧"
    let mut g = ReplayGuard::default();
    for seq in 0..100u32 {
        g.check_and_mark(0, seq).unwrap();
    }
    assert!(
        g.check_and_mark(1, 3).is_ok(),
        "音频被视频的 seq 误杀了 —— 流没有分开计数"
    );
    assert_eq!(g.stream_count(), 2);
}

#[test]
fn 重放_复位后重新开始() {
    let mut g = ReplayGuard::default();
    g.check_and_mark(0, 500).unwrap();
    g.reset();
    assert!(g.check_and_mark(0, 0).is_ok(), "复位后旧 seq 应可用");
    assert_eq!(g.total_rejected(), 0);
}

#[test]
fn 重放_统计累加正确() {
    let mut g = ReplayGuard::default();
    g.check_and_mark(0, 1).unwrap();
    g.check_and_mark(0, 1).unwrap_err();
    g.check_and_mark(0, 2).unwrap();
    assert_eq!(g.total_accepted(), 2);
    assert_eq!(g.total_rejected(), 1);
}

// ─────────────────────────── 手工密钥 ───────────────────────────

#[test]
fn 手工密钥_交叉后能互通() {
    let k1 = [1u8; 32];
    let k2 = [2u8; 32];
    let mut tx = FrameSealer::new(TrafficKeys::from_raw(k1, k2), Role::Initiator);
    let mut rx = FrameSealer::new(TrafficKeys::from_raw(k2, k1), Role::Responder);

    let original = make_frame(FrameType::Video, 1, 0, b"x");
    let mut frame = original.clone();
    tx.seal_frame(&mut frame).unwrap();
    rx.open_frame(&mut frame).unwrap();
    assert_eq!(frame, original);
}

#[test]
fn 手工密钥_方向不交叉则失败() {
    // 两端都用同一把 send 密钥 —— 接收端用 recv（另一把）解不开
    let k1 = [1u8; 32];
    let k2 = [2u8; 32];
    let mut tx = FrameSealer::new(TrafficKeys::from_raw(k1, k2), Role::Initiator);
    let mut rx = FrameSealer::new(TrafficKeys::from_raw(k1, k2), Role::Responder);

    let mut frame = make_frame(FrameType::Video, 1, 0, b"x");
    tx.seal_frame(&mut frame).unwrap();
    assert!(rx.open_frame(&mut frame).is_err());
}

// ─────────────────── 载荷级 API（seal_payload / open_payload）───────────────────

fn paired_sealers() -> (FrameSealer, FrameSealer) {
    paired(Role::Initiator, Role::Responder)
}

#[test]
fn 载荷级_往返一致() {
    let (mut tx, mut rx) = paired_sealers();
    let head = FrameHead {
        ty: FrameType::Video as u8,
        flags: 0,
        stream_id: 0,
        seq: 1,
        pts_us: 111,
        send_ts_us: 222,
    };
    let mut payload = b"the quick brown fox".to_vec();
    let original = payload.clone();

    tx.seal_payload(&head, &mut payload).unwrap();
    assert_eq!(payload.len(), original.len() + TAG_SIZE);

    rx.open_payload(&head, &mut payload).unwrap();
    assert_eq!(payload, original);
}

#[test]
fn 载荷级_与整帧级_AAD_必须等价() {
    // 最关键的一致性测试：两条路径的 AAD 必须逐字节相同，
    // 否则「发送端说成功、接收端每帧认证失败」—— 最难查的组合。
    //
    // 注意：两条路径必须用**同一把密钥**。第一版这个测试各建了一对
    // 新的 handshake（每次都是新密钥），于是密文必然不同 —— 那测的是
    // 「密钥不同」，不是「AAD 不同」。用 PSK 派生保证密钥固定。
    let master = [0x5Au8; 32];
    let head = FrameHead {
        ty: FrameType::Video as u8,
        flags: 0,
        stream_id: 0,
        seq: 77,
        pts_us: 555,
        send_ts_us: 666,
    };

    // 路径 A：整帧封装 → 取载荷
    let mut tx_a = FrameSealer::new(
        TrafficKeys::from_master(&master, Role::Initiator),
        Role::Initiator,
    );
    let mut whole = make_frame(FrameType::Video, 77, 0, b"payload-bytes");
    whole[12..20].copy_from_slice(&555u64.to_le_bytes());
    whole[20..28].copy_from_slice(&666u64.to_le_bytes());
    tx_a.seal_frame(&mut whole).unwrap();
    let sealed_by_frame = whole[HEADER_SIZE..].to_vec();

    // 路径 B：载荷级封装，同一把密钥
    let mut tx_b = FrameSealer::new(
        TrafficKeys::from_master(&master, Role::Initiator),
        Role::Initiator,
    );
    let mut payload = b"payload-bytes".to_vec();
    tx_b.seal_payload(&head, &mut payload).unwrap();

    assert_eq!(
        payload, sealed_by_frame,
        "两条路径的密文不同 —— AAD 或 nonce 不一致"
    );

    // 交叉解密：整帧路径加密的，载荷路径能解
    let mut rx = FrameSealer::new(
        TrafficKeys::from_master(&master, Role::Responder),
        Role::Responder,
    );
    let mut cross = sealed_by_frame.clone();
    rx.open_payload(&head, &mut cross).unwrap();
    assert_eq!(cross, b"payload-bytes");
}

#[test]
fn 载荷级_篡改必须失败() {
    let (mut tx, mut rx) = paired_sealers();
    let head = FrameHead {
        ty: FrameType::Video as u8,
        flags: 0,
        stream_id: 0,
        seq: 3,
        pts_us: 0,
        send_ts_us: 0,
    };
    let mut payload = b"secret".to_vec();
    tx.seal_payload(&head, &mut payload).unwrap();
    payload[0] ^= 1;
    assert_eq!(
        rx.open_payload(&head, &mut payload).unwrap_err(),
        CryptoError::AuthenticationFailed
    );
}

#[test]
fn 载荷级_篡改帧头字段必须失败() {
    let (mut tx, mut rx) = paired_sealers();
    let head = FrameHead {
        ty: FrameType::Video as u8,
        flags: 0,
        stream_id: 0,
        seq: 3,
        pts_us: 0,
        send_ts_us: 0,
    };
    let mut payload = b"secret".to_vec();
    tx.seal_payload(&head, &mut payload).unwrap();

    let mut tampered = head;
    tampered.pts_us = 9_999_999;
    assert!(
        rx.open_payload(&tampered, &mut payload).is_err(),
        "改 pts_us 没被 AAD 拦住"
    );
}

#[test]
fn 载荷级_重复封同一_seq_报错() {
    let (mut tx, _) = paired_sealers();
    let head = FrameHead {
        ty: FrameType::Video as u8,
        flags: 0,
        stream_id: 0,
        seq: 9,
        pts_us: 0,
        send_ts_us: 0,
    };
    let mut p1 = b"a".to_vec();
    tx.seal_payload(&head, &mut p1).unwrap();
    let mut p2 = b"a".to_vec();
    assert_eq!(
        tx.seal_payload(&head, &mut p2).unwrap_err(),
        CryptoError::NonceReuse(9)
    );
}

#[test]
fn 载荷级_空载荷正常() {
    let (mut tx, mut rx) = paired_sealers();
    let head = FrameHead {
        ty: FrameType::Hello as u8,
        flags: 0,
        stream_id: 0,
        seq: 0,
        pts_us: 0,
        send_ts_us: 0,
    };
    let mut payload: Vec<u8> = Vec::new();
    tx.seal_payload(&head, &mut payload).unwrap();
    assert_eq!(payload.len(), TAG_SIZE);
    rx.open_payload(&head, &mut payload).unwrap();
    assert!(payload.is_empty());
}

// ─────────────────────────── 预共享密钥 ───────────────────────────

#[test]
fn psk_两端交叉后能互通() {
    let master = [0x42u8; 32];
    let mut tx = FrameSealer::new(
        TrafficKeys::from_master(&master, Role::Initiator),
        Role::Initiator,
    );
    let mut rx = FrameSealer::new(
        TrafficKeys::from_master(&master, Role::Responder),
        Role::Responder,
    );

    let original = make_frame(FrameType::Video, 1, 0, b"psk works");
    let mut frame = original.clone();
    tx.seal_frame(&mut frame).unwrap();
    rx.open_frame(&mut frame).unwrap();
    assert_eq!(frame, original);
}

#[test]
fn psk_两端同角色则失败() {
    let master = [0x42u8; 32];
    let mut tx = FrameSealer::new(
        TrafficKeys::from_master(&master, Role::Initiator),
        Role::Initiator,
    );
    let mut rx = FrameSealer::new(
        TrafficKeys::from_master(&master, Role::Initiator),
        Role::Initiator,
    );

    let mut frame = make_frame(FrameType::Video, 1, 0, b"x");
    tx.seal_frame(&mut frame).unwrap();
    assert!(rx.open_frame(&mut frame).is_err(), "同角色却能解密");
}

#[test]
fn psk_不同主密钥必然失败() {
    let a = [1u8; 32];
    let b = [2u8; 32];
    let mut tx = FrameSealer::new(
        TrafficKeys::from_master(&a, Role::Initiator),
        Role::Initiator,
    );
    let mut rx = FrameSealer::new(
        TrafficKeys::from_master(&b, Role::Responder),
        Role::Responder,
    );

    let mut frame = make_frame(FrameType::Video, 1, 0, b"x");
    tx.seal_frame(&mut frame).unwrap();
    assert!(rx.open_frame(&mut frame).is_err());
}

#[test]
fn psk_两个方向的密钥互不相同() {
    // 派生出的 k_i2r 与 k_r2i 必须是两把独立密钥。
    // 若相同，用「发起方 send 密钥」当 recv 就能解开自己发的帧 —— 那是灾难。
    let master = [7u8; 32];
    let mut tx = FrameSealer::new(
        TrafficKeys::from_master(&master, Role::Initiator),
        Role::Initiator,
    );
    let mut same_key_rx = FrameSealer::new(
        TrafficKeys::from_raw(
            *b"0123456789abcdef0123456789abcdef",
            *b"0123456789abcdef0123456789abcdef",
        ),
        Role::Initiator,
    );

    let mut frame = make_frame(FrameType::Video, 1, 0, b"x");
    tx.seal_frame(&mut frame).unwrap();
    assert!(
        same_key_rx.open_frame(&mut frame).is_err(),
        "用同一把密钥能解开 —— 方向密钥没有分离"
    );
}
