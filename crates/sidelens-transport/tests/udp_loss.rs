//! UDP 传输 + 丢包与 NACK 恢复测试
//!
//! 这是第一个**真实丢包**环境下的验证。之前的 NACK 与分片代码只有单元测试，
//! 现在让它们在同一台机器上通过真实 UDP 套接字跑起来。
//!
//! # 测试策略
//!
//! 用 `PacketLossSimulator` 的**确定性 xorshift**：同一种子产生同样丢包序列，
//! 所以失败可复现。这很重要 —— 随机丢包测试挂了没法调试。

use std::time::Duration;

use sidelens_core::fragment::Reassembler;
use sidelens_core::{NackGenerator, RetransmitCache, RetransmitConfig, StoreSpec};
use sidelens_proto::{encode_frame_owned, FrameSpec, FrameType};
use sidelens_transport::udp::MAX_DATAGRAM;
use sidelens_transport::{PacketLossSimulator, UdpReceiver, UdpSender};

fn build_frame(seq: u32, payload_len: usize) -> Vec<u8> {
    let payload: Vec<u8> = (0..payload_len)
        .map(|i| ((seq as usize + i) % 251) as u8)
        .collect();
    encode_frame_owned(&FrameSpec {
        ty: FrameType::Video,
        flags: 0,
        stream_id: 0,
        seq,
        pts_us: Some(seq as u64 * 16_667),
        send_ts_us: Some(seq as u64 * 16_667),
        payload: &payload,
    })
    .unwrap()
}

#[tokio::test]
async fn no_loss_all_frames_arrive() {
    let recv = UdpReceiver::bind(0).await.expect("bind 失败");
    let port = recv.port().expect("port");
    let send = UdpSender::connect(
        &format!("127.0.0.1:{port}"),
        PacketLossSimulator::disabled(),
    )
    .await
    .expect("connect 失败");

    let mut asm = Reassembler::new(100.0, 64);
    let mut buf = vec![0u8; 2048];

    // 后台发 20 帧
    tokio::spawn(async move {
        for seq in 1..=20 {
            let frame = build_frame(seq, 8000);
            send.send_frame(&frame).await;
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    });

    // 关键：每次 recv_frame 只读一个数据报，而 8KB 帧需要 7 个数据报才齐。
    // 所以循环条件必须是「收满 20 帧」而不是「循环 20 次」。
    let mut got = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while got.len() < 20 && tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(
            Duration::from_millis(200),
            recv.recv_frame(&mut asm, &mut buf),
        )
        .await
        {
            Ok(Some(inbound)) => got.push(inbound.frame.seq),
            Ok(None) => continue,
            Err(_) => break,
        }
    }

    got.sort_unstable();
    assert_eq!(got.len(), 20, "无丢包时应全部到达，实际 {}", got.len());
    assert_eq!(got, (1..=20).collect::<Vec<_>>());
}

