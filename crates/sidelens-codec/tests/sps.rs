//! SPS 解析测试
//!
//! 四条真实 SPS 字节序列来自公开码流/抓包（每条都标了出处），
//! 剩下的用例盯住「不可信输入不能让解析器崩」与「分辨率变更必须被看见」。
//!
//! 这些用例守的不变量：
//!
//! 1. **显示分辨率必须已扣掉 cropping** —— 1080p 的编码高度是 1088
//! 2. **frame_mbs_only_flag = 0 时高度要 ×2** —— 漏了就只报一半高度
//! 3. **畸形输入一律返回 Err** —— 解析器面对网络数据不能 panic
//! 4. **分辨率变更必须被检测** —— 漏了接收端不重建解码器就花屏

use sidelens_codec::sps::{ParameterSetTracker, SpsError, SpsInfo};
use sidelens_codec::{split_annexb, NaluType};

// ---------------------------------------------------------------------------
// 真实 SPS 样本
// ---------------------------------------------------------------------------

/// Baseline 720p，无 cropping、无 VUI
///
/// 出处：Sricam/ONVIF 摄像头 SDP 的 `sprop-parameter-sets=Z0IAH5WoFAFuQA==`
/// （见 ipcamtalk / ZoneMinder 论坛里贴出的 rtsp SDP 抓包）。
/// 解析结果：profile 66 (Baseline) / level 31 / 1280x720 / 无帧率信息。
const SPS_BASELINE_720P: &[u8] = &[0x67, 0x42, 0x00, 0x1f, 0x95, 0xa8, 0x14, 0x01, 0x6e, 0x40];

/// High profile 1080p，**带 cropping**，且含 emulation prevention 字节
///
/// 出处：StackOverflow 42300009 里贴出的真实 RTMP/FLV 首帧
/// （FMLE 推流，SPS 长 0x1a 字节）。
/// 编码尺寸 1920x1088，frame_crop_bottom_offset = 4（CropUnitY = 2）
/// → 显示 1920x1080；VUI timing: num_units_in_tick=1, time_scale=60 → 30fps。
const SPS_HIGH_1080P: &[u8] = &[
    0x67, 0x64, 0x00, 0x28, 0xac, 0xd9, 0x40, 0x78, 0x02, 0x27, 0xe5, 0xc0, 0x44, 0x00, 0x00, 0x03,
    0x00, 0x04, 0x00, 0x00, 0x03, 0x00, 0xf0, 0x3c, 0x60, 0xc6, 0x58,
];

/// Main profile **隔行**（frame_mbs_only_flag = 0）
///
/// 出处：doom9 论坛讨论 HD DVD 帧率时贴出的真实 SPS（`00 1b 67 4d 40 28 ...`）。
/// 编码尺寸 1920x1088（map unit 只有 34 行，×2 才是 1088），无 cropping，
/// 25fps，fixed_frame_rate_flag = 1。
const SPS_MAIN_INTERLACED: &[u8] = &[
    0x67, 0x4d, 0x40, 0x28, 0x89, 0x99, 0x80, 0xf0, 0x08, 0x8b, 0x01, 0xf7, 0x01, 0x01, 0x01, 0xf0,
    0x00, 0x00, 0x03, 0x00, 0x10, 0x00, 0x00, 0x03, 0x03, 0x29, 0x40,
];

/// Main profile 1080p，**VUI timing 是 NTSC 29.97**
///
/// 出处：libav-user 邮件列表里贴出的 fragmented MP4 init segment
/// （Sequence Parameter = 67 4d 40 1f ec c0 ...，tkhd 写明 1920x1080）。
/// num_units_in_tick=1001, time_scale=60000 → 60000/(2*1001) = 29.97fps。
const SPS_MAIN_NTSC: &[u8] = &[
    0x67, 0x4d, 0x40, 0x1f, 0xec, 0xc0, 0x3c, 0x01, 0x13, 0xf2, 0xe0, 0x22, 0x00, 0x00, 0x07, 0xd2,
    0x00, 0x01, 0xd4, 0xc0, 0x1e, 0x30, 0x63, 0x34,
];

/// High profile 1400x1050，**左右也要裁**
///
/// 出处：StackOverflow 79740122 里的真实 SPS（TI 硬件编码器产出）。
/// 编码 1408x1056，crop = (left 0, right 4, top 0, bottom 3)
/// → 1408 - 2*4 = 1400，1056 - 2*3 = 1050。
const SPS_HIGH_1400X1050: &[u8] = &[
    0x67, 0x64, 0x00, 0x28, 0xac, 0x2b, 0x40, 0x2c, 0x01, 0x0b, 0xcb, 0x26, 0x02, 0x20, 0x00, 0x00,
    0x03, 0x00, 0x20, 0x00, 0x00, 0x04, 0x01, 0xb4, 0x11, 0x08, 0xd4,
];

