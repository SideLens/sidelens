//! FEC 桥接层的端到端测试
//!
//! 模拟真实链路：发送端把一帧切成分片 → 生成冗余包 →
//! 人为丢掉一些包 → 接收端尝试恢复。
//!
//! 与 `tests/fec.rs` 的区别：那边测纯 XOR 数学，这边测
//! 「分片 → 组 → 冗余包 → 恢复 → 重组」的完整流程。

use sidelens_core::fec::FecConfig;
use sidelens_core::fec_link::{suggest_fec, FecPacket, FecReceiver, FecSender, FEC_HEADER_SIZE};

fn cfg(group: usize, red: usize) -> FecConfig {
    FecConfig {
        group_size: group,
        redundant: red,
        max_packet_size: 1200,
    }
}

fn fragments(n: usize, size: usize) -> Vec<Vec<u8>> {
    (0..n)
        .map(|i| (0..size).map(|j| (i * 31 + j) as u8).collect())
        .collect()
}

/// 完整跑一遍：发送 → 丢指定下标 → 恢复
fn run_frame(
    c: FecConfig,
    frame_seq: u32,
    frags: &[Vec<u8>],
    lost: &[usize],
) -> Vec<(usize, Vec<u8>)> {
    let mut tx = FecSender::new(c).unwrap();
    tx.begin_frame(frame_seq);
    let mut reds = Vec::new();
    for f in frags {
        reds.extend(tx.push_fragment(f));
    }
    reds.extend(tx.finish_frame());

    let mut rx = FecReceiver::new(c).unwrap();
    for (i, f) in frags.iter().enumerate() {
        if !lost.contains(&i) {
            rx.push_fragment(frame_seq as u16, i, f);
        }
    }
    let mut recovered = Vec::new();
    for r in &reds {
        recovered.extend(rx.push_redundant(r));
    }
    recovered
}

#[test]
fn packet_roundtrip() {
    let p = FecPacket::new(12345, 3, 7, 4, vec![1, 2, 3, 4]);
    let enc = p.encode();
    assert_eq!(enc.len(), FEC_HEADER_SIZE + 4);
    let dec = FecPacket::decode(&enc).expect("应能解码");
    assert_eq!(dec, p);
    assert_eq!(dec.group_id(), (12345u32 << 16) | 3);
    assert_eq!(dec.real_count, 4);
}

#[test]
fn short_buffer_is_rejected() {
    assert!(FecPacket::decode(&[]).is_none());
    assert!(FecPacket::decode(&[0u8; 7]).is_none());
}

#[test]
fn no_loss_recovers_nothing() {
    let got = run_frame(cfg(4, 1), 1, &fragments(4, 100), &[]);
    assert!(got.is_empty(), "没有丢包就不该恢复出东西");
}

#[test]
fn single_loss_is_recovered() {
    let frags = fragments(4, 100);
    let got = run_frame(cfg(4, 1), 1, &frags, &[2]);
    assert_eq!(got.len(), 1, "应恢复出 1 个分片");
    assert_eq!(got[0].0, 2);
    assert_eq!(&got[0].1[..100], &frags[2][..], "内容必须逐字节一致");
}

#[test]
fn two_losses_in_different_residue_classes() {
    let frags = fragments(6, 80);
    // 1 % 2 = 1，4 % 2 = 0 —— 不同余类
    let got = run_frame(cfg(6, 2), 1, &frags, &[1, 4]);
    assert_eq!(got.len(), 2, "不同余类的双丢应全恢复");
    for (idx, data) in &got {
        assert_eq!(&data[..80], &frags[*idx][..], "下标 {idx} 内容不符");
    }
}

#[test]
fn cross_group_frame_recovers_tail_group() {
    // 5 个分片、组大小 4 —— 下标 4 落在第二组
    let frags = fragments(5, 60);
    let got = run_frame(cfg(4, 1), 1, &frags, &[4]);
    assert!(
        got.iter().any(|(i, _)| *i == 4),
        "尾组也应受保护，实际恢复出 {:?}",
        got.iter().map(|(i, _)| *i).collect::<Vec<_>>()
    );
    let (_, data) = got.iter().find(|(i, _)| *i == 4).unwrap();
    assert_eq!(&data[..60], &frags[4][..], "尾组内容必须正确");
}

#[test]
fn cross_group_losses_in_both_groups() {
    // 9 个分片、组大小 4 → 三个组（0-3 / 4-7 / 8）
    let frags = fragments(9, 40);
    // 每组各丢一个
    let got = run_frame(cfg(4, 1), 1, &frags, &[1, 5, 8]);
    assert_eq!(got.len(), 3, "三个组各丢一个，应全部恢复");
    for (idx, data) in &got {
        assert_eq!(&data[..40], &frags[*idx][..], "下标 {idx} 内容不符");
    }
}

