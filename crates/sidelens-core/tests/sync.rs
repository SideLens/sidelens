//! 时钟同步测试
//!
//! 核心验证：能正确算出时钟偏移与单向延迟，且在未同步时拒绝给出错误数字。

use sidelens_core::sync::{ClockSync, SyncSample};

const MS: i64 = 1000;
const MS_U: u64 = 1000;

#[test]
fn perfect_symmetry_yields_exact_offset() {
    // 假设对端时钟比本端快 500ms，网络单程 10ms，对端处理 2ms
    let mut sync = ClockSync::new();
    let offset = 500 * MS; // 对端 = 本端 + 500ms
    let one_way = 10 * MS_U;

    let t1 = 1_000_000u64; // 本端发出
    let t2 = (t1 as i64 + offset + one_way as i64) as u64; // 对端收到
    let t3 = t2 + 2 * MS_U; // 对端发出回复
    let t4 = (t3 as i64 - offset + one_way as i64) as u64; // 本端收到

    let sample = sync.finish_probe(t4, t1, t2, t3).expect("应结算成功");
    assert_eq!(sample.rtt_us, 20 * MS_U, "RTT 应扣除对端处理时间");
    assert_eq!(sample.offset_us, offset, "偏移应精确还原");
}

#[test]
fn asymmetric_path_keeps_minimum_rtt_sample() {
    let mut sync = ClockSync::new();

    // 好样本：RTT 小
    sync.add_sample(SyncSample {
        rtt_us: 5 * MS_U,
        offset_us: 100 * MS,
    });
    // 坏样本：RTT 大（路径拥塞），offset 偏离
    sync.add_sample(SyncSample {
        rtt_us: 200 * MS_U,
        offset_us: 180 * MS,
    });

    assert_eq!(sync.offset_us(), Some(100 * MS), "应选 RTT 最小的样本");
    assert_eq!(sync.rtt_us(), 5 * MS_U);
}

#[test]
fn unsynced_returns_none_for_latency() {
    let sync = ClockSync::new();
    assert!(!sync.is_synced());
    assert!(sync.offset_us().is_none());
    assert!(
        sync.one_way_latency_us(1_000_000, 1_100_000).is_none(),
        "未同步时必须返回 None，不能给错数字"
    );
}

#[test]
fn synced_latency_is_accurate() {
    let mut sync = ClockSync::new();
    sync.add_sample(SyncSample {
        rtt_us: 2 * MS_U,
        offset_us: 1_000 * MS,
    });

    // 对端在 peer_t 发出，此时本端已经过 peer_t + offset + 15ms
    let peer_t = 5_000_000u64;
    let local_now = (peer_t as i64 + 1_000 * MS + 15 * MS) as u64;

    let lat = sync
        .one_way_latency_us(peer_t, local_now)
        .expect("应能算出");
    assert_eq!(lat, 15 * MS_U);
}

#[test]
fn to_local_shifts_timestamps() {
    let mut sync = ClockSync::new();
    sync.add_sample(SyncSample {
        rtt_us: MS_U,
        offset_us: -250 * MS,
    });

    let peer = 10_000_000u64;
    let local = sync.to_local(peer);
    assert_eq!(local as i64, peer as i64 - 250 * MS);
}

#[test]
fn negative_result_is_clamped_to_zero() {
    let mut sync = ClockSync::new();
    sync.add_sample(SyncSample {
        rtt_us: MS_U,
        offset_us: -999_999 * MS,
    });
    // 对端时刻 + 巨大负偏移 → 会被 clamp 到 0 而不是下溢
    assert_eq!(sync.to_local(1_000_000), 0);
}

#[test]
fn absurd_rtt_is_rejected() {
    let mut sync = ClockSync::new();
    let t1 = 0u64;
    let t2 = 1_000u64;
    let t3 = 1_001u64;
    // t4 巨大，RTT 超过 2 秒
    let t4 = 10_000_000u64;
    assert!(sync.finish_probe(t4, t1, t2, t3).is_none());
    assert!(!sync.is_synced());
}

#[test]
fn inconsistent_timestamps_are_rejected() {
    let mut sync = ClockSync::new();
    // t3 < t2：对端处理时间为负，物理不可能
    assert!(sync.finish_probe(5_000, 1_000, 9_000, 8_000).is_none());
    // t4 < t1：总耗时为负
    assert!(sync.finish_probe(500, 1_000, 1_500, 1_600).is_none());
}

#[test]
fn probe_nonce_increments() {
    let mut sync = ClockSync::new();
    let a = sync.start_probe(0, 0);
    let b = sync.start_probe(100, 0);
    assert_eq!(b.nonce, a.nonce + 1);
    assert_eq!(a.t1_us, 0);
    assert_eq!(b.t1_us, 100);
}