#[tokio::test]
async fn loss_causes_missing_frames() {
    let recv = UdpReceiver::bind(0).await.expect("bind 失败");
    let port = recv.port().expect("port");

    // 30% 丢包，固定种子
    let send = UdpSender::connect(
        &format!("127.0.0.1:{port}"),
        PacketLossSimulator::new(0.3, 0xDEAD_BEEF),
    )
    .await
    .expect("connect 失败");

    let mut asm = Reassembler::new(50.0, 64);
    let mut buf = vec![0u8; 2048];

    let sender_stats = send.stats.clone();
    tokio::spawn(async move {
        for seq in 1..=50 {
            let frame = build_frame(seq, 4000);
            send.send_frame(&frame).await;
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    });

    let mut got = Vec::new();
    for _ in 0..50 {
        match tokio::time::timeout(
            Duration::from_millis(200),
            recv.recv_frame(&mut asm, &mut buf),
        )
        .await
        {
            Ok(Some(inbound)) => got.push(inbound.frame.seq),
            Ok(None) => continue,
            Err(_) => break,
        }
    }

    // 等发送端跑完
    tokio::time::sleep(Duration::from_millis(200)).await;
    let s = sender_stats.snapshot();
    assert!(
        s.simulated_drops > 0,
        "30% 丢包率下应当有丢包，实际 {}",
        s.simulated_drops
    );
    assert!(
        got.len() < 50,
        "有丢包时到达帧数应少于 50，实际 {}（丢包 {}）",
        got.len(),
        s.simulated_drops
    );
}

#[tokio::test]
async fn simulator_is_deterministic() {
    let a = PacketLossSimulator::new(0.5, 12345);
    let b = PacketLossSimulator::new(0.5, 12345);

    let seq_a: Vec<bool> = (0..100).map(|_| a.should_drop()).collect();
    let seq_b: Vec<bool> = (0..100).map(|_| b.should_drop()).collect();

    assert_eq!(seq_a, seq_b, "同种子必须产生同样序列，否则测试不可复现");

    let drops = seq_a.iter().filter(|d| **d).count();
    assert!(
        (30..70).contains(&drops),
        "50% 丢包率在 100 次里应落在合理区间，实际 {drops}"
    );
}

#[tokio::test]
async fn disabled_simulator_never_drops() {
    let sim = PacketLossSimulator::disabled();
    for _ in 0..1000 {
        assert!(!sim.should_drop());
    }
}

#[tokio::test]
async fn full_rate_simulator_drops_everything() {
    let sim = PacketLossSimulator::new(1.0, 7);
    for _ in 0..10 {
        assert!(sim.should_drop(), "100% 丢包率应丢弃每一包");
    }
}

#[tokio::test]
async fn large_frame_fragments_over_udp() {
    let recv = UdpReceiver::bind(0).await.expect("bind 失败");
    let port = recv.port().expect("port");
    let send = UdpSender::connect(
        &format!("127.0.0.1:{port}"),
        PacketLossSimulator::disabled(),
    )
    .await
    .expect("connect 失败");

    // 20KB 帧，必须分多片
    let frame = build_frame(1, 20_000);
    assert!(
        frame.len() > MAX_DATAGRAM,
        "测试前提：帧应大于 MTU，实际 {}",
        frame.len()
    );

    send.send_frame(&frame).await;

    let mut asm = Reassembler::new(500.0, 64);
    let mut buf = vec![0u8; 2048];

    let mut result = None;
    for _ in 0..30 {
        match tokio::time::timeout(
            Duration::from_millis(300),
            recv.recv_frame(&mut asm, &mut buf),
        )
        .await
        {
            Ok(Some(inbound)) => {
                result = Some(inbound.frame);
                break;
            }
            Ok(None) => continue,
            Err(_) => break,
        }
    }

    let f = result.expect("20KB 帧应能通过分片完整传过来");
    assert_eq!(f.payload.len(), 20_000);
    let expected: Vec<u8> = (0..20_000).map(|i| ((1 + i) % 251) as u8).collect();
    assert_eq!(f.payload, expected, "重组后必须逐字节一致");
}

#[tokio::test]
async fn nack_recovers_lost_frame() {
    // 这是一条完整链路：发送 → 丢包 → 检测 → NACK → 重传 → 恢复
    let recv = UdpReceiver::bind(0).await.expect("bind 失败");
    let port = recv.port().expect("port");

    // 30% 丢包
    let send = UdpSender::connect(
        &format!("127.0.0.1:{port}"),
        PacketLossSimulator::new(0.3, 0xA5A5_1234),
    )
    .await
    .expect("connect 失败");

    let mut asm = Reassembler::new(30.0, 64);
    let mut nackgen = NackGenerator::new(0.0, 32); // 0ms 延迟，测试用
    let mut cache = RetransmitCache::new(RetransmitConfig::default());
    let mut buf = vec![0u8; 2048];

    // 发送 30 帧，每帧都存进重传缓存
    let mut seq_to_bytes = std::collections::HashMap::new();
    for seq in 1..=30u32 {
        let frame = build_frame(seq, 3000);
        seq_to_bytes.insert(seq, frame.clone());
        cache.store(
            StoreSpec::video(seq, seq as u64 * 16_667, frame.clone()),
            sidelens_proto::now_us(),
        );
        send.send_frame(&frame).await;
        tokio::time::sleep(Duration::from_millis(1)).await;
    }

    tokio::time::sleep(Duration::from_millis(100)).await;

    // 收所有能到的
    let mut received = Vec::new();
    loop {
        match tokio::time::timeout(
            Duration::from_millis(100),
            recv.recv_frame(&mut asm, &mut buf),
        )
        .await
        {
            Ok(Some(inbound)) => received.push(inbound.frame.seq),
            Ok(None) => continue,
            Err(_) => break,
        }
    }

    received.sort_unstable();
    received.dedup();

    // 缺哪些？
    let missing: Vec<u32> = (1..=30).filter(|s| !received.contains(s)).collect();
    assert!(!missing.is_empty(), "30% 丢包下应当有缺帧，实际无缺失");

    // 生成 NACK 并模拟"接收端请求 → 发送端重传"
    for seq in &missing {
        nackgen.mark_missing(*seq, sidelens_proto::now_us());
    }
    let req = nackgen
        .poll(sidelens_proto::now_us() + 1, 0)
        .expect("应生成 NACK");

    // 发送端按 NACK 重传
    let mut retransmitted = 0;
    for seq in &req.seqs {
        if let Some(frame) = cache.take_for_retransmit(*seq, sidelens_proto::now_us()) {
            send.send_frame(&frame.bytes).await;
            retransmitted += 1;
        }
    }
    assert!(retransmitted > 0, "应至少重传一帧");

    // 接收端：把重传的帧收进来（清掉半成品再重组）
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut recovered = Vec::new();
    loop {
        match tokio::time::timeout(
            Duration::from_millis(100),
            recv.recv_frame(&mut asm, &mut buf),
        )
        .await
        {
            Ok(Some(inbound)) => recovered.push(inbound.frame.seq),
            Ok(None) => continue,
            Err(_) => break,
        }
    }

    recovered.sort_unstable();
    recovered.dedup();

    assert!(
        !recovered.is_empty(),
        "重传的帧应当能被接收，实际一份都没到"
    );
}

#[tokio::test]
async fn udp_stats_track_traffic() {
    let recv = UdpReceiver::bind(0).await.expect("bind 失败");
    let port = recv.port().expect("port");
    let send = UdpSender::connect(
        &format!("127.0.0.1:{port}"),
        PacketLossSimulator::disabled(),
    )
    .await
    .expect("connect 失败");

    let mut asm = Reassembler::new(100.0, 64);
    let mut buf = vec![0u8; 2048];

    let frame = build_frame(1, 3000);
    send.send_frame(&frame).await;

    tokio::time::sleep(Duration::from_millis(50)).await;

    // 读一两个包
    let _ = tokio::time::timeout(
        Duration::from_millis(200),
        recv.recv_frame(&mut asm, &mut buf),
    )
    .await;

    let s_out = send.stats.snapshot();
    let s_in = recv.stats.snapshot();

    assert!(s_out.datagrams_out > 0, "发送端应记录数据报");
    assert!(s_out.bytes_out > 0, "发送端应记录字节");
    assert!(s_in.bytes_in > 0, "接收端应记录字节");
    assert_eq!(s_out.simulated_drops, 0, "无丢包模拟时不应有丢弃");
}
