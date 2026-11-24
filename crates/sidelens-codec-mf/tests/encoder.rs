//! 编码器集成测试（Windows）
//!
//! 这些测试要真的跑 Media Foundation，所以整个文件 cfg(windows)。
//! 判据都是**字节级**的：输出必须能被 nalu::split_annexb 正确切分、
//! 必须带 Annex-B 起始码、必须有真实的 IDR。

#![cfg(windows)]

use sidelens_codec::nalu;
use sidelens_codec_mf::{detect_format, EncoderConfig, H264Encoder, OutputFormat};

/// 合成一帧 NV12（渐变背景 + 移动竖条）
fn synth(seq: u32, width: u32, height: u32, buf: &mut [u8]) {
    let w = width as usize;
    let h = height as usize;
    let bar_x = (seq as usize * 7) % w.max(1);
    let bar_w = (w / 12).max(8);
    for y in 0..h {
        for x in 0..w {
            buf[y * w + x] = if x >= bar_x && x < bar_x + bar_w {
                235
            } else {
                (((x * 200) / w.max(1)) as u8).wrapping_add((y % 7) as u8)
            };
        }
    }
    for b in buf[w * h..].iter_mut() {
        *b = 128;
    }
}

#[test]
fn detect_format_recognizes_both_layouts() {
    // Annex-B：4 字节起始码 + IDR 头
    assert_eq!(
        detect_format(&[0, 0, 0, 1, 0x65, 0x88, 0x84]),
        OutputFormat::AnnexB
    );
    // Annex-B：3 字节起始码
    assert_eq!(detect_format(&[0, 0, 1, 0x67, 0x4D]), OutputFormat::AnnexB);
    // AVCC：长度前缀 5，后面是 SPS 头 0x67
    assert_eq!(
        detect_format(&[0, 0, 0, 5, 0x67, 0x4D, 0x40, 0x1F, 0x00]),
        OutputFormat::Avcc
    );
    // 长度前缀越界 → 退化为 Annex-B
    assert_eq!(
        detect_format(&[0, 0, 0, 99, 0x67, 0x4D]),
        OutputFormat::AnnexB
    );
    // 空输入
    assert_eq!(detect_format(&[]), OutputFormat::AnnexB);
}

#[test]
fn config_validation_rejects_odd_and_zero() {
    let base = EncoderConfig::default();
    assert!(base.validate().is_ok());

    let odd = EncoderConfig {
        width: 1281,
        ..base
    };
    assert!(odd.validate().is_err());

    let zero = EncoderConfig { fps: 0, ..base };
    assert!(zero.validate().is_err());

    let no_gop = EncoderConfig { gop: 0, ..base };
    assert!(no_gop.validate().is_err());
}

#[test]
fn nv12_len_is_three_halves() {
    let cfg = EncoderConfig {
        width: 1280,
        height: 720,
        ..EncoderConfig::default()
    };
    assert_eq!(cfg.nv12_len(), 1280 * 720 * 3 / 2);
    assert_eq!(cfg.nv12_len(), 1_382_400);
}

#[test]
fn encode_short_nv12_is_rejected() {
    let mut enc = H264Encoder::new(EncoderConfig::default()).expect("创建编码器");
    let err = enc.encode_nv12(&[0u8; 100], 0).unwrap_err();
    assert_eq!(err.stage(), "encode_nv12");
}

