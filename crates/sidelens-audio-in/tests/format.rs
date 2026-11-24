//! 纯逻辑测试：格式校验、f32 → i16 转换、降混、声道映射
//!
//! 这些测试在**所有平台**都能跑 —— 这正是把转换逻辑从 `wasapi.rs` 里
//! 拆出来的理由。下面每一条都对应一个真实会踩的坑，
//! 注释里写清「错了会怎样」，而不是复述断言。

use sidelens_audio_in::{
    convert_frames, downmix_f32_to_mono, downmix_i16_to_mono, f32_to_i16, formats_compatible,
    i16_to_f32, plan_for, push_silence, AudioFormatError, DeviceFormat, DeviceSamples, MixPlan,
    PcmFormat, SampleKind, MAX_CHANNELS, MAX_SAMPLE_RATE, MIN_CHANNELS, MIN_SAMPLE_RATE,
};

// ---------------------------------------------------------------- 格式校验

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
    // 采样率 0 是典型的手滑（配置项没填），必须在进 COM 之前拦下，
    // 否则会变成看不懂的 E_INVALIDARG。
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
    assert!(PcmFormat::checked(MIN_SAMPLE_RATE, MAX_CHANNELS + 1).is_err());
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
    assert!((f.duration_ms(960) - 20.0).abs() < 1e-9);
    assert_eq!(f.frames_for_ms(20.0), 960);
    assert_eq!(f.frames_for_ms(2.5), 120);
    assert_eq!(f.frames_for_ms(60.0), 2_880);
    // 0ms 也至少给 1 帧，避免调用方拿到空缓冲后除零
    assert_eq!(f.frames_for_ms(0.0), 1);
}

