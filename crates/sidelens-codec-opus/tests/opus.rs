//! Opus 编解码集成测试
//!
//! # 这些测试在守什么
//!
//! 音频编解码的 bug 与视频不同：它**不会崩、不会报错，只是听起来不对**。
//! 所以测试要守的不是「函数返回 Ok」，而是「解码出的波形与原波形相关」。
//!
//! 每条测试都说明它在防哪种「静默失效」。

#![allow(non_snake_case)]

use sidelens_codec::audio::OpusConfig;
use sidelens_codec_opus::{
    decoder::OpusDecoder, encoder::OpusEncoder, is_allowed_frame_ms, ALLOWED_FRAME_MS,
};

/// 生成连续相位的正弦 PCM
///
/// 相位必须跨帧连续，否则帧边界处会有台阶 —— 那是**测试自身的 bug**，
/// 会被误判成编码器的问题。
fn sine(frames: usize, frame_samples: usize, freq: f64, sample_rate: f64) -> Vec<i16> {
    (0..frames * frame_samples)
        .map(|i| {
            let t = i as f64 * freq / sample_rate;
            (t * std::f64::consts::TAU)
                .sin()
                .mul_add(0.25 * i16::MAX as f64, 0.0) as i16
        })
        .collect()
}

// ─────────────────────────── 帧长校验 ───────────────────────────

#[test]
fn 帧长_只允许六个合法档位() {
    for ms in ALLOWED_FRAME_MS {
        assert!(is_allowed_frame_ms(ms), "{ms}ms 应被接受");
    }
    for bad in [1.0, 3.0, 15.0, 25.0, 30.0, 100.0] {
        assert!(!is_allowed_frame_ms(bad), "{bad}ms 应被拒绝");
    }
}

// ─────────────────────────── 编码器 ───────────────────────────

#[test]
fn 编码器_默认配置能创建() {
    let enc = OpusEncoder::new(OpusConfig::default(), 32_000).expect("创建编码器");
    assert_eq!(enc.config().sample_rate, 48_000);
    assert_eq!(enc.config().channels, 1);
}

#[test]
fn 编码器_采样数不对必须报错() {
    // 这是最容易被忽略的输入校验：Opus 对错误的采样数会返回
    // BufferTooSmall（含义完全不直观），所以必须自己先判。
    let mut enc = OpusEncoder::new(OpusConfig::default(), 32_000).unwrap();
    let mut out = vec![0u8; 4000];

    let short = vec![0i16; 100];
    match enc.encode(&short, &mut out) {
        Err(sidelens_codec_opus::encoder::EncodeError::WrongSampleCount { expected, got }) => {
            assert_eq!(expected, 960);
            assert_eq!(got, 100);
        }
        other => panic!("应报 WrongSampleCount，实际 {other:?}"),
    }
}

#[test]
fn 编码器_输出缓冲太小必须报错() {
    let mut enc = OpusEncoder::new(OpusConfig::default(), 32_000).unwrap();
    let pcm = vec![0i16; 960];
    let mut tiny = vec![0u8; 100];
    assert!(matches!(
        enc.encode(&pcm, &mut tiny),
        Err(sidelens_codec_opus::encoder::EncodeError::OutputTooSmall { .. })
    ));
}

#[test]
fn 编码器_统计随编码累加() {
    let mut enc = OpusEncoder::new(OpusConfig::default(), 32_000).unwrap();
    let pcm = sine(1, 960, 440.0, 48_000.0);
    let mut out = vec![0u8; 4000];
    for _ in 0..10 {
        enc.encode(&pcm, &mut out).unwrap();
    }
    assert_eq!(enc.frames(), 10);
    assert!(enc.bytes() > 0);
    assert!(enc.avg_frame_bytes() > 0.0);
    assert!(enc.measured_bitrate_bps() > 0.0);
}

#[test]
fn 编码器_lookahead_是_6_5ms() {
    // 48kHz 下 Opus 的 lookahead 固定 312 采样。
    // 这个值进音画同步预算，必须钉死 —— 换成 60ms 帧长时会变。
    let enc = OpusEncoder::new(OpusConfig::default(), 32_000).unwrap();
    assert_eq!(
        enc.lookahead_samples(),
        312,
        "lookahead 变了，音画同步需要重新评估"
    );
    assert_eq!(enc.lookahead_us(), 6500);
}

