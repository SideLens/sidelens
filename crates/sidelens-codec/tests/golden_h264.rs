//! 用**真实 H.264 码流**验证 NALU/SPS 解析（golden 回归）
//!
//! # 为什么需要真实码流
//!
//! `annexb_edge.rs` 用的是手工构造的字节序列，覆盖边界情况很好，
//! 但它是**我们自己想出来的边界**。真实编码器吐出的码流有它自己的特征：
//!
//! - 一帧切 7 个 slice（多 slice 编码）
//! - 每个 access unit 前面有 AUD（type 9）
//! - 关键帧是 IDR，`nal_ref_idc` 为 3
//! - SPS 的裁剪参数、VUI 都有真实值
//!
//! 这些特征用手写测试很难覆盖全。
//!
//! # golden 文件怎么来的
//!
//! ```powershell
//! sidelenssend.exe --udp 127.0.0.1:28900 30 8192 1 --h264 --size 1280x720 \
//!     --h264-dump crates/sidelens-codec/tests/data/golden-720p.h264
//! ```
//!
//! 31 帧、136 KB。发送端自报 `encode_frames=31`。
//!
//! # 期望值怎么确定的（关键）
//!
//! **不是**用本项目的解析器跑出来的 —— 那样就是自证。
//! 用的是完全独立的 Node 实现 `tools/h264-inspect.mjs`（自己写 bit reader、
//! 自己处理 emulation prevention bytes），两边结论一致才算真验证。
//!
//! ```text
//! node tools/h264-inspect.mjs crates/sidelens-codec/tests/data/golden-720p.h264
//!   帧数: 31   SPS: 1280x720   IDR: 7   PPS: 1   AUD: 31
//! ```
//!
//! 独立实现第一版就错了（按「每个 slice 一帧」算出 2527），修正切帧逻辑后才对上。
//! 这件事本身就是「为什么要独立验证」的最好论据。

use sidelens_codec::nalu::{annexb_to_avcc, avcc_to_annexb, split_annexb, NaluType, ParameterSets};

/// 真实 720p 码流（31 帧，136 KB）
const GOLDEN: &[u8] = include_bytes!("data/golden-720p.h264");

// ── 与独立解析器对齐的期望值 ─────────────────────────────────
const EXPECT_FRAMES: usize = 31;
const EXPECT_WIDTH: u32 = 1280;
const EXPECT_HEIGHT: u32 = 720;
const EXPECT_AUD: usize = 31;
const EXPECT_SPS: usize = 1;
const EXPECT_PPS: usize = 1;

#[test]
fn golden_is_not_empty() {
    assert!(
        GOLDEN.len() > 100_000,
        "golden 文件应约 136 KB，实际 {} 字节",
        GOLDEN.len()
    );
}

#[test]
fn split_finds_expected_nalu_counts() {
    let nalus = split_annexb(GOLDEN);
    assert!(!nalus.is_empty(), "应能切出 NALU");

    let aud = nalus.iter().filter(|n| n.ty.as_u8() == 9).count();
    let sps = nalus.iter().filter(|n| n.ty.as_u8() == 7).count();
    let pps = nalus.iter().filter(|n| n.ty.as_u8() == 8).count();

    assert_eq!(aud, EXPECT_AUD, "AUD 数量应等于帧数");
    assert_eq!(sps, EXPECT_SPS, "SPS 数量");
    assert_eq!(pps, EXPECT_PPS, "PPS 数量");
}

#[test]
fn aud_count_equals_frame_count() {
    // 这是「切帧」的正确判据：每个 access unit 前有一个 AUD。
    // 用「每个 slice 一帧」会算成 217（一帧 7 个 slice）。
    let nalus = split_annexb(GOLDEN);
    let aud = nalus.iter().filter(|n| n.ty.as_u8() == 9).count();
    assert_eq!(
        aud, EXPECT_FRAMES,
        "AUD 数 {aud} 应等于帧数 {EXPECT_FRAMES}（若算出 217 说明按 slice 数了）"
    );
}

