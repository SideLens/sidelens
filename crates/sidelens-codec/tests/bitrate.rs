//! 码率统计测试

use sidelens_codec::bitrate::{BitrateConfig, BitrateTracker, IntervalStats};

/// 60fps 的帧间隔（微秒）
const FRAME_US: u64 = 16_667;

fn tracker() -> BitrateTracker {
    BitrateTracker::new(
        BitrateConfig {
            window_ms: 1000,
            smoothing: 0.5,
        },
        32, // 协议头
    )
}

#[test]
fn empty_tracker_reports_zero() {
    let t = tracker();
    assert_eq!(t.bitrate_bps(), 0.0);
    assert_eq!(t.fps(), 0.0);
    assert_eq!(t.avg_frame_bytes(), 0.0);
    assert_eq!(t.window_len(), 0);
}

#[test]
fn single_frame_is_not_enough_for_rate() {
    let mut t = tracker();
    t.record(8000, 0, true);
    // 一帧算不出速率（需要时间跨度）
    assert_eq!(t.bitrate_bps(), 0.0);
    assert_eq!(t.total_frames(), 1);
}

#[test]
fn steady_bitrate_is_computed() {
    let mut t = tracker();
    // 60 帧，每帧 1000 字节，间隔 16.667ms
    for i in 0..60u64 {
        t.record(1000, i * FRAME_US, i % 60 == 0);
    }

    // 1000 字节 * 8 bit * 60 fps = 480,000 bps
    let bps = t.bitrate_bps();
    assert!(
        (bps - 480_000.0).abs() < 10_000.0,
        "码率应接近 480kbps，实际 {bps}"
    );
}

#[test]
fn fps_is_accurate() {
    let mut t = tracker();
    for i in 0..61u64 {
        t.record(1000, i * FRAME_US, false);
    }
    let fps = t.fps();
    assert!((fps - 60.0).abs() < 1.0, "帧率应接近 60，实际 {fps}");
}

#[test]
fn overhead_increases_bitrate() {
    let mut t = tracker();
    for i in 0..60u64 {
        t.record(1000, i * FRAME_US, false);
    }
    let plain = t.bitrate_bps();
    let with_overhead = t.bitrate_with_overhead_bps();
    assert!(
        with_overhead > plain,
        "含头码率应大于纯数据码率：{with_overhead} vs {plain}"
    );
    // 1000 字节数据 + 32 字节头 → 约 3.2% 增加
    let ratio = with_overhead / plain;
    assert!((ratio - 1.032).abs() < 0.01, "开销比例异常：{ratio}");
}

#[test]
fn old_frames_are_evicted() {
    let mut t = tracker();
    for i in 0..60u64 {
        t.record(1000, i * FRAME_US, false);
    }
    let before = t.window_len();
    // 跳到 3 秒后
    t.record(1000, 3_000_000, false);
    assert!(
        t.window_len() < before,
        "窗口外的记录应被清理：{} → {}",
        before,
        t.window_len()
    );
}

#[test]
fn keyframe_ratio_is_tracked() {
    let mut t = tracker();
    // 每 30 帧一个关键帧
    for i in 0..60u64 {
        t.record(1000, i * FRAME_US, i % 30 == 0);
    }
    let ratio = t.keyframe_ratio();
    assert!((ratio - 2.0 / 60.0).abs() < 0.01, "关键帧占比异常：{ratio}");
    assert_eq!(t.total_keyframes(), 2);
}

#[test]
fn smoothing_dampens_spike() {
    // 平滑系数低 → 单个大 I 帧不应把读数拉爆
    let mut smooth = BitrateTracker::new(
        BitrateConfig {
            window_ms: 1000,
            smoothing: 0.05,
        },
        32,
    );
    // 先建立基线
    for i in 0..60u64 {
        smooth.record(1000, i * FRAME_US, false);
    }
    let baseline = smooth.bitrate_bps();

    // 突然来一个巨大的 I 帧
    smooth.record(500_000, 60 * FRAME_US, true);
    let after = smooth.bitrate_bps();

    // 平滑后不应暴涨到 10 倍
    assert!(after < baseline * 3.0, "平滑不足：{baseline} → {after}");
}

#[test]
fn suggest_target_on_overbudget() {
    let mut t = tracker();
    for i in 0..60u64 {
        t.record(2000, i * FRAME_US, false); // 约 960kbps
    }
    // 预算 500kbps → 应建议降
    let suggestion = t.suggest_target(500_000.0);
    assert!(suggestion.is_some(), "超预算应给建议");
    assert!(suggestion.unwrap() < 500_000.0);
}

#[test]
fn suggest_target_on_underbudget() {
    let mut t = tracker();
    for i in 0..60u64 {
        t.record(500, i * FRAME_US, false); // 约 240kbps
    }
    // 预算 2000kbps → 远低于，应建议升
    let suggestion = t.suggest_target(2_000_000.0);
    assert!(suggestion.is_some(), "远低于预算应给建议");
    assert!(suggestion.unwrap() > 240_000.0);
}

#[test]
fn no_suggestion_within_budget_range() {
    let mut t = tracker();
    for i in 0..60u64 {
        t.record(1000, i * FRAME_US, false); // 约 480kbps
    }
    // 预算 500kbps → 在合理范围内，不调整
    assert!(t.suggest_target(500_000.0).is_none());
}

#[test]
fn interval_stats_percentiles() {
    let mut s = IntervalStats::new(100);
    for i in 1..=100u64 {
        s.record_interval(i * 1000); // 1ms 到 100ms
    }
    let p50 = s.percentile(0.5);
    assert!(
        p50.as_millis() >= 45 && p50.as_millis() <= 55,
        "p50 应接近 50ms，实际 {p50:?}"
    );
}

#[test]
fn jitter_ratio_detects_instability() {
    let mut stable = IntervalStats::new(100);
    for _ in 0..50 {
        stable.record_interval(16_667);
    }
    assert!(stable.jitter_ratio() < 0.01, "稳定间隔的变异系数应接近 0");

    let mut unstable = IntervalStats::new(100);
    for i in 0..50 {
        unstable.record_interval(if i % 2 == 0 { 5_000 } else { 30_000 });
    }
    assert!(
        unstable.jitter_ratio() > 0.3,
        "波动间隔的变异系数应明显更大，实际 {}",
        unstable.jitter_ratio()
    );
}

#[test]
fn empty_interval_stats_are_safe() {
    let s = IntervalStats::new(10);
    assert_eq!(s.percentile(0.5).as_micros(), 0);
    assert_eq!(s.jitter_ratio(), 0.0);
}