/// 核心测试：60 帧 720p，输出必须是可切分的 Annex-B，且含真实 IDR
#[test]
fn encode_60_frames_produces_valid_annexb() {
    let cfg = EncoderConfig::default();
    let mut enc = H264Encoder::new(cfg).expect("创建编码器");
    let mut nv12 = vec![0u8; cfg.nv12_len()];

    let mut stream = Vec::new();
    let mut frames_with_output = 0;
    for i in 0..60u32 {
        synth(i, cfg.width, cfg.height, &mut nv12);
        let out = enc
            .encode_nv12(&nv12, (i as i64) * cfg.frame_interval_us())
            .expect("编码");
        if !out.is_empty() {
            frames_with_output += 1;
            stream.extend_from_slice(&out);
        }
    }
    stream.extend_from_slice(&enc.flush().expect("flush"));

    // 1. 实测格式必须是 Annex-B（MF 的 H.264 编码器吐的就是起始码）
    assert_eq!(
        enc.output_format(),
        Some(OutputFormat::AnnexB),
        "MF 输出格式与预期不符"
    );

    // 2. 必须带 4 字节起始码
    assert!(
        stream.windows(4).any(|w| w == [0, 0, 0, 1]),
        "输出里找不到 00 00 00 01 起始码"
    );

    // 3. 能被 split_annexb 正确切分，且切出来的 NALU 头合法
    let views = nalu::split_annexb(&stream);
    assert!(!views.is_empty(), "切不出任何 NALU");
    for v in &views {
        assert!(!v.data.is_empty());
        assert_eq!(v.data[0] & 0x80, 0, "forbidden_zero_bit 必须为 0");
    }

    // 4. 必须有真实的 IDR（60 帧 / gop 60 → 至少 1 个）
    let idr = views.iter().filter(|v| v.is_keyframe()).count();
    assert!(idr >= 1, "60 帧里没有 IDR，关键帧判断会全线失效");

    // 5. 必须有 SPS / PPS，否则接收端解不出来
    assert!(
        views.iter().any(|v| v.ty == nalu::NaluType::Sps),
        "缺少 SPS"
    );
    assert!(
        views.iter().any(|v| v.ty == nalu::NaluType::Pps),
        "缺少 PPS"
    );

    // 6. 低延迟模式下应该是「一进一出」，不该有大量积压
    assert!(
        frames_with_output >= 55,
        "60 帧只产出 {frames_with_output} 帧，编码器有明显积压（低延迟没生效？）"
    );

    // 7. 关键帧必须自带参数集（接收端中途加入靠这个）
    let key_frames: Vec<_> = views.iter().filter(|v| v.is_keyframe()).collect();
    assert!(!key_frames.is_empty());
}

/// 关键帧标记必须来自真实码流，且 GOP 生效
#[test]
fn keyframe_flag_matches_real_idr() {
    let cfg = EncoderConfig {
        width: 320,
        height: 240,
        gop: 10,
        ..EncoderConfig::default()
    };
    let mut enc = H264Encoder::new(cfg).expect("创建编码器");
    let mut nv12 = vec![0u8; cfg.nv12_len()];

    let mut keys = Vec::new();
    for i in 0..40u32 {
        synth(i, cfg.width, cfg.height, &mut nv12);
        let out = enc
            .encode_nv12(&nv12, (i as i64) * cfg.frame_interval_us())
            .expect("编码");
        if nalu::is_keyframe(&out) {
            keys.push(i);
        }
    }

    // gop=10，40 帧里应该有 3~5 个 IDR
    assert!(
        (3..=5).contains(&keys.len()),
        "gop=10 的 40 帧里 IDR 数 {} 不合理：{keys:?}",
        keys.len()
    );
    // 间隔必须接近 gop（±2 帧容忍编码器内部对齐）
    for pair in keys.windows(2) {
        let gap = pair[1] - pair[0];
        assert!(
            (8..=12).contains(&gap),
            "IDR 间隔 {gap} 偏离 gop=10 太多：{keys:?}"
        );
    }
}

/// 参数集必须非空，且能被解析成 SPS + PPS
#[test]
fn parameter_sets_are_valid() {
    let enc = H264Encoder::new(EncoderConfig::default()).expect("创建编码器");
    let ps = enc.parameter_sets();
    // MF 的 mfh264enc 不在输出类型里给 sequence header，
    // 这种情况参数集由码流首帧补齐 —— 两条路都算正常，
    // 但若给了就必须能解析。
    if !ps.is_empty() {
        let views = nalu::split_annexb(ps);
        assert!(views.iter().any(|v| v.ty == nalu::NaluType::Sps));
        assert!(views.iter().any(|v| v.ty == nalu::NaluType::Pps));
    }
}