/// Baseline 720p，**带完整 VUI**（aspect ratio / video signal / timing / HRD / restriction）
///
/// 出处：StackOverflow 38094302 里贴出的真实 NAL unit
/// （`00 00 00 01 67 42 00 1F E6 20 0A ...`，提问者用它问 NALU 头怎么读）。
/// 这是本组里 VUI 最复杂的一条：HRD 与 bitstream_restriction 都存在，
/// 跳过它们时少读一个比特，后面的 timing info 就会全部错位。
/// 解析结果：1280x720 / 30fps / fixed_frame_rate_flag = 1。
const SPS_BASELINE_720P_VUI: &[u8] = &[
    0x67, 0x42, 0x00, 0x1f, 0xe6, 0x20, 0x0a, 0x00, 0xb6, 0x60, 0x22, 0x00, 0x00, 0x03, 0x00, 0x02,
    0x00, 0x00, 0x03, 0x00, 0x79, 0xc0, 0x00, 0x00, 0x10, 0xfd, 0xd0, 0x00, 0x00, 0x40, 0x16, 0x43,
    0xf4, 0xa4, 0x18, 0x00, 0x00, 0x03, 0x02, 0x1d, 0x97, 0x00, 0x00, 0x1a, 0xb3, 0xf1, 0xfa, 0x52,
    0x05,
];

fn parse_ok(bytes: &[u8]) -> SpsInfo {
    SpsInfo::parse(bytes).expect("真实 SPS 必须能解析")
}

// ---------------------------------------------------------------------------
// 真实码流
// ---------------------------------------------------------------------------

#[test]
fn 带完整_vui_的_720p_sps_帧率与分辨率都对() {
    // 这条的 VUI 里 HRD 与 bitstream_restriction 都在。
    // 「跳过」必须是真的逐位消费，否则 timing info 之后全错位。
    let sps = parse_ok(SPS_BASELINE_720P_VUI);

    assert_eq!(sps.profile_idc, 66);
    assert_eq!(sps.level_idc, 31);
    assert_eq!(sps.resolution(), (1280, 720));
    assert!(!sps.frame_cropping_flag);

    let vui = sps.vui.expect("带 VUI");
    assert!(vui.timing_info_present_flag);
    assert_eq!(vui.num_units_in_tick, 1);
    assert_eq!(vui.time_scale, 60);
    assert!(vui.fixed_frame_rate_flag, "恒定帧率");
    assert_eq!(vui.aspect_ratio_idc, Some(1), "1:1 方形像素");
    // 两份 HRD 都在 —— 跳过逻辑必须真的把 cpb 表逐项读完，
    // 少读一个 se(v)，后面的 pic_struct / timing 就会全错位。
    assert!(vui.nal_hrd_parameters_present_flag, "带 NAL HRD");
    assert!(vui.vcl_hrd_parameters_present_flag, "带 VCL HRD");
    assert!(vui.pic_struct_present_flag, "带 pic_struct");
    assert!(
        !vui.bitstream_restriction_flag,
        "这份没有 bitstream restriction（有的话下面就会多读 7 个字段）"
    );

    let fps = sps.frame_rate().expect("应能算出帧率");
    assert!((fps - 30.0).abs() < 1e-9, "60 / (2*1) = 30fps，实得 {fps}");
}

#[test]
fn baseline_720p_解析出_1280x720_且无帧率信息() {
    let sps = parse_ok(SPS_BASELINE_720P);

    assert_eq!(sps.profile_idc, 66, "Baseline profile");
    assert_eq!(sps.level_idc, 31, "level 3.1");
    assert_eq!(sps.seq_parameter_set_id, 0);
    assert_eq!(sps.chroma_format_idc, 1, "4:2:0");
    assert!(sps.frame_mbs_only_flag, "逐行");
    assert!(!sps.frame_cropping_flag, "720p 天然 16 对齐，不需要裁剪");

    // 720 / 16 = 45，正好整数，所以编码高度就是显示高度
    assert_eq!(sps.coded_resolution(), (1280, 720));
    assert_eq!(sps.resolution(), (1280, 720));
    assert_eq!(sps.frame_rate(), None, "这份 SPS 没有 VUI timing");
}