#[test]
fn slices_per_frame_is_seven() {
    // MF H.264 编码器把一帧切成 7 个 slice（多 slice 编码）。
    // 这个数字很重要：它决定了「一个分片丢失会影响多少」——
    // 7 个 slice 意味着单帧内有多条独立的熵编码路径，丢一个只坏一片。
    let nalus = split_annexb(GOLDEN);
    let slices = nalus
        .iter()
        .filter(|n| n.ty.as_u8() == 1 || n.ty.as_u8() == 5)
        .count();
    let per_frame = slices / EXPECT_FRAMES;
    assert_eq!(
        per_frame, 7,
        "每帧 slice 数应为 7，实际 {per_frame}（总 slice {slices} / 帧 {EXPECT_FRAMES}）"
    );
}

#[test]
fn parameter_sets_are_extracted() {
    let mut ps = ParameterSets::new();
    assert!(ps.update_from(GOLDEN), "应从码流里提取到 SPS/PPS");
    assert!(ps.is_complete(), "SPS 与 PPS 都应就绪");
    assert!(ps.sps().is_some());
    assert!(ps.pps().is_some());
}

#[test]
fn parameter_sets_have_correct_nalu_types() {
    let mut ps = ParameterSets::new();
    ps.update_from(GOLDEN);
    let sps = ps.sps().expect("SPS");
    let pps = ps.pps().expect("PPS");

    // `sps()` / `pps()` 返回的是**裸 NALU 字节（不含起始码）** ——
    // 见 `ParameterSets::update_from` 存的是 `nalu.data`。
    // 起始码由 `to_annexb()` 补。
    //
    // 第一版测试假设它含起始码，`split_annexb` 切出 0 个 NALU 就失败了。
    // 这个契约值得用测试钉住：裸数据 + to_annexb 补起始码。
    assert!(!sps.is_empty());
    assert!(!pps.is_empty());
    assert_eq!(
        sps[0] & 0x1f,
        7,
        "SPS 首字节的 nalu_type 应为 7，实际 {}",
        sps[0] & 0x1f
    );
    assert_eq!(
        pps[0] & 0x1f,
        8,
        "PPS 首字节的 nalu_type 应为 8，实际 {}",
        pps[0] & 0x1f
    );
    assert_eq!(
        NaluType::from_header(sps[0]),
        NaluType::Sps,
        "from_header 应识别为 SPS"
    );

    // to_annexb 应补上起始码，切回来还是 2 个 NALU
    let combined = ps.to_annexb();
    let nalus = split_annexb(&combined);
    assert_eq!(nalus.len(), 2, "to_annexb 应产出 SPS + PPS 两个 NALU");
    assert_eq!(nalus[0].ty, NaluType::Sps);
    assert_eq!(nalus[1].ty, NaluType::Pps);
}

#[test]
fn keyframe_detection_matches_idr_count() {
    // # 这个测试的期望值我写错过两次，值得记
    //
    // 独立解析器输出 `IDR slice: 7`，我把它当成「7 个关键帧」，断言
    // `frames_with_idr == 7`，实际得到 1。
    //
    // 查真实 NALU 类型序列：
    //
    // ```text
    // 9 7 8 6 6 5 5 5 5 5 5 5  9 1 1 1 1 1 1 1  9 1 1 1 1 1 1 1 ...
    //   ^SPS/PPS^ ^7 个 IDR slice^ ^AUD^ ^7 个 P slice^ ...
    // ```
    //
    // **7 个 IDR 全部在第一个 access unit 里** —— 同一帧的 7 个 slice
    // （多 slice 编码），不是 7 帧。这个 golden 只编了 1 秒、GOP 60，
    // 所以只有 1 个关键帧。
    //
    // 教训：统计字段名必须无歧义。解析器里已把 `idr` 改名为 `idrSlices`。
    let nalus = split_annexb(GOLDEN);

    // 按 AUD 切分成若干 access unit
    let mut units: Vec<Vec<sidelens_codec::nalu::NaluView>> = Vec::new();
    for n in &nalus {
        if n.ty == NaluType::Aud {
            units.push(Vec::new());
        }
        if let Some(cur) = units.last_mut() {
            cur.push(*n);
        }
    }

    assert_eq!(units.len(), EXPECT_FRAMES, "access unit 数应等于帧数");

    let frames_with_idr = units
        .iter()
        .filter(|u| u.iter().any(|n| n.ty == NaluType::IdrSlice))
        .count();
    let idr_slices: usize = units
        .iter()
        .map(|u| u.iter().filter(|n| n.ty == NaluType::IdrSlice).count())
        .sum();

    // 1 秒 / GOP 60 -> 只有 1 个关键帧
    assert_eq!(
        frames_with_idr, 1,
        "含 IDR 的帧数应为 1（1 秒、GOP 60），实际 {frames_with_idr}"
    );
    // 每帧 7 个 slice -> 7 个 IDR slice
    assert_eq!(
        idr_slices, 7,
        "IDR slice 数应为 7（1 帧 × 7 slice），实际 {idr_slices}"
    );
    // 第一个 access unit 就是关键帧（SPS/PPS 在它前面）
    assert!(
        units[0].iter().any(|n| n.ty == NaluType::IdrSlice),
        "第一个 access unit 应含 IDR"
    );
}

