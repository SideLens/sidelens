//! nalu.rs 的三个实测 bug 的回归测试
//!
//! # 这些测试守的是「静默的数据污染」
//!
//! 三个 bug 都不会让程序崩溃、不会报错、不会打印异常 ——
//! 它们只是让**错误的数据流过去**。所以必须用测试钉住行为。

#![allow(non_snake_case)]

use sidelens_codec::nalu::{split_annexb, NaluType, ParameterSets};

// ─────────────── Bug 1：无起始码的碎片被当 NALU ───────────────

#[test]
fn 无起始码的输入必须返回空() {
    // 这是最危险的一个：一段 UDP 分片中途的字节，首字节 0x67 碰巧
    // 低 5 位是 7（SPS 类型），原实现会把它当 SPS 缓存下来。
    let fragment = [0x67u8, 0x1f, 0x2a, 0x88, 0x99];
    let nalus = split_annexb(&fragment);
    assert!(
        nalus.is_empty(),
        "无起始码的碎片被当成了 {} 个 NALU（类型 {:?}）",
        nalus.len(),
        nalus.first().map(|n| n.ty)
    );
}

#[test]
fn 碎片不得污染参数集缓存() {
    // 完整的攻击链：碎片 → 被当 SPS → 缓存 → 前置到关键帧前发给对端
    let mut ps = ParameterSets::new();
    let garbage = [0x67u8, 0x1f, 0x2a, 0x88, 0x99];
    let changed = ps.update_from(&garbage);
    assert!(!changed, "垃圾碎片被当成了参数集更新");
    assert!(ps.sps().is_none(), "垃圾碎片污染了 SPS 缓存");
    assert!(
        ps.to_annexb().is_empty(),
        "垃圾被拼进了待转发的前缀：{:02x?}",
        ps.to_annexb()
    );
}

#[test]
fn 真起始码仍然正常切分() {
    // 修复不能把正常路径也弄坏
    let stream = [0, 0, 0, 1, 0x67, 0x42, 0x00, 0x1f, 0, 0, 0, 1, 0x68, 0xce];
    let nalus = split_annexb(&stream);
    assert_eq!(nalus.len(), 2);
    assert_eq!(nalus[0].ty, NaluType::Sps);
    assert_eq!(nalus[1].ty, NaluType::Pps);
}

// ─────────────── 设计缺陷：前置填充 0 产生幽灵 NALU ───────────────

#[test]
fn 前置填充零不得产生幽灵_nalu() {
    let stream = [0u8, 0, 0, 0, 1, 0x67, 0x42, 0x00, 0x1f];
    let nalus = split_annexb(&stream);
    assert_eq!(
        nalus.len(),
        1,
        "前置 0 产生了幽灵 NALU：{:?}",
        nalus.iter().map(|n| n.ty).collect::<Vec<_>>()
    );
    assert_eq!(nalus[0].ty, NaluType::Sps);
}

#[test]
fn 前置非零数据仍然算一个_nalu() {
    // 修复不能把「真有前置数据」也丢掉
    let stream = [0x09u8, 0xf0, 0, 0, 0, 1, 0x67, 0x42, 0x00, 0x1f];
    let nalus = split_annexb(&stream);
    assert_eq!(nalus.len(), 2, "前置的 AUD 被误删了");
    assert_eq!(nalus[0].ty, NaluType::Aud);
}

// ─────────────── Bug 2：cabac_zero_word 被裁掉 ───────────────

#[test]
fn cabac_zero_word_不得被裁掉() {
    // 规范 7.4.1 允许 CABAC 码流末尾追加 00 00，那是切片数据的一部分。
    // 裁掉会让 NALU 长度与内部 slice 长度不一致。
    let stream = [
        0, 0, 0, 1, 0x65, 0x88, 0x84, 0x00, 0x00, // IDR + cabac_zero_word
        0, 0, 0, 1, 0x41, 0x22,
    ];
    let nalus = split_annexb(&stream);
    assert_eq!(nalus.len(), 2);
    assert_eq!(
        nalus[0].data.len(),
        5,
        "IDR 长度应为 5（含 cabac_zero_word），实际 {} → {:02x?}",
        nalus[0].data.len(),
        nalus[0].data
    );
    assert_eq!(nalus[0].data, &[0x65, 0x88, 0x84, 0x00, 0x00]);
}

#[test]
fn 额外的填充零仍然被裁() {
    // 超过 2 个的 0 才是填充，必须裁掉
    let stream = [
        0, 0, 0, 1, 0x65, 0x88, 0x84, 0x00, 0x00, 0x00, 0x00, 0x00, // 末尾 5 个 0
        0, 0, 0, 1, 0x41, 0x22,
    ];
    let nalus = split_annexb(&stream);
    assert_eq!(nalus.len(), 2);
    assert_eq!(
        nalus[0].data.len(),
        5,
        "应保留 3 字节数据 + 2 字节 cabac_zero_word，实际 {} → {:02x?}",
        nalus[0].data.len(),
        nalus[0].data
    );
}

#[test]
fn 最后一个_nalu_不裁尾随零() {
    // 没有下一个起始码做参照物时，无从判断哪几个 0 是填充。
    // 宁可保留（下游 RBSP 解析器本来就要处理 trailing bits）。
    let stream = [0, 0, 0, 1, 0x65, 0x88, 0x00, 0x00];
    let nalus = split_annexb(&stream);
    assert_eq!(nalus.len(), 1);
    assert_eq!(nalus[0].data.len(), 4, "最后一个 NALU 的尾随 0 被裁了");
}

#[test]
fn 修复后_avcc_转换不含空_nalu() {
    // 原实现对 len == 0 的项会产出 00 00 00 01 空 NALU
    let stream = [0, 0, 0, 1, 0x67, 0x42, 0x00, 0x1f];
    let avcc = sidelens_codec::nalu::annexb_to_avcc(&stream);
    // 4 字节长度 + 4 字节数据
    assert_eq!(avcc.len(), 8);
    let len = u32::from_be_bytes([avcc[0], avcc[1], avcc[2], avcc[3]]) as usize;
    assert_eq!(len, 4, "AVCC 长度前缀错了");
}