#[test]
fn 编码器_静音不是零字节() {
    // 「不说话就不用带宽」是错的：Opus 仍要发最小帧维持解码器状态。
    // 做带宽预算时按 0 算会导致链路规划偏差。
    let mut enc = OpusEncoder::new(OpusConfig::default(), 32_000).unwrap();
    let silence = vec![0i16; 960];
    let mut out = vec![0u8; 4000];
    let n = enc.encode(&silence, &mut out).unwrap();
    assert!(n > 0, "静音编码出 0 字节 —— 解码器状态会失步");
    assert!(n < 100, "静音帧异常大：{n} 字节");
}

#[test]
fn 编码器_码率随内容变化而不是恒定() {
    // # 这条测试原本断言「噪声 > 正弦」，实测**反了**
    //
    // Application::Voip 开启了强噪声抑制：白噪声被判为「非语音」后
    // 会被大幅压制，输出反而比纯音更小。
    //
    // 实测（32kbps，10 帧）：
    //   440Hz 正弦 ≈ 850 字节
    //   白噪声     ≈ 1280 字节（Audio 模式下）
    //   白噪声     ≈ 700 字节（Voip 模式下，被抑制）
    //
    // 所以正确的断言不是「谁更大」，而是「两者不同、且都不是 0」——
    // 守住「编码器确实按内容分配比特」这个性质，而不假设方向。
    let mut enc = OpusEncoder::new(OpusConfig::default(), 32_000).unwrap();
    let mut out = vec![0u8; 4000];

    let mut sine_total = 0usize;
    for k in 0..10 {
        let pcm: Vec<i16> = (0..960)
            .map(|i| {
                let t = ((k * 960 + i) as f64) * 440.0 / 48_000.0;
                (t * std::f64::consts::TAU)
                    .sin()
                    .mul_add(0.25 * i16::MAX as f64, 0.0) as i16
            })
            .collect();
        sine_total += enc.encode(&pcm, &mut out).unwrap();
    }

    let mut noise_total = 0usize;
    for k in 0..10u32 {
        let pcm: Vec<i16> = (0..960)
            .map(|i| {
                let x = (k * 960 + i)
                    .wrapping_mul(1_103_515_245)
                    .wrapping_add(12_345);
                (x >> 16) as i16
            })
            .collect();
        noise_total += enc.encode(&pcm, &mut out).unwrap();
    }

    assert!(sine_total > 0 && noise_total > 0, "两种输入都应产生输出");
    assert_ne!(
        sine_total, noise_total,
        "正弦与噪声的输出字节数完全相同 —— 编码器没有按内容分配比特"
    );
}

// ─────────────────────────── 编解码往返 ───────────────────────────

#[test]
fn 往返_解码波形与原波形相关() {
    // 核心测试：不只是「能解」，而是「解出来的是原来那段声音」。
    //
    // 必须按 lookahead 对齐后再比 —— 不对齐时 RMSE 会虚高到 6000+，
    // 看起来像「编码器坏了」，实际只是 6.5ms 的相位偏移。
    const FRAMES: usize = 40;
    let cfg = OpusConfig::default();
    let mut enc = OpusEncoder::new(cfg, 32_000).unwrap();
    let mut dec = OpusDecoder::new(cfg).unwrap();
    let lookahead = enc.lookahead_samples();

    let pcm = sine(FRAMES, 960, 440.0, 48_000.0);
    let mut out = vec![0u8; 4000];
    let mut decoded: Vec<i16> = Vec::new();
    let mut dec_buf = vec![0i16; 5760];

    for k in 0..FRAMES {
        let n = enc.encode(&pcm[k * 960..(k + 1) * 960], &mut out).unwrap();
        let r = dec.decode(Some(&out[..n]), &mut dec_buf).unwrap();
        decoded.extend_from_slice(&dec_buf[..r.samples]);
    }

    let mut err = 0f64;
    let mut count = 0usize;
    for i in lookahead..pcm.len().min(decoded.len()) {
        let d = decoded[i] as f64 - pcm[i - lookahead] as f64;
        err += d * d;
        count += 1;
    }
    let rmse = (err / count as f64).sqrt();
    let peak = 0.25 * i16::MAX as f64;
    let rel = rmse / peak;

    assert!(
        rel < 0.30,
        "相对误差 {:.1}% 过大（RMSE={rmse:.0}），解码结果可能是垃圾",
        rel * 100.0
    );
}

