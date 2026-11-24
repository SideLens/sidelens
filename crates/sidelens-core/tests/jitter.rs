//! 逐条对应 JS 版的 6 个抖动缓冲测试

use sidelens_core::{JitterBuffer, JitterConfig};
use sidelens_proto::FrameType;

const MS: u64 = 1000;

fn mk(seq: u32, pts_us: u64) -> sidelens_core::jitter::QueuedFrame {
    sidelens_core::jitter::QueuedFrame {
        ty: FrameType::Video,
        seq,
        pts_us,
        send_ts_us: 0,
        arrived_at_us: 0,
        flags: 0,
        is_retransmit: false,
        payload: vec![0u8; 4],
    }
}

fn cfg(target_ms: f64, max_ms: f64) -> JitterConfig {
    JitterConfig {
        target_ms,
        max_ms,
        ..Default::default()
    }
}

#[test]
fn does_not_emit_before_target_delay() {
    let mut jb = JitterBuffer::new(cfg(50.0, 200.0));
    jb.push_at(mk(1, 1000), 0);
    assert!(jb.pop_at(10 * MS).is_none(), "未到 50ms 目标延迟不应出帧");
}

#[test]
fn emits_in_order_after_target_delay() {
    let mut jb = JitterBuffer::new(cfg(10.0, 200.0));
    for i in 1..=3u32 {
        jb.push_at(mk(i, i as u64 * 1000), 0);
    }

    let mut got = Vec::new();
    for _ in 0..3 {
        if let Some(f) = jb.pop_at(50 * MS) {
            got.push(f.seq);
        }
    }
    assert_eq!(got, vec![1, 2, 3]);
}

#[test]
fn reorders_by_pts() {
    let mut jb = JitterBuffer::new(cfg(0.0, 200.0));
    // 先投 seq=2（PTS 较大），再投 seq=1（PTS 较小）
    jb.push_at(mk(2, 2000), 0);
    jb.push_at(mk(1, 1000), 0);

    let later = 60 * MS;
    assert_eq!(jb.pop_at(later).unwrap().seq, 1);
    assert_eq!(jb.pop_at(later).unwrap().seq, 2);
}

#[test]
fn drops_only_already_played_frames() {
    let mut jb = JitterBuffer::new(cfg(0.0, 200.0));
    jb.push_at(mk(5, 5000), 0);
    assert_eq!(jb.pop_at(30 * MS).unwrap().seq, 5);

    // 5 已播出，此时 seq=3 才是真迟到
    let out = jb.push_at(mk(3, 3000), 30 * MS);
    assert_eq!(jb.stats.reordered, 1);
    assert!(!out.needs_keyframe);
    assert!(out.late);
    assert!(jb.is_empty(), "迟到帧不应入队");
}

#[test]
fn overflow_clears_and_requests_keyframe() {
    let mut jb = JitterBuffer::new(cfg(20.0, 5.0));
    jb.push_at(mk(1, 1), 0);
    // 第二帧的到达时刻距第一帧 1 秒，远超 max_ms=5
    let out = jb.push_at(mk(2, 2000), 1_000 * MS);

    assert!(out.needs_keyframe, "积压过深应请求关键帧");
    assert!(jb.is_empty(), "应清空队列");
    assert_eq!(jb.stats.keyframe_requests, 1);
    // 清空时队列里的 2 帧（seq=1 与 seq=2）都算丢弃
    assert_eq!(jb.stats.dropped, 2, "被清空的所有帧都计入 dropped");
}

#[test]
fn shrink_converges_to_floor() {
    let mut jb = JitterBuffer::new(JitterConfig {
        target_ms: 60.0,
        shrink_per_sec: 100.0,
        min_ms: 10.0,
        ..Default::default()
    });
    jb.shrink(1000.0);
    assert!(
        (jb.target_ms() - 10.0).abs() < f64::EPSILON,
        "应收敛到下限 10ms"
    );
}