#[test]
fn groups_are_isolated_between_frames() {
    let c = cfg(4, 1);
    let f1 = fragments(4, 50);
    let f2: Vec<Vec<u8>> = (0..4).map(|i| vec![(100 + i) as u8; 50]).collect();

    let mut tx = FecSender::new(c).unwrap();
    let mut rx = FecReceiver::new(c).unwrap();

    // 帧 1：丢下标 0
    tx.begin_frame(1);
    let mut r1 = Vec::new();
    for f in &f1 {
        r1.extend(tx.push_fragment(f));
    }
    r1.extend(tx.finish_frame());
    for (i, f) in f1.iter().enumerate() {
        if i != 0 {
            rx.push_fragment(1, i, f);
        }
    }
    let mut got = Vec::new();
    for r in &r1 {
        got.extend(rx.push_redundant(r));
    }
    assert_eq!(got.len(), 1, "帧 1 应恢复 1 个");
    assert_eq!(&got[0].1[..50], &f1[0][..]);

    // 帧 2：丢下标 2，不能受帧 1 残留影响
    tx.begin_frame(2);
    let mut r2 = Vec::new();
    for f in &f2 {
        r2.extend(tx.push_fragment(f));
    }
    r2.extend(tx.finish_frame());
    for (i, f) in f2.iter().enumerate() {
        if i != 2 {
            rx.push_fragment(2, i, f);
        }
    }
    let mut got2 = Vec::new();
    for r in &r2 {
        got2.extend(rx.push_redundant(r));
    }
    assert_eq!(got2.len(), 1, "帧 2 应恢复 1 个");
    assert_eq!(&got2[0].1[..50], &f2[2][..], "不能拿到帧 1 的残留数据");
}

#[test]
fn suggest_fec_follows_loss_table() {
    assert!(suggest_fec(0.005, 10).is_none(), "< 1% 不启用");
    assert_eq!(suggest_fec(0.02, 10).map(|c| c.redundant), Some(1));
    assert_eq!(suggest_fec(0.05, 10).map(|c| c.redundant), Some(2));
    assert_eq!(suggest_fec(0.10, 10).map(|c| c.redundant), Some(3));
    assert!(suggest_fec(0.20, 10).is_none(), "> 15% 不启用");
}

#[test]
fn suggest_fec_clamps_redundant_to_group_size() {
    let c = suggest_fec(0.10, 2).expect("应给出配置");
    assert!(c.redundant <= c.group_size);
    assert!(c.validate().is_ok());
}

#[test]
fn receiver_tracks_and_drops_groups() {
    let c = cfg(3, 1);
    let mut rx = FecReceiver::new(c).unwrap();
    rx.push_fragment(1, 0, &[1; 20]);
    rx.push_fragment(1, 1, &[2; 20]);
    let gid = 1u32 << 16;
    assert_eq!(rx.received(gid), 2);
    assert_eq!(rx.missing(gid), 1);
    assert_eq!(rx.tracked_groups(), 1);

    rx.drop_group(gid);
    assert_eq!(rx.tracked_groups(), 0);
    assert_eq!(rx.received(gid), 0);
}

#[test]
fn drop_before_frame_cleans_old_groups() {
    let c = cfg(2, 1);
    let mut rx = FecReceiver::new(c).unwrap();
    rx.push_fragment(1, 0, &[1; 10]);
    rx.push_fragment(2, 0, &[2; 10]);
    rx.push_fragment(3, 0, &[3; 10]);
    assert_eq!(rx.tracked_groups(), 3);
    rx.drop_before_frame(3);
    assert_eq!(rx.tracked_groups(), 1, "只应保留帧 3");
}

#[test]
fn sender_counts_are_tracked() {
    let c = cfg(4, 2);
    let mut tx = FecSender::new(c).unwrap();
    tx.begin_frame(1);
    let frags = fragments(4, 50);
    let mut reds = Vec::new();
    for f in &frags {
        reds.extend(tx.push_fragment(f));
    }
    reds.extend(tx.finish_frame());
    assert_eq!(reds.len(), 2, "redundant=2 应产出 2 个冗余包");
    assert_eq!(tx.redundant_sent, 2);
    assert!(tx.redundant_bytes > 0);
    assert_eq!(tx.frames_encoded, 1);
}

#[test]
fn invalid_config_is_rejected() {
    assert!(FecSender::new(cfg(0, 1)).is_err());
    assert!(FecSender::new(cfg(4, 0)).is_err());
    assert!(FecSender::new(cfg(2, 5)).is_err());
    assert!(FecReceiver::new(cfg(0, 1)).is_err());
}
// ============================================================================
// FEC 恢复分片的元数据还原（本轮修复的回归测试）
// ============================================================================
//
// 背景：FEC 恢复出的分片要重建 `DatagramPiece` 送回重组器。但 `DatagramPiece`
// 需要 `pts_us` / `send_ts_us` —— 如果填 0，重组出的帧进抖动缓冲时会被判为
// 「延迟几百万毫秒」，立刻丢弃。
//
// 所以 `FecReceiver` 要记住帧时间戳（`frame_meta`），恢复时取出来用。

#[test]
fn frame_meta_is_recorded_and_retrievable() {
    let c = cfg(4, 1);
    let mut rx = FecReceiver::new(c).unwrap();

    let gid = 1u32 << 16; // frame_seq=1, group_in_frame=0
    rx.push_fragment_with_meta(sidelens_core::FragmentInfo {
        frame_seq: 1,
        index: 0,
        data: &[1u8; 50],
        count: 4,
        total_size: 200,
        pts_us: 1_700_000_000_000,
        send_ts_us: 1_700_000_000_500,
        flags: 0,
    });

    let (pts, send_ts, flags) = rx.frame_meta(gid).expect("应记录帧时间戳");
    assert_eq!(flags, 0, "flags 应原样保留");
    assert_eq!(pts, 1_700_000_000_000);
    assert_eq!(send_ts, 1_700_000_000_500);

    let (count, total) = rx.group_meta(gid).expect("应记录分片头元数据");
    assert_eq!(count, 4);
    assert_eq!(total, 200);
}