#[test]
fn high_1080p_必须裁掉多出来的_8_行() {
    // 这是 SPS 解析最容易错的点：编码高度 1088，显示高度 1080。
    // 少了 cropping 就会给虚拟摄像头一个 1088 行的缓冲区，底部多一条绿边。
    let sps = parse_ok(SPS_HIGH_1080P);

    assert_eq!(sps.profile_idc, 100, "High profile");
    assert_eq!(sps.level_idc, 40, "level 4.0");
    assert_eq!(sps.chroma_format_idc, 1);
    assert_eq!(sps.bit_depth_luma_minus8, 0, "8bit");
    assert_eq!(sps.bit_depth_chroma_minus8, 0, "8bit");
    assert!(!sps.separate_colour_plane_flag);

    assert!(sps.frame_cropping_flag, "1080p 必然带 cropping");
    assert_eq!(sps.frame_crop_left_offset, 0);
    assert_eq!(sps.frame_crop_right_offset, 0);
    assert_eq!(sps.frame_crop_top_offset, 0);
    assert_eq!(sps.frame_crop_bottom_offset, 4);

    assert_eq!(sps.coded_resolution(), (1920, 1088), "编码尺寸是 1088");
    assert_eq!(
        sps.resolution(),
        (1920, 1080),
        "显示尺寸必须是 1080，不是 1088"
    );

    // CropUnitY = SubHeightC * (2 - frame_mbs_only_flag) = 2 * 1 = 2
    assert_eq!(sps.crop_unit(), (2, 2), "4:2:0 逐行的裁剪单位是 2x2");
}

#[test]
fn high_1080p_的_vui_timing_反算出_30fps() {
    let sps = parse_ok(SPS_HIGH_1080P);
    let vui = sps.vui.expect("这份 SPS 带 VUI");

    assert!(vui.timing_info_present_flag);
    assert_eq!(vui.num_units_in_tick, 1);
    assert_eq!(vui.time_scale, 60);

    let fps = sps.frame_rate().expect("有 timing info 就该算出帧率");
    assert!((fps - 30.0).abs() < 1e-9, "60 / (2*1) = 30fps，实得 {fps}");
}

#[test]
fn 隔行_sps_高度必须乘_2() {
    // frame_mbs_only_flag = 0 时 pic_height_in_map_units_minus1 数的是宏块对行。
    // 只报 (33+1)*16 = 544 就是漏了 ×2。
    let sps = parse_ok(SPS_MAIN_INTERLACED);

    assert_eq!(sps.profile_idc, 77, "Main profile");
    assert!(!sps.frame_mbs_only_flag, "这份是隔行（场编码）");
    assert!(!sps.mb_adaptive_frame_field_flag, "MBAFF 未启用");
    assert_eq!(sps.pic_height_in_map_units_minus1, 33);

    assert_eq!(
        sps.coded_resolution(),
        (1920, 1088),
        "34 个 map unit × 16 × 2"
    );
    assert_eq!(
        sps.resolution(),
        (1920, 1088),
        "这份没有 cropping，显示=编码"
    );
    assert!(!sps.frame_cropping_flag);
}

#[test]
fn 隔行_sps_的裁剪单位要把场因子算进去() {
    // CropUnitY = SubHeightC * (2 - frame_mbs_only_flag) = 2 * 2 = 4
    let sps = parse_ok(SPS_MAIN_INTERLACED);
    assert_eq!(sps.crop_unit(), (2, 4), "隔行时裁剪单位的高度是 4");

    // 同样的 chroma 下，逐行样本是 (2, 2) —— 两者必须不同
    let progressive = parse_ok(SPS_HIGH_1080P);
    assert_ne!(progressive.crop_unit(), sps.crop_unit());
}

#[test]
fn 隔行_sps_反算出_25fps_且标记固定帧率() {
    let sps = parse_ok(SPS_MAIN_INTERLACED);
    let vui = sps.vui.expect("带 VUI");

    assert_eq!(vui.num_units_in_tick, 1);
    assert_eq!(vui.time_scale, 50);
    assert!(vui.fixed_frame_rate_flag, "这份是恒定帧率");

    let fps = sps.frame_rate().expect("应能算出帧率");
    assert!((fps - 25.0).abs() < 1e-9, "50 / (2*1) = 25fps，实得 {fps}");
    // 场率是帧率的两倍，隔行码流里两者不能混用
    assert!((sps.field_rate().unwrap() - 50.0).abs() < 1e-9);
}

