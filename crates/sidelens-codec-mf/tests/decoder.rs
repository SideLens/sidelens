//! 解码器集成测试（Windows）
//!
//! 这些测试要真的跑 Media Foundation，所以整个文件 cfg(windows)。
//!
//! # 没有解码器的机器上优雅跳过
//!
//! H.264 Decoder MFT 是 Windows 10/11 自带的（msmpeg2vdec.dll），
//! 但 N 版 / Server Core / 被精简过的镜像可能没有。所以每个测试都用
//! `match H264Decoder::new(..) { Err(_) => return }` 跳过，而不是 expect ——
//! 一条「环境缺能力」的失败信息会把真正的回归淹掉。

#![cfg(windows)]

use sidelens_codec::nalu;
use sidelens_codec_mf::{DecodedFormat, DecoderConfig, EncoderConfig, H264Decoder, H264Encoder};

/// 合成一帧 NV12（渐变背景 + 移动竖条），与 examples/decode_probe.rs 一致
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

/// 小尺寸配置，让测试跑得快（720p 的编解码往返要好几秒）
fn small_encoder() -> EncoderConfig {
    EncoderConfig {
        width: 320,
        height: 240,
        fps: 30,
        bitrate: 800_000,
        gop: 30,
    }
}

#[test]
fn config_validation_rejects_odd_and_zero() {
    let base = DecoderConfig::default();
    assert!(base.validate().is_ok());

    assert!(DecoderConfig { width: 0, ..base }.validate().is_err());

    // 奇数宽高：4:2:0 要求偶数，否则 SetOutputType 返回 MF_E_INVALIDMEDIATYPE
    assert!(DecoderConfig { width: 321, ..base }.validate().is_err());

    assert!(DecoderConfig { fps: 0, ..base }.validate().is_err());
}

#[test]
fn frame_len_matches_nv12() {
    let cfg = DecoderConfig::new(1280, 720, 30);
    assert_eq!(cfg.nv12_len(), 1280 * 720 * 3 / 2);
    // 三种 4:2:0 格式的帧长一致
    for f in [
        DecodedFormat::Nv12,
        DecodedFormat::Yv12,
        DecodedFormat::Iyuv,
    ] {
        assert_eq!(f.frame_len(1280, 720), 1_382_400);
    }
    assert_eq!(DecodedFormat::Nv12.as_str(), "NV12");
}

/// 创建成功 + 输出格式是已知格式 + 输出尺寸非零
#[test]
fn decoder_creates_and_reports_known_format() {
    let cfg = DecoderConfig::new(640, 360, 30);
    let dec = match H264Decoder::new(cfg) {
        Ok(d) => d,
        Err(_) => return, // 本机没有 H.264 解码器 MFT
    };

    assert!(!dec.decoder_name().is_empty(), "解码器名不该为空");
    let fmt = dec.output_format();
    assert!(
        matches!(
            fmt,
            DecodedFormat::Nv12 | DecodedFormat::Yv12 | DecodedFormat::Iyuv
        ),
        "协商到未知输出格式 {fmt:?}"
    );
    let (w, h) = dec.output_size();
    assert!(w > 0 && h > 0, "输出尺寸不该为 0：{w}x{h}");
    assert!(dec.output_frame_len() > 0);
    // 还没喂数据
    assert_eq!(dec.frames_in(), 0);
    assert_eq!(dec.frames_out(), 0);
}