// ── adapt（抖动自适应目标延迟）───────────────────────────────
//
// 这一组测试针对的是本轮发现的真问题：`shrink` / `grow` 早就写好了，
// 但生产路径一次都没调用，于是 `target_ms` 永远是默认的 20ms，
// 实测 `playable_ms` p50 也就永远是 20.95ms —— 其中 20ms 是纯等待。

use sidelens_core::jitter::{ADAPT_HOLD_US, ADAPT_MAX_STEP_MS, JITTER_SAFETY_FACTOR};

/// 造一个「抖动已经建立」的缓冲
///
/// 正确公式下抖动 = `(Δarrive - Δsend)`，所以这里让 send_ts 前进 10ms、
/// 到达时刻前进 10ms + jitter，差值就是 jitter。
fn with_jitter(jitter_us: u64) -> JitterBuffer {
    let mut jb = JitterBuffer::new(JitterConfig {
        target_ms: 20.0,
        min_ms: 10.0,
        max_ms: 200.0,
        ..Default::default()
    });
    jb.push_at(mk_with(1, 0), 0);
    jb.push_at(mk_with(2, 10 * MS), 10 * MS + jitter_us);
    jb
}

#[test]
fn adapt_lowers_target_when_jitter_is_small() {
    let mut jb = with_jitter(1000); // 1ms 抖动
    let jitter_ms = jb.stats.avg_jitter_ms;
    assert!(jitter_ms > 0.0, "抖动 EWMA 应已建立，实测 {jitter_ms}");

    let before = jb.target_ms();
    let change = jb.adapt(1_000_000).expect("应发生下调");
    let after = jb.target_ms();

    assert!(after < before, "小抖动应下调，{before} -> {after}");
    assert_eq!(change, (before, after));
    // 单次降幅不得超过 ADAPT_MAX_STEP_MS
    assert!(
        before - after <= ADAPT_MAX_STEP_MS + 1e-9,
        "单次降幅 {} 超过上限 {ADAPT_MAX_STEP_MS}",
        before - after
    );
    assert_eq!(jb.stats.adapt_downs, 1);
    assert_eq!(jb.stats.adapt_ups, 0);
}

#[test]
fn adapt_raises_target_immediately_when_jitter_grows() {
    // 用 14ms —— 必须在 `JITTER_SAMPLE_CAP_US`（15ms）以内。
    //
    // # 为什么不能再用 30ms（第三十八轮改的）
    //
    // 30ms 的样本会被限幅逻辑丢弃（它更可能是「等重传」而非真实抖动），
    // 于是 `jitter_ewma` 保持 0，`adapt` 返回 None，测试失败。
    //
    // 这不是测试写错了，而是**语义变了**：现在 >15ms 的样本不算抖动。
    // 想验证「上调」只能用限幅内的值。
    let mut jb = with_jitter(14_000); // 14ms 抖动
    let before = jb.target_ms();
    let change = jb.adapt(1_000_000).expect("应发生上调");
    let after = jb.target_ms();

    assert!(after > before, "大抖动应立即上调，{before} -> {after}");
    assert!(change.0 < change.1);
    // 上调是全额生效，不受单步上限约束
    assert!(
        after - before > ADAPT_MAX_STEP_MS,
        "上调应全额生效（{} 应大于单步上限 {ADAPT_MAX_STEP_MS}）",
        after - before
    );
    assert_eq!(jb.stats.adapt_ups, 1);
    assert_eq!(jb.stats.adapt_downs, 0);
}

#[test]
fn adapt_respects_floor() {
    let mut jb = with_jitter(100); // 0.1ms，极小
                                   // 反复降，每次都推进超过驻留期
    let mut t = 1_000_000u64;
    for _ in 0..50 {
        jb.adapt(t);
        t += ADAPT_HOLD_US;
    }
    assert!(
        jb.target_ms() >= 10.0 - 1e-9,
        "不得低于 min_ms=10，实测 {}",
        jb.target_ms()
    );
    assert_eq!(jb.target_ms(), 10.0, "应收敛到下限");
}