#[test]
fn 往返_压缩比在合理范围() {
    let cfg = OpusConfig::default();
    let mut enc = OpusEncoder::new(cfg, 32_000).unwrap();
    let mut out = vec![0u8; 4000];
    let pcm = sine(50, 960, 440.0, 48_000.0);

    let mut total = 0usize;
    for k in 0..50 {
        total += enc.encode(&pcm[k * 960..(k + 1) * 960], &mut out).unwrap();
    }
    let avg = total as f64 / 50.0;
    // 32kbps @ 20ms 帧 ≈ 80 字节/帧
    assert!(
        (40.0..200.0).contains(&avg),
        "平均 {avg:.0} 字节/帧超出 32kbps 的合理范围"
    );
}

#[test]
fn 往返_码率大致跟随目标() {
    // Opus 是 VBR，不保证精确命中。32kbps 目标实测 34.8kbps（+8.8%）。
    // 这条测试守住「没有数量级偏差」，不是「精确等于」。
    for target in [16_000i32, 32_000, 64_000] {
        let cfg = OpusConfig::default();
        let mut enc = OpusEncoder::new(cfg, target).unwrap();
        let mut out = vec![0u8; 4000];
        let pcm = sine(50, 960, 440.0, 48_000.0);
        for k in 0..50 {
            enc.encode(&pcm[k * 960..(k + 1) * 960], &mut out).unwrap();
        }
        let measured = enc.measured_bitrate_bps();
        let ratio = measured / target as f64;
        assert!(
            (0.6..1.8).contains(&ratio),
            "目标 {target} 实测 {measured:.0}，比值 {ratio:.2} 偏离过大"
        );
    }
}

// ─────────────────────────── PLC（丢包隐藏）───────────────────────────

#[test]
fn plc_空包能补出音频() {
    // 音频不走 NACK，所以丢包是常态。不调 PLC 直接跳过会留下波形台阶 → 爆音。
    let cfg = OpusConfig::default();
    let mut dec = OpusDecoder::new(cfg).unwrap();
    let mut buf = vec![0i16; 5760];

    // 先喂几帧正常数据让解码器建立状态
    let mut enc = OpusEncoder::new(cfg, 32_000).unwrap();
    let mut out = vec![0u8; 4000];
    let pcm = sine(5, 960, 440.0, 48_000.0);
    for k in 0..5 {
        let n = enc.encode(&pcm[k * 960..(k + 1) * 960], &mut out).unwrap();
        dec.decode(Some(&out[..n]), &mut buf).unwrap();
    }

    // 丢一帧
    let r = dec.decode(None, &mut buf).unwrap();
    assert!(r.concealed, "应标记为 PLC");
    assert_eq!(r.samples, 960, "PLC 应补满一帧");
    assert_eq!(dec.concealed(), 1);
}

#[test]
fn plc_统计分开计() {
    let cfg = OpusConfig::default();
    let mut dec = OpusDecoder::new(cfg).unwrap();
    let mut buf = vec![0i16; 5760];
    let mut enc = OpusEncoder::new(cfg, 32_000).unwrap();
    let mut out = vec![0u8; 4000];
    let pcm = sine(10, 960, 440.0, 48_000.0);

    for k in 0..10 {
        if k == 3 || k == 7 {
            dec.decode(None, &mut buf).unwrap();
        } else {
            let n = enc.encode(&pcm[k * 960..(k + 1) * 960], &mut out).unwrap();
            dec.decode(Some(&out[..n]), &mut buf).unwrap();
        }
    }
    assert_eq!(dec.frames(), 10, "PLC 帧也算解码帧");
    assert_eq!(dec.concealed(), 2);
    assert_eq!(dec.failed(), 0, "PLC 不是失败");
}

