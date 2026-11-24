//! Annex-B 切分的边界行为
//!
//! 这些用例覆盖 `split_annexb` 最容易出错的地方：
//! 混合起始码、内部含起始码序列、尾随 0 的歧义。

use sidelens_codec::nalu::{split_annexb, NaluType};

/// 构造 4 字节起始码 + 数据的片段
fn nalu4(header: u8, rest: &[u8]) -> Vec<u8> {
    let mut v = vec![0, 0, 0, 1, header];
    v.extend_from_slice(rest);
    v
}

#[test]
fn typical_frame_splits_into_three() {
    // 典型关键帧：[SPS][PPS][IDR]
    let sps = [0x67u8, 0x42, 0x00, 0x1e];
    let pps = [0x68u8, 0xce, 0x3c, 0x80];
    let idr = [0x65u8, 0x88, 0x84, 0x11];
    let mut buf = Vec::new();
    for n in [&sps[..], &pps[..], &idr[..]] {
        buf.extend_from_slice(&[0, 0, 0, 1]);
        buf.extend_from_slice(n);
    }

    let got = split_annexb(&buf);
    assert_eq!(got.len(), 3);
    assert_eq!(got[0].ty, NaluType::Sps);
    assert_eq!(got[0].data, &sps);
    assert_eq!(got[1].ty, NaluType::Pps);
    assert_eq!(got[1].data, &pps);
    assert_eq!(got[2].ty, NaluType::IdrSlice);
    assert_eq!(got[2].data, &idr);
}

#[test]
fn mixed_start_code_lengths_are_handled() {
    // 真实码流常混用 3 字节与 4 字节起始码
    let a = [0x67u8, 0xAA];
    let b = [0x68u8, 0xBB];
    let c = [0x65u8, 0xCC];
    let mut buf = Vec::new();
    buf.extend_from_slice(&[0, 0, 0, 1]);
    buf.extend_from_slice(&a);
    buf.extend_from_slice(&[0, 0, 1]);
    buf.extend_from_slice(&b);
    buf.extend_from_slice(&[0, 0, 0, 1]);
    buf.extend_from_slice(&c);

    let got = split_annexb(&buf);
    assert_eq!(got.len(), 3, "混合起始码应切出 3 个");
    assert_eq!(got[0].data, &a);
    assert_eq!(got[1].data, &b);
    assert_eq!(got[2].data, &c);
}

#[test]
fn start_code_inside_nalu_splits_it() {
    // 已知限制：NALU 数据内部出现 00 00 01 时会被误判为起始码。
    //
    // 这不是 bug，是 Annex-B 的固有性质 —— 编码器用 emulation
    // prevention（插入 0x03）避免这种序列出现在 RBSP 里。
    // 若上游传的是原始 RBSP 而非 Annex-B，切分会出错，
    // 所以接入时必须确认数据源格式。
    let a = [0x65u8, 0x00, 0x00, 0x01, 0x99];
    let mut buf = Vec::new();
    buf.extend_from_slice(&[0, 0, 0, 1]);
    buf.extend_from_slice(&a);

    let got = split_annexb(&buf);
    // 被切成两段：0x65 与 0x99
    assert_eq!(got.len(), 2, "内部起始码会导致误切（已知限制）");
    assert_eq!(got[0].data, &[0x65]);
}

#[test]
fn trailing_zero_is_ambiguous_and_trimmed() {
    // Annex-B 的固有歧义：无法区分「数据末尾的 0」与「填充用的 0」。
    //
    // # 这条测试的断言被改过（实测抓到的真 bug）
    //
    // 原来的断言是「末尾的 0 全裁掉」，理由是「数据末尾的 0 可以靠
    // RBSP 长度判断」。那个理由**站不住**：
    //
    // - H.264 的 RBSP 长度**没有**显式字段，只能靠 `rbsp_stop_one_bit`
    //   反向定位 —— 而 `cabac_zero_word`（规范 7.4.1 允许在 CABAC
    //   码流末尾追加的 `00 00`）恰好破坏那个定位
    // - `cabac_zero_word` 是**切片数据的一部分**，裁掉会让 NALU 长度
    //   与内部 slice 长度不一致 → 硬件解码器报错或花屏
    //
    // 现在的规则：**保留最多 2 个尾随 0**（那是 cabac_zero_word），
    // 超过的才算填充。所以这条测试的输入（1 个尾随 0）现在会被完整保留。
    let mut buf = nalu4(0x65, &[0x88, 0x00]);
    buf.extend_from_slice(&[0, 0, 0, 1]);
    buf.extend_from_slice(&[0x41, 0x22]);

    let got = split_annexb(&buf);
    assert_eq!(got.len(), 2);
    assert_eq!(
        got[0].data,
        &[0x65, 0x88, 0x00],
        "1 个尾随 0 可能是 cabac_zero_word，必须保留"
    );
    assert_eq!(got[1].data, &[0x41, 0x22]);
}

#[test]
fn data_before_first_start_code_is_kept() {
    // 某些编码器会省略第一帧的起始码
    let mut buf = vec![0x67u8, 0x42, 0x00, 0x1e];
    buf.extend_from_slice(&[0, 0, 0, 1]);
    buf.extend_from_slice(&[0x65, 0x88]);

    let got = split_annexb(&buf);
    assert_eq!(got.len(), 2, "首段无起始码也应保留");
    assert_eq!(got[0].ty, NaluType::Sps);
    assert_eq!(got[0].data, &[0x67, 0x42, 0x00, 0x1e]);
}

#[test]
fn empty_start_code_only_yields_nothing() {
    // 只有起始码没有数据
    let got = split_annexb(&[0, 0, 0, 1]);
    assert!(got.is_empty(), "空 NALU 不应产出");
}