#[test]
fn adapt_holds_off_within_dwell_window() {
    let mut jb = with_jitter(1000);
    assert!(jb.adapt(1_000_000).is_some(), "首次应调整");
    // 驻留期内再调用：不动
    assert!(
        jb.adapt(1_000_000 + ADAPT_HOLD_US - 1).is_none(),
        "驻留期内不应调整"
    );
    // 驻留期满：可以再调
    assert!(
        jb.adapt(1_000_000 + ADAPT_HOLD_US).is_some(),
        "驻留期满应可调整"
    );
}

#[test]
fn adapt_does_nothing_before_jitter_is_established() {
    let mut jb = JitterBuffer::new(JitterConfig::default());
    // 一帧都没推：抖动 EWMA = 0
    assert!(jb.adapt(1_000_000).is_none(), "无样本时不应调整");

    // 只推一帧：还不够（需要 2 帧才能算 transit 差）
    jb.push_at(mk(1, 1000), 0);
    assert!(jb.adapt(1_000_000).is_none(), "单帧时不应调整");
}

#[test]
fn adapt_target_tracks_safety_factor() {
    // 12ms 抖动 → 12 × 2.5 = 30ms > 当前 20ms，走**上调**路径（全额生效）。
    // 不能用 4ms：4 × 2.5 = 10 < 20 是下调，会被单步上限挡住，
    // 测到的就不是 safety_factor 而是 ADAPT_MAX_STEP_MS。
    let mut jb = with_jitter(12_000);
    let jitter_ms = jb.stats.avg_jitter_ms;
    let expected = (jitter_ms * JITTER_SAFETY_FACTOR).clamp(10.0, 200.0);
    assert!(expected > 20.0, "本用例要求走上调路径，expected={expected}");

    jb.adapt(1_000_000);
    assert!(
        (jb.target_ms() - expected).abs() < 1e-6,
        "目标应等于 抖动×{JITTER_SAFETY_FACTOR} = {expected}，实测 {}",
        jb.target_ms()
    );
}

#[test]
fn adapt_is_idempotent_at_convergence() {
    // 走上调路径一步到位，之后就是收敛点（下调路径要分多步，不是收敛态）
    let mut jb = with_jitter(12_000);
    jb.adapt(1_000_000);
    let converged = jb.target_ms();

    assert!(
        jb.adapt(1_000_000 + ADAPT_HOLD_US).is_none(),
        "已收敛时不应再调整（{converged}）"
    );
    assert_eq!(jb.target_ms(), converged);
    assert_eq!(jb.stats.adaptations, 1, "只应调整过 1 次");
}
// ── 抖动估计公式（本轮修的真 bug）────────────────────────────
//
// 原公式 `|(arrived - send_ts) - (last_arrival - send_ts)|` 里
// `send_ts` 自己抵消，实际算的是 `|arrived - last_arrival|` ——
// **到达间隔**，不是抖动。60fps 下恒在 16.7ms。

#[test]
fn jitter_is_transit_delta_not_arrival_gap() {
    let mut jb = JitterBuffer::new(JitterConfig::default());

    // 60fps：帧间隔 16667us，网络完全稳定（抖动 0）
    let interval = 16_667u64;
    jb.push_at(mk_with(1, 0), 0);
    jb.push_at(mk_with(2, interval), interval);

    assert!(
        jb.stats.avg_jitter_ms < 0.01,
        "网络稳定时抖动应接近 0，实测 {:.4}ms（若约 16.7 说明算成了到达间隔）",
        jb.stats.avg_jitter_ms
    );
}

#[test]
fn jitter_reflects_only_network_delay_variation() {
    let mut jb = JitterBuffer::new(JitterConfig::default());
    let interval = 16_667u64;

    // 帧间隔稳定，但第 2 帧晚到 2ms → 抖动 = 2ms
    jb.push_at(mk_with(1, 0), 0);
    jb.push_at(mk_with(2, interval), interval + 2_000);

    let j = jb.stats.avg_jitter_ms;
    assert!((j - 2.0).abs() < 0.01, "抖动应为 2.00ms，实测 {j:.4}ms");
}