#[test]
fn duration_handles_zero_format_without_panic() {
    // 绕过 checked 构造非法格式，确认不会除零 panic
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

// ---------------------------------------------------------- 设备格式解析

#[test]
fn device_format_reports_sample_kind() {
    // 本机实测的混音格式
    let realtek = DeviceFormat::new(48_000, 2, 32, true);
    assert_eq!(realtek.sample_kind().unwrap(), SampleKind::F32);
    assert_eq!(realtek.to_string(), "48000Hz/2ch/32f");

    let int16 = DeviceFormat::new(44_100, 2, 16, false);
    assert_eq!(int16.sample_kind().unwrap(), SampleKind::I16);
    assert_eq!(int16.to_string(), "44100Hz/2ch/16i");
}

#[test]
fn device_format_rejects_exotic_bit_depths() {
    // 24bit 在共享模式里会被提升成 32bit 容器，真出现就说明驱动很奇怪 ——
    // 明确报错好过把位模式当整数解释（听起来是满幅白噪声）。
    for (bits, is_float) in [
        (24u16, false),
        (32, false),
        (16, true),
        (8, false),
        (64, true),
    ] {
        let d = DeviceFormat::new(48_000, 2, bits, is_float);
        match d.sample_kind() {
            Err(AudioFormatError::UnsupportedBitDepth {
                bits_per_sample,
                is_float: f,
            }) => {
                assert_eq!(bits_per_sample, bits);
                assert_eq!(f, is_float);
            }
            other => panic!("{bits}bit float={is_float} 应被拒绝，实际 {other:?}"),
        }
    }
}

#[test]
fn device_format_frame_bytes_and_duration() {
    let f32_stereo = DeviceFormat::new(48_000, 2, 32, true);
    assert_eq!(f32_stereo.frame_bytes(), 8);
    assert!((f32_stereo.duration_ms(480) - 10.0).abs() < 1e-9);

    let i16_mono = DeviceFormat::new(48_000, 1, 16, false);
    assert_eq!(i16_mono.frame_bytes(), 2);
    assert_eq!(i16_mono.duration_ms(0), 0.0);

    // 采样率 0 不能除零
    let bad = DeviceFormat::new(0, 2, 32, true);
    assert_eq!(bad.duration_ms(480), 0.0);
}

#[test]
fn formats_compatible_requires_exact_rate() {
    let dev48 = DeviceFormat::new(48_000, 2, 32, true);
    assert!(formats_compatible(&dev48, PcmFormat::new(48_000, 1)));
    // 声道数不同不影响兼容性（要降混，不是报错）
    assert!(formats_compatible(&dev48, PcmFormat::new(48_000, 2)));

    let dev441 = DeviceFormat::new(44_100, 2, 32, true);
    assert!(!formats_compatible(&dev441, PcmFormat::new(48_000, 1)));
}

// ------------------------------------------------------------ f32 → i16

#[test]
fn f32_to_i16_maps_full_scale_exactly() {
    // -1.0 必须精确落在 i16::MIN，否则负半周损失 1 个 LSB 动态范围
    assert_eq!(f32_to_i16(-1.0), i16::MIN);
    // +1.0 会算出 32768，clamp 回 i16::MAX
    assert_eq!(f32_to_i16(1.0), i16::MAX);
    assert_eq!(f32_to_i16(0.0), 0);
    // 半幅
    assert_eq!(f32_to_i16(0.5), 16_384);
    assert_eq!(f32_to_i16(-0.5), -16_384);
}

#[test]
fn f32_to_i16_clamps_instead_of_wrapping() {
    // 真实设备会送来越界值（软限幅、混音叠加、后处理）。
    // 回绕的话 1.02 会变成负满幅 —— 听感是刺耳的爆音。
    for s in [1.02f32, 1.5, 100.0, f32::MAX] {
        assert_eq!(f32_to_i16(s), i16::MAX, "{s} 应被压平成满幅");
    }
    for s in [-1.02f32, -1.5, -100.0, f32::MIN] {
        assert_eq!(f32_to_i16(s), i16::MIN, "{s} 应被压平成负满幅");
    }
}

#[test]
fn f32_to_i16_handles_nan_and_inf() {
    // NaN 走 as i16 在 Rust 里是 0，但这里显式挡掉，行为一眼可见。
    assert_eq!(f32_to_i16(f32::NAN), 0);
    assert_eq!(f32_to_i16(f32::INFINITY), 0);
    assert_eq!(f32_to_i16(f32::NEG_INFINITY), 0);
}

#[test]
fn f32_i16_roundtrip_is_stable() {
    for v in [0i16, 1, -1, 16_384, -16_384, i16::MAX, i16::MIN] {
        let f = i16_to_f32(v);
        assert!((-1.0..=1.0).contains(&f), "{v} → {f} 越界");
        let back = f32_to_i16(f);
        // 32767 会因为 32768 缩放而回到 32767；其余精确往返
        assert_eq!(back, v, "{v} 往返后变成 {back}");
    }
}

// ---------------------------------------------------------------- 降混

#[test]
fn downmix_averages_stereo() {
    // 同相内容（人声居中）：平均后增益为 1
    assert!((downmix_f32_to_mono(&[0.5, 0.5]) - 0.5).abs() < 1e-6);
    // 硬右声像：取第一个声道会得到 0（内容整个消失），平均得到 0.5
    assert!((downmix_f32_to_mono(&[0.0, 1.0]) - 0.5).abs() < 1e-6);
    assert!((downmix_f32_to_mono(&[1.0, 0.0]) - 0.5).abs() < 1e-6);
}

#[test]
fn downmix_i16_uses_wide_accumulator() {
    // 8 声道满幅：用 i16 累加会溢出成噪音，必须用 i32
    let loud = [i16::MAX; 8];
    assert_eq!(downmix_i16_to_mono(&loud), i16::MAX);
    let quiet = [i16::MIN; 8];
    assert_eq!(downmix_i16_to_mono(&quiet), i16::MIN);

    assert_eq!(downmix_i16_to_mono(&[100, 200]), 150);
    assert_eq!(downmix_i16_to_mono(&[0, 0]), 0);
}

#[test]
fn downmix_of_empty_frame_is_zero() {
    // 调用方不该传空帧，但不能 panic
    assert_eq!(downmix_f32_to_mono(&[]), 0.0);
    assert_eq!(downmix_i16_to_mono(&[]), 0);
}

// ------------------------------------------------------------- 声道映射

#[test]
fn plan_selection() {
    // 请求单声道：任何设备都要降混
    assert_eq!(plan_for(2, 1), MixPlan::DownmixToMono);
    assert_eq!(plan_for(1, 1), MixPlan::DownmixToMono);
    assert_eq!(plan_for(8, 1), MixPlan::DownmixToMono);
    // 设备单声道、请求多声道：广播
    assert_eq!(plan_for(1, 2), MixPlan::FanoutFromAverage(2));
    // 设备声道不够：广播（宁可给平均值也不要静音）
    assert_eq!(plan_for(2, 6), MixPlan::FanoutFromAverage(6));
    // 设备声道够：直通
    assert_eq!(plan_for(2, 2), MixPlan::Direct(2));
    assert_eq!(plan_for(6, 2), MixPlan::Direct(2));
}

// -------------------------------------------------------- 端到端转换

#[test]
fn convert_f32_stereo_to_i16_mono() {
    // 本机真实路径：48000Hz/2ch/f32 → 48000Hz/1ch/i16
    let src: Vec<f32> = vec![0.5, 0.5, -0.5, -0.5, 1.0, 0.0];
    let mut out = Vec::new();
    let frames = convert_frames(&mut out, DeviceSamples::F32(&src), 2, 1);
    assert_eq!(frames, 3);
    assert_eq!(out, vec![16_384, -16_384, 16_384]);
}

#[test]
fn convert_i16_stereo_to_i16_mono() {
    let src: Vec<i16> = vec![100, 300, -100, -300];
    let mut out = Vec::new();
    let frames = convert_frames(&mut out, DeviceSamples::I16(&src), 2, 1);
    assert_eq!(frames, 2);
    assert_eq!(out, vec![200, -200]);
}

#[test]
fn convert_appends_without_clearing() {
    // read() 的语义是「追加」，不是「覆盖」——
    // 调用方会复用同一个缓冲拼多个 tick 的数据。
    let mut out = vec![7i16, 8];
    let src: Vec<f32> = vec![0.0, 0.0];
    convert_frames(&mut out, DeviceSamples::F32(&src), 2, 1);
    assert_eq!(out, vec![7, 8, 0]);
}

#[test]
fn convert_ignores_partial_trailing_frame() {
    // 交错数据长度不是声道数的整数倍时，尾巴必须丢掉而不是越界。
    let src: Vec<f32> = vec![0.1, 0.1, 0.2];
    let mut out = Vec::new();
    let frames = convert_frames(&mut out, DeviceSamples::F32(&src), 2, 1);
    assert_eq!(frames, 1);
    assert_eq!(out.len(), 1);
}

#[test]
fn convert_handles_empty_input() {
    let mut out = Vec::new();
    assert_eq!(convert_frames(&mut out, DeviceSamples::F32(&[]), 2, 1), 0);
    assert_eq!(convert_frames(&mut out, DeviceSamples::I16(&[]), 1, 1), 0);
    assert!(out.is_empty());
}

#[test]
fn convert_mono_device_to_stereo_request_fans_out() {
    let src: Vec<f32> = vec![0.25, 0.5];
    let mut out = Vec::new();
    let frames = convert_frames(&mut out, DeviceSamples::F32(&src), 1, 2);
    assert_eq!(frames, 2);
    assert_eq!(out.len(), 4);
    // 两个声道内容相同
    assert_eq!(out[0], out[1]);
    assert_eq!(out[2], out[3]);
}

#[test]
fn convert_direct_takes_leading_channels() {
    // 设备 6 声道、请求 2 声道：直通前两个。
    // 这不是正确的 5.1 降混矩阵（中置人声会丢），
    // 但保证「不静音、不溢位」——真正的矩阵留给上层按内容类型决定。
    let src: Vec<f32> = vec![0.5, -0.5, 0.9, 0.9, 0.9, 0.9];
    let mut out = Vec::new();
    let frames = convert_frames(&mut out, DeviceSamples::F32(&src), 6, 2);
    assert_eq!(frames, 1);
    assert_eq!(out.len(), 2);
    assert_eq!(out[0], 16_384);
    assert_eq!(out[1], -16_384);
}

#[test]
fn convert_silent_packet_through_push_silence() {
    // AUDCLNT_BUFFERFLAGS_SILENT 时 pData 是 NULL，
    // 必须按帧数填 0 —— 不填的话时间线上会出现空洞（爆音）。
    let mut out = Vec::new();
    let frames = push_silence(&mut out, 480, 1);
    assert_eq!(frames, 480);
    assert_eq!(out.len(), 480);
    assert!(out.iter().all(|&s| s == 0));

    // 立体声请求下填的是 帧数 × 2
    let mut stereo = Vec::new();
    push_silence(&mut stereo, 480, 2);
    assert_eq!(stereo.len(), 960);
}

#[test]
fn device_samples_len_and_emptiness() {
    let f = [0.0f32; 4];
    let i = [0i16; 3];
    assert_eq!(DeviceSamples::F32(&f).len(), 4);
    assert!(!DeviceSamples::F32(&f).is_empty());
    assert_eq!(DeviceSamples::I16(&i).len(), 3);
    assert!(DeviceSamples::I16(&[]).is_empty());
}

#[test]
fn headless_capture_reports_no_device_format() {
    // headless 没有设备，谎报一个格式会让诊断日志显示不存在的硬件。
    use sidelens_audio_in::{AudioCapture, CaptureMode};
    let cap = AudioCapture::headless(PcmFormat::default());
    assert_eq!(cap.mode(), CaptureMode::Headless);
    assert_eq!(cap.mode().as_str(), "headless");
    assert!(cap.device_format().is_none());
    assert_eq!(cap.format(), PcmFormat::default());
    assert_eq!(cap.latency_ms(), 0.0);
    assert_eq!(cap.device_period_ms(), (0.0, 0.0));
}