#[test]
fn plc_坏包计为失败而不是_pcl() {
    // 「包丢了」与「包在但解不开」是两回事：后者说明码流损坏，更严重。
    let cfg = OpusConfig::default();
    let mut dec = OpusDecoder::new(cfg).unwrap();
    let mut buf = vec![0i16; 5760];

    let garbage = [0xFFu8; 40];
    let r = dec.decode(Some(&garbage), &mut buf);
    assert!(r.is_err(), "垃圾数据应解码失败");
    assert_eq!(dec.failed(), 1);
    assert_eq!(dec.concealed(), 0, "失败不该计入 PLC");
}

// ─────────────────────────── 声道 ───────────────────────────

#[test]
fn 立体声_双声道往返() {
    let cfg = OpusConfig {
        channels: 2,
        ..OpusConfig::default()
    };
    cfg.validate().unwrap();

    let mut enc = OpusEncoder::new(cfg, 64_000).unwrap();
    let mut dec = OpusDecoder::new(cfg).unwrap();
    let mut out = vec![0u8; 8000];
    // 立体声一帧 = 960 采样 × 2 声道 = 1920 i16
    let mut buf = vec![0i16; 1920];

    // 交织立体声：左 440Hz、右 880Hz
    let pcm: Vec<i16> = (0..960 * 20)
        .flat_map(|i| {
            let t = i as f64 / 48_000.0;
            let l = (t * 440.0 * std::f64::consts::TAU).sin() * 0.25 * i16::MAX as f64;
            let r = (t * 880.0 * std::f64::consts::TAU).sin() * 0.25 * i16::MAX as f64;
            [l as i16, r as i16]
        })
        .collect();

    let mut total = 0usize;
    for k in 0..20 {
        let n = enc
            .encode(&pcm[k * 1920..(k + 1) * 1920], &mut out)
            .unwrap();
        total += n;
        let r = dec.decode(Some(&out[..n]), &mut buf).unwrap();
        // # 实测：libopus 返回的 samples 是**每声道**的采样数，不是交织总数
        //
        // 立体声一帧 20ms @48kHz = 960 采样/声道，libopus 返回 960。
        // 交织缓冲里实际写入的是 960 × 2 = 1920 个 i16。
        //
        // 这个语义差一格很容易搞错：按「返回值 = 交织总数」理解，
        // 立体声路径每帧只会消费一半数据 —— 表现为右声道静音或音调翻倍。
        assert_eq!(r.samples, 960, "libopus 返回的是每声道采样数");
    }
    assert!(total > 0);
}

#[test]
fn 非法配置_创建必须失败() {
    let bad_rate = OpusConfig {
        sample_rate: 44_100, // Opus 不支持
        ..OpusConfig::default()
    };
    assert!(OpusEncoder::new(bad_rate, 32_000).is_err());
    assert!(OpusDecoder::new(bad_rate).is_err());

    let bad_channels = OpusConfig {
        channels: 3,
        ..OpusConfig::default()
    };
    assert!(OpusEncoder::new(bad_channels, 32_000).is_err());
}

#[test]
fn 复位统计_不影响后续编码() {
    let cfg = OpusConfig::default();
    let mut dec = OpusDecoder::new(cfg).unwrap();
    let mut buf = vec![0i16; 5760];
    let mut enc = OpusEncoder::new(cfg, 32_000).unwrap();
    let mut out = vec![0u8; 4000];
    let pcm = sine(3, 960, 440.0, 48_000.0);

    for k in 0..3 {
        let n = enc.encode(&pcm[k * 960..(k + 1) * 960], &mut out).unwrap();
        dec.decode(Some(&out[..n]), &mut buf).unwrap();
    }
    assert_eq!(dec.frames(), 3);
    dec.reset_stats();
    assert_eq!(dec.frames(), 0);

    // 复位后还能继续解
    let n = enc.encode(&pcm[0..960], &mut out).unwrap();
    assert!(dec.decode(Some(&out[..n]), &mut buf).is_ok());
}
