//! 纯格式校验（跨平台，零平台 API）
//!
//! 这些测试在任何平台都跑 —— 这正是把 `PcmFormat` 从 WASAPI 里拆出来的理由。

use sidelens_audio_out::{
    AudioFormatError, PcmFormat, MAX_CHANNELS, MAX_SAMPLE_RATE, MIN_CHANNELS, MIN_SAMPLE_RATE,
};

#[test]
fn default_is_opus_canonical() {
    let f = PcmFormat::default();
    assert_eq!(f.sample_rate, 48_000);
    assert_eq!(f.channels, 1);
    assert!(f.is_canonical());
}

#[test]
fn checked_accepts_typical_formats() {
    for (rate, ch) in [
        (8_000u32, 1u16),
        (16_000, 1),
        (44_100, 2),
        (48_000, 1),
        (48_000, 2),
        (96_000, 2),
        (192_000, 8),
    ] {
        let f = PcmFormat::checked(rate, ch).unwrap_or_else(|e| panic!("{rate}/{ch} 应合法：{e}"));
        assert_eq!(f.sample_rate, rate);
        assert_eq!(f.channels, ch);
    }
}

#[test]
fn checked_rejects_sample_rate_out_of_range() {
    for rate in [0u32, 1, 7_999, 192_001, 1_000_000] {
        match PcmFormat::checked(rate, 1) {
            Err(AudioFormatError::SampleRateOutOfRange { sample_rate }) => {
                assert_eq!(sample_rate, rate)
            }
            other => panic!("{rate}Hz 应被拒绝，实际 {other:?}"),
        }
    }
}

#[test]
fn checked_rejects_channels_out_of_range() {
    for ch in [0u16, 9, 16, u16::MAX] {
        match PcmFormat::checked(48_000, ch) {
            Err(AudioFormatError::ChannelsOutOfRange { channels }) => assert_eq!(channels, ch),
            other => panic!("{ch} 声道应被拒绝，实际 {other:?}"),
        }
    }
}

#[test]
fn boundaries_are_inclusive() {
    assert!(PcmFormat::checked(MIN_SAMPLE_RATE, MIN_CHANNELS).is_ok());
    assert!(PcmFormat::checked(MAX_SAMPLE_RATE, MAX_CHANNELS).is_ok());
    assert!(PcmFormat::checked(MIN_SAMPLE_RATE - 1, MIN_CHANNELS).is_err());
    assert!(PcmFormat::checked(MAX_SAMPLE_RATE + 1, MAX_CHANNELS).is_err());
    assert!(PcmFormat::checked(MIN_SAMPLE_RATE, MIN_CHANNELS - 1).is_err());
    assert!(PcmFormat::checked(MAX_SAMPLE_RATE, MAX_CHANNELS + 1).is_err());
}

#[test]
fn frame_arithmetic() {
    let mono = PcmFormat::new(48_000, 1);
    assert_eq!(mono.frame_samples(), 1);
    assert_eq!(mono.frame_bytes(), 2);
    assert_eq!(mono.bytes_per_sec(), 96_000);

    let stereo = PcmFormat::new(48_000, 2);
    assert_eq!(stereo.frame_samples(), 2);
    assert_eq!(stereo.frame_bytes(), 4);
    assert_eq!(stereo.bytes_per_sec(), 192_000);
}

#[test]
fn duration_and_frames_roundtrip() {
    let f = PcmFormat::default();
    // 960 采样 @48kHz 单声道 = 20ms
    assert!((f.duration_ms(960) - 20.0).abs() < 1e-9);
    assert_eq!(f.frames_for_ms(20.0), 960);
    // 2.5ms 帧长（Opus 最小）→ 120 采样
    assert_eq!(f.frames_for_ms(2.5), 120);
    // 60ms 帧长（Opus 最大）→ 2880 采样
    assert_eq!(f.frames_for_ms(60.0), 2_880);
    // 0ms 也至少给 1 帧，避免除零
    assert_eq!(f.frames_for_ms(0.0), 1);
}

#[test]
fn duration_handles_zero_format_without_panic() {
    // 故意构造非法格式（绕过 checked），确认不会除零 panic
    let bad = PcmFormat::new(0, 0);
    assert_eq!(bad.duration_ms(960), 0.0);
    assert_eq!(bad.frames_for_ms(20.0), 0);
    assert_eq!(bad.frame_samples(), 1);
}

#[test]
fn display_and_equality() {
    let a = PcmFormat::new(48_000, 1);
    let b = PcmFormat::new(48_000, 1);
    let c = PcmFormat::new(44_100, 2);
    assert_eq!(a, b);
    assert_ne!(a, c);
    assert_eq!(a.to_string(), "48000Hz/1ch/i16");
    assert!(!c.is_canonical());
}

#[test]
fn validate_is_idempotent_and_matches_checked() {
    let f = PcmFormat::new(48_000, 2);
    assert!(f.validate().is_ok());
    assert!(f.validate().is_ok());
    assert_eq!(PcmFormat::checked(48_000, 2).unwrap(), f);
}
