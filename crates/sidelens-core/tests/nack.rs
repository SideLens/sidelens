//! NACK 生成器测试
//!
//! 核心验证点：延迟请求能避免"乱序误报"，这是 NACK 设计里最容易做错的地方。

use sidelens_core::{NackGenerator, NackRequest};

const MS: u64 = 1000;

#[test]
fn empty_generator_yields_nothing() {
    let mut g = NackGenerator::new(5.0, 32);
    assert!(g.poll(1000 * MS, 0).is_none());
    assert_eq!(g.pending_count(), 0);
}

#[test]
fn missing_seq_is_not_requested_before_delay() {
    let mut g = NackGenerator::new(5.0, 32);
    g.mark_missing(10, 0);

    // 还没到 5ms 延迟
    assert!(g.poll(3 * MS, 0).is_none());
    assert_eq!(g.pending_count(), 1);

    // 到了 5ms 之后才请求
    let req = g.poll(6 * MS, 0).expect("应生成请求");
    assert_eq!(req.seqs, vec![10]);
    // 关键：记录要保留，否则重传再丢就没人管了
    assert_eq!(g.pending_count(), 1);
}

#[test]
fn unfulfilled_missing_is_retried() {
    let mut g = NackGenerator::new(5.0, 32).with_retry(20.0, 3);
    g.mark_missing(7, 0);

    // 首次请求
    assert!(g.poll(6 * MS, 0).is_some());
    // retry_ms = 20，还没到
    assert!(g.poll(20 * MS, 0).is_none());
    // 到了，重发
    let again = g.poll(30 * MS, 0).expect("应重发");
    assert_eq!(again.seqs, vec![7]);
    assert_eq!(g.pending_count(), 1);
}

#[test]
fn gives_up_after_max_retries() {
    let mut g = NackGenerator::new(0.0, 32).with_retry(1.0, 2);
    g.mark_missing(3, 0);

    assert!(g.poll(MS, 0).is_some()); // 第 1 次
    assert!(g.poll(10 * MS, 0).is_some()); // 第 2 次
                                           // 第 3 次应当放弃而不是继续请求
    assert!(g.poll(100 * MS, 0).is_none());
    assert_eq!(g.gave_up, 1);
    assert_eq!(g.pending_count(), 0);
}

#[test]
fn received_frame_heals_and_stops_retrying() {
    let mut g = NackGenerator::new(0.0, 32).with_retry(1.0, 5);
    g.mark_missing(9, 0);
    assert!(g.poll(MS, 0).is_some());

    // 原包（或重传）到了
    g.mark_received(9);
    assert_eq!(g.pending_count(), 0);
    // 不应再请求
    assert!(g.poll(100 * MS, 0).is_none());
}

#[test]
fn reordered_frame_heals_before_request() {
    let mut g = NackGenerator::new(5.0, 32);

    // seq 10 缺失（其实只是乱序，还在路上）
    g.mark_missing(10, 0);
    // 3ms 后原包到了
    g.mark_received(10);

    // 等到延迟之后：不应该发出任何请求
    assert!(g.poll(10 * MS, 0).is_none(), "乱序自愈后不应请求重传");
    assert_eq!(g.healed_before_request, 1);
    assert_eq!(g.pending_count(), 0);
}

#[test]
fn batch_limits_seqs_per_request() {
    let mut g = NackGenerator::new(0.0, 3);
    for seq in 1..=10 {
        g.mark_missing(seq, 0);
    }

    let req = g.poll(10 * MS, 0).expect("应生成请求");
    assert_eq!(req.len(), 3, "单次请求不应超过 max_batch");
    assert_eq!(req.seqs, vec![1, 2, 3], "应按序号升序批量取出");

    // 记录全部保留（可能重发），但下一次 poll 应取未请求过的
    assert_eq!(g.pending_count(), 10);
    let next = g.poll(10 * MS, 0).expect("应继续取下一批");
    assert_eq!(next.seqs, vec![4, 5, 6], "第二批应接着取");
}

#[test]
fn requests_are_deduplicated_and_sorted() {
    let req = NackRequest::new(0, vec![5, 2, 5, 3, 2]);
    assert_eq!(req.seqs, vec![2, 3, 5]);
}

#[test]
fn nack_json_roundtrip() {
    let req = NackRequest::new(1, vec![7, 8, 9]);
    let bytes = req.to_bytes();
    let parsed = NackRequest::from_bytes(&bytes).expect("应能解析");
    assert_eq!(parsed, req);
}

#[test]
fn malformed_json_returns_none() {
    assert!(NackRequest::from_bytes(b"not json").is_none());
    assert!(NackRequest::from_bytes(b"").is_none());
}

#[test]
fn repeated_missing_same_seq_counts_once() {
    let mut g = NackGenerator::new(0.0, 32);
    g.mark_missing(42, 0);
    g.mark_missing(42, 1000);
    g.mark_missing(42, 2000);
    assert_eq!(g.total_missing, 1, "同一序号重复标记只算一次缺失");
    assert_eq!(g.pending_count(), 1);
}

#[test]
fn forget_before_drops_old_entries() {
    let mut g = NackGenerator::new(0.0, 32);
    for seq in 1..=10 {
        g.mark_missing(seq, 0);
    }
    // 已经收到关键帧 5，之前的重传都没意义了
    g.forget_before(5);
    assert_eq!(g.pending_count(), 6, "只剩 5..=10");
}

#[test]
fn stats_track_requests() {
    let mut g = NackGenerator::new(0.0, 32);
    for seq in 1..=5 {
        g.mark_missing(seq, 0);
    }
    g.poll(10 * MS, 0);
    assert_eq!(g.total_requests, 1);
    assert_eq!(g.total_seqs_requested, 5);
}