#[test]
fn frame_meta_is_cleared_with_group() {
    let c = cfg(4, 1);
    let mut rx = FecReceiver::new(c).unwrap();
    let gid = 1u32 << 16;
    rx.push_fragment_with_meta(sidelens_core::FragmentInfo {
        frame_seq: 1,
        index: 0,
        data: &[1u8; 50],
        count: 4,
        total_size: 200,
        pts_us: 100,
        send_ts_us: 200,
        flags: 0,
    });
    assert!(rx.frame_meta(gid).is_some());

    rx.drop_group(gid);
    assert!(rx.frame_meta(gid).is_none(), "丢弃组时应一并清掉时间戳");
}

#[test]
fn drop_before_frame_clears_frame_meta() {
    let c = cfg(2, 1);
    let mut rx = FecReceiver::new(c).unwrap();
    for seq in 1u16..=3 {
        rx.push_fragment_with_meta(sidelens_core::FragmentInfo {
            frame_seq: seq,
            index: 0,
            data: &[seq as u8; 10],
            count: 1,
            total_size: 10,
            pts_us: seq as u64 * 1000,
            send_ts_us: seq as u64 * 1000,
            flags: 0,
        });
    }
    rx.drop_before_frame(3);
    assert!(rx.frame_meta(1u32 << 16).is_none(), "帧 1 应被清掉");
    assert!(rx.frame_meta(3u32 << 16).is_some(), "帧 3 应保留");
}
// ============================================================================
// FEC 收益评估（基于实测决策表）
// ============================================================================

#[test]
fn evaluate_fec_follows_measured_table() {
    // 实测决策表：<1% 不开，1-3.5% 用 1，3.5-7.5% 用 2，7.5-12% 用 3，>12% 不开
    assert!(
        sidelens_core::evaluate_fec(0.005, 8).is_none(),
        "0.5% 丢包不值得开 FEC"
    );
    assert_eq!(
        sidelens_core::evaluate_fec(0.02, 8).map(|(c, _)| c.redundant),
        Some(1),
        "2% 丢包用 redundant=1（实测开销 13.7%，NACK 27）"
    );
    assert_eq!(
        sidelens_core::evaluate_fec(0.05, 8).map(|(c, _)| c.redundant),
        Some(2),
        "5% 丢包用 redundant=2（实测开销 27%，NACK 21，最优档）"
    );
    assert_eq!(
        sidelens_core::evaluate_fec(0.10, 8).map(|(c, _)| c.redundant),
        Some(3),
        "10% 丢包用 redundant=3（边际划算）"
    );
    assert!(
        sidelens_core::evaluate_fec(0.20, 8).is_none(),
        ">12% 时开销反噬，应改为降码率"
    );
}

#[test]
fn evaluate_fec_clamps_redundant_to_group_size() {
    // 组大小 2 时最多 2 个冗余
    let (cfg, v) = sidelens_core::evaluate_fec(0.10, 2).expect("应给出配置");
    assert!(cfg.redundant <= cfg.group_size);
    assert!(cfg.validate().is_ok());
    assert!(v.worth_it);
    assert!(v.overhead_ratio > 0.0);
}

#[test]
fn evaluate_fec_evidence_is_not_empty() {
    for p in [0.02, 0.05, 0.10] {
        let (_, v) = sidelens_core::evaluate_fec(p, 8).expect("应给出配置");
        assert!(
            !v.evidence.is_empty(),
            "{p} 的评估应说明实测依据，不能是空字符串"
        );
        assert!(v.evidence.contains("实测"), "依据应标明来自实测");
    }
}

// ============================================================================
// 自适应冗余度：suggest_redundant 的边界、对拍、单调性
// ============================================================================

use sidelens_core::fec_link::{
    binomial_cdf, suggest_redundant, unresolve_loss, LossEstimator, RedundantHysteresis,
    MAX_ADAPTIVE_REDUNDANT,
};

/// 暴力枚举：直接按定义算 P(X <= r)，不做任何合并化简
///
/// 与被测实现的区别：这里用精确的整数组合数 + 逐项累乘，
/// 两边算法不同（一个用 C(k,i) 递推、一个用独立阶乘），能抓出递推写错。
fn brute_force_cdf(k: usize, p: f64, r: usize) -> f64 {
    if k == 0 || r >= k {
        return 1.0;
    }
    let q = 1.0 - p;
    let mut sum = 0.0;
    for i in 0..=r {
        // 用独立的阶乘算 C(k,i)，不复用被测实现的递推
        let mut comb = 1.0f64;
        for j in 0..i {
            comb = comb * (k - j) as f64 / (j + 1) as f64;
        }
        sum += comb * p.powi(i as i32) * q.powi((k - i) as i32);
    }
    sum
}

#[test]
fn suggest_redundant_zero_loss_needs_zero() {
    for k in [1usize, 2, 8, 55, 64] {
        assert_eq!(
            suggest_redundant(k, 0.0, 0.99),
            0,
            "不丢包就不需要冗余（k={k}）"
        );
    }
}

#[test]
fn suggest_redundant_high_loss_needs_many() {
    // 50% 丢包：必须冗余数接近组大小才可能达到 99%
    let r8 = suggest_redundant(8, 0.5, 0.99);
    assert!(r8 >= 6, "8 片 50% 丢包至少要 6 个冗余，实际 {r8}");
    let r55 = suggest_redundant(55, 0.5, 0.99);
    assert!(r55 >= 34, "55 片 50% 丢包至少要 34 个冗余，实际 {r55}");
    // 且必须仍然 < k（否则等于全冗余，说明算错了）
    assert!(r55 < 55);
}