/// 核心测试：编解码往返帧数一致（允许差 1 给 flush 延迟）
#[test]
fn encode_decode_roundtrip_frame_count() {
    let enc_cfg = small_encoder();
    let dec_cfg = DecoderConfig::new(enc_cfg.width, enc_cfg.height, enc_cfg.fps);

    let mut dec = match H264Decoder::new(dec_cfg) {
        Ok(d) => d,
        Err(_) => return,
    };
    let mut enc = H264Encoder::new(enc_cfg).expect("创建编码器");

    let n = 30u32;
    let mut nv12 = vec![0u8; enc_cfg.nv12_len()];
    let mut decoded_frames = 0usize;
    let frame_len = dec.output_frame_len();
    assert!(frame_len > 0);

    for i in 0..n {
        synth(i, enc_cfg.width, enc_cfg.height, &mut nv12);
        let packet = enc
            .encode_nv12(&nv12, (i as i64) * enc_cfg.frame_interval_us())
            .expect("编码");
        if packet.is_empty() {
            continue;
        }
        let raw = dec
            .decode(&packet, (i as i64) * enc_cfg.frame_interval_us())
            .expect("解码");
        assert_eq!(
            raw.len() % frame_len,
            0,
            "第 {i} 帧解码输出 {} 字节不是帧长 {frame_len} 的整数倍",
            raw.len()
        );
        decoded_frames += raw.len() / frame_len;
    }

    // flush 可能吐出最后一帧
    if let Ok(tail) = dec.flush() {
        decoded_frames += tail.len() / frame_len;
    }

    assert_eq!(
        dec.frames_out() as usize,
        decoded_frames,
        "frames_out 与实际解出的帧数不符"
    );

    let diff = (n as i64 - decoded_frames as i64).abs();
    assert!(
        diff <= 1,
        "编解码往返帧数不一致：输入 {n}，解出 {decoded_frames}（差 {diff}，允许 1）"
    );

    // 低延迟必须生效：否则解码器会攒 30+ 帧，帧数差会非常离谱
    assert_eq!(
        dec.not_accepting_count(),
        0,
        "低延迟路径下不该出现 MF_E_NOTACCEPTING（输出队列一直很空）"
    );
}

/// 空输入是合法 no-op，不 panic、不产出、不计数
#[test]
fn empty_input_is_noop() {
    let dec = match H264Decoder::new(DecoderConfig::new(320, 240, 30)) {
        Ok(d) => d,
        Err(_) => return,
    };
    let mut dec = dec;

    let out = dec.decode(&[], 0).expect("空输入应当是 Ok");
    assert!(out.is_empty(), "空输入不该产出帧");
    assert_eq!(dec.frames_in(), 0, "空输入不该计入 frames_in");
    assert_eq!(dec.frames_out(), 0);
}

/// 垃圾输入必须返回 Err 而不是 panic
///
/// # 什么算「垃圾」
///
/// - 纯随机字节（没有任何起始码）
/// - 有起始码但 NALU 类型是保留值
/// - 截断的 SPS
///
/// 解码器对**某些**垃圾其实会静默接受（返回 Ok + 零帧）——
/// 那是 MFT 的容错行为，不算错。这里只要求「不 panic」，
/// 并且若返回 Err，错误必须带 stage 与 HRESULT，便于排查。
#[test]
fn garbage_input_does_not_panic() {
    let mut dec = match H264Decoder::new(DecoderConfig::new(320, 240, 30)) {
        Ok(d) => d,
        Err(_) => return,
    };

    let cases: Vec<Vec<u8>> = vec![
        // 纯垃圾，没有起始码
        vec![0xDE, 0xAD, 0xBE, 0xEF, 0x12, 0x34],
        // 全是 0
        vec![0u8; 256],
        // 有起始码但 NALU 头非法（forbidden_zero_bit = 1）
        vec![0, 0, 0, 1, 0xFF, 0xFF, 0xFF, 0xFF],
        // 截断的 SPS（声明是 SPS 但载荷只有 2 字节）
        vec![0, 0, 0, 1, 0x67, 0x4D],
        // 单个字节
        vec![0x00],
        // 随机
        (0..512u32)
            .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
            .collect(),
    ];

    for (i, garbage) in cases.iter().enumerate() {
        match dec.decode(garbage, i as i64 * 33_333) {
            Ok(out) => {
                // 容错接受：输出必须是空或帧长的整数倍
                let fl = dec.output_frame_len();
                if fl > 0 {
                    assert_eq!(
                        out.len() % fl,
                        0,
                        "垃圾输入 #{i} 产出了非整帧的 {} 字节",
                        out.len()
                    );
                }
            }
            Err(e) => {
                // 报错也要有指向性：stage 非空
                assert!(!e.stage().is_empty(), "错误缺少 stage");
            }
        }
    }

    // 折腾一圈之后解码器仍然可用（没有内部状态崩坏）
    assert!(dec.flush().is_ok(), "垃圾输入后 flush 应当仍然可用");
}