#[test]
fn annexb_avcc_roundtrip_preserves_nalu_count() {
    // Annex-B -> AVCC -> Annex-B 必须无损（NALU 数量和内容都不变）
    let before = split_annexb(GOLDEN);
    let avcc = annexb_to_avcc(GOLDEN);
    assert!(!avcc.is_empty(), "AVCC 转换不应为空");
    let back = avcc_to_annexb(&avcc).expect("应能转回 Annex-B");
    let after = split_annexb(&back);
    assert_eq!(
        before.len(),
        after.len(),
        "往返后 NALU 数量应不变（前 {} 后 {}）",
        before.len(),
        after.len()
    );
    // 逐 NALU 比内容
    for (i, (a, b)) in before.iter().zip(after.iter()).enumerate() {
        assert_eq!(a.data, b.data, "第 {i} 个 NALU 内容不一致");
    }
}

#[test]
fn no_nalu_is_empty() {
    // 真实码流里不该出现零长度 NALU（起始码紧挨着起始码）
    for (i, n) in split_annexb(GOLDEN).iter().enumerate() {
        assert!(!n.data.is_empty(), "第 {i} 个 NALU 是空的");
    }
}

#[test]
fn sps_encodes_expected_resolution() {
    // 从 SPS 原始字节里手算分辨率，验证与独立解析器一致。
    //
    // # 为什么在这里手算而不是调 `sidelens_core::sps`
    //
    // 本 crate（`sidelens-codec`）不依赖 `sidelens-core`，而且这里要的
    // 恰恰是**独立于项目解析器**的验证。所以就地写一个最小的
    // Exp-Golomb 读取，只读 SPS 前 30 个 bit 就够拿到宽高。
    //
    // 这也与 `tools/h264-inspect.mjs` 形成三方交叉：
    // 项目解析器 / Node 解析器 / 这个测试里的手算。
    let mut ps = ParameterSets::new();
    ps.update_from(GOLDEN);
    let sps = ps.sps().expect("SPS");

    // 去 emulation prevention bytes（00 00 03 -> 00 00）
    let mut rbsp = Vec::with_capacity(sps.len());
    let mut zeros = 0;
    for &b in sps.iter().skip(1) {
        // skip(1) 跳过 NAL header
        if zeros >= 2 && b == 0x03 {
            zeros = 0;
            continue;
        }
        rbsp.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }

    struct Bits<'a> {
        d: &'a [u8],
        pos: usize,
    }
    impl Bits<'_> {
        fn u(&mut self, n: usize) -> u32 {
            let mut v = 0u32;
            for _ in 0..n {
                let byte = self.d.get(self.pos / 8).copied().unwrap_or(0);
                v = (v << 1) | (((byte >> (7 - (self.pos % 8))) & 1) as u32);
                self.pos += 1;
            }
            v
        }
        fn ue(&mut self) -> u32 {
            let mut lz = 0;
            while self.u(1) == 0 {
                lz += 1;
                if lz > 32 {
                    return 0;
                }
            }
            ((1u32 << lz) - 1) + self.u(lz as usize)
        }
    }

    let mut b = Bits { d: &rbsp, pos: 0 };
    let profile = b.u(8);
    let _constraints = b.u(8);
    let _level = b.u(8);
    let _sps_id = b.ue();

    // profile 77 (Main) / 100 (High) 等需要读 chroma_format_idc
    if matches!(
        profile,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        let chroma = b.ue();
        if chroma == 3 {
            let _ = b.u(1);
        }
        let _ = b.ue();
        let _ = b.ue();
        let _ = b.u(1);
        if b.u(1) == 1 {
            // scaling lists：直接放弃精确解析（Main profile 走不到这里）
            return;
        }
    }

    let _log2_max_frame_num = b.ue();
    let poc_type = b.ue();
    if poc_type == 0 {
        let _ = b.ue();
    }
    let _max_num_ref = b.ue();
    let _gaps = b.u(1);

    let width_mbs = b.ue() + 1;
    let height_map_units = b.ue() + 1;
    let frame_mbs_only = b.u(1);
    if frame_mbs_only == 0 {
        let _ = b.u(1);
    }
    let _direct8x8 = b.u(1);

    let mut crop = [0u32; 4];
    if b.u(1) == 1 {
        for c in crop.iter_mut() {
            *c = b.ue();
        }
    }

    // Main profile (77) => chroma_format_idc 隐含为 1，裁剪单位 = (2, 2)
    let crop_unit_x = 2u32;
    let crop_unit_y = 2u32 * if frame_mbs_only == 1 { 1 } else { 2 };

    let coded_w = width_mbs * 16;
    let coded_h = height_map_units * 16 * if frame_mbs_only == 1 { 1 } else { 2 };
    let w = coded_w - (crop[0] + crop[1]) * crop_unit_x;
    let h = coded_h - (crop[2] + crop[3]) * crop_unit_y;

    assert_eq!(w, EXPECT_WIDTH, "SPS 手算宽度应为 {EXPECT_WIDTH}，实际 {w}");
    assert_eq!(
        h, EXPECT_HEIGHT,
        "SPS 手算高度应为 {EXPECT_HEIGHT}，实际 {h}"
    );
}

