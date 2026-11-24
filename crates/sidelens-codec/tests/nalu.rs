//! NALU 码流处理测试
//!
//! 用人工构造的 H.264 片段验证切分逻辑 —— 不需要真实编码器。

use sidelens_codec::{
    annexb_to_avcc, avcc_to_annexb, is_keyframe, split_annexb, NaluType, ParameterSets,
};

/// 构造一个 NALU（4 字节起始码 + 头 + 负载）
fn nalu4(header: u8, payload: &[u8]) -> Vec<u8> {
    let mut v = vec![0, 0, 0, 1, header];
    v.extend_from_slice(payload);
    v
}

/// 构造一个 NALU（3 字节起始码）
fn nalu3(header: u8, payload: &[u8]) -> Vec<u8> {
    let mut v = vec![0, 0, 1, header];
    v.extend_from_slice(payload);
    v
}

#[test]
fn parses_single_nalu() {
    let buf = nalu4(0x65, &[0xAA; 10]); // 0x65 = IDR
    let nalus = split_annexb(&buf);
    assert_eq!(nalus.len(), 1);
    assert_eq!(nalus[0].ty, NaluType::IdrSlice);
    assert_eq!(nalus[0].len(), 11, "含 NALU 头，不含起始码");
}

#[test]
fn parses_multiple_nalus() {
    let mut buf = nalu4(0x67, &[0x11; 4]); // SPS
    buf.extend(nalu4(0x68, &[0x22; 4])); // PPS
    buf.extend(nalu4(0x65, &[0x33; 20])); // IDR

    let nalus = split_annexb(&buf);
    assert_eq!(nalus.len(), 3);
    assert_eq!(nalus[0].ty, NaluType::Sps);
    assert_eq!(nalus[1].ty, NaluType::Pps);
    assert_eq!(nalus[2].ty, NaluType::IdrSlice);
}

#[test]
fn handles_three_byte_start_codes() {
    // 真实码流里 3 字节起始码很常见，只认 4 字节的实现会丢一半 NALU
    let mut buf = nalu3(0x67, &[0x11; 4]);
    buf.extend(nalu3(0x68, &[0x22; 4]));
    buf.extend(nalu3(0x41, &[0x33; 10])); // 非 IDR

    let nalus = split_annexb(&buf);
    assert_eq!(nalus.len(), 3, "3 字节起始码必须被识别");
    assert_eq!(nalus[0].ty, NaluType::Sps);
    assert_eq!(nalus[2].ty, NaluType::NonIdrSlice);
}

#[test]
fn handles_mixed_start_code_lengths() {
    let mut buf = nalu4(0x67, &[0x11; 4]);
    buf.extend(nalu3(0x68, &[0x22; 4]));
    buf.extend(nalu4(0x65, &[0x33; 10]));

    let nalus = split_annexb(&buf);
    assert_eq!(nalus.len(), 3);
    assert_eq!(nalus[2].ty, NaluType::IdrSlice);
}

#[test]
fn empty_input_yields_nothing() {
    assert!(split_annexb(&[]).is_empty());
}

#[test]
fn input_without_start_code_yields_nothing() {
    // # 这条测试的断言被翻转过（实测抓到的真 bug）
    //
    // 原断言：「无起始码的输入当成一个 NALU」。
    // 但代码**无法区分**这两种情况：
    //
    // - 真的是一帧没有起始码的裸 NALU
    // - 一段不含起始码的碎片（UDP 分片中途、丢包残片、AVCC 误喂）
    //
    // 后果不对称：把碎片当 NALU 时，只要首字节低 5 位碰巧是 7，
    // 就会被 ParameterSets 缓存成 SPS，再前置到关键帧前发给对端 →
    // **整路视频黑屏**。而真正的「省略起始码」场景在真实码流里
    // 几乎不存在（Annex-B 的定义就是带起始码）。
    //
    // 所以选安全的一边。见 tests/nalu_regression.rs。
    let buf = vec![0x65, 0xAA, 0xBB];
    let nalus = split_annexb(&buf);
    assert!(
        nalus.is_empty(),
        "无起始码的输入不应产出 NALU，实际 {} 个",
        nalus.len()
    );
}

#[test]
fn keyframe_detection_scans_all_nalus() {
    // IDR 不在第一个 —— 只看首个 NALU 的实现会漏判
    let mut buf = nalu4(0x67, &[0x11; 4]); // SPS
    buf.extend(nalu4(0x68, &[0x22; 4])); // PPS
    buf.extend(nalu4(0x65, &[0x33; 10])); // IDR
    assert!(is_keyframe(&buf), "SPS/PPS 在前、IDR 在后也算关键帧");

    let p_frame = nalu4(0x41, &[0x33; 10]);
    assert!(!is_keyframe(&p_frame), "非 IDR 不是关键帧");
}