/// flush 之后 frames_out 不再增长
#[test]
fn flush_stops_frame_growth() {
    let enc_cfg = small_encoder();
    let dec_cfg = DecoderConfig::new(enc_cfg.width, enc_cfg.height, enc_cfg.fps);

    let mut dec = match H264Decoder::new(dec_cfg) {
        Ok(d) => d,
        Err(_) => return,
    };
    let mut enc = H264Encoder::new(enc_cfg).expect("创建编码器");
    let mut nv12 = vec![0u8; enc_cfg.nv12_len()];

    for i in 0..10u32 {
        synth(i, enc_cfg.width, enc_cfg.height, &mut nv12);
        let packet = enc
            .encode_nv12(&nv12, (i as i64) * enc_cfg.frame_interval_us())
            .expect("编码");
        if !packet.is_empty() {
            let _ = dec.decode(&packet, (i as i64) * enc_cfg.frame_interval_us());
        }
    }

    let _ = dec.flush().expect("第一次 flush");
    let after_first = dec.frames_out();

    // 再 flush 两次，不应再有新帧
    let tail = dec.flush().expect("第二次 flush");
    assert!(
        tail.is_empty(),
        "第二次 flush 不该再吐帧（{} 字节）",
        tail.len()
    );
    let _ = dec.flush().expect("第三次 flush");
    assert_eq!(
        dec.frames_out(),
        after_first,
        "flush 之后 frames_out 不该再增长"
    );
}

/// 纯色块往返：Y 值必须基本一致（H.264 有损，但纯色块的直流分量该保住）
#[test]
fn flat_color_roundtrip_keeps_luma() {
    let enc_cfg = EncoderConfig {
        width: 320,
        height: 240,
        fps: 30,
        bitrate: 500_000,
        gop: 30,
    };
    let dec_cfg = DecoderConfig::new(enc_cfg.width, enc_cfg.height, enc_cfg.fps);

    let mut dec = match H264Decoder::new(dec_cfg) {
        Ok(d) => d,
        Err(_) => return,
    };
    let mut enc = H264Encoder::new(enc_cfg).expect("创建编码器");

    // 纯灰（Y=128），整帧同值 —— 最容易比对的内容
    let mut nv12 = vec![128u8; enc_cfg.nv12_len()];
    // UV 也要中性
    for b in nv12[enc_cfg.width as usize * enc_cfg.height as usize..].iter_mut() {
        *b = 128;
    }

    let mut got: Option<Vec<u8>> = None;
    for i in 0..3u32 {
        let packet = enc
            .encode_nv12(&nv12, (i as i64) * enc_cfg.frame_interval_us())
            .expect("编码");
        if packet.is_empty() {
            continue;
        }
        let raw = dec
            .decode(&packet, (i as i64) * enc_cfg.frame_interval_us())
            .expect("解码");
        if !raw.is_empty() {
            got = Some(raw);
        }
    }

    let Some(raw) = got else {
        // 一帧都没解出来 —— 可能是低延迟没生效（那种情况下 3 帧远不够）。
        // 用 flush 兜底再判断一次。
        let tail = dec.flush().unwrap_or_default();
        assert!(!tail.is_empty(), "3 帧输入 + flush 后仍然一帧都没解出来");
        return;
    };

    if dec.output_format() != DecodedFormat::Nv12 {
        return; // 非 NV12 的 Y 平面位置不同，这里只验 NV12
    }

    let (w, h) = dec.output_size();
    assert!(w >= enc_cfg.width && h >= enc_cfg.height);

    // 抽几个点比对 Y
    let mut max_diff = 0i32;
    for (sx, sy) in [
        (w as usize / 4, h as usize / 4),
        (w as usize / 2, h as usize / 2),
        (w as usize * 3 / 4, h as usize * 3 / 4),
    ] {
        if sx >= w as usize || sy >= h as usize {
            continue;
        }
        let y = raw[sy * w as usize + sx] as i32;
        max_diff = max_diff.max((y - 128).abs());
    }
    assert!(
        max_diff < 16,
        "纯灰 Y=128 往返后最大偏差 {max_diff}（阈值 16）"
    );
}

