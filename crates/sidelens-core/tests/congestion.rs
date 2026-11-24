//! 拥塞控制的单元与行为测试
//!
//! 这些测试覆盖三类问题：
//!
//! 1. **数学正确性**：最小二乘斜率、时钟偏移消除
//! 2. **行为正确性**：拥塞时降、空闲时升、保持期不升
//! 3. **边界**：样本不足、码率触顶触底、异常输入

use sidelens_core::congestion::{
    CongestionConfig, CongestionController, CongestionSignal, DelayTrend, Feedback, RateState,
    Reason,
};

/// 构造一个固定 RTT 的样本序列
///
/// `base_delay_us` 是「恒定偏移 + 真实单向延迟」，模拟两端时钟不同源。
fn feed_constant(cc: &mut CongestionController, n: usize, base_delay_us: i64, start_us: u64) {
    for i in 0..n {
        let send = start_us + (i as u64) * 1000;
        let arrival = (send as i64 + base_delay_us) as u64;
        cc.add_delay_sample(send, arrival);
    }
}

#[test]
fn constant_delay_has_zero_slope() {
    let mut cc = CongestionController::default();
    feed_constant(&mut cc, 20, 25_000, 1_000_000);
    let s = cc.slope().expect("样本足够应能拟合");
    assert!(s.abs() < 1e-9, "恒定延迟的斜率应为 0，实际 {s}");
}

#[test]
fn clock_offset_does_not_affect_slope() {
    // 两端时钟差 5 秒（5000ms），但延迟恒定 —— 斜率仍应为 0
    let mut a = CongestionController::default();
    feed_constant(&mut a, 20, 5_000_000, 1_000_000);
    let mut b = CongestionController::default();
    feed_constant(&mut b, 20, 25_000, 1_000_000);

    let sa = a.slope().unwrap();
    let sb = b.slope().unwrap();
    assert!(
        sa.abs() < 1e-9 && sb.abs() < 1e-9,
        "偏移不应影响斜率：{sa} vs {sb}"
    );
}

#[test]
fn growing_queue_produces_positive_slope() {
    let mut cc = CongestionController::default();
    // 每 1ms 发送一个包，延迟每包增加 0.5ms —— 即斜率 0.5
    for i in 0..20u64 {
        let send = 1_000_000 + i * 1000;
        let delay_us = 20_000 + (i as i64) * 500;
        cc.add_delay_sample(send, (send as i64 + delay_us) as u64);
    }
    let s = cc.slope().expect("应能拟合");
    assert!((s - 0.5).abs() < 0.02, "斜率应约 0.5，实际 {s}");
}

#[test]
fn slope_needs_min_samples() {
    let mut cc = CongestionController::default();
    feed_constant(&mut cc, 3, 25_000, 1_000_000);
    assert!(cc.slope().is_none(), "样本不足时不应给出斜率");
}

#[test]
fn trend_ignores_backwards_timestamps() {
    let mut t = DelayTrend::new(10);
    t.add(1_000_000, 1_020_000);
    // 发送时刻倒退（乱序或时钟跳变）应被丢弃
    t.add(999_000, 1_019_000);
    assert_eq!(t.len(), 1, "倒退的时间戳不应计入样本");
}

#[test]
fn trend_evicts_old_samples() {
    let mut t = DelayTrend::new(4);
    for i in 0..10u64 {
        t.add(1_000_000 + i * 1000, 1_020_000 + i * 1000);
    }
    assert_eq!(t.len(), 4, "窗口容量为 4");
}

#[test]
fn delay_growth_triggers_decrease() {
    let mut cc = CongestionController::default();
    let start = cc.target_bps();

    // 明显增长的排队延迟：每包 +1ms，斜率 1.0 远超阈值 0.015
    for i in 0..20u64 {
        let send = 1_000_000 + i * 1000;
        let delay_us = 20_000 + (i as i64) * 1000;
        cc.add_delay_sample(send, (send as i64 + delay_us) as u64);
    }

    let d = cc.update(&Feedback::default(), 2_000_000);
    assert_eq!(d.state, RateState::Decrease);
    assert_eq!(d.reason, Reason::DelayGradient);
    assert!(
        d.target_bps < start,
        "应降码率：{} → {}",
        start,
        d.target_bps
    );
}

