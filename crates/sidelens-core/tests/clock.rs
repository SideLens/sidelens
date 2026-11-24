//! 逐条对应 JS 版的 6 个时钟测试

use sidelens_core::{AvSync, RttEstimator, SyncVerdict};

const MS: u64 = 1000;

#[test]
fn median_resists_outliers() {
    let mut r = RttEstimator::new(32);
    for v in [10.0, 11.0, 9.0, 10.0, 999.0] {
        r.add(v);
    }
    assert!((r.median() - 10.0).abs() < 1e-9);
    assert!((r.min() - 9.0).abs() < 1e-9);
}

#[test]
fn empty_estimator_is_safe() {
    let r = RttEstimator::new(32);
    assert_eq!(r.median(), 0.0);
    assert_eq!(r.min(), 0.0);
    assert!(r.is_empty());
}

#[test]
fn video_plays_without_audio() {
    let mut s = AvSync::default();
    assert_eq!(s.judge(123), SyncVerdict::Play);
}

#[test]
fn drops_video_far_behind_audio() {
    let mut s = AvSync::new(40.0, 100.0);
    s.on_audio(10_000 * MS, 20 * MS);
    assert_eq!(s.judge(5_000 * MS), SyncVerdict::Drop);
    assert_eq!(s.dropped_late, 1);
}

#[test]
fn waits_when_video_ahead() {
    let mut s = AvSync::new(40.0, 120.0);
    s.on_audio(1_000 * MS, 20 * MS);
    assert_eq!(s.judge(2_000 * MS), SyncVerdict::Wait);
}

#[test]
fn plays_within_alignment_window() {
    let mut s = AvSync::new(40.0, 120.0);
    s.on_audio(1_000 * MS, 20 * MS);
    // 音频时钟 = 1_020_000us，视频 1_020_000us，漂移 0
    assert_eq!(s.judge(1_020 * MS), SyncVerdict::Play);
    assert_eq!(s.synced, 1);
}