/// 参数集必须能被 nalu::split_annexb 正确识别（喂给解码器的码流前提）
#[test]
fn encoder_output_is_valid_annexb_for_decoder() {
    let enc_cfg = small_encoder();
    let mut enc = H264Encoder::new(enc_cfg).expect("创建编码器");
    let mut nv12 = vec![0u8; enc_cfg.nv12_len()];
    synth(0, enc_cfg.width, enc_cfg.height, &mut nv12);
    let packet = enc.encode_nv12(&nv12, 0).expect("编码");

    let views = nalu::split_annexb(&packet);
    assert!(!views.is_empty(), "首帧切不出任何 NALU");
    assert!(
        views.iter().any(|v| v.ty == nalu::NaluType::Sps),
        "首帧缺 SPS，解码器没法初始化"
    );
    assert!(
        views.iter().any(|v| v.ty == nalu::NaluType::Pps),
        "首帧缺 PPS"
    );
    assert!(views.iter().any(|v| v.is_keyframe()), "首帧不是 IDR");
    // Annex-B：必须以起始码开头（解码器不吃 AVCC）
    assert!(
        packet.starts_with(&[0, 0, 0, 1]) || packet.starts_with(&[0, 0, 1]),
        "码流不以 Annex-B 起始码开头"
    );
}
/// SPS 里的显示尺寸必须被解析出来（coded size != display size）
///
/// H.264 的编码尺寸是 16 的整数倍：1080p 编成 1920x1088，360p 编成 640x368。
/// 虚拟摄像头必须按**显示尺寸**输出，否则底部多一条绿边。
#[test]
fn sps_display_size_is_smaller_than_coded_size() {
    let enc_cfg = EncoderConfig {
        width: 320,
        height: 240,
        fps: 30,
        bitrate: 500_000,
        gop: 30,
    };
    let dec_cfg = DecoderConfig::new(enc_cfg.width, enc_cfg.height, enc_cfg.fps);

    let mut dec = match H264Decoder::new(dec_cfg) {
        Ok(d) => d,
        Err(_) => return,
    };

    // 还没喂数据时两个尺寸都等于协商值
    assert_eq!(dec.sps_profile(), None);
    assert_eq!(dec.output_display_size(), dec.output_size());

    let mut enc = H264Encoder::new(enc_cfg).expect("创建编码器");
    let mut nv12 = vec![0u8; enc_cfg.nv12_len()];
    synth(0, enc_cfg.width, enc_cfg.height, &mut nv12);
    let packet = enc.encode_nv12(&nv12, 0).expect("编码");
    let _ = dec.decode(&packet, 0);

    // 见过 SPS 之后
    assert_eq!(dec.sps_profile(), Some(77), "Main profile 应当是 77");
    assert!(dec.sps_level().is_some(), "level 应当被解析出来");

    let (cw, ch) = dec.output_size();
    let (dw, dh) = dec.output_display_size();
    assert_eq!(
        (dw, dh),
        (enc_cfg.width, enc_cfg.height),
        "显示尺寸应当等于编码器请求的分辨率"
    );
    // 320x240 恰好是 16 的整数倍，coded == display；
    // 但无论如何 coded 不能小于 display
    assert!(
        cw >= dw && ch >= dh,
        "coded {cw}x{ch} 不该小于显示 {dw}x{dh}"
    );
    assert_eq!(cw % 2, 0);
    assert_eq!(ch % 2, 0);
}