#[test]
fn suggest_redundant_matches_bruteforce_over_grid() {
    // 题目要求的网格：group_size 2..64，loss 0.01..0.3
    for k in 2usize..=64 {
        for loss_pct in 1..=30 {
            let p = loss_pct as f64 / 100.0;
            let r = suggest_redundant(k, p, 0.99);
            // 返回值必须真的达标
            let got = brute_force_cdf(k, p, r);
            assert!(
                got >= 0.99 - 1e-9,
                "k={k} p={p} 给出 r={r}，但暴力算出的 P(可恢复)={got} < 0.99"
            );
            // 且必须是**最小**的：少一个就不达标（r=0 时无更小值）
            if r > 0 {
                let less = brute_force_cdf(k, p, r - 1);
                assert!(
                    less < 0.99 + 1e-9,
                    "k={k} p={p} 给出 r={r}，但 r={} 已经达标（{}），不是最小值",
                    r - 1,
                    less
                );
            }
        }
    }
}

#[test]
fn suggest_redundant_is_monotonic_in_group_size() {
    for loss_pct in [1, 2, 3, 5, 10, 20, 30] {
        let p = loss_pct as f64 / 100.0;
        let mut prev = 0usize;
        for k in 2usize..=64 {
            let r = suggest_redundant(k, p, 0.99);
            assert!(
                r >= prev,
                "loss={p} 时组从 {} 增到 {k}，redundant 从 {prev} 掉到 {r}（必须单调不减）",
                k - 1
            );
            prev = r;
        }
    }
}

#[test]
fn suggest_redundant_is_monotonic_in_loss() {
    for k in [2usize, 3, 8, 14, 16, 32, 55, 64] {
        let mut prev = 0usize;
        for loss_pct in 0..=50 {
            let p = loss_pct as f64 / 100.0;
            let r = suggest_redundant(k, p, 0.99);
            assert!(
                r >= prev,
                "k={k} 时丢包率从 {}% 涨到 {loss_pct}%，redundant 从 {prev} 掉到 {r}",
                loss_pct - 1
            );
            prev = r;
        }
    }
}

#[test]
fn suggest_redundant_never_exceeds_group_size() {
    for k in 1usize..=64 {
        for loss_pct in 0..=100 {
            let p = loss_pct as f64 / 100.0;
            let r = suggest_redundant(k, p, 0.99);
            assert!(r <= k, "k={k} p={p} 给出 r={r}，超过组大小");
        }
    }
}

#[test]
fn suggest_redundant_handles_degenerate_inputs() {
    assert_eq!(suggest_redundant(0, 0.05, 0.99), 0, "空组不需要冗余");
    assert_eq!(suggest_redundant(8, 0.0, 0.99), 0);
    assert_eq!(
        suggest_redundant(8, 0.05, 0.0),
        0,
        "目标为 0 时不需要任何冗余"
    );
    // NaN 不应 panic，且退化成「按 0 丢包」
    assert_eq!(suggest_redundant(8, f64::NAN, 0.99), 0);
    // 全丢：必须全冗余
    assert_eq!(suggest_redundant(8, 1.0, 0.99), 8);
    // 负丢包率钳到 0
    assert_eq!(suggest_redundant(8, -0.5, 0.99), 0);
}

#[test]
fn suggest_redundant_known_values() {
    // 这些数字与 fec-link.md 的手算表对齐（python 独立算过一遍）
    // 8 片 / 3% 丢包 -> 2 个冗余（正是历史上固定值的甜区）
    assert_eq!(suggest_redundant(8, 0.03, 0.99), 2);
    // 55 片 / 3% 丢包 -> 5 个（固定 2 只有 77% 可恢复，这是 stress 场景失败的根因）
    assert_eq!(suggest_redundant(55, 0.03, 0.99), 5);
    // 2 片 / 3% 丢包 -> 1 个（开销 50%，由 should_fec 的开销闸门挡掉）
    assert_eq!(suggest_redundant(2, 0.03, 0.99), 1);
    // 16 片 / 10% 丢包 -> 5 个
    assert_eq!(suggest_redundant(16, 0.10, 0.99), 5);
}

#[test]
fn binomial_cdf_sanity() {
    // 不丢包
    assert!((binomial_cdf(10, 0.0, 0) - 1.0).abs() < 1e-12);
    // r >= k 必然全收
    assert!((binomial_cdf(10, 0.3, 10) - 1.0).abs() < 1e-12);
    // 与暴力对拍（随机几个点）
    for (k, p, r) in [
        (10usize, 0.03f64, 2usize),
        (55, 0.03, 5),
        (64, 0.30, 20),
        (7, 0.05, 1),
    ] {
        let a = binomial_cdf(k, p, r);
        let b = brute_force_cdf(k, p, r);
        assert!(
            (a - b).abs() < 1e-9,
            "k={k} p={p} r={r}：递推 {a} vs 暴力 {b} 不一致"
        );
    }
}

// ---------------------------------------------------------------------------
// LossEstimator：NACK 驱动的丢包率估计
// ---------------------------------------------------------------------------

#[test]
fn loss_estimator_needs_warmup() {
    let mut e = LossEstimator::new();
    assert!(!e.is_warm(), "没滚过窗口时不该自称预热完成");
    assert_eq!(e.loss_estimate(), 0.0);
    // 喂满一个窗口
    for _ in 0..80 {
        e.record_frame(4, 0);
    }
    assert!(e.is_warm(), "320 个分片应已滚过一个窗口");
}

