//! 运行中改码率的单元测试
//!
//! # 为什么要「优雅跳过」
//!
//! 本 crate 只有 Windows 上有实现，且**即使**在 Windows 上也可能没有任何
//! 可用的同步 H.264 编码器 MFT（精简版系统、CI 容器、被策略禁用的 Media
//! Foundation）。那种情况下 `H264Encoder::new` 返回 Err —— 这是**环境**问题，
//! 不是被测代码的问题。
//!
//! 所以每个用例都用 `match H264Encoder::new(...)` 判 Err 就 return，
//! 而不是 `expect`。用 `#[ignore]` 反而更糟：ignore 需要人工记得加
//! `--ignored` 才跑，久了就没人跑，测试形同虚设。
//!
//! # 判据分两层
//!
//! 1. **接口层**（一定成立）：校验、状态同步、读回。
//!    这些不依赖编码器是否真的按码率裁比特，任何有 MFT 的机器上都该过。
//! 2. **行为层**（有条件成立）：输出字节数是否跟着变。
//!    实测结论是「取决于内容复杂度」（见 encoder.rs 里 `set_bitrate` 的文档），
//!    所以这里**不**断言「降码率必须让输出变小」—— 那会在低熵内容上假失败。
//!    只断言「设过之后编码仍然正常产出合法码流」。

#![cfg(windows)]

use sidelens_codec::nalu;
use sidelens_codec_mf::{max_bitrate_for, EncoderConfig, H264Encoder, MAX_BITRATE_BPS};

const W: u32 = 320;
const H: u32 = 240;
const FPS: u32 = 30;

fn cfg(bitrate: u32) -> EncoderConfig {
    EncoderConfig {
        width: W,
        height: H,
        fps: FPS,
        bitrate,
        gop: 60,
    }
}