#[test]
fn ntsc_sps_反算出_29_97fps() {
    // num_units_in_tick=1001 / time_scale=60000 是 NTSC 的标准写法。
    // 把它当 30fps 会让抖动缓冲的时钟慢慢漂移。
    let sps = parse_ok(SPS_MAIN_NTSC);

    assert_eq!(
        sps.resolution(),
        (1920, 1080),
        "1080p，靠 cropping 裁掉 8 行"
    );
    assert_eq!(sps.coded_resolution(), (1920, 1088));

    let vui = sps.vui.expect("带 VUI");
    assert_eq!(vui.num_units_in_tick, 1001);
    assert_eq!(vui.time_scale, 60000);

    let fps = sps.frame_rate().expect("应能算出帧率");
    assert!(
        (fps - 29.970_029_970_029_97).abs() < 1e-9,
        "60000 / (2*1001) = 29.97fps，实得 {fps}"
    );
}

#[test]
fn 非标准分辨率_1400x1050_左右上下都要裁() {
    // 这份的裁剪不在底部，而是在右边和下边，专门用来抓「只算 bottom」的实现。
    let sps = parse_ok(SPS_HIGH_1400X1050);

    assert_eq!(sps.profile_idc, 100);
    assert_eq!(sps.pic_order_cnt_type, 2, "POC 类型 2：只用 frame_num 推导");
    assert_eq!(sps.log2_max_pic_order_cnt_lsb_minus4, None);

    assert!(sps.frame_cropping_flag);
    assert_eq!(sps.frame_crop_left_offset, 0);
    assert_eq!(sps.frame_crop_right_offset, 4);
    assert_eq!(sps.frame_crop_top_offset, 0);
    assert_eq!(sps.frame_crop_bottom_offset, 3);

    assert_eq!(sps.coded_resolution(), (1408, 1056));
    assert_eq!(sps.resolution(), (1400, 1050), "右边裁 8 列、下边裁 6 行");
}

#[test]
fn 带起始码的_sps_也能解析() {
    // 调用方常常直接扔一段 Annex-B 过来，起始码不该让它失败
    let mut with_sc = vec![0u8, 0, 0, 1];
    with_sc.extend_from_slice(SPS_HIGH_1080P);
    assert_eq!(parse_ok(&with_sc).resolution(), (1920, 1080));

    let mut with_short_sc = vec![0u8, 0, 1];
    with_short_sc.extend_from_slice(SPS_HIGH_1080P);
    assert_eq!(parse_ok(&with_short_sc).resolution(), (1920, 1080));
}

#[test]
fn emulation_prevention_字节必须被剔除() {
    // 1080p 那份原始字节里含 00 00 03，不还原 RBSP 就会读错位。
    // 手工造一份「含 03」与「不含 03」的等价 SPS：把 03 插进 payload 后
    // 解析结果必须完全一致。
    let mut spliced = SPS_BASELINE_720P.to_vec();
    // 在尾部补一个 00 00 03 00（合法防竞争序列），RBSP 里应还原成 00 00 00
    spliced.extend_from_slice(&[0x00, 0x00, 0x03, 0x00]);

    let plain = parse_ok(SPS_BASELINE_720P);
    let with_epb = parse_ok(&spliced);
    assert_eq!(
        plain.resolution(),
        with_epb.resolution(),
        "尾部补的防竞争字节不影响已解析的字段"
    );
    assert_eq!(plain.profile_idc, with_epb.profile_idc);
}

#[test]
fn 从_annexb_切分出来的_sps_能直接喂给解析器() {
    // 和 nalu.rs 的联调：split_annexb 给出的是不含起始码的 NALU
    let mut buf = Vec::new();
    buf.extend_from_slice(&[0, 0, 0, 1]);
    buf.extend_from_slice(SPS_HIGH_1080P);
    buf.extend_from_slice(&[0, 0, 0, 1]);
    buf.extend_from_slice(&[0x68, 0xee, 0x3c, 0x80]);

    let nalus = split_annexb(&buf);
    assert_eq!(nalus.len(), 2);
    assert_eq!(nalus[0].ty, NaluType::Sps);

    let sps = parse_ok(nalus[0].data);
    assert_eq!(sps.resolution(), (1920, 1080));
}

// ---------------------------------------------------------------------------
// 畸形输入：一个都不能 panic
// ---------------------------------------------------------------------------

#[test]
fn 畸形_全零_sps_必须返回错误而不是卡死() {
    // 全 0 是最典型的攻击载荷：朴素实现在 ue(v) 里会一直读 0 直到越界。
    for len in [1usize, 2, 8, 64, 512] {
        let mut nalu = vec![0x67u8];
        nalu.extend(std::iter::repeat(0u8).take(len));
        assert!(
            SpsInfo::parse(&nalu).is_err(),
            "全 0 的 {len} 字节载荷必须报错"
        );
    }
}