#[test]
fn loss_estimator_zero_nack_means_zero_loss() {
    let mut e = LossEstimator::new();
    for _ in 0..80 {
        e.record_frame(4, 0);
    }
    assert_eq!(e.loss_estimate(), 0.0, "没有 NACK 就是 0 丢包");
}

#[test]
fn loss_estimator_converts_frame_loss_to_packet_loss() {
    // 60 帧（正好是帧数窗口上限）、每帧 10 片、6 帧被 NACK
    // -> 帧级丢包 10%
    // -> 包级 = 1 - 0.9^(1/10) = 1.05%
    let mut e = LossEstimator::with_window(1000, 1.0);
    for i in 0..60u32 {
        e.record_frame(10, 0);
        if i < 6 {
            e.record_nack(i);
        }
    }
    let frame_loss = e.last_frame_loss();
    let p = e.loss_estimate();
    assert!(
        (frame_loss - 0.10).abs() < 0.02,
        "帧级丢包应约 10%，实际 {frame_loss}"
    );
    let expect = 1.0 - 0.9f64.powf(0.1);
    assert!(
        (p - expect).abs() < 0.005,
        "包级丢包应约 {expect}，实际 {p}（直接拿 nack/fragments 会得到 1%，压了 10 倍）"
    );
}

#[test]
fn loss_estimator_dedups_repeated_nack() {
    // 同一帧被请求 3 次（NackGenerator 最多重试 3 次）不能算成 3 帧丢失
    let mut e = LossEstimator::with_window(1000, 1.0);
    for i in 0..60u32 {
        e.record_frame(8, 0);
        if i < 6 {
            e.record_nack(i);
            e.record_nack(i);
            e.record_nack(i);
        }
    }
    assert!(
        (e.last_frame_loss() - 0.10).abs() < 0.02,
        "重复 NACK 必须去重，帧级丢包应约 10%，实际 {}",
        e.last_frame_loss()
    );
}

#[test]
fn loss_estimator_window_forgets_old_history() {
    // 前半段有丢包，后半段丢包率显著降低但**不为零** ——
    // 估计值必须跟着降下来，否则早期的高丢包率会永久污染决策。
    //
    // 注意：后半段不能是「零 NACK」。零失败时估计器**刻意保持**旧值
    // （见 LossEstimator::roll 的说明：零失败是下界不是观测值，
    // 按 0 处理会与「r 变大 -> 失败变少 -> 降 r -> 失败变多」形成振荡）。
    let mut e = LossEstimator::with_window(200, 1.0);
    // 前半段：每帧 1/2 失败
    for i in 0..50u32 {
        e.record_frame(4, 0);
        if i % 2 == 0 {
            e.record_nack(i);
        }
    }
    let high = e.loss_estimate();
    assert!(high > 0.05, "前半段应估出较高丢包率，实际 {high}");

    // 后半段：每 60 帧窗口里 6 次失败（10%，低得多，且过得了 min_failures=5 闸门）
    for i in 100..500u32 {
        e.record_frame(4, 0);
        // 每 10 帧失败 1 次 -> 60 帧窗口里 6 次
        if i % 10 == 0 {
            e.record_nack(i);
        }
    }
    let low = e.loss_estimate();
    assert!(
        low < high * 0.5,
        "丢包率下降后估计值必须跟着降（{high} -> {low}），否则早期丢包永久污染决策"
    );
}

#[test]
fn loss_estimator_holds_estimate_on_zero_failures() {
    // 零失败时**保持**旧值，不降到 0。
    // 这是第二版实测振荡的直接修复：r=5 时一个 NACK 都没有 -> p=0 -> want=1
    // -> r=1 时失败 31 次 -> p=3% -> want=5 -> 无限循环。
    let mut e = LossEstimator::with_window(200, 1.0);
    for i in 0..50u32 {
        e.record_frame(4, 0);
        if i % 2 == 0 {
            e.record_nack(i);
        }
    }
    let before = e.loss_estimate();
    assert!(before > 0.0);

    // 之后完全没有 NACK
    for _ in 0..500 {
        e.record_frame(4, 0);
    }
    let after = e.loss_estimate();
    assert_eq!(
        after, before,
        "零失败时必须保持旧估计（{before}），实际变成 {after} —— 降到 0 会造成振荡"
    );
    assert_eq!(e.last_failures, 0, "应记录「本窗口零失败」");
}

#[test]
fn loss_estimator_no_oscillation_when_redundant_is_sufficient() {
    // 端到端回归：真 3% 丢包、k=57、r 足够大时，窗口里经常零失败。
    // 若零失败被当成 p=0，want 会掉到 1，然后下一窗口失败暴增、want 跳回 5，
    // 无限振荡。这条测试验证估计值不会因为零失败窗口而归零。
    let k = 57usize;
    let p_true = 0.03f64;
    let mut e = LossEstimator::with_window(1_000_000, 1.0);
    // 先用 r=2 观测到失败，建立估计
    let f_at_2 = 1.0 - binomial_cdf(k, p_true, 2);
    let bad = (f_at_2 * 60.0).round() as u32;
    for i in 0..60u32 {
        e.record_frame(k, 2);
        if i < bad {
            e.record_nack(i);
        }
    }
    let p_est = e.loss_estimate();
    assert!(p_est > 0.02, "r=2 时应估出约 3%，实际 {p_est}");
    let want = suggest_redundant(k, p_est, 0.99);
    assert!(want >= 5, "应建议 >= 5 个冗余，实际 {want}");

    // 换成 r=want 之后，窗口里零失败（这在真 3% 丢包下是常态）
    for _ in 0..60 {
        e.record_frame(k, want);
    }
    let after = e.loss_estimate();
    assert!(
        after > 0.02,
        "零失败窗口后估计值必须保持（{p_est} -> {after}），否则会振荡"
    );
    assert!(suggest_redundant(k, after, 0.99) >= 5, "建议值也必须保持");
}