#[test]
fn packet_loss_triggers_decrease() {
    let mut cc = CongestionController::default();
    // 延迟恒定（无延迟信号），但丢包 15%
    feed_constant(&mut cc, 20, 25_000, 1_000_000);
    let fb = Feedback {
        loss_ratio: 0.15,
        rtt_ms: 30.0,
        fresh: true,
    };
    let start = cc.target_bps();
    let d = cc.update(&fb, 2_000_000);
    assert_eq!(d.state, RateState::Decrease);
    assert_eq!(d.reason, Reason::Loss);
    assert!(d.target_bps < start);
}

#[test]
fn both_signals_decrease_harder_than_one() {
    // 只有丢包
    let mut a = CongestionController::default();
    feed_constant(&mut a, 20, 25_000, 1_000_000);
    let da = a.update(
        &Feedback {
            loss_ratio: 0.15,
            rtt_ms: 30.0,
            fresh: true,
        },
        2_000_000,
    );

    // 延迟 + 丢包
    let mut b = CongestionController::default();
    for i in 0..20u64 {
        let send = 1_000_000 + i * 1000;
        let delay_us = 20_000 + (i as i64) * 1000;
        b.add_delay_sample(send, (send as i64 + delay_us) as u64);
    }
    let db = b.update(
        &Feedback {
            loss_ratio: 0.15,
            rtt_ms: 30.0,
            fresh: true,
        },
        2_000_000,
    );

    assert!(
        db.target_bps < da.target_bps,
        "两个信号叠加应降更多：只有丢包 {} vs 两者 {} ",
        da.target_bps,
        db.target_bps
    );
    assert_eq!(db.reason, Reason::DelayAndLoss);
}

#[test]
fn high_loss_decreases_even_harder() {
    let mut a = CongestionController::default();
    feed_constant(&mut a, 20, 25_000, 1_000_000);
    let da = a.update(
        &Feedback {
            loss_ratio: 0.12,
            rtt_ms: 30.0,
            fresh: true,
        },
        2_000_000,
    );

    let mut b = CongestionController::default();
    feed_constant(&mut b, 20, 25_000, 1_000_000);
    let db = b.update(
        &Feedback {
            loss_ratio: 0.25,
            rtt_ms: 30.0,
            fresh: true,
        },
        2_000_000,
    );

    assert!(
        db.target_bps < da.target_bps,
        "高丢包应降更多：{} vs {}",
        da.target_bps,
        db.target_bps
    );
}

#[test]
fn clean_network_increases_gradually() {
    let mut cc = CongestionController::default();
    feed_constant(&mut cc, 20, 25_000, 1_000_000);
    let start = cc.target_bps();

    let d = cc.update(&Feedback::default(), 2_000_000);
    assert_eq!(d.state, RateState::Increase);
    assert_eq!(d.reason, Reason::Underuse);
    assert!(d.target_bps > start, "空闲网络应升码率");
    // 单次增量受 max_increase_step_bps 限制
    assert!(
        d.target_bps - start <= cc.config().max_increase_step_bps + 1.0,
        "单次增量不应超过上限"
    );
}

#[test]
fn hold_period_blocks_increase_after_decrease() {
    let mut cc = CongestionController::default();
    feed_constant(&mut cc, 20, 25_000, 1_000_000);

    // 先触发一次降码率
    let t0 = 2_000_000u64;
    let d1 = cc.update(
        &Feedback {
            loss_ratio: 0.15,
            rtt_ms: 30.0,
            fresh: true,
        },
        t0,
    );
    assert_eq!(d1.state, RateState::Decrease);
    let after_decrease = d1.target_bps;

    // 保持期内（默认 300ms）网络已恢复，但不允许升
    let d2 = cc.update(&Feedback::default(), t0 + 100_000);
    assert_eq!(d2.state, RateState::Hold);
    assert_eq!(d2.reason, Reason::Holding);
    assert_eq!(d2.target_bps, after_decrease, "保持期内码率不变");

    // 保持期结束后可以升
    let d3 = cc.update(&Feedback::default(), t0 + 400_000);
    assert_eq!(d3.state, RateState::Increase);
    assert!(d3.target_bps > after_decrease);
}