/// 合成一帧 NV12（渐变背景 + 移动竖条）—— 与 tests/encoder.rs 同一个发生器
fn synth(seq: u32, buf: &mut [u8]) {
    let (w, h) = (W as usize, H as usize);
    let bar_x = (seq as usize * 7) % w;
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

// ---- 纯函数：不需要编码器 ----

#[test]
fn max_bitrate_is_one_and_a_half() {
    assert_eq!(max_bitrate_for(1), 1); // 1 + 1/2 整数除法 = 1
    assert_eq!(max_bitrate_for(2), 3);
    assert_eq!(max_bitrate_for(500_000), 750_000);
    assert_eq!(max_bitrate_for(2_000_000), 3_000_000);
}

#[test]
fn max_bitrate_never_below_mean() {
    // 上限 100 Mbps 时 1.5 倍是 150 Mbps，仍在 u32 内
    assert_eq!(max_bitrate_for(MAX_BITRATE_BPS), 150_000_000);

    // 饱和后仍必须 >= 入参：max < mean 会让码控语义反过来
    assert_eq!(max_bitrate_for(u32::MAX), u32::MAX);
    for v in [0u32, 1, 2, 3, 999, 1_000_000, u32::MAX / 3, u32::MAX / 2] {
        assert!(
            max_bitrate_for(v) >= v,
            "max_bitrate_for({v}) = {} 小于入参",
            max_bitrate_for(v)
        );
    }
}

#[test]
fn max_bitrate_constant_is_100_mbps() {
    assert_eq!(MAX_BITRATE_BPS, 100_000_000);
}

// ---- 接口层：需要编码器，但不需要它真的按码率裁 ----

#[test]
fn bitrate_reflects_initial_config() {
    let enc = match H264Encoder::new(cfg(1_500_000)) {
        Ok(e) => e,
        Err(_) => return, // 没有可用 MFT：环境问题，跳过
    };
    assert_eq!(enc.bitrate(), 1_500_000);
    assert_eq!(enc.config().bitrate, 1_500_000);
}

#[test]
fn set_bitrate_updates_config() {
    let mut enc = match H264Encoder::new(cfg(2_000_000)) {
        Ok(e) => e,
        Err(_) => return,
    };

    enc.set_bitrate(500_000).expect("set_bitrate 应当成功");
    assert_eq!(enc.bitrate(), 500_000);
    // config() 必须反映真实值，不能停在创建时的 2 Mbps
    assert_eq!(enc.config().bitrate, 500_000);

    enc.set_bitrate(8_000_000).expect("set_bitrate 应当成功");
    assert_eq!(enc.bitrate(), 8_000_000);
    assert_eq!(enc.config().bitrate, 8_000_000);
}

#[test]
fn set_bitrate_rejects_zero_without_panic() {
    let mut enc = match H264Encoder::new(cfg(2_000_000)) {
        Ok(e) => e,
        Err(_) => return,
    };
    let err = enc.set_bitrate(0).unwrap_err();
    assert_eq!(err.stage(), "set_bitrate");
    // 失败不能改动状态
    assert_eq!(enc.bitrate(), 2_000_000);
    assert_eq!(enc.config().bitrate, 2_000_000);
}

#[test]
fn set_bitrate_rejects_over_limit_without_panic() {
    let mut enc = match H264Encoder::new(cfg(2_000_000)) {
        Ok(e) => e,
        Err(_) => return,
    };
    let err = enc.set_bitrate(MAX_BITRATE_BPS + 1).unwrap_err();
    assert_eq!(err.stage(), "set_bitrate");
    assert_eq!(enc.bitrate(), 2_000_000);

    // 边界值本身必须被接受
    enc.set_bitrate(MAX_BITRATE_BPS).expect("上限值应当被接受");
    assert_eq!(enc.bitrate(), MAX_BITRATE_BPS);
}

#[test]
fn bitrate_readback_matches_set_value() {
    let mut enc = match H264Encoder::new(cfg(2_000_000)) {
        Ok(e) => e,
        Err(_) => return,
    };
    enc.set_bitrate(750_000).expect("set_bitrate 应当成功");

    // mfh264enc 支持 ICodecAPI 的读写；别的 MFT 可能不支持，
    // 那就跳过这条断言（不是失败）。
    if let Ok(v) = enc.bitrate_readback() {
        assert_eq!(v, 750_000, "GetValue 读回的值与设进去的不一致");
    }
}

// ---- 行为层：设过码率之后，编码必须仍然正常 ----

#[test]
fn encoding_survives_midstream_bitrate_change() {
    let mut enc = match H264Encoder::new(cfg(2_000_000)) {
        Ok(e) => e,
        Err(_) => return,
    };
    let mut nv12 = vec![0u8; enc.config().nv12_len()];
    let interval = enc.config().frame_interval_us();

    let mut stream = Vec::new();
    for i in 0..30u32 {
        synth(i, &mut nv12);
        stream.extend_from_slice(&enc.encode_nv12(&nv12, (i as i64) * interval).expect("编码"));
    }

    // mid-stream 改码率
    enc.set_bitrate(400_000).expect("set_bitrate 应当成功");

    for i in 30..60u32 {
        synth(i, &mut nv12);
        stream.extend_from_slice(&enc.encode_nv12(&nv12, (i as i64) * interval).expect("编码"));
    }
    stream.extend_from_slice(&enc.flush().expect("flush"));

    // 改码率不能把码流搞坏：仍然必须是可切分的 Annex-B
    let views = nalu::split_annexb(&stream);
    assert!(!views.is_empty(), "改码率后切不出任何 NALU");
    for v in &views {
        assert!(!v.data.is_empty());
        assert_eq!(v.data[0] & 0x80, 0, "forbidden_zero_bit 必须为 0");
    }
    assert!(
        views.iter().any(|v| v.ty == nalu::NaluType::Sps),
        "改码率后缺少 SPS"
    );
    assert!(
        views.iter().any(|v| v.ty == nalu::NaluType::Pps),
        "改码率后缺少 PPS"
    );
    assert!(views.iter().any(|v| v.is_keyframe()), "60 帧里没有 IDR");
}

#[test]
fn force_keyframe_does_not_break_stream() {
    let mut enc = match H264Encoder::new(cfg(1_000_000)) {
        Ok(e) => e,
        Err(_) => return,
    };
    let mut nv12 = vec![0u8; enc.config().nv12_len()];
    let interval = enc.config().frame_interval_us();

    let mut stream = Vec::new();
    for i in 0..20u32 {
        synth(i, &mut nv12);
        stream.extend_from_slice(&enc.encode_nv12(&nv12, (i as i64) * interval).expect("编码"));
    }

    // 不支持 ForceKeyFrame 的 MFT 会返回 Err —— 那也不算失败
    let _ = enc.force_keyframe();

    for i in 20..40u32 {
        synth(i, &mut nv12);
        stream.extend_from_slice(&enc.encode_nv12(&nv12, (i as i64) * interval).expect("编码"));
    }
    stream.extend_from_slice(&enc.flush().expect("flush"));

    let views = nalu::split_annexb(&stream);
    assert!(!views.is_empty());
    for v in &views {
        assert_eq!(v.data[0] & 0x80, 0);
    }
}

/// 反复改码率（模拟拥塞控制器每 200ms 调一次）
#[test]
fn rapid_bitrate_changes_are_stable() {
    let mut enc = match H264Encoder::new(cfg(2_000_000)) {
        Ok(e) => e,
        Err(_) => return,
    };
    let mut nv12 = vec![0u8; enc.config().nv12_len()];
    let interval = enc.config().frame_interval_us();

    let mut stream = Vec::new();
    let targets = [
        2_000_000u32,
        1_700_000,
        1_445_000,
        500_000,
        425_000,
        1_000_000,
    ];
    for i in 0..60u32 {
        if i % 10 == 0 {
            let t = targets[(i / 10) as usize % targets.len()];
            enc.set_bitrate(t).expect("set_bitrate 应当成功");
            assert_eq!(enc.bitrate(), t);
        }
        synth(i, &mut nv12);
        stream.extend_from_slice(&enc.encode_nv12(&nv12, (i as i64) * interval).expect("编码"));
    }
    stream.extend_from_slice(&enc.flush().expect("flush"));

    let views = nalu::split_annexb(&stream);
    assert!(!views.is_empty(), "频繁改码率后切不出 NALU");
    assert!(views.iter().any(|v| v.is_keyframe()), "没有 IDR");
    // 每帧都必须有输出（低延迟模式下一进一出），不能因为改码率而卡住
    assert_eq!(enc.frames_in(), 60);
    assert!(
        enc.frames_out() >= 55,
        "60 帧只产出 {} 帧，改码率导致编码器积压",
        enc.frames_out()
    );
}