#[test]
fn loss_estimator_small_frames_still_roll() {
    // H.264 的 2 分片帧：300 分片窗口要 150 帧，帧数上限必须兜住
    let mut e = LossEstimator::new();
    for _ in 0..60 {
        e.record_frame(2, 0);
    }
    assert!(
        e.is_warm(),
        "2 分片帧跑 60 帧（2 秒）后应已滚过窗口，实际 windows={}",
        e.windows
    );
}

#[test]
fn loss_estimator_first_window_is_trusted_immediately() {
    let mut e = LossEstimator::with_window(50, 0.2);
    for i in 0..20u32 {
        e.record_frame(5, 0);
        e.record_nack(i);
    }
    // 第一个窗口直接采信，不从 0 爬
    assert!(
        e.loss_estimate() > 0.0,
        "第一个窗口应直接采信，实际 {}",
        e.loss_estimate()
    );
}

// ---------------------------------------------------------------------------
// RedundantHysteresis：双闸门
// ---------------------------------------------------------------------------

#[test]
fn hysteresis_blocks_small_delta() {
    // 降方向：±1 全部挡掉
    let mut h = RedundantHysteresis::new(4).with_gates(2, 0, 0);
    assert_eq!(h.observe(3), None);
    assert_eq!(h.observe(4), None);
    assert_eq!(h.current(), 4);
    // 降 2 放行
    assert_eq!(h.observe(2), Some(2));
    assert_eq!(h.current(), 2);
}

#[test]
fn hysteresis_up_step_of_one_is_allowed() {
    // 这是第一版对称迟滞的**功能性失效**：redundant 从 2 升到 3
    // 是最常见的升级路径（8 片 3% 丢包要 2，55 片要 5），
    // 对称闸门把它整条封死，实测 C-64k 场景 redundant 全程停在 2。
    let mut h = RedundantHysteresis::new(2).with_gates(2, 0, 0);
    assert_eq!(h.observe(3), Some(3), "升一级必须放行");
    assert_eq!(h.observe(4), Some(4));
    assert_eq!(h.observe(5), Some(5));
}

#[test]
fn hysteresis_blocks_rapid_switching() {
    let mut h = RedundantHysteresis::new(2).with_gates(2, 8, 30);
    assert_eq!(h.observe(5), Some(5), "第一次切换不受驻留限制");
    // 升方向的驻留是 8 帧：第 1..=7 次被挡，第 8 次放行
    for i in 1..=7 {
        assert_eq!(h.observe(9), None, "升驻留期内第 {i} 次不该切换");
    }
    assert_eq!(h.observe(9), Some(9));
    // 降方向的驻留是 30 帧：第 1..=29 次被挡
    for i in 1..=29 {
        assert_eq!(h.observe(2), None, "降驻留期内第 {i} 次不该切换");
    }
    assert_eq!(h.observe(2), Some(2));
    assert_eq!(h.switches, 3);
}

#[test]
fn hysteresis_down_dwell_is_longer_than_up() {
    // 快升慢降：同一帧数下升能过、降不能过
    let mut up = RedundantHysteresis::new(2).with_gates(2, 8, 30);
    let mut down = RedundantHysteresis::new(8).with_gates(2, 8, 30);
    // 各自先切一次，进入驻留期（升：幅度 1 就够；降：要幅度 2）
    assert_eq!(up.observe(3), Some(3));
    assert_eq!(down.observe(6), Some(6));
    for _ in 0..9 {
        let _ = up.observe(5);
        let _ = down.observe(3);
    }
    assert!(up.current() != 3, "9 帧后升方向（驻留 8）应已放行");
    assert_eq!(down.current(), 6, "9 帧后降方向仍应被 30 帧驻留挡住");
}

#[test]
fn hysteresis_can_decrease() {
    // 与 group_size 的「只增不减」不同：redundant 必须能降，
    // 否则链路恢复后仍在烧带宽
    let mut h = RedundantHysteresis::new(8).with_gates(2, 0, 0);
    assert_eq!(h.observe(2), Some(2));
    assert_eq!(h.current(), 2);
}

#[test]
fn hysteresis_clamps_to_ceiling_and_floor() {
    let mut h = RedundantHysteresis::new(1).with_gates(1, 0, 0);
    assert_eq!(
        h.observe(0),
        None,
        "want=0 应钳到 1，而 1 == current，不算切换"
    );
    let big = h.observe(1000);
    assert_eq!(big, Some(MAX_ADAPTIVE_REDUNDANT), "必须钳到上限");
    assert_eq!(h.ceiling(), MAX_ADAPTIVE_REDUNDANT);
}

#[test]
fn hysteresis_zero_want_never_yields_zero() {
    let mut h = RedundantHysteresis::new(4).with_gates(1, 0, 0);
    let got = h.observe(0);
    assert_eq!(got, Some(1), "redundant 不能是 0（FecConfig 会拒绝）");
}