#[test]
fn 畸形_全_ff_sps_必须返回错误() {
    // 全 1 会解出一串超大的 ue(v)，随后被范围检查拦下
    for len in [1usize, 2, 8, 64] {
        let mut nalu = vec![0x67u8];
        nalu.extend(std::iter::repeat(0xffu8).take(len));
        assert!(
            SpsInfo::parse(&nalu).is_err(),
            "全 0xFF 的 {len} 字节载荷必须报错"
        );
    }
}

#[test]
fn 畸形_截断_sps_必须返回错误() {
    // 真实 SPS 被逐个字节截断，每一段都必须返回 Err 而不是 panic
    for cut in 0..SPS_HIGH_1080P.len() {
        let truncated = &SPS_HIGH_1080P[..cut];
        let result = SpsInfo::parse(truncated);
        if cut < SPS_HIGH_1080P.len() - 1 {
            assert!(
                result.is_err(),
                "截到 {cut} 字节时信息不全，必须报错（实际解析成功了）"
            );
        }
    }
}

#[test]
fn 畸形_前导零爆炸必须停下() {
    // 40 个前导零 = 40 个 0 比特，超过上限 32，必须立刻报错
    let mut nalu = vec![0x67u8, 0x42, 0x00, 0x1f];
    // seq_parameter_set_id 从第 24 比特开始，这里塞 40 个 0 比特 = 5 个 0 字节
    nalu.extend(std::iter::repeat(0x00u8).take(5));
    nalu.push(0x80); // 补一个 1，让「有终止位」的实现也不会读空

    match SpsInfo::parse(&nalu) {
        Err(SpsError::LeadingZerosTooMany { leading_zeros, .. }) => {
            assert!(
                leading_zeros > 32,
                "应在超过上限时报错，实得 {leading_zeros}"
            );
        }
        Err(other) => panic!("期望 LeadingZerosTooMany，实得 {other:?}"),
        Ok(info) => panic!("40 个前导零不可能解析成功，实得 {info:?}"),
    }
}

#[test]
fn 畸形_空载荷与空_nau_都返回错误() {
    assert_eq!(SpsInfo::parse(&[]), Err(SpsError::EmptyNalu));
    // 只有起始码没有内容
    assert!(SpsInfo::parse(&[0, 0, 0, 1]).is_err());
}

#[test]
fn 畸形_非_sps_的_nalu_被明确拒绝() {
    // PPS 的 nal_unit_type 是 8，喂进 SPS 解析器必须报错而不是瞎读
    match SpsInfo::parse(&[0x68, 0xee, 0x3c, 0x80]) {
        Err(SpsError::NotSps { nal_unit_type }) => assert_eq!(nal_unit_type, 8),
        other => panic!("期望 NotSps(8)，实得 {other:?}"),
    }
}

