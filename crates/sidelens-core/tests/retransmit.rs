//! 重传缓存测试
//!
//! 核心验证点：关键帧永不淘汰（否则接收端永远恢复不了）、过期帧不重传。

use sidelens_core::retransmit::{RetransmitCache, RetransmitConfig, StoreSpec};
use sidelens_proto::{FrameType, FLAG_KEYFRAME};

const MS: u64 = 1000;

fn small_config() -> RetransmitConfig {
    RetransmitConfig {
        max_frames: 5,
        max_bytes: 10_000,
        max_age_ms: 100.0,
        max_retransmits: 2,
    }
}

fn frame(size: usize) -> Vec<u8> {
    vec![0u8; size]
}

#[test]
fn store_and_retrieve() {
    let mut c = RetransmitCache::new(small_config());
    c.store(StoreSpec::video(1, 1000, frame(100)), 0);

    assert!(c.contains(1));
    assert_eq!(c.len(), 1);
    assert_eq!(c.bytes(), 100);

    let f = c.take_for_retransmit(1, 10 * MS).expect("应能取到");
    assert_eq!(f.seq, 1);
    assert_eq!(f.bytes.len(), 100);
}

#[test]
fn expired_frame_is_refused() {
    let mut c = RetransmitCache::new(small_config());
    c.store(StoreSpec::video(1, 1000, frame(100)), 0);

    // max_age_ms = 100，等到 150ms
    assert!(c.take_for_retransmit(1, 150 * MS).is_none());
    assert_eq!(c.stats.refused_expired, 1);
}

#[test]
fn retransmit_count_is_limited() {
    let mut c = RetransmitCache::new(small_config());
    c.store(StoreSpec::video(1, 1000, frame(100)), 0);

    // max_retransmits = 2
    assert!(c.take_for_retransmit(1, MS).is_some());
    assert!(c.take_for_retransmit(1, 2 * MS).is_some());
    assert!(
        c.take_for_retransmit(1, 3 * MS).is_none(),
        "超过重传次数上限应拒绝"
    );
    assert_eq!(c.stats.refused_over_limit, 1);
}

#[test]
fn keyframe_is_never_evicted() {
    let mut c = RetransmitCache::new(small_config());

    // 先存一个关键帧
    c.store(
        StoreSpec::with_flags(FrameType::Video, FLAG_KEYFRAME, 0, 1, 1000, frame(100)),
        0,
    );
    // 再塞 20 个普通帧，远超 max_frames = 5
    for seq in 2..=21 {
        c.store(StoreSpec::video(seq, 1000, frame(100)), 0);
    }

    assert!(c.contains(1), "关键帧必须保留，否则接收端永远无法恢复");
    assert!(c.len() <= 6, "普通帧应被淘汰，实际 {}", c.len());
    assert!(c.stats.evicted_by_count > 0);
}

#[test]
fn byte_limit_triggers_eviction() {
    let cfg = RetransmitConfig {
        max_frames: 1000,
        max_bytes: 500,
        ..small_config()
    };
    let mut c = RetransmitCache::new(cfg);
    for seq in 1..=10 {
        c.store(StoreSpec::video(seq, 1000, frame(100)), 0);
    }
    assert!(c.bytes() <= 500, "字节上限应生效，实际 {}", c.bytes());
}

#[test]
fn age_eviction_happens_on_next_store() {
    let cfg = RetransmitConfig {
        max_age_ms: 10.0,
        ..small_config()
    };
    let mut c = RetransmitCache::new(cfg);
    c.store(StoreSpec::video(1, 1000, frame(100)), 0);
    assert_eq!(c.len(), 1);

    // 50ms 后再存一帧，触发淘汰扫描
    c.store(StoreSpec::video(2, 1000, frame(100)), 50 * MS);
    assert!(!c.contains(1), "过期的 seq 1 应被淘汰");
    assert!(c.contains(2));
    assert_eq!(c.stats.evicted_by_age, 1);
}

#[test]
fn missing_seq_returns_none() {
    let mut c = RetransmitCache::new(small_config());
    assert!(c.take_for_retransmit(999, 0).is_none());
}

#[test]
fn stats_bytes_served_accumulates() {
    let mut c = RetransmitCache::new(small_config());
    c.store(StoreSpec::video(1, 1000, frame(200)), 0);
    c.take_for_retransmit(1, MS);
    c.take_for_retransmit(1, 2 * MS);
    assert_eq!(c.stats.served, 2);
    assert_eq!(c.stats.bytes_served, 400);
}

#[test]
fn clear_empties_everything() {
    let mut c = RetransmitCache::new(small_config());
    c.store(StoreSpec::video(1, 1000, frame(100)), 0);
    c.clear();
    assert!(c.is_empty());
    assert_eq!(c.bytes(), 0);
}
