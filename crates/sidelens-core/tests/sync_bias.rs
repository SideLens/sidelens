//! 系统偏差校准
//!
//! 验证"用户态收包延迟"造成的固定偏移能被显式扣除。

use sidelens_core::sync::{ClockSync, SyncSample};

#[test]
fn bias_correction_shifts_offset() {
    let mut sync = ClockSync::new();
    sync.add_sample(SyncSample {
        rtt_us: 5_000,
        offset_us: -5_500,
    });

    assert_eq!(sync.offset_us(), Some(-5_500));
    sync.apply_bias_correction(5_500);
    assert_eq!(sync.offset_us(), Some(0), "校准后应恰好归零");
}

#[test]
fn bias_correction_on_unsynced_is_noop() {
    let mut sync = ClockSync::new();
    sync.apply_bias_correction(1000);
    assert!(sync.offset_us().is_none(), "未同步时校准应无效果");
}

#[test]
fn bias_correction_affects_latency_estimate() {
    let mut sync = ClockSync::new();
    sync.add_sample(SyncSample {
        rtt_us: 5_000,
        offset_us: -5_000,
    });

    // 对端在 10_000_000 发出，本端 10_020_000 收到
    let peer = 10_000_000u64;
    let local = 10_020_000u64;

    let before = sync.one_way_latency_us(peer, local).unwrap();
    assert_eq!(before, 25_000, "未校准时延迟被高估 5ms");

    sync.apply_bias_correction(5_000);
    let after = sync.one_way_latency_us(peer, local).unwrap();
    assert_eq!(after, 20_000, "校准后是真实延迟");
}