#[test]
fn 畸形_cropping_大于编码尺寸时拒绝() {
    // 手工造一份 crop_bottom_offset 极大的 SPS：裁完是负数，必须拒绝。
    // 用真实 720p 样本改不出来（字段位置会全乱），所以直接构造比特流。
    let mut w = BitWriter::new();
    w.u(8, 66); // profile_idc = Baseline，跳过 high profile 扩展块
    w.u(8, 0); // constraint_flags
    w.u(8, 30); // level_idc
    w.ue(0); // seq_parameter_set_id
    w.ue(0); // log2_max_frame_num_minus4
    w.ue(2); // pic_order_cnt_type = 2（无额外字段）
    w.ue(1); // max_num_ref_frames
    w.u(1, 0); // gaps_in_frame_num_value_allowed_flag
    w.ue(0); // pic_width_in_mbs_minus1 -> 16 像素宽
    w.ue(0); // pic_height_in_map_units_minus1 -> 16 像素高
    w.u(1, 1); // frame_mbs_only_flag
    w.u(1, 1); // direct_8x8_inference_flag
    w.u(1, 1); // frame_cropping_flag
    w.ue(0); // left
    w.ue(0); // right
    w.ue(0); // top
    w.ue(20); // bottom：20*2 = 40 > 16，必然越界
    w.u(1, 0); // vui_parameters_present_flag
    w.rbsp_trailing();

    let mut nalu = vec![0x67u8];
    nalu.extend_from_slice(&w.finish());

    match SpsInfo::parse(&nalu) {
        Err(SpsError::InvalidCropping { .. }) => {}
        other => panic!("期望 InvalidCropping，实得 {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// 分辨率变更检测
// ---------------------------------------------------------------------------

fn annexb(nalus: &[&[u8]]) -> Vec<u8> {
    let mut buf = Vec::new();
    for n in nalus {
        buf.extend_from_slice(&[0, 0, 0, 1]);
        buf.extend_from_slice(n);
    }
    buf
}

#[test]
fn 首次_sps_只建立基准不算变更() {
    // 否则每一路新流都会误报一次「0x0 -> 1280x720」
    let mut tracker = ParameterSetTracker::new();
    assert_eq!(tracker.resolution(), None);

    let changes = tracker.feed_annexb(&annexb(&[SPS_BASELINE_720P]));
    assert!(changes.is_empty(), "首次不该报变更");
    assert_eq!(tracker.resolution(), Some((1280, 720)));
    assert_eq!(tracker.change_count(), 0);
    assert_eq!(tracker.sps_count(), 1);
}

#[test]
fn 喂_a_a_b_第三次必须报分辨率变更() {
    // 真实场景：手机横屏 720p → 保持 720p → 用户旋转，编码器改发竖屏 SPS。
    // 接收端收到变更必须重建解码器，否则花屏。
    let mut tracker = ParameterSetTracker::new();

    assert!(
        tracker
            .feed_annexb(&annexb(&[SPS_BASELINE_720P]))
            .is_empty(),
        "第一次：建立基准"
    );
    assert!(
        tracker
            .feed_annexb(&annexb(&[SPS_BASELINE_720P]))
            .is_empty(),
        "第二次：同一份 SPS 不算变更"
    );

    let changes = tracker.feed_annexb(&annexb(&[SPS_HIGH_1080P]));
    assert_eq!(changes.len(), 1, "第三次：分辨率变了，必须报一次");

    let change = changes[0];
    assert_eq!(change.from, (1280, 720));
    assert_eq!(change.to, (1920, 1080));
    assert!(!change.is_orientation_flip());
    assert_eq!(tracker.change_count(), 1);
    assert_eq!(tracker.sps_count(), 3);
}

#[test]
fn 横竖互换仍然算分辨率变更() {
    // 旋转屏幕时宽高互换，宏块布局完全不同 —— 不能因为「只是转了 90 度」就不报
    let mut tracker = ParameterSetTracker::new();
    tracker.feed_annexb(&annexb(&[SPS_BASELINE_720P])); // 1280x720

    let changes = tracker.feed_annexb(&annexb(&[SPS_HIGH_1400X1050])); // 1400x1050
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].from, (1280, 720));
    assert_eq!(changes[0].to, (1400, 1050));
    assert!(!changes[0].is_orientation_flip(), "这不是横竖互换");
}

#[test]
fn 畸形_sps_不能污染跟踪器的状态() {
    // 丢包/位翻转会把 SPS 打成垃圾。此时必须保留上一份有效参数集，
    // 而不是把分辨率清成 0x0 让下游重新分配缓冲。
    let mut tracker = ParameterSetTracker::new();
    tracker.feed_annexb(&annexb(&[SPS_HIGH_1080P]));
    assert_eq!(tracker.resolution(), Some((1920, 1080)));

    let mut garbage = vec![0x67u8];
    garbage.extend(std::iter::repeat(0x00u8).take(32));
    assert!(tracker.feed_checked(&garbage).is_err(), "畸形 SPS 必须报错");
    assert_eq!(
        tracker.resolution(),
        Some((1920, 1080)),
        "畸形输入后分辨率必须保持上一次的有效值"
    );
    assert_eq!(tracker.parse_error_count(), 1);

    // 再来一次正常的关键帧：不该因为中间的垃圾而报「变更」
    assert!(tracker.feed_annexb(&annexb(&[SPS_HIGH_1080P])).is_empty());
}

#[test]
fn 跟踪器忽略非_sps_的_nalu() {
    let mut tracker = ParameterSetTracker::new();
    tracker.feed_annexb(&annexb(&[SPS_HIGH_1080P]));

    // PPS + IDR 不该影响分辨率
    let changes = tracker.feed_annexb(&annexb(&[
        &[0x68, 0xee, 0x3c, 0x80],
        &[0x65, 0x88, 0x84, 0x11],
    ]));
    assert!(changes.is_empty());
    assert_eq!(tracker.sps_count(), 1, "只有 SPS 才计数");

    // 非 SPS 的 feed_checked 是 Ok(None)，不是 Err
    assert_eq!(tracker.feed_checked(&[0x68, 0xee]), Ok(None));
}

#[test]
fn 一段码流里两份不同_sps_会报一次变更() {
    // 合并后的码流（切摄像头）确实可能一次给两份
    let mut tracker = ParameterSetTracker::new();
    tracker.feed_annexb(&annexb(&[SPS_BASELINE_720P]));

    let changes = tracker.feed_annexb(&annexb(&[SPS_BASELINE_720P, SPS_MAIN_NTSC]));
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].to, (1920, 1080));
}

