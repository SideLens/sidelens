//! 媒体源状态机测试
//!
//! 这些规则是 Windows Media Foundation 的硬性契约。
//! 写错任何一条，消费端会在某个远离原因的地方失败（典型是"设备能选但黑屏"）。

use sidelens_vcam::source::{FrameRate, Operation, Resolution, SourceConfig, SourceState};
use sidelens_vcam::TransitionError;

#[test]
fn initial_state_is_stopped() {
    assert_eq!(SourceState::default(), SourceState::Stopped);
}

#[test]
fn stopped_can_start() {
    assert_eq!(SourceState::Stopped.start(), Ok(SourceState::Started));
}

#[test]
fn stopped_cannot_stop() {
    let err = SourceState::Stopped.stop().unwrap_err();
    assert_eq!(
        err,
        TransitionError::InvalidState {
            from: SourceState::Stopped,
            op: Operation::Stop
        }
    );
}

#[test]
fn stopped_cannot_pause() {
    assert!(SourceState::Stopped.pause().is_err());
}

#[test]
fn started_cannot_start_again() {
    let err = SourceState::Started.start().unwrap_err();
    assert_eq!(
        err,
        TransitionError::InvalidState {
            from: SourceState::Started,
            op: Operation::Start
        }
    );
}

#[test]
fn started_can_stop_and_pause() {
    assert_eq!(SourceState::Started.stop(), Ok(SourceState::Stopped));
    assert_eq!(SourceState::Started.pause(), Ok(SourceState::Paused));
}

#[test]
fn paused_can_start_and_stop() {
    assert_eq!(SourceState::Paused.start(), Ok(SourceState::Started));
    assert_eq!(SourceState::Paused.stop(), Ok(SourceState::Stopped));
}

#[test]
fn paused_cannot_pause_again() {
    assert!(SourceState::Paused.pause().is_err());
}

#[test]
fn shutdown_is_terminal() {
    for state in [
        SourceState::Stopped,
        SourceState::Started,
        SourceState::Paused,
    ] {
        assert_eq!(state.shutdown(), Ok(SourceState::Shutdown));
    }
    assert!(SourceState::Shutdown.is_terminal());
}

#[test]
fn shutdown_state_rejects_everything() {
    for op in [Operation::Start, Operation::Stop, Operation::Pause] {
        let result = match op {
            Operation::Start => SourceState::Shutdown.start(),
            Operation::Stop => SourceState::Shutdown.stop(),
            Operation::Pause => SourceState::Shutdown.pause(),
            _ => unreachable!(),
        };
        assert_eq!(result, Err(TransitionError::Shutdown), "{op:?} 应被拒绝");
    }
}

#[test]
fn shutdown_is_idempotent_error() {
    assert_eq!(
        SourceState::Shutdown.shutdown(),
        Err(TransitionError::Shutdown)
    );
}

#[test]
fn only_started_can_deliver_frames() {
    assert!(SourceState::Started.can_deliver_frames());
    assert!(!SourceState::Stopped.can_deliver_frames());
    assert!(
        !SourceState::Paused.can_deliver_frames(),
        "暂停时不应投递帧"
    );
    assert!(!SourceState::Shutdown.can_deliver_frames());
}

#[test]
fn full_lifecycle_is_walkable() {
    let s = SourceState::Stopped;
    let s = s.start().unwrap();
    let s = s.pause().unwrap();
    let s = s.start().unwrap();
    let s = s.stop().unwrap();
    let s = s.shutdown().unwrap();
    assert!(s.is_terminal());
}

#[test]
fn resolution_support_rules() {
    assert!(Resolution::new(1280, 720).is_supported());
    assert!(Resolution::new(1920, 1080).is_supported());
    assert!(Resolution::new(3840, 2160).is_supported());

    assert!(!Resolution::new(100, 100).is_supported(), "太小");
    assert!(!Resolution::new(4096, 2160).is_supported(), "太宽");
    assert!(!Resolution::new(1281, 720).is_supported(), "奇数宽");
    assert!(!Resolution::new(1280, 721).is_supported(), "奇数高");
}

#[test]
fn buffer_sizes_are_correct() {
    let r = Resolution::new(1280, 720);
    // NV12: w*h*1.5 = 1280*720*1.5 = 1382400
    assert_eq!(r.nv12_size(), 1280 * 720 * 3 / 2);
    // RGB32: w*h*4
    assert_eq!(r.rgb32_size(), 1280 * 720 * 4);
}

#[test]
fn frame_rate_conversion() {
    assert!((FrameRate::new(30, 1).fps() - 30.0).abs() < 1e-9);
    assert!((FrameRate::new(30000, 1001).fps() - 29.97).abs() < 0.01);
    assert_eq!(FrameRate::new(1, 0).fps(), 0.0, "除零应返回 0 而不是 panic");
}

#[test]
fn frame_rate_support_range() {
    assert!(FrameRate::new(30, 1).is_supported());
    assert!(FrameRate::new(60, 1).is_supported());
    assert!(FrameRate::new(120, 1).is_supported());
    assert!(!FrameRate::new(0, 1).is_supported(), "0fps 不合理");
    assert!(!FrameRate::new(240, 1).is_supported(), "240fps 超出范围");
}

#[test]
fn config_validates_and_computes_interval() {
    let cfg = SourceConfig::default();
    assert!(cfg.validate().is_ok());
    // 30fps → 33333us
    assert_eq!(cfg.frame_interval_us(), 33_333);

    let bad = SourceConfig {
        resolution: Resolution::new(1, 1),
        ..Default::default()
    };
    assert!(bad.validate().is_err());
}

#[test]
fn sixty_fps_interval() {
    let cfg = SourceConfig {
        frame_rate: FrameRate::new(60, 1),
        ..Default::default()
    };
    assert_eq!(cfg.frame_interval_us(), 16_666);
}
