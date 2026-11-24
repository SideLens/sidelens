//! 端到端往返测试：客户端 → 服务端，验证协议 + 传输 + 分片

use std::time::Duration;

use sidelens_proto::{encode_frame_owned, FrameSpec, FrameType, FLAG_KEYFRAME};
use sidelens_transport::{WsClient, WsServer};

/// 起一个绑在随机端口的服务端，返回 (server, 实际端口)
async fn spawn_server() -> (std::sync::Arc<WsServer>, u16) {
    // 用 0 让 OS 分配端口，避免与其他进程冲突
    let server = WsServer::bind(0).await.expect("绑定失败");
    let port = server.port();
    let s = std::sync::Arc::new(server);
    let srv = s.clone();
    tokio::spawn(async move {
        let _ = srv.serve().await;
    });
    // 给 accept 循环一点时间进入就绪
    tokio::time::sleep(Duration::from_millis(50)).await;
    (s, port)
}

#[tokio::test]
async fn single_frame_roundtrip() {
    let (server, port) = spawn_server().await;
    let mut rx = server.subscribe();

    let client = WsClient::connect(&format!("ws://127.0.0.1:{port}"), 64)
        .await
        .expect("连接失败");

    let payload = b"hello sidelens";
    let buf = encode_frame_owned(&FrameSpec {
        ty: FrameType::Video,
        seq: 1,
        pts_us: Some(12345),
        send_ts_us: Some(67890),
        payload,
        ..FrameSpec::new(FrameType::Video, b"")
    })
    .unwrap();

    assert!(client.send(buf).await, "发送应成功");

    let frame = tokio::time::timeout(Duration::from_secs(3), rx.recv())
        .await
        .expect("超时")
        .expect("接收失败");

    assert_eq!(frame.ty, FrameType::Video);
    assert_eq!(frame.seq, 1);
    assert_eq!(frame.pts_us, 12345);
    assert_eq!(frame.send_ts_us, 67890);
    assert_eq!(frame.payload, payload);
}

#[tokio::test]
async fn sixty_frames_in_order() {
    let (server, port) = spawn_server().await;
    let mut rx = server.subscribe();

    let client = WsClient::connect(&format!("ws://127.0.0.1:{port}"), 256)
        .await
        .expect("连接失败");

    // payload 的首字节直接编码"期望的 seq"，这样断言可以同时校验
    // 顺序与内容。seq 由客户端从 1 开始递增。
    for expected_seq in 1..=60u32 {
        let payload = vec![(expected_seq & 0xff) as u8; 1024];
        assert!(
            client.send_frame(FrameType::Video, 0, 0, &payload).await,
            "第 {expected_seq} 帧发送失败"
        );
    }

    // 收满 60 帧
    let mut got = Vec::new();
    for _ in 0..60 {
        let f = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("超时")
            .expect("接收失败");
        got.push((f.seq, f.payload[0]));
    }

    assert_eq!(got.len(), 60);
    for (i, (seq, first_byte)) in got.iter().enumerate() {
        let expected = (i + 1) as u32;
        assert_eq!(*seq, expected, "第 {i} 帧序号错乱");
        assert_eq!(*first_byte, (expected & 0xff) as u8, "第 {i} 帧内容错乱");
    }
}

#[tokio::test]
async fn control_frame_json_survives() {
    let (server, port) = spawn_server().await;
    let mut rx = server.subscribe();

    let client = WsClient::connect(&format!("ws://127.0.0.1:{port}"), 16)
        .await
        .expect("连接失败");

    let json = br#"{"v":0,"name":"test-phone"}"#;
    let buf = encode_frame_owned(&FrameSpec::new(FrameType::Hello, json)).unwrap();
    client.send(buf).await;

    let f = tokio::time::timeout(Duration::from_secs(3), rx.recv())
        .await
        .expect("超时")
        .expect("接收失败");

    assert_eq!(f.ty, FrameType::Hello);
    let parsed = f.json().expect("控制帧应能解析为 JSON");
    assert_eq!(parsed["name"], "test-phone");
}

#[tokio::test]
async fn large_frame_survives_fragmentation() {
    let (server, port) = spawn_server().await;
    let mut rx = server.subscribe();

    let client = WsClient::connect(&format!("ws://127.0.0.1:{port}"), 16)
        .await
        .expect("连接失败");

    // 256 KB 会跨多个 WebSocket 消息分片
    let payload: Vec<u8> = (0..256 * 1024).map(|i| (i * 31 % 251) as u8).collect();
    client
        .send_frame(FrameType::Video, 0, FLAG_KEYFRAME, &payload)
        .await;

    let f = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("超时")
        .expect("接收失败");

    assert_eq!(f.payload.len(), payload.len());
    assert_eq!(f.payload, payload, "大帧内容必须逐字节一致");
    assert!(f.is_keyframe());
}

#[tokio::test]
async fn server_handles_two_clients() {
    let (server, port) = spawn_server().await;
    let mut rx = server.subscribe();

    let a = WsClient::connect(&format!("ws://127.0.0.1:{port}"), 16)
        .await
        .expect("A 连接失败");
    let b = WsClient::connect(&format!("ws://127.0.0.1:{port}"), 16)
        .await
        .expect("B 连接失败");

    a.send_frame(FrameType::Video, 0, 0, b"from-a").await;
    b.send_frame(FrameType::Video, 0, 0, b"from-b").await;

    let mut seen = Vec::new();
    for _ in 0..2 {
        let f = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .expect("超时")
            .expect("接收失败");
        seen.push(String::from_utf8_lossy(&f.payload).to_string());
    }
    seen.sort();
    assert_eq!(seen, vec!["from-a", "from-b"]);
}

#[tokio::test]
async fn backpressure_drops_instead_of_blocking() {
    // 队列深度设为 1，快速发 100 帧：一定会触发丢帧，但不能 panic 或卡死
    let (_server, port) = spawn_server().await;
    let client = WsClient::connect(&format!("ws://127.0.0.1:{port}"), 1)
        .await
        .expect("连接失败");

    let mut accepted = 0;
    for _ in 0..100 {
        if client.send_frame(FrameType::Video, 0, 0, b"x").await {
            accepted += 1;
        }
    }

    assert!(
        client
            .stats
            .dropped_backpressure
            .load(std::sync::atomic::Ordering::Relaxed)
            > 0,
        "队列只有 1 格，必然发生背压丢帧"
    );
    assert!(accepted > 0, "至少应有部分帧被接受");
}

#[tokio::test]
async fn downlink_keyframe_request_reaches_client() {
    let (server, port) = spawn_server().await;
    let mut rx = server.subscribe();

    let client = WsClient::connect(&format!("ws://127.0.0.1:{port}"), 16)
        .await
        .expect("连接失败");
    let mut down = client.subscribe();

    // 先发一帧以便服务端知道 client_id
    client.send_frame(FrameType::Video, 0, 0, b"x").await;
    let f = tokio::time::timeout(Duration::from_secs(3), rx.recv())
        .await
        .expect("超时")
        .expect("接收失败");

    // 服务端回发关键帧请求
    let req = sidelens_transport::control_frame(
        FrameType::KeyframeReq,
        &serde_json::json!({"reason": "test"}),
    );
    assert!(server.send_to(&f.client_id, req).await, "回发应成功");

    let down_frame = tokio::time::timeout(Duration::from_secs(3), down.recv())
        .await
        .expect("超时")
        .expect("下行接收失败");

    assert_eq!(down_frame.ty, FrameType::KeyframeReq);
}