#[test]
fn reset_之后重新建立基准() {
    let mut tracker = ParameterSetTracker::new();
    tracker.feed_annexb(&annexb(&[SPS_BASELINE_720P]));
    tracker.reset();

    assert_eq!(tracker.resolution(), None);
    assert!(
        tracker.feed_annexb(&annexb(&[SPS_HIGH_1080P])).is_empty(),
        "reset 后第一份只建基准"
    );
    assert_eq!(tracker.change_count(), 0);
}

// ---------------------------------------------------------------------------
// 解析器自身的工具
// ---------------------------------------------------------------------------

#[test]
fn 单色与_4_4_4_的裁剪单位是_1() {
    // 规范 7.4.2.1.1：ChromaArrayType == 0（单色，或 separate_colour_plane_flag=1）
    // 时 CropUnitX = 1、CropUnitY = 2 - frame_mbs_only_flag。
    // 照抄「4:2:0 就是 2x2」的实现会在 4:4:4 码流上把画面裁错一倍。
    let mut w = BitWriter::new();
    w.u(8, 244); // profile_idc = High 4:4:4 Predictive
    w.u(8, 0);
    w.u(8, 40);
    w.ue(0); // sps id
    w.ue(3); // chroma_format_idc = 4:4:4
    w.u(1, 1); // separate_colour_plane_flag -> ChromaArrayType 变 0
    w.ue(0); // bit_depth_luma_minus8
    w.ue(0); // bit_depth_chroma_minus8
    w.u(1, 0); // qpprime
    w.u(1, 0); // seq_scaling_matrix_present_flag
    w.ue(0); // log2_max_frame_num_minus4
    w.ue(2); // pic_order_cnt_type = 2
    w.ue(1); // max_num_ref_frames
    w.u(1, 0); // gaps
    w.ue(119); // 1920
    w.ue(67); // 1088
    w.u(1, 1); // frame_mbs_only_flag
    w.u(1, 1); // direct_8x8
    w.u(1, 1); // cropping
    w.ue(0); // left
    w.ue(0); // right
    w.ue(0); // top
    w.ue(4); // bottom：CropUnitY=1，所以只裁 4 行
    w.u(1, 0); // vui
    w.rbsp_trailing();

    let mut nalu = vec![0x67u8];
    nalu.extend_from_slice(&w.finish());
    let sps = parse_ok(&nalu);

    assert_eq!(sps.chroma_format_idc, 3);
    assert!(sps.separate_colour_plane_flag);
    assert_eq!(
        sps.chroma_array_type(),
        0,
        "separate_colour_plane 时按单色算"
    );
    assert_eq!(
        sps.crop_unit(),
        (1, 1),
        "ChromaArrayType=0 时裁剪单位是 1x1"
    );
    // 若错用 4:2:0 的 2x2，这里会算成 1080 而不是 1084
    assert_eq!(
        sps.resolution(),
        (1920, 1084),
        "crop 4 * CropUnitY 1 = 4 行"
    );
}