#[test]
fn nalu_type_classification() {
    assert_eq!(NaluType::from_header(0x65), NaluType::IdrSlice);
    assert_eq!(NaluType::from_header(0x41), NaluType::NonIdrSlice);
    assert_eq!(NaluType::from_header(0x67), NaluType::Sps);
    assert_eq!(NaluType::from_header(0x68), NaluType::Pps);
    assert_eq!(NaluType::from_header(0x06), NaluType::Sei);
    assert_eq!(NaluType::from_header(0x09), NaluType::Aud);

    assert!(NaluType::IdrSlice.is_keyframe());
    assert!(!NaluType::NonIdrSlice.is_keyframe());
    assert!(NaluType::Sps.is_parameter_set());
    assert!(NaluType::IdrSlice.is_slice());
    assert!(!NaluType::Sei.is_slice());
}

#[test]
fn parameter_sets_are_captured() {
    let mut ps = ParameterSets::new();
    assert!(!ps.is_complete());

    let mut buf = nalu4(0x67, &[0x11; 8]);
    buf.extend(nalu4(0x68, &[0x22; 4]));

    assert!(ps.update_from(&buf), "首次提取应报告变化");
    assert!(ps.is_complete());
    assert_eq!(ps.sps().unwrap().len(), 9);
    assert_eq!(ps.pps().unwrap().len(), 5);

    // 同样的参数集再喂一次：不应报告变化
    assert!(!ps.update_from(&buf), "内容未变不应报告变化");
}

#[test]
fn parameter_set_update_is_detected() {
    let mut ps = ParameterSets::new();
    ps.update_from(&nalu4(0x67, &[0x11; 8]));

    // 分辨率变化 → SPS 变了
    assert!(
        ps.update_from(&nalu4(0x67, &[0x99; 8])),
        "SPS 变化应被检测到"
    );
    assert_eq!(ps.sps().unwrap()[1], 0x99);
}

#[test]
fn parameter_sets_serialize_to_annexb() {
    let mut ps = ParameterSets::new();
    ps.update_from(&{
        let mut b = nalu4(0x67, &[0x11; 4]);
        b.extend(nalu4(0x68, &[0x22; 4]));
        b
    });

    let out = ps.to_annexb();
    let reparsed = split_annexb(&out);
    assert_eq!(reparsed.len(), 2);
    assert_eq!(reparsed[0].ty, NaluType::Sps);
    assert_eq!(reparsed[1].ty, NaluType::Pps);
}

#[test]
fn annexb_avcc_roundtrip() {
    let mut buf = nalu4(0x67, &[0x11; 4]);
    buf.extend(nalu4(0x65, &[0x33; 20]));

    let avcc = annexb_to_avcc(&buf);
    // AVCC 用 4 字节长度前缀，没有起始码
    let first_len = u32::from_be_bytes([avcc[0], avcc[1], avcc[2], avcc[3]]) as usize;
    assert_eq!(first_len, 5, "第一个 NALU 是 1 字节头 + 4 字节负载");

    let back = avcc_to_annexb(&avcc).expect("应能转回");
    let nalus = split_annexb(&back);
    assert_eq!(nalus.len(), 2);
    assert_eq!(nalus[1].ty, NaluType::IdrSlice);
}

#[test]
fn avcc_rejects_truncated_input() {
    // 长度前缀声称 100 字节，实际只有 3 字节
    let mut buf = vec![0, 0, 0, 100];
    buf.extend_from_slice(&[1, 2, 3]);
    assert!(
        avcc_to_annexb(&buf).is_none(),
        "越界应返回 None 而不是 panic"
    );
}

#[test]
fn trailing_zero_bytes_are_trimmed_beyond_two() {
    // 真实码流里 NALU 之间常有尾随 0 填充。
    //
    // # 但最多保留 2 个（实测抓到的真 bug）
    //
    // H.264 规范 7.4.1 允许 CABAC 码流在 NALU 末尾追加 `cabac_zero_word`
    // （就是 `00 00`），那是**切片数据的一部分**。全裁掉会让 NALU 长度
    // 与内部 slice 长度不一致 → 硬件解码器报错或花屏。
    //
    // 现在的规则：保留最多 2 个，超过的才算填充。
    let mut buf = nalu4(0x67, &[0x11; 4]);
    buf.extend_from_slice(&[0, 0, 0, 0]); // 4 个尾随 0
    buf.extend(nalu4(0x65, &[0x33; 10]));

    let nalus = split_annexb(&buf);
    assert_eq!(nalus.len(), 2);
    assert_eq!(
        nalus[0].len(),
        7,
        "应保留 5 字节数据 + 2 字节 cabac_zero_word，实际 {}",
        nalus[0].len()
    );
}