#[test]
fn bitrate_never_below_floor() {
    let cfg = CongestionConfig::default();
    let mut cc = CongestionController::new(cfg);
    feed_constant(&mut cc, 20, 25_000, 1_000_000);

    // 连续降 100 次
    let mut t = 2_000_000u64;
    for _ in 0..100 {
        t += 10_000;
        cc.update(
            &Feedback {
                loss_ratio: 0.5,
                rtt_ms: 30.0,
                fresh: true,
            },
            t,
        );
    }
    assert!(
        cc.target_bps() >= cfg.min_bitrate_bps - 1.0,
        "码率不应低于下限 {}，实际 {}",
        cfg.min_bitrate_bps,
        cc.target_bps()
    );
}

#[test]
fn bitrate_never_above_ceiling() {
    let cfg = CongestionConfig::default();
    let mut cc = CongestionController::new(cfg);
    feed_constant(&mut cc, 20, 25_000, 1_000_000);

    let mut t = 2_000_000u64;
    for _ in 0..1000 {
        t += 50_000;
        cc.update(&Feedback::default(), t);
    }
    assert!(
        cc.target_bps() <= cfg.max_bitrate_bps + 1.0,
        "码率不应超过上限 {}，实际 {}",
        cfg.max_bitrate_bps,
        cc.target_bps()
    );
    assert_eq!(cc.state(), RateState::Hold, "触顶后应停在 Hold");
}

#[test]
fn resolution_hint_appears_near_floor() {
    let cfg = CongestionConfig::default();
    let mut cc = CongestionController::new(cfg);

    // 起始码率远高于下限，不应给建议
    assert!(cc.resolution_hint().is_none());

    // 压到下限
    feed_constant(&mut cc, 20, 25_000, 1_000_000);
    let mut t = 2_000_000u64;
    for _ in 0..100 {
        t += 10_000;
        cc.update(
            &Feedback {
                loss_ratio: 0.5,
                rtt_ms: 30.0,
                fresh: true,
            },
            t,
        );
    }

    let hint = cc.resolution_hint().expect("触底时应建议降分辨率");
    assert!(hint.0 < 1280, "建议的分辨率应低于 720p，实际 {hint:?}");
}

#[test]
fn signal_classification() {
    let mut cc = CongestionController::default();
    feed_constant(&mut cc, 20, 25_000, 1_000_000);

    // 无拥塞
    assert_eq!(cc.signal(&Feedback::default()), CongestionSignal::None);
    // 只有丢包
    assert_eq!(
        cc.signal(&Feedback {
            loss_ratio: 0.15,
            rtt_ms: 0.0,
            fresh: true,
        }),
        CongestionSignal::PacketLoss
    );
}

#[test]
fn reset_restores_start_state() {
    let cfg = CongestionConfig::default();
    let mut cc = CongestionController::new(cfg);
    feed_constant(&mut cc, 20, 25_000, 1_000_000);
    cc.update(
        &Feedback {
            loss_ratio: 0.5,
            rtt_ms: 0.0,
            fresh: true,
        },
        2_000_000,
    );
    assert!(cc.target_bps() < cfg.start_bitrate_bps);

    cc.reset();
    assert_eq!(cc.target_bps(), cfg.start_bitrate_bps);
    assert_eq!(cc.state(), RateState::Hold);
    assert!(cc.slope().is_none(), "reset 后样本应清空");
}

#[test]
fn counters_track_decisions() {
    let mut cc = CongestionController::default();
    feed_constant(&mut cc, 20, 25_000, 1_000_000);
    cc.update(
        &Feedback {
            loss_ratio: 0.5,
            rtt_ms: 0.0,
            fresh: true,
        },
        2_000_000,
    );
    assert_eq!(cc.decisions, 1);
    assert_eq!(cc.decreases, 1);
    assert_eq!(cc.increases, 0);
}

#[test]
fn first_update_uses_default_dt() {
    // 第一次 update 没有上次时刻，应能正常升而不是 panic
    let mut cc = CongestionController::default();
    feed_constant(&mut cc, 20, 25_000, 1_000_000);
    let start = cc.target_bps();
    let d = cc.update(&Feedback::default(), 1_000_000);
    assert!(d.target_bps > start);
}