#[test]
fn jitter_ignores_frame_interval_changes() {
    let mut jb = JitterBuffer::new(JitterConfig::default());

    // 帧间隔从 16.7ms 变成 33.3ms（掉了一帧），但网络稳定
    // 正确实现应算 0 抖动；错误实现会算成 16.7ms
    jb.push_at(mk_with(1, 0), 0);
    jb.push_at(mk_with(2, 16_667), 16_667);
    jb.push_at(mk_with(3, 50_000), 50_000);

    assert!(
        jb.stats.avg_jitter_ms < 0.01,
        "帧间隔变化不是抖动，实测 {:.4}ms",
        jb.stats.avg_jitter_ms
    );
}

/// 带 send_ts 的帧构造器
fn mk_with(seq: u32, send_ts_us: u64) -> sidelens_core::jitter::QueuedFrame {
    let mut f = mk(seq, send_ts_us);
    f.send_ts_us = send_ts_us;
    f
}

// ── 抖动样本限幅（第三十八轮）──────────────────────────────
//
// 3% 丢包 + NACK 重传时，被重传救回来的帧到达时刻推迟 ~30ms，
// 而 Δsend 只有 33ms，于是 |Δarrive - Δsend| = 30ms 的**假抖动**。
// 实测 jitter_ewma 从 0.05ms 涨到 31ms（600 倍），target_ms 从 10 涨到 78。

use sidelens_core::jitter::JITTER_SAMPLE_CAP_US;

#[test]
fn jitter_sample_above_cap_is_ignored() {
    let mut jb = JitterBuffer::new(JitterConfig::default());

    // 先建立一个正常样本（1ms）
    jb.push_at(mk_with(1, 0), 0);
    jb.push_at(mk_with(2, 16_667), 17_667);
    let baseline = jb.stats.avg_jitter_ms;
    assert!(baseline > 0.0 && baseline < 2.0, "基线抖动 {baseline}ms");

    // 再来一个 40ms 的「等重传」样本（超过 15ms 上限）
    jb.push_at(mk_with(3, 33_334), 33_334 + 16_667 + 40_000);
    assert_eq!(
        jb.stats.avg_jitter_ms, baseline,
        "超过上限的样本不应改变 EWMA（基线 {baseline}，实际 {}）",
        jb.stats.avg_jitter_ms
    );
}

#[test]
fn jitter_sample_below_cap_is_kept() {
    let mut jb = JitterBuffer::new(JitterConfig::default());
    jb.push_at(mk_with(1, 0), 0);
    // 10ms 在限幅内，应被采纳
    jb.push_at(mk_with(2, 16_667), 16_667 + 10_000);
    assert!(
        jb.stats.avg_jitter_ms > 0.5,
        "10ms 样本应被采纳，实际 {}",
        jb.stats.avg_jitter_ms
    );
}

#[test]
fn sample_cap_is_15ms() {
    // 钉住常量值：改它必须同时改文档里那张场景表
    assert_eq!(JITTER_SAMPLE_CAP_US, 15_000);
}

#[test]
fn retransmit_frame_does_not_produce_jitter_sample() {
    // 重传帧（is_retransmit=true）完全跳过抖动估计
    let mut jb = JitterBuffer::new(JitterConfig::default());
    jb.push_at(mk_with(1, 0), 0);

    let mut f = mk_with(2, 16_667);
    f.is_retransmit = true;
    jb.push_at(f, 16_667 + 50_000); // 晚到 50ms

    assert_eq!(
        jb.stats.avg_jitter_ms, 0.0,
        "重传帧不应产生抖动样本，实际 {}",
        jb.stats.avg_jitter_ms
    );
}