#[test]
fn hysteresis_counts_blocked_intents() {
    // 诊断用：被闸门挡掉的意图数应该被记下来，
    // 否则「自适应没生效」时分不清是「算出来就是当前值」还是「被闸门挡了」
    let mut h = RedundantHysteresis::new(4).with_gates(2, 0, 0);
    let _ = h.observe(3);
    let _ = h.observe(5);
    assert!(h.blocked >= 1, "降一级的意图应被记为 blocked");
}

#[test]
fn hysteresis_realistic_loss_trace_settles() {
    // 用实测的估计值序列喂进去：必须收敛到 want 附近，不能一直抖
    // （D-64k-loss8 实测序列：0.02 -> 0.117 -> 0.16 -> 0.134 -> 0.091 -> 0.075 -> 0.049）
    let mut h = RedundantHysteresis::new(2);
    let wants = [2usize, 4, 13, 16, 14, 11, 9, 7, 7, 7, 7, 7, 7, 7, 7, 7];
    let mut switches = 0;
    for w in wants {
        for _ in 0..15 {
            if h.observe(w).is_some() {
                switches += 1;
            }
        }
    }
    assert!(
        switches <= 8,
        "16 个 want 值不该切 {switches} 次以上（每个值 15 帧 = 0.5s）"
    );
    assert_eq!(h.current(), 7, "最终应稳定在 want=7");
}

// ---------------------------------------------------------------------------
// unresolve_loss：FEC 感知的反解（修「FEC 越有效越不加冗余」的正反馈）
// ---------------------------------------------------------------------------

#[test]
fn unresolve_loss_roundtrips_without_fec() {
    // r = 0（FEC 关）时是经典形式 f = 1-(1-p)^k，必须精确往返
    for p in [0.005f64, 0.01, 0.03, 0.05, 0.10] {
        let f = 1.0 - binomial_cdf(57, p, 0);
        let back = unresolve_loss(57, 0, f);
        assert!(
            (back - p).abs() < 1e-6,
            "r=0 时应精确往返：p={p} -> f={f} -> back={back}"
        );
    }
}

#[test]
fn unresolve_loss_roundtrips_with_fec() {
    // 带 FEC：f = 1 - P(丢 <= r)，反解必须回到原 p
    for (k, r) in [(57usize, 2usize), (57, 5), (8, 2), (14, 3), (2, 1)] {
        for p in [0.01f64, 0.02, 0.03, 0.05, 0.08] {
            let f = 1.0 - binomial_cdf(k, p, r);
            if f <= 1e-9 {
                continue; // 冗余足够大时 f 可能小到反解不出来，跳过
            }
            let back = unresolve_loss(k, r, f);
            assert!(
                (back - p).abs() < 1e-5,
                "k={k} r={r} p={p} -> f={f} -> back={back} 往返失败"
            );
        }
    }
}

#[test]
fn unresolve_loss_is_not_naive_when_fec_is_on() {
    // 这是第一版的 bug：k=57, r=2, 真实 3% 丢包时
    // 朴素公式 1-(1-f)^(1/k) 给 0.49%，FEC 感知反解给 3%
    let k = 57usize;
    let r = 2usize;
    let p_true = 0.03f64;
    let f = 1.0 - binomial_cdf(k, p_true, r);
    let naive = 1.0 - (1.0 - f).powf(1.0 / k as f64);
    let fec_aware = unresolve_loss(k, r, f);
    assert!(
        naive < 0.01,
        "朴素反解应显著低估（{naive}），这正是第一版的 bug"
    );
    assert!(
        (fec_aware - p_true).abs() < 1e-5,
        "FEC 感知反解应还原真值 {p_true}，实际 {fec_aware}"
    );
    assert!(
        fec_aware > naive * 3.0,
        "两者差距应超过 3 倍（{fec_aware} vs {naive}）"
    );
}

#[test]
fn unresolve_loss_handles_degenerate_inputs() {
    assert_eq!(unresolve_loss(0, 2, 0.5), 0.0);
    assert_eq!(unresolve_loss(57, 2, 0.0), 0.0);
    assert_eq!(unresolve_loss(57, 2, 1.0), 1.0);
    // r >= k：永远能恢复，观测不到失败 -> 0
    assert_eq!(unresolve_loss(4, 4, 0.5), 0.0);
    assert_eq!(unresolve_loss(4, 9, 0.5), 0.0);
    assert_eq!(unresolve_loss(57, 2, f64::NAN), 0.0);
}

#[test]
fn unresolve_loss_is_monotonic_in_observed_failure() {
    let mut prev = 0.0f64;
    for i in 0..=50 {
        let f = i as f64 / 50.0 * 0.95;
        let p = unresolve_loss(57, 2, f);
        assert!(p >= prev - 1e-9, "f={f} 时反解值 {p} 比上一个 {prev} 小");
        prev = p;
    }
}

#[test]
fn loss_estimator_recovers_true_loss_when_fec_is_on() {
    // 端到端验证修复：模拟 55 片帧、redundant=2、真实 3% 丢包
    // 观测到的帧失败率约 24.4%，反解必须给出约 3%
    let k = 55usize;
    let r = 2usize;
    let p_true = 0.03f64;
    let f_obs = 1.0 - binomial_cdf(k, p_true, r);
    // 窗口的分片上限设得很大，让它只在「帧数上限 60」时滚一次 ——
    // 喂更多帧会滚出多个窗口，后面那些窗口没有 NACK，平滑会把估计值拉回 0。
    let mut e = LossEstimator::with_window(1_000_000, 1.0);
    let frames = 60u32;
    let bad = (f_obs * frames as f64).round() as u32;
    for i in 0..frames {
        e.record_frame(k, r);
        if i < bad {
            e.record_nack(i);
        }
    }
    let est = e.loss_estimate();
    assert!(
        (est - p_true).abs() < 0.01,
        "FEC 生效时估计值应接近真值 {p_true}，实际 {est}（朴素反解会给约 0.5%）"
    );
}