#[test]
fn gop_interval_is_uniform() {
    // 关键帧间隔必须恒定（编码器按 --gop 配置，默认 60）。
    //
    // # 为什么这个断言有价值
    //
    // GOP 不恒定不会让码流非法，但会让下游的假设全部偏移：
    // - FEC 的分组策略假设「关键帧比 P 帧大得多」
    // - 抖动缓冲的 needs_keyframe 逻辑假设「丢了关键帧要等下一个 GOP」
    // - 码率统计的关键帧占比会失真
    //
    // golden 只有 1 秒 / 31 帧，GOP 60 —— 所以只有 1 个关键帧，
    // 无法验证间隔。这里断言「关键帧数 == 1 且在第 0 帧」，
    // 间隔验证由 tools/verify-h264-stream.ps1 的 GOP 断言覆盖
    // （它跑 5 秒 / 300 帧，得到 5-6 个关键帧）。
    let nalus = split_annexb(GOLDEN);
    let mut units: Vec<Vec<sidelens_codec::nalu::NaluView>> = Vec::new();
    for n in &nalus {
        if n.ty == NaluType::Aud {
            units.push(Vec::new());
        }
        if let Some(cur) = units.last_mut() {
            cur.push(*n);
        }
    }

    let keyframes: Vec<usize> = units
        .iter()
        .enumerate()
        .filter(|(_, u)| u.iter().any(|n| n.ty == NaluType::IdrSlice))
        .map(|(i, _)| i)
        .collect();

    assert_eq!(
        keyframes.len(),
        1,
        "31 帧 / GOP 60 应只有 1 个关键帧，实际 {keyframes:?}"
    );
    assert_eq!(keyframes[0], 0, "关键帧应在第 0 帧");
}

#[test]
fn every_nalu_has_valid_header() {
    // forbidden_zero_bit 必须为 0（H.264 规范硬要求）
    for (i, n) in split_annexb(GOLDEN).iter().enumerate() {
        assert_eq!(
            n.data[0] & 0x80,
            0,
            "第 {i} 个 NALU 的 forbidden_zero_bit 应为 0（类型 {}）",
            n.ty.as_u8()
        );
    }
}