#[test]
fn simulated_congestion_event_recovers() {
    // 端到端模拟：正常 → 拥塞（延迟涨） → 恢复 → 再次升
    let mut cc = CongestionController::default();
    let mut t = 1_000_000u64;

    // 阶段 1：网络正常，码率上升
    for _ in 0..50 {
        let send = t;
        cc.add_delay_sample(send, send + 20_000);
        cc.update(&Feedback::default(), t);
        t += 20_000;
    }
    let healthy = cc.target_bps();
    assert!(healthy > CongestionConfig::default().start_bitrate_bps);

    // 阶段 2：队列开始积压，延迟每 20ms 增 10ms
    let mut extra = 0i64;
    for _ in 0..10 {
        extra += 10_000;
        cc.add_delay_sample(t, (t as i64 + 20_000 + extra) as u64);
        cc.update(
            &Feedback {
                loss_ratio: 0.0,
                rtt_ms: 100.0,
                fresh: true,
            },
            t,
        );
        t += 20_000;
    }
    assert!(
        cc.target_bps() < healthy,
        "拥塞后码率应下降：{healthy} → {}",
        cc.target_bps()
    );

    // 阶段 3：恢复（延迟回到恒定），保持期后应重新上升
    let after_congestion = cc.target_bps();
    let mut steps_to_recover = 0;
    for i in 0..40 {
        cc.add_delay_sample(t, t + 20_000);
        cc.update(&Feedback::default(), t);
        t += 20_000;
        if steps_to_recover == 0 && cc.target_bps() > after_congestion + 1.0 {
            steps_to_recover = i + 1;
        }
    }
    assert!(
        cc.target_bps() > after_congestion,
        "网络恢复后码率应回升：{after_congestion} → {}",
        cc.target_bps()
    );
    // 关键回归：恢复必须快。
    //
    // 实测过的失败版本：拥塞后窗口残留样本导致连续降 4 次触底，
    // 再用纯百分比增长爬升 —— 26 秒才回到 206kbps。
    // 用户在这段时间里看到的一直是糊的。
    assert!(
        steps_to_recover <= 15,
        "恢复应在 15 步（约 300ms）内开始，实际 {steps_to_recover} 步"
    );
    assert!(
        cc.target_bps() >= healthy * 0.9,
        "恢复后应接近拥塞前的码率 {healthy}，实际 {}",
        cc.target_bps()
    );
}
// ============================================================================
// 振荡陷阱的回归测试
// ============================================================================
//
// 下面两个测试对应本轮实测发现的两个真实缺陷。它们不是「顺手加的覆盖」，
// 而是**先看到错误行为、再修、再锁住**。

#[test]
fn congestion_window_is_cleared_after_decrease() {
    // 缺陷 1：窗口里残留拥塞期样本，网络恢复后斜率仍为正，继续降直到触底。
    let mut cc = CongestionController::default();
    let mut t = 1_000_000u64;

    // 正常阶段
    for _ in 0..50 {
        cc.add_delay_sample(t, t + 20_000);
        cc.update(&Feedback::default(), t);
        t += 20_000;
    }
    let healthy = cc.target_bps();

    // 拥塞：延迟持续增长
    let mut extra = 0i64;
    for _ in 0..10 {
        extra += 10_000;
        cc.add_delay_sample(t, (t as i64 + 20_000 + extra) as u64);
        cc.update(
            &Feedback {
                loss_ratio: 0.0,
                rtt_ms: 100.0,
                fresh: true,
            },
            t,
        );
        t += 20_000;
    }
    let after_one_decrease = cc.target_bps();

    // 关键断言：一次拥塞事件只应降一次，不应连环降。
    // 修复前这里会连续降 4-5 次（1624k → 376k → 200k）。
    assert!(
        after_one_decrease > healthy * 0.5,
        "一次拥塞不应降超过一半：{healthy} → {after_one_decrease}"
    );
    assert!(
        after_one_decrease > cc.config().min_bitrate_bps * 1.5,
        "不应直接触底：{after_one_decrease}"
    );
}