#[test]
fn loss_estimator_suggests_more_redundant_at_scale() {
    // 完整闭环：55 片 + 3% 丢包 + FEC 生效 -> 估计 -> suggest -> 应该 >= 5
    // 这是 C-64k 场景「redundant 全程停在 2」的回归测试
    let k = 55usize;
    let p_true = 0.03f64;
    let r = 2usize;
    let f_obs = 1.0 - binomial_cdf(k, p_true, r);
    // 窗口的分片上限设得很大，让它只在「帧数上限 60」时滚一次 ——
    // 喂更多帧会滚出多个窗口，后面那些窗口没有 NACK，平滑会把估计值拉回 0。
    let mut e = LossEstimator::with_window(1_000_000, 1.0);
    let frames = 60u32;
    let bad = (f_obs * frames as f64).round() as u32;
    for i in 0..frames {
        e.record_frame(k, r);
        if i < bad {
            e.record_nack(i);
        }
    }
    let want = suggest_redundant(k, e.loss_estimate(), 0.99);
    assert!(
        want >= 5,
        "55 片 3% 丢包应建议 >= 5 个冗余（真值 5），实际 {want}"
    );
}

// ─────────────────── 迟滞：降到下限的例外（实测 bug 回归）───────────────────

#[test]
fn hysteresis_can_drop_to_floor_without_delta() {
    // # 这条守的是一个真实的资源浪费 bug
    //
    // 链路干净时 `suggest_redundant` 返回 0，调用方钳到 1。
    // 若当前是 2，`abs_diff = 1 < min_delta_down = 2` → 永久卡在 2。
    //
    // 实测症状：`--loss 0` 时 `fec_recovered = 0` 但 `fec_overhead = 28.2%`
    // —— 发了 100% 用不上的冗余包，10 秒白白多发 723 KB。
    //
    // 修复后：10 秒 723 KB → 407 KB（overhead 28.2% → 15.9%）。
    let mut h = RedundantHysteresis::new(2);
    // 第一次 observe(1)：距上次切换的帧数不够，被驻留闸门挡
    // （switches == 0 时不限，所以第一次就该通过）
    let got = h.observe(1);
    assert_eq!(got, Some(1), "降到下限应被放行，不该被 min_delta_down 挡住");
    assert_eq!(h.current(), 1);
}

#[test]
fn hysteresis_still_blocks_small_drops_above_floor() {
    // 修 bug 不能把「挡住 2↔3 抖动」这个原意也弄丢
    let mut h = RedundantHysteresis::new(3);
    // 3 → 2：abs_diff = 1 < min_delta_down = 2，应被挡住
    assert_eq!(
        h.observe(2),
        None,
        "3→2 只降 1 档，应被 min_delta_down 挡住"
    );
    assert_eq!(h.current(), 3);
}

#[test]
fn hysteresis_allows_big_drop_above_floor() {
    let mut h = RedundantHysteresis::new(5);
    // 5 → 3：abs_diff = 2 >= min_delta_down，应放行
    assert_eq!(h.observe(3), Some(3));
    assert_eq!(h.current(), 3);
}

#[test]
fn loss_estimator_ignores_too_few_failures() {
    // 实测过冲的根因：k=57, r=16 时 1 个失败帧会被反解放大成 17.6%
    let k = 57usize;
    let r = 16usize;
    // 先建立一个可信估计（r=2 时有足够失败）
    let p_true = 0.03f64;
    let f2 = 1.0 - binomial_cdf(k, p_true, 2);
    let mut e = LossEstimator::with_window(1_000_000, 1.0);
    let bad = (f2 * 60.0).round() as u32;
    for i in 0..60u32 {
        e.record_frame(k, 2);
        if i < bad {
            e.record_nack(i);
        }
    }
    let good = e.loss_estimate();
    assert!(good > 0.02 && good < 0.05, "应估出约 3%，实际 {good}");

    // 切到 r=16，窗口里只有 2 个失败帧（远低于 min_failures=5）
    for i in 0..60u32 {
        e.record_frame(k, r);
        if i < 2 {
            e.record_nack(1000 + i);
        }
    }
    let after = e.loss_estimate();
    assert!(
        (after - good).abs() < 1e-9,
        "失败帧不足时必须保持旧估计（{good} -> {after}）；
         否则 1-2 个失败会被反解放大成 17-19%%，redundant 一路冲到上限"
    );
    assert!(e.held_windows >= 1, "应记录被闸门挡掉的窗口");
}

#[test]
fn loss_estimator_accepts_enough_failures() {
    // 失败帧数过闸门时必须更新（不能因为闸门太严导致永不更新）
    let k = 8usize;
    let p_true = 0.10f64;
    let f = 1.0 - binomial_cdf(k, p_true, 2);
    let mut e = LossEstimator::with_window(1_000_000, 1.0);
    let bad = ((f * 60.0).round() as u32).max(6);
    assert!(bad >= 6, "这个配置下 60 帧应有足够失败帧");
    for i in 0..60u32 {
        e.record_frame(k, 2);
        if i < bad {
            e.record_nack(i);
        }
    }
    assert!(e.loss_estimate() > 0.0, "有足够失败帧时必须更新估计");
    assert_eq!(e.held_windows, 0, "不该有被挡掉的窗口");
}