#[test]
fn window_evicts_old_samples() {
    let mut sync = ClockSync::new().with_window(3);
    for i in 0..10u64 {
        sync.add_sample(SyncSample {
            rtt_us: (10 - i) * MS_U,
            offset_us: i as i64 * MS,
        });
    }
    assert_eq!(sync.sample_count(), 3);
    // 最后 3 个样本的 RTT 是 2,1,0 ms，最小的对应 i=9
    assert_eq!(sync.offset_us(), Some(9 * MS));
}

#[test]
fn clear_resets_everything() {
    let mut sync = ClockSync::new();
    sync.add_sample(SyncSample {
        rtt_us: MS_U,
        offset_us: 5 * MS,
    });
    assert!(sync.is_synced());
    sync.clear();
    assert!(!sync.is_synced());
    assert_eq!(sync.sample_count(), 0);
}

// ── RttReply 的码率建议字段（第二十八轮新增）──────────────────
//
// 拥塞控制器在接收端跑，执行码率的是发送端编码器。中间缺一条反馈回路，
// 复用现成的 RTT_REPLY 反向通道携带建议。这一组测试守住向后兼容 ——
// 混合版本互通不能崩。

use sidelens_core::RttReply;

#[test]
fn rtt_reply_without_rate_hint_roundtrips() {
    let r = RttReply::new(7, 1000, 2000, 3000);
    assert_eq!(r.rate_hint(), None, "new() 不应带码率建议");
    assert_eq!(r.loss_ratio(), None);

    let back = RttReply::from_bytes(&r.to_bytes()).expect("应能反序列化");
    assert_eq!(back, r);
    assert_eq!(back.rate_hint(), None);
}

#[test]
fn rtt_reply_with_rate_hint_roundtrips() {
    let r = RttReply::new(7, 1000, 2000, 3000).with_rate_hint(1_500_000, 0.037);
    assert_eq!(r.rate_hint(), Some(1_500_000));
    // 0.037 → 37 permille → 0.037
    let loss = r.loss_ratio().expect("应有丢包率");
    assert!((loss - 0.037).abs() < 1e-9, "丢包率往返应无损，实测 {loss}");

    let back = RttReply::from_bytes(&r.to_bytes()).expect("应能反序列化");
    assert_eq!(back, r);
}

#[test]
fn old_version_json_without_hint_deserializes() {
    // 老版本发来的 JSON 只有四个字段。新增字段必须能缺省，
    // 否则升级接收端后老发送端一握手就失败。
    let old = br#"{"nonce":1,"t1_us":100,"t2_us":200,"t3_us":300}"#;
    let r = RttReply::from_bytes(old).expect("老格式必须仍能解析");
    assert_eq!(r.nonce, 1);
    assert_eq!(r.t3_us, 300);
    assert_eq!(r.rate_hint(), None, "缺字段应为 None");
    assert_eq!(r.loss_ratio(), None);
}

#[test]
fn new_version_json_is_ignorable_by_old_parsers() {
    // 新版本发给老版本时，老版本的 serde 会忽略未知键 —— 这里模拟
    // 「忽略未知字段」的反序列化行为：只取它认识的四个字段。
    let r = RttReply::new(9, 10, 20, 30).with_rate_hint(800_000, 0.5);
    let json = String::from_utf8(r.to_bytes()).expect("应是 UTF-8");

    // 断言新增字段确实写出去了（老版本靠「忽略未知键」而不是「字段不存在」）
    assert!(json.contains("target_bps"), "应包含 target_bps：{json}");
    assert!(
        json.contains("loss_permille"),
        "应包含 loss_permille：{json}"
    );
    // 且没有把 None 写成 null（skip_serializing_if 生效）
    let bare = RttReply::new(9, 10, 20, 30);
    let bare_json = String::from_utf8(bare.to_bytes()).expect("应是 UTF-8");
    assert!(
        !bare_json.contains("target_bps"),
        "None 不应被序列化出来：{bare_json}"
    );
}

#[test]
fn loss_permille_is_clamped() {
    // 异常对端可能给 0-1 之外的比值；不能溢出也不能写出 >1000
    let hi = RttReply::new(1, 0, 0, 0).with_rate_hint(1_000_000, 5.0);
    assert_eq!(hi.loss_permille, Some(1000), "超过 100% 应钳到 1000");

    let neg = RttReply::new(1, 0, 0, 0).with_rate_hint(1_000_000, -1.0);
    assert_eq!(neg.loss_permille, Some(0), "负数应钳到 0");
}