#[test]
fn recovery_from_floor_is_fast() {
    // 缺陷 2：纯百分比增长在低码率时太慢（200k × 8% = 16k/秒，回到 1.5M 要 80 秒）。
    let cfg = CongestionConfig::default();
    let mut cc = CongestionController::new(cfg);
    let mut t = 1_000_000u64;

    // 正常阶段爬到较高码率
    for _ in 0..50 {
        cc.add_delay_sample(t, t + 20_000);
        cc.update(&Feedback::default(), t);
        t += 20_000;
    }
    let healthy = cc.target_bps();

    // 一次拥塞
    let mut extra = 0i64;
    for _ in 0..10 {
        extra += 10_000;
        cc.add_delay_sample(t, (t as i64 + 20_000 + extra) as u64);
        cc.update(
            &Feedback {
                loss_ratio: 0.0,
                rtt_ms: 100.0,
                fresh: true,
            },
            t,
        );
        t += 20_000;
    }

    // 恢复 40 步（800ms）
    for _ in 0..40 {
        cc.add_delay_sample(t, t + 20_000);
        cc.update(&Feedback::default(), t);
        t += 20_000;
    }

    // 修复前 40 步只能从 200k 爬到 206k（+3%）。
    // 修复后应该基本回到拥塞前水平。
    assert!(
        cc.target_bps() >= healthy * 0.85,
        "800ms 内应恢复到拥塞前 85% 以上：{healthy} → {}",
        cc.target_bps()
    );
}

#[test]
fn pre_congestion_bitrate_is_remembered() {
    let mut cc = CongestionController::default();
    let mut t = 1_000_000u64;
    for _ in 0..50 {
        cc.add_delay_sample(t, t + 20_000);
        cc.update(&Feedback::default(), t);
        t += 20_000;
    }
    let healthy = cc.target_bps();
    assert_eq!(cc.pre_congestion_bps(), 0.0, "还没拥塞过");

    cc.update(
        &Feedback {
            loss_ratio: 0.5,
            rtt_ms: 30.0,
            fresh: true,
        },
        t,
    );
    assert!(
        (cc.pre_congestion_bps() - healthy).abs() < 1.0,
        "应记住拥塞前码率 {healthy}，实际 {}",
        cc.pre_congestion_bps()
    );
}
// ============================================================================
// 时间窗口淘汰（修复结构性缺陷）
// ============================================================================
//
// 只按样本数淘汰时，窗口时间跨度 = 样本数 × 帧间隔。低帧率下会拉到 2 秒，
// 拥塞判定严重滞后 —— 而低帧率往往正是弱网时，形成正反馈。

#[test]
fn trend_window_evicts_by_time() {
    // 容量 100（足够大），但时间窗口只有 100ms
    let mut t = DelayTrend::with_window(100, 100);
    // 每 20ms 一个样本，跨度 200ms —— 应只保留最后约 5 个
    for i in 0..11u64 {
        t.add(1_000_000 + i * 20_000, 1_020_000 + i * 20_000);
    }
    assert!(
        t.len() <= 6,
        "100ms 窗口内每 20ms 一个样本，最多 6 个，实际 {}",
        t.len()
    );
    assert!(t.len() >= 5, "但也不该少于 5 个，实际 {}", t.len());
}

#[test]
fn trend_without_time_window_keeps_by_capacity() {
    // window_ms = 0 表示不限时间，只按容量
    let mut t = DelayTrend::with_window(5, 0);
    for i in 0..20u64 {
        t.add(1_000_000 + i * 100_000, 1_020_000 + i * 100_000);
    }
    assert_eq!(t.len(), 5, "应保留最后 5 个样本");
}

#[test]
fn low_framerate_does_not_delay_congestion_detection() {
    // 低帧率（10fps，每 100ms 一帧）+ 拥塞：
    // 时间窗口 400ms 应保证 4-5 个样本就够判定，而不是等到 20 个（2 秒）。
    let mut cc = CongestionController::new(CongestionConfig {
        trend_min_samples: 4,
        ..Default::default()
    });
    let mut t = 1_000_000u64;

    // 每 100ms 一帧，延迟每帧涨 50ms（明显积压）
    for i in 0..6u64 {
        let send = t;
        let delay_us = 20_000 + (i as i64) * 50_000;
        cc.add_delay_sample(send, (send as i64 + delay_us) as u64);
        t += 100_000;
    }

    let slope = cc.slope().expect("6 个样本应够");
    assert!(
        slope > 0.3,
        "10fps 下延迟每帧涨 50ms，斜率应约 0.5，实际 {slope}"
    );

    let d = cc.update(
        &Feedback {
            loss_ratio: 0.0,
            rtt_ms: 200.0,
            fresh: true,
        },
        t,
    );
    assert_eq!(d.state, RateState::Decrease, "低帧率下也应立刻判出拥塞");
}