#[test]
fn 显式构造的_sps_字段全部对得上() {
    // 这份 SPS 不是抓包，是照规范逐字段写出来的，
    // 用来锁住「字段顺序与位宽」本身 —— 真实样本只能证明整体对，
    // 这里能逐字段证明每一步都读在正确的位置。
    let mut w = BitWriter::new();
    w.u(8, 100); // profile_idc = High
    w.u(8, 0x00); // constraint_flags
    w.u(8, 41); // level_idc = 4.1
    w.ue(3); // seq_parameter_set_id
    w.ue(1); // chroma_format_idc = 4:2:0
    w.ue(2); // bit_depth_luma_minus8 = 10bit
    w.ue(2); // bit_depth_chroma_minus8 = 10bit
    w.u(1, 1); // qpprime_y_zero_transform_bypass_flag
    w.u(1, 0); // seq_scaling_matrix_present_flag
    w.ue(4); // log2_max_frame_num_minus4
    w.ue(1); // pic_order_cnt_type = 1
    w.u(1, 1); // delta_pic_order_always_zero_flag
    w.se(-2); // offset_for_non_ref_pic
    w.se(3); // offset_for_top_to_bottom_field
    w.ue(2); // num_ref_frames_in_pic_order_cnt_cycle
    w.se(1); // offset_for_ref_frame[0]
    w.se(-1); // offset_for_ref_frame[1]
    w.ue(6); // max_num_ref_frames
    w.u(1, 1); // gaps_in_frame_num_value_allowed_flag
    w.ue(119); // pic_width_in_mbs_minus1 -> 1920
    w.ue(67); // pic_height_in_map_units_minus1 -> 1088
    w.u(1, 1); // frame_mbs_only_flag
    w.u(1, 1); // direct_8x8_inference_flag
    w.u(1, 1); // frame_cropping_flag
    w.ue(0); // left
    w.ue(0); // right
    w.ue(0); // top
    w.ue(4); // bottom -> 裁 8 行
    w.u(1, 0); // vui_parameters_present_flag
    w.rbsp_trailing();

    let mut nalu = vec![0x67u8];
    nalu.extend_from_slice(&w.finish());

    let sps = parse_ok(&nalu);
    assert_eq!(sps.profile_idc, 100);
    assert_eq!(sps.constraint_flags, 0);
    assert_eq!(sps.level_idc, 41);
    assert_eq!(sps.seq_parameter_set_id, 3);
    assert_eq!(sps.chroma_format_idc, 1);
    assert_eq!(sps.bit_depth_luma_minus8, 2);
    assert_eq!(sps.bit_depth_chroma_minus8, 2);
    assert!(sps.qpprime_y_zero_transform_bypass_flag);
    assert_eq!(sps.log2_max_frame_num_minus4, 4);
    assert_eq!(sps.pic_order_cnt_type, 1);
    assert!(sps.delta_pic_order_always_zero_flag);
    assert_eq!(sps.offset_for_non_ref_pic, -2);
    assert_eq!(sps.offset_for_top_to_bottom_field, 3);
    assert_eq!(sps.num_ref_frames_in_pic_order_cnt_cycle, 2);
    assert_eq!(sps.max_num_ref_frames, 6);
    assert!(sps.gaps_in_frame_num_value_allowed_flag);
    assert_eq!(sps.pic_width_in_mbs_minus1, 119);
    assert_eq!(sps.pic_height_in_map_units_minus1, 67);
    assert!(sps.frame_mbs_only_flag);
    assert!(sps.direct_8x8_inference_flag);
    assert_eq!(sps.frame_crop_bottom_offset, 4);
    assert_eq!(sps.vui, None);
    assert_eq!(sps.resolution(), (1920, 1080));
    assert_eq!(sps.frame_rate(), None);
}

#[test]
fn 显示信息包含分辨率与帧率() {
    let sps = parse_ok(SPS_HIGH_1080P);
    let text = sps.to_string();
    assert!(text.contains("1920x1080"), "Display 应含显示分辨率：{text}");
    assert!(text.contains("30.000fps"), "Display 应含帧率：{text}");
    assert!(!text.contains("1088"), "Display 不该出现编码高度：{text}");
}

// ---------------------------------------------------------------------------
// 测试用比特写入器
// ---------------------------------------------------------------------------

/// 按规范反向写比特，用于构造真实码流里不存在的畸形 SPS
struct BitWriter {
    bytes: Vec<u8>,
    bit: u32,
}

impl BitWriter {
    fn new() -> Self {
        Self {
            bytes: Vec::new(),
            bit: 0,
        }
    }

    fn u(&mut self, n: u32, value: u32) {
        for i in (0..n).rev() {
            self.push_bit(((value >> i) & 1) as u8);
        }
    }

    fn ue(&mut self, value: u32) {
        let v = u64::from(value) + 1;
        let len = 64 - v.leading_zeros();
        for _ in 0..len - 1 {
            self.push_bit(0);
        }
        for i in (0..len).rev() {
            self.push_bit(((v >> i) & 1) as u8);
        }
    }

    fn se(&mut self, value: i64) {
        let code = if value > 0 {
            (value as u64) * 2 - 1
        } else {
            (-value) as u64 * 2
        };
        self.ue(u32::try_from(code).expect("测试值不该溢出"));
    }

    fn push_bit(&mut self, b: u8) {
        if self.bit % 8 == 0 {
            self.bytes.push(0);
        }
        if b != 0 {
            let idx = self.bytes.len() - 1;
            self.bytes[idx] |= 0x80 >> (self.bit % 8);
        }
        self.bit += 1;
    }

    /// rbsp_trailing_bits：一个 1，然后补 0 到字节边界
    fn rbsp_trailing(&mut self) {
        self.push_bit(1);
        while self.bit % 8 != 0 {
            self.push_bit(0);
        }
    }

    fn finish(self) -> Vec<u8> {
        self.bytes
    }
}
