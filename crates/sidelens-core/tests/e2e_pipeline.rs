//! 跨模块端到端集成测试
//!
//! # 为什么要有这一层
//!
//! 每个模块自己的单测只证明「我在自己的假设下是对的」：
//! fec 只测 XOR 数学，fragment 只测切片拼接，fec_link 只测冗余包编号。
//! 但真实链路上这些模块是串起来跑的：
//!
//!     编码帧 → 分片 → FEC 组 → (网络丢包) → FEC 恢复 → 重组 → 抖动缓冲 → 交付
//!
//! 任何一个环节的假设与相邻环节不一致（比如恢复出的分片长度是补齐长度、
//! 冗余包的 real_count 没填对、组号拆分方式与接收端不一致），单测全绿而链路全废。
//! 本文件专门验证「串起来之后仍然正确」。
//!
//! # 覆盖范围
//!
//! | 组 | 覆盖 |
//! | --- | --- |
//! | 1 | 分片 → FEC → 恢复 → 重组 |
//! | 2 | 抖动缓冲排序、拥塞控制与帧大小的联动 |
//! | 3 | 协议层编解码往返、粘包、四时间戳时钟同步 |
//! | 4 | 弱网综合场景、FEC 配置自适应 |
//!
//! 全部同步逻辑，不引入 tokio：时钟一律用显式参数模拟，避免真实定时器带来的抖动。

// 测试名按需求保留 FEC / NACK 等大写专有名词，不强制 snake_case
#![allow(non_snake_case)]

use std::collections::BTreeMap;

use sidelens_core::congestion::{
    CongestionConfig, CongestionController, Feedback, RateState, Reason,
};
use sidelens_core::fec::FecConfig;
use sidelens_core::fec_link::{suggest_fec, FecPacket, FecReceiver, FecSender};
use sidelens_core::jitter::{JitterBuffer, JitterConfig, QueuedFrame};
use sidelens_core::sync::ClockSync;
use sidelens_core::{
    fragment_frame, DatagramPiece, NackGenerator, NackRequest, ReassembledFrame, Reassembler,
    SAFE_MTU_PAYLOAD,
};
use sidelens_proto::{
    encode_frame_owned, FrameDecoder, FrameSpec, FrameType, FLAG_KEYFRAME, HEADER_SIZE,
};

// ============================================================================
// 公共夹具
// ============================================================================

/// 构造 FEC 配置（max_packet_size 与安全 MTU 对齐）
fn fec_cfg(group_size: usize, redundant: usize) -> FecConfig {
    FecConfig {
        group_size,
        redundant,
        max_packet_size: SAFE_MTU_PAYLOAD,
    }
}

/// 造一个已编码的帧，同时返回它的原始载荷（用于逐字节比对）
///
/// 载荷用 (i * 7 + 13) % 251 生成：251 是质数，保证相邻分片、相邻帧的
/// 字节模式都不相同，任何错位、丢片、补零都会在比对时暴露。
fn build_frame(seq: u32, payload_len: usize, keyframe: bool) -> (Vec<u8>, Vec<u8>) {
    let payload: Vec<u8> = (0..payload_len)
        .map(|i| ((i * 7 + 13) % 251) as u8)
        .collect();
    let encoded = encode_frame_owned(&FrameSpec {
        ty: FrameType::Video,
        flags: if keyframe { FLAG_KEYFRAME } else { 0 },
        stream_id: 0,
        seq,
        pts_us: Some(seq as u64 * 33_333),
        send_ts_us: Some(seq as u64 * 33_333 + 1_000),
        payload: &payload,
    })
    .expect("帧编码不应失败");
    (encoded, payload)
}

/// 读帧头里声明的载荷长度（不含 32 字节帧头）
fn header_payload_len(bytes: &[u8]) -> usize {
    assert!(
        bytes.len() >= HEADER_SIZE,
        "数据报不足帧头长度：{}",
        bytes.len()
    );
    u32::from_le_bytes([bytes[28], bytes[29], bytes[30], bytes[31]]) as usize
}

/// 把一个线上数据报投进重组器
///
/// # 关键细节
///
/// FEC 恢复出的分片尾部可能带补齐的 0（长度 = 组内最长包）。
/// 这里严格按帧头里的 payload_len 截取载荷，与真实接收端一致 ——
/// 补齐字节因此被自动丢弃，不会污染重组结果。
fn push_datagram(asm: &mut Reassembler, bytes: &[u8], now_us: u64) -> Option<ReassembledFrame> {
    assert!(
        bytes.len() >= HEADER_SIZE,
        "数据报不足帧头长度：{}",
        bytes.len()
    );
    let ty = FrameType::from_u8(bytes[4]).expect("帧类型应合法");
    let payload_len = header_payload_len(bytes);
    assert!(
        HEADER_SIZE + payload_len <= bytes.len(),
        "帧头声明的载荷 {} 字节超过数据报实际长度 {}",
        payload_len,
        bytes.len()
    );
    asm.push(
        DatagramPiece {
            stream_id: u16::from_le_bytes([bytes[6], bytes[7]]),
            seq: u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]),
            ty,
            flags: bytes[5],
            pts_us: u64::from_le_bytes(bytes[12..20].try_into().unwrap()),
            send_ts_us: u64::from_le_bytes(bytes[20..28].try_into().unwrap()),
            payload: &bytes[HEADER_SIZE..HEADER_SIZE + payload_len],
        },
        now_us,
    )
}

/// 确定性伪随机（xorshift32），避免引入 rand crate
///
/// 弱网模拟必须可复现：同一个种子跑出来的丢包序列完全一样，
/// 否则「成功率 > 90%」这种断言会在 CI 上随机翻车。
struct XorShift32 {
    state: u32,
}

impl XorShift32 {
    fn new(seed: u32) -> Self {
        Self {
            // 种子为 0 时 xorshift 永远输出 0，换一个固定非零值兜底
            state: if seed == 0 { 0x9e37_79b9 } else { seed },
        }
    }

    fn next_u32(&mut self) -> u32 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }

    /// 返回 [0, 100) 的整数，用于按百分比判定丢包
    fn next_percent(&mut self) -> u32 {
        self.next_u32() % 100
    }
}

/// 发送端：把一帧切成分片并产出冗余包
///
/// 返回 (分片列表, 冗余包列表)，两者都是线上字节，可直接做丢包模拟。
fn 发送一帧(
    tx: &mut FecSender,
    frame_seq: u32,
    encoded: &[u8],
) -> (Vec<Vec<u8>>, Vec<FecPacket>) {
    let frags = fragment_frame(encoded, SAFE_MTU_PAYLOAD);
    tx.begin_frame(frame_seq);
    let mut reds = Vec::new();
    for f in &frags {
        reds.extend(tx.push_fragment(f));
    }
    reds.extend(tx.finish_frame());
    (frags, reds)
}

// ============================================================================
// 组 1：分片 → FEC → 恢复 → 重组
// ============================================================================

/// 无丢包时，分片与冗余包全部到达，重组结果必须与原始载荷逐字节一致。
///
/// 这条是整条链路的基线：它一旦失败，说明分片头、FEC 组划分、
/// 重组器的任一处对「一帧长什么样」的理解与另一处不同。
#[test]
fn 完整链路_无丢包_能重组出原始帧() {
    let cfg = fec_cfg(4, 1);
    let (encoded, payload) = build_frame(1, 8000, true);

    let mut tx = FecSender::new(cfg).expect("FEC 配置应合法");
    let (frags, reds) = 发送一帧(&mut tx, 1, &encoded);
    assert!(
        frags.len() > 1,
        "8000 字节的帧必须被切分，实际 {} 片",
        frags.len()
    );
    assert!(!reds.is_empty(), "应有冗余包产出");
    assert_eq!(tx.frames_encoded, 1, "应记录编码了 1 帧");
    assert_eq!(
        tx.redundant_sent as usize,
        reds.len(),
        "冗余包计数应与实际产出数量一致"
    );

    let mut rx = FecReceiver::new(cfg).expect("FEC 配置应合法");
    for (i, f) in frags.iter().enumerate() {
        rx.push_fragment(1, i, f);
    }
    let mut recovered: Vec<(usize, Vec<u8>)> = Vec::new();
    for r in &reds {
        recovered.extend(rx.push_redundant(r));
    }
    assert!(recovered.is_empty(), "没有丢包时不应恢复出任何分片");
    assert_eq!(rx.fragments_recovered, 0, "恢复计数应保持 0");

    let mut asm = Reassembler::new(100.0, 64);
    let mut out = None;
    for f in &frags {
        if let Some(r) = push_datagram(&mut asm, f, 0) {
            out = Some(r);
        }
    }
    let frame = out.expect("收齐全部分片后应能重组出完整帧");
    assert_eq!(
        frame.payload, payload,
        "重组出的载荷必须与原始载荷逐字节一致"
    );
    assert_eq!(frame.payload.len(), 8000, "载荷长度必须等于原始长度");
    assert_eq!(frame.seq, 1, "帧序号必须保留");
    assert!(frame.is_keyframe(), "关键帧标志必须穿过分片与重组");
    assert_eq!(asm.stats.frames_out, 1, "重组器应只吐出 1 帧");
    assert_eq!(asm.pending_count(), 0, "重组完成后不应残留半成品");
}

/// 丢 1 个分片时，FEC 应零延迟恢复，且恢复出的分片能直接拼回完整帧。
#[test]
fn 丢一个分片_FEC恢复后能重组() {
    let cfg = fec_cfg(4, 1);
    let (encoded, payload) = build_frame(2, 8000, false);
    let lost = 2usize;

    let mut tx = FecSender::new(cfg).expect("FEC 配置应合法");
    let (frags, reds) = 发送一帧(&mut tx, 2, &encoded);
    assert!(frags.len() > lost, "分片数必须大于丢包下标");

    let mut rx = FecReceiver::new(cfg).expect("FEC 配置应合法");
    for (i, f) in frags.iter().enumerate() {
        if i != lost {
            rx.push_fragment(2, i, f);
        }
    }
    let mut recovered: Vec<(usize, Vec<u8>)> = Vec::new();
    for r in &reds {
        recovered.extend(rx.push_redundant(r));
    }
    assert_eq!(recovered.len(), 1, "单丢应恢复出恰好 1 个分片");
    assert_eq!(recovered[0].0, lost, "恢复出的分片下标必须是丢失的那个");
    assert_eq!(
        recovered[0].1, frags[lost],
        "恢复出的分片必须与原始分片逐字节一致"
    );
    assert_eq!(rx.fragments_recovered, 1, "恢复计数应为 1");

    let mut asm = Reassembler::new(100.0, 64);
    let mut out = None;
    for (i, f) in frags.iter().enumerate() {
        let dg: &[u8] = if i == lost {
            recovered[0].1.as_slice()
        } else {
            f.as_slice()
        };
        if let Some(r) = push_datagram(&mut asm, dg, 0) {
            out = Some(r);
        }
    }
    let frame = out.expect("FEC 补上缺片后应能重组");
    assert_eq!(frame.payload, payload, "补片后重组内容必须与原始载荷一致");
    assert_eq!(asm.stats.duplicates, 0, "补片不应被当成重复分片");
}

/// 丢 3 个分片（超过 redundant=1 的能力）时：
/// FEC 必须老实返回「恢复不了」，剩下的交给 NACK 精确点名缺失分片。
///
/// 这里同时验证「静默失败」这个最危险的场景：宁可什么都不返回，
/// 也不能返回算错的字节（错字节会直接污染解码器）。
#[test]
fn 丢超过FEC能力_重组失败但NACK能补上() {
    let cfg = fec_cfg(4, 1);
    let (encoded, _payload) = build_frame(3, 8000, false);
    let lost = [0usize, 1, 2];

    let mut tx = FecSender::new(cfg).expect("FEC 配置应合法");
    let (frags, reds) = 发送一帧(&mut tx, 3, &encoded);

    let mut rx = FecReceiver::new(cfg).expect("FEC 配置应合法");
    for (i, f) in frags.iter().enumerate() {
        if !lost.contains(&i) {
            rx.push_fragment(3, i, f);
        }
    }
    let mut recovered: Vec<(usize, Vec<u8>)> = Vec::new();
    for r in &reds {
        recovered.extend(rx.push_redundant(r));
    }
    assert!(
        recovered.is_empty(),
        "丢了 3 片而 redundant=1，FEC 不应恢复出任何东西，实际恢复 {} 个",
        recovered.len()
    );
    assert_eq!(rx.fragments_recovered, 0, "恢复计数必须为 0");

    // 重组必然失败，但必须留下半成品供诊断
    let mut asm = Reassembler::new(100.0, 64);
    let mut out = None;
    for (i, f) in frags.iter().enumerate() {
        if !lost.contains(&i) {
            if let Some(r) = push_datagram(&mut asm, f, 0) {
                out = Some(r);
            }
        }
    }
    assert!(out.is_none(), "缺 3 片时不应吐出完整帧");
    assert_eq!(asm.pending_count(), 1, "半成品应留在重组器里");

    let missing = asm.missing_fragments(0, 3);
    assert_eq!(missing, vec![0, 1, 2], "重组器应精确报出缺失的分片下标");

    // 用缺失下标驱动 NACK：这里把「分片下标」当作请求序号（分片级重传）
    let mut nack = NackGenerator::new(5.0, 32);
    for m in &missing {
        nack.mark_missing(*m as u32, 1_000);
    }
    assert_eq!(nack.pending_count(), 3, "应记录 3 个待请求序号");
    assert!(
        nack.poll(2_000, 0).is_none(),
        "延迟窗口（5ms）未到，不应发出请求"
    );

    let req = nack.poll(10_000, 0).expect("延迟到期后应发出 NACK 请求");
    assert_eq!(req.stream, 0, "视频流序号应为 0");
    assert_eq!(req.seqs, vec![0, 1, 2], "请求里必须包含全部缺失分片下标");
    assert_eq!(nack.total_requests, 1, "应记录 1 次请求");
    assert_eq!(nack.total_seqs_requested, 3, "应记录请求了 3 个序号");

    // 请求要能过网络：编码解码往返不丢信息
    let round = NackRequest::from_bytes(&req.to_bytes()).expect("NACK 请求应能解码");
    assert_eq!(round, req, "NACK 请求编解码必须完全一致");
}

/// 20000 字节的大帧会跨多个 FEC 组（每组 4 个分片），
/// 每组各丢 1 个分片时应各自独立恢复，最后完整重组。
///
/// 这条专打「组号复用」的坑：如果组号只按帧序号算，同一帧的多个组
/// 会互相覆盖状态，第二个组之后全部恢复失败。
#[test]
fn 大帧跨多个FEC组_每组独立恢复() {
    let cfg = fec_cfg(4, 1);
    let (encoded, payload) = build_frame(4, 20_000, true);

    let mut tx = FecSender::new(cfg).expect("FEC 配置应合法");
    let (frags, reds) = 发送一帧(&mut tx, 4, &encoded);
    assert!(
        frags.len() >= 17,
        "20000 字节应切出至少 17 片，实际 {}",
        frags.len()
    );
    let 组数 = frags.len().div_ceil(cfg.group_size);
    assert!(组数 >= 5, "应跨至少 5 个 FEC 组，实际 {}", 组数);
    assert_eq!(reds.len(), 组数, "每个组都应产出 1 个冗余包");

    // 每组各丢它的第一个分片（组内下标 0），保证下标一定存在
    let lost: Vec<usize> = (0..组数).map(|g| g * cfg.group_size).collect();
    assert_eq!(lost.len(), 组数, "每组都应恰好丢 1 片，实际 {:?}", lost);

    let mut rx = FecReceiver::new(cfg).expect("FEC 配置应合法");
    for (i, f) in frags.iter().enumerate() {
        if !lost.contains(&i) {
            rx.push_fragment(4, i, f);
        }
    }
    let mut recovered: Vec<(usize, Vec<u8>)> = Vec::new();
    for r in &reds {
        recovered.extend(rx.push_redundant(r));
    }
    assert_eq!(
        recovered.len(),
        lost.len(),
        "每组丢 1 片都应恢复，期望 {} 个实际 {} 个",
        lost.len(),
        recovered.len()
    );
    assert_eq!(
        rx.fragments_recovered,
        lost.len() as u64,
        "恢复计数应等于丢片数"
    );

    let mut by_index: BTreeMap<usize, Vec<u8>> = BTreeMap::new();
    for (idx, data) in &recovered {
        assert!(
            lost.contains(idx),
            "恢复出的下标 {} 不在丢包集合里，说明组间状态串了",
            idx
        );
        assert_eq!(
            data.as_slice(),
            frags[*idx].as_slice(),
            "分片 {} 恢复内容与原始不一致",
            idx
        );
        by_index.insert(*idx, data.clone());
    }

    let mut asm = Reassembler::new(100.0, 64);
    let mut out = None;
    for (i, f) in frags.iter().enumerate() {
        let dg: &[u8] = match by_index.get(&i) {
            Some(d) => d.as_slice(),
            None => f.as_slice(),
        };
        if let Some(r) = push_datagram(&mut asm, dg, 0) {
            out = Some(r);
        }
    }
    let frame = out.expect("每组都恢复后应能重组出 20000 字节的帧");
    assert_eq!(frame.payload.len(), 20_000, "重组长度必须是 20000");
    assert_eq!(frame.payload, payload, "跨组恢复后内容必须与原始载荷一致");
    assert_eq!(asm.stats.frames_out, 1, "应只吐出 1 帧");
}

/// 已知限制的显式验证：FEC 恢复出的分片长度 = 组内最长包长度，尾部是补齐的 0。
///
/// 只要调用方按帧头里的 payload_len 截取载荷（真实接收端就是这么做的），
/// 补齐字节不会污染重组结果。这条测试把这个前提钉住：
/// 将来若有人「优化」成把整段缓冲全量投递，这里会立刻失败。
#[test]
fn 丢尾片_FEC补齐字节不应污染重组() {
    let cfg = fec_cfg(4, 1);
    let (encoded, payload) = build_frame(5, 20_000, false);

    let mut tx = FecSender::new(cfg).expect("FEC 配置应合法");
    let (frags, reds) = 发送一帧(&mut tx, 5, &encoded);
    let last = frags.len() - 1;
    assert!(
        frags[last].len() < frags[0].len(),
        "尾片应短于满片（尾片 {} 字节，满片 {} 字节）",
        frags[last].len(),
        frags[0].len()
    );

    let mut rx = FecReceiver::new(cfg).expect("FEC 配置应合法");
    for (i, f) in frags.iter().enumerate() {
        if i != last {
            rx.push_fragment(5, i, f);
        }
    }
    let mut recovered: Vec<(usize, Vec<u8>)> = Vec::new();
    for r in &reds {
        recovered.extend(rx.push_redundant(r));
    }
    assert_eq!(recovered.len(), 1, "尾片单丢也应能恢复");
    let (idx, data) = &recovered[0];
    assert_eq!(*idx, last, "恢复出的应是尾片");
    assert!(
        data.len() > frags[last].len(),
        "恢复长度（{}）应大于尾片真实长度（{}），尾部是补齐的 0",
        data.len(),
        frags[last].len()
    );
    assert_eq!(
        header_payload_len(data),
        header_payload_len(&frags[last]),
        "帧头里的 payload_len 必须仍是真实长度，补齐只发生在尾部"
    );
    assert_eq!(
        &data[..frags[last].len()],
        &frags[last][..],
        "补齐前的字节必须与原始尾片一致"
    );

    let mut asm = Reassembler::new(100.0, 64);
    let mut out = None;
    for (i, f) in frags.iter().enumerate() {
        let dg: &[u8] = if i == last {
            data.as_slice()
        } else {
            f.as_slice()
        };
        if let Some(r) = push_datagram(&mut asm, dg, 0) {
            out = Some(r);
        }
    }
    let frame = out.expect("按帧头截断后应能正常重组");
    assert_eq!(frame.payload.len(), 20_000, "补齐字节不应被算进载荷");
    assert_eq!(frame.payload, payload, "重组内容必须与原始载荷一致");
}

// ============================================================================
// 组 2：抖动缓冲 + 拥塞控制
// ============================================================================

/// 乱序到达的 10 帧进入抖动缓冲后，必须按 PTS 重排，pop 出来 seq 单调递增。
#[test]
fn 乱序到达的帧_抖动缓冲能正确排序() {
    let mut jb = JitterBuffer::new(JitterConfig::default());
    // 固定置换：确定性乱序，避免测试自身抖动
    let 到达顺序 = [7usize, 2, 9, 0, 5, 1, 8, 3, 6, 4];

    for &i in &到达顺序 {
        let seq = i as u32 + 1;
        let frame = QueuedFrame {
            ty: FrameType::Video,
            seq,
            pts_us: seq as u64 * 33_333,
            send_ts_us: seq as u64 * 33_333,
            arrived_at_us: 0,
            flags: 0,
            is_retransmit: false,
            payload: vec![i as u8; 100],
        };
        let outcome = jb.push_at(frame, 0);
        assert!(!outcome.late, "帧 {} 还没播过，不应被判迟到", seq);
        assert!(!outcome.needs_keyframe, "队列深度为 0 时不应请求关键帧");
    }

    assert_eq!(jb.len(), 10, "10 帧都应进队列");
    assert_eq!(jb.stats.received, 10, "收到计数应为 10");
    assert_eq!(jb.stats.max_depth, 10, "最大深度应为 10");
    assert_eq!(jb.stats.reordered, 0, "乱序但未播过的帧不该算重排丢弃");

    let mut popped = Vec::new();
    let mut last_seq = 0u32;
    while let Some(f) = jb.pop_at(20_000) {
        assert!(
            f.seq > last_seq,
            "pop 顺序必须按 seq 单调递增：上一个 {} 当前 {}",
            last_seq,
            f.seq
        );
        last_seq = f.seq;
        popped.push(f.seq);
    }
    assert_eq!(
        popped,
        vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10],
        "应按 seq 升序交付全部 10 帧"
    );
    assert_eq!(jb.stats.played, 10, "播放计数应为 10");
    assert!(jb.is_empty(), "取完后队列应为空");
}

/// 制造一次真拥塞（延迟梯度超阈值），断言目标码率下降，
/// 并且按 target_bps / 8 / fps 算出的帧大小同步变小。
///
/// 这条把「拥塞控制」与「编码器实际输出」之间的换算关系钉住：
/// 码率降了但帧大小没降（或降反了）说明公式用错了单位（bit / byte）。
#[test]
fn 拥塞控制降码率后_帧大小应相应减小() {
    const FPS: f64 = 30.0;

    let mut cc = CongestionController::new(CongestionConfig::default());
    let 原码率 = cc.target_bps();
    let 原帧大小 = 原码率 / 8.0 / FPS;

    // 每 10ms 发一个包，排队延迟每包涨 1ms → 斜率 0.1，远超阈值 0.015
    for i in 0..12u64 {
        let send = 1_000_000 + i * 10_000;
        cc.add_delay_sample(send, send + 5_000 + i * 1_000);
    }
    let slope = cc.slope().expect("样本足够时应能算出延迟梯度");
    assert!(
        slope > cc.config().delay_gradient_threshold,
        "斜率 {:.4} 应超过阈值 {:.4}",
        slope,
        cc.config().delay_gradient_threshold
    );

    let d = cc.update(
        &Feedback {
            loss_ratio: 0.0,
            rtt_ms: 20.0,
            fresh: true,
        },
        1_000_000,
    );
    assert_eq!(d.state, RateState::Decrease, "延迟持续增长必须降码率");
    assert_eq!(d.reason, Reason::DelayGradient, "降码率原因应是延迟梯度");
    assert!(d.target_bps < 原码率, "目标码率必须下降");
    assert!(
        (d.target_bps - 原码率 * 0.85).abs() < 1.0,
        "单次降幅应是 decrease_factor(0.85)：期望 {:.0} 实际 {:.0}",
        原码率 * 0.85,
        d.target_bps
    );
    assert_eq!(cc.decreases, 1, "应记录 1 次降码率");

    let 新帧大小 = d.target_bps / 8.0 / FPS;
    assert!(
        新帧大小 < 原帧大小,
        "帧大小应随码率下降：原 {:.0} 字节 → 新 {:.0} 字节",
        原帧大小,
        新帧大小
    );
    assert!(
        (新帧大小 / 原帧大小 - 0.85).abs() < 1e-6,
        "帧大小降幅应与码率降幅一致"
    );
}

/// 承接拥塞场景：链路恢复（延迟恒定、无丢包）后，码率与帧大小都应回到原水平。
#[test]
fn 拥塞恢复后_帧大小能回到原水平() {
    const FPS: f64 = 30.0;

    let mut cc = CongestionController::new(CongestionConfig::default());
    let 原码率 = cc.target_bps();
    let 原帧大小 = 原码率 / 8.0 / FPS;

    // 第一步：制造拥塞
    for i in 0..12u64 {
        let send = 1_000_000 + i * 10_000;
        cc.add_delay_sample(send, send + 5_000 + i * 1_000);
    }
    let 拥塞后 = cc
        .update(
            &Feedback {
                loss_ratio: 0.0,
                rtt_ms: 20.0,
                fresh: true,
            },
            1_000_000,
        )
        .target_bps;
    assert!(拥塞后 < 原码率, "前提：拥塞后码率必须已下降");

    // 第二步：网络恢复 —— 延迟恒定（斜率 0），无丢包
    for i in 0..12u64 {
        let send = 2_000_000 + i * 10_000;
        cc.add_delay_sample(send, send + 5_000);
    }
    assert!(
        cc.slope().map(|s| s.abs() < 1e-9).unwrap_or(false),
        "恒定延迟的斜率应为 0，实际 {:?}",
        cc.slope()
    );

    // 第三步：持续推进时间（跨过 300ms 保持期），等待码率回升
    let mut now = 2_100_000u64;
    let mut 回升 = false;
    for _ in 0..40 {
        now += 100_000;
        let d = cc.update(
            &Feedback {
                loss_ratio: 0.0,
                rtt_ms: 20.0,
                fresh: true,
            },
            now,
        );
        if d.target_bps >= 原码率 {
            回升 = true;
            break;
        }
    }
    assert!(
        回升,
        "无拥塞持续 4 秒后码率应回到原水平，实际 {:.0} bps（原 {:.0} bps）",
        cc.target_bps(),
        原码率
    );
    assert!(cc.increases > 0, "应记录过升码率");
    assert!(
        cc.target_bps() <= cc.config().max_bitrate_bps,
        "码率不得超过配置上限"
    );

    let 恢复后帧大小 = cc.target_bps() / 8.0 / FPS;
    assert!(
        恢复后帧大小 >= 原帧大小,
        "帧大小应回到原水平：原 {:.0} 字节 → 恢复后 {:.0} 字节",
        原帧大小,
        恢复后帧大小
    );
}

// ============================================================================
// 组 3：协议层 + 时钟
// ============================================================================

/// 编码 → 解码往返，类型、标志、序号、时间戳、载荷都必须一致。
#[test]
fn 帧编码解码往返_载荷与时间戳一致() {
    let payload: Vec<u8> = (0..3000).map(|i| (i % 253) as u8).collect();
    let encoded = encode_frame_owned(&FrameSpec {
        ty: FrameType::Video,
        flags: FLAG_KEYFRAME,
        stream_id: 3,
        seq: 12345,
        pts_us: Some(7_654_321),
        send_ts_us: Some(7_654_999),
        payload: &payload,
    })
    .expect("编码不应失败");
    assert_eq!(
        encoded.len(),
        HEADER_SIZE + payload.len(),
        "编码后总长应为帧头 + 载荷"
    );

    let mut dec = FrameDecoder::new();
    assert_eq!(dec.push(&encoded), 1, "应解出 1 帧");
    assert_eq!(dec.pending_bytes(), 0, "不应残留半帧字节");

    let meta = dec.frames()[0];
    assert_eq!(meta.ty, FrameType::Video, "帧类型必须一致");
    assert_eq!(meta.flags, FLAG_KEYFRAME, "flags 必须一致");
    assert!(meta.is_keyframe(), "关键帧标志应可判定");
    assert_eq!(meta.stream_id, 3, "流号必须一致");
    assert_eq!(meta.seq, 12345, "帧序号必须一致");
    assert_eq!(meta.pts_us, 7_654_321, "显示时间戳必须一致");
    assert_eq!(meta.send_ts_us, 7_654_999, "发送时间戳必须一致");
    assert_eq!(meta.payload_len(), payload.len(), "载荷长度必须一致");
    assert_eq!(dec.payload(meta), payload.as_slice(), "载荷必须逐字节一致");
}

/// 3 个帧拼成一块（粘包）一次推入，解码器应一次解出 3 帧且顺序正确。
#[test]
fn 多帧连续解码_解码器能正确处理粘包() {
    let mut 拼接 = Vec::new();
    let mut 载荷们: Vec<Vec<u8>> = Vec::new();
    for i in 0..3u32 {
        let payload: Vec<u8> = (0..500 + i as usize * 100)
            .map(|j| ((j + i as usize) % 251) as u8)
            .collect();
        let encoded = encode_frame_owned(&FrameSpec {
            ty: FrameType::Video,
            flags: if i == 0 { FLAG_KEYFRAME } else { 0 },
            stream_id: 0,
            seq: 100 + i,
            pts_us: Some(i as u64 * 33_333),
            send_ts_us: Some(i as u64 * 33_333 + 500),
            payload: &payload,
        })
        .expect("编码不应失败");
        拼接.extend_from_slice(&encoded);
        载荷们.push(payload);
    }

    let mut dec = FrameDecoder::new();
    let n = dec.push(&拼接);
    assert_eq!(n, 3, "粘包应一次解出 3 帧，实际 {}", n);
    assert_eq!(dec.frames().len(), 3, "frames() 里应有 3 条元数据");
    assert_eq!(dec.pending_bytes(), 0, "不应残留字节");

    let metas: Vec<_> = dec.frames().to_vec();
    for (i, meta) in metas.iter().enumerate() {
        assert_eq!(meta.seq, 100 + i as u32, "第 {} 帧的序号错位", i);
        assert_eq!(
            meta.pts_us,
            i as u64 * 33_333,
            "第 {} 帧的显示时间戳错位",
            i
        );
        assert_eq!(
            dec.payload(*meta),
            载荷们[i].as_slice(),
            "第 {} 帧的载荷错位（说明帧边界算错了）",
            i
        );
    }
    assert_eq!(metas[0].flags, FLAG_KEYFRAME, "第 1 帧的关键帧标志应保留");
    assert_eq!(metas[1].flags, 0, "第 2 帧不应带关键帧标志");
}

/// 四时间戳协商：同机模拟（去程 = 回程），offset 应为 0、RTT 应等于去+回。
/// 再验证一个真实存在的时钟偏移能被正确算出来，并用于换算时间戳。
#[test]
fn 时钟同步_四时间戳能算出偏移() {
    // ---- 场景 1：同机（两端时钟同源），去程 20ms、对端处理 1ms、回程 20ms
    let mut sync_a = ClockSync::new();
    let probe = sync_a.start_probe(1_000_000, 0);
    assert_eq!(probe.t1_us, 1_000_000, "T1 应是本端发出时刻");

    let t1 = probe.t1_us;
    let t2 = t1 + 20_000; // 对端收到（去程 20ms）
    let t3 = t2 + 1_000; // 对端发出（处理 1ms）
    let t4 = t3 + 20_000; // 本端收到（回程 20ms）
                          // 同机模拟的关键约束：t2 - t1 == t4 - t3
    assert_eq!(t2 - t1, t4 - t3, "同机场景去程应等于回程");

    let sample = sync_a
        .finish_probe(t4, t1, t2, t3)
        .expect("时间戳自洽时应给出样本");
    assert_eq!(sample.rtt_us, 40_000, "RTT 应是去程 + 回程（40ms）");
    assert_eq!(sample.offset_us, 0, "同机场景的时钟偏移应为 0");
    assert!(sync_a.is_synced(), "结算后应标记为已同步");
    assert_eq!(sync_a.offset_us(), Some(0));
    assert_eq!(sync_a.rtt_us(), 40_000);
    assert_eq!(sync_a.to_local(t2), t2, "偏移为 0 时对端时刻应原样返回");
    assert_eq!(
        sync_a.one_way_latency_us(t1, t2),
        Some(20_000),
        "单程延迟应等于 20ms"
    );

    // ---- 场景 2：对端时钟比本端快 7ms
    let mut sync_b = ClockSync::new();
    let probe = sync_b.start_probe(5_000_000, 0);
    let t1 = probe.t1_us;
    let 偏移 = 7_000i64;
    let t2 = (t1 + 20_000) as i64 + 偏移;
    let t3 = t2 + 1_000;
    let t4 = (t3 + 20_000 - 偏移) as u64;
    let sample = sync_b
        .finish_probe(t4, t1, t2 as u64, t3 as u64)
        .expect("时间戳自洽时应给出样本");
    assert_eq!(sample.offset_us, 偏移, "应算出对端时钟快 7ms");
    assert_eq!(sample.rtt_us, 40_000, "RTT 不受时钟偏移影响");
    assert_eq!(
        sync_b.to_local(t2 as u64),
        (t2 + 偏移) as u64,
        "换算后对端时刻应落在本端时基上"
    );

    // ---- 不自洽的时间戳必须被丢弃，不能给出错误数字
    let mut sync_c = ClockSync::new();
    assert!(
        sync_c.finish_probe(1_000, 2_000, 1_500, 1_600).is_none(),
        "T4 < T1 的时间戳应被拒绝"
    );
    assert!(
        !sync_c.is_synced(),
        "被拒绝的样本不应让同步器进入已同步状态"
    );
    assert_eq!(
        sync_c.one_way_latency_us(0, 1_000),
        None,
        "未同步时应返回 None"
    );
}

// ============================================================================
// 组 4：综合场景
// ============================================================================

/// 弱网综合场景：3% 包级丢包、50 帧 × 4000 字节。
///
/// 完整链路：编码 → 分片 → FEC → 丢包 → FEC 恢复 → 重组 → (失败则) NACK 补片 → 重组。
/// 统计 FEC 恢复数、NACK 请求数、最终成功率，断言成功率 > 90%。
///
/// 丢包序列由 xorshift32 决定，种子固定 —— 同一份代码在任何机器上跑出的
/// 丢包模式完全一致，避免「本机过、CI 挂」。
///
/// 种子选 1 是刻意的：它让 50 帧里同时出现「无损」「单丢（FEC 救回）」
/// 「双丢（FEC 救不回，走 NACK）」三种情况，两条恢复路径都被真实执行到。
/// 若换成全部单丢的种子，NACK 分支会静默地一次都不跑，测试形同虚设。
#[test]
fn 弱网综合场景_3pct丢包下完整链路可用() {
    let 帧数: usize = 50;
    let 帧载荷: usize = 4000;
    let 丢包百分比: u32 = 3;

    let cfg = fec_cfg(4, 1);
    let mut rng = XorShift32::new(1);
    let mut tx = FecSender::new(cfg).expect("FEC 配置应合法");
    let mut rx = FecReceiver::new(cfg).expect("FEC 配置应合法");
    let mut asm = Reassembler::new(100.0, 64);
    let mut nack = NackGenerator::new(5.0, 32);

    let mut 发送分片 = 0usize;
    let mut 发送冗余 = 0usize;
    let mut 收到冗余 = 0usize;
    let mut fec恢复 = 0usize;
    let mut nack请求 = 0usize;
    let mut nack补齐 = 0usize;
    let mut 直接成功 = 0usize;
    let mut 成功 = 0usize;

    for f in 0..帧数 {
        let seq = f as u32 + 1;
        let (encoded, payload) = build_frame(seq, 帧载荷, f % 15 == 0);
        let (frags, reds) = 发送一帧(&mut tx, seq, &encoded);
        发送分片 += frags.len();
        发送冗余 += reds.len();

        // 每个数据分片独立判定是否丢失
        let mut 到达分片 = Vec::new();
        for i in 0..frags.len() {
            if rng.next_percent() >= 丢包百分比 {
                到达分片.push(i);
            }
        }
        // 冗余包自己也可能丢
        let mut 到达冗余 = Vec::new();
        for k in 0..reds.len() {
            if rng.next_percent() >= 丢包百分比 {
                到达冗余.push(k);
            }
        }
        收到冗余 += 到达冗余.len();

        for &i in &到达分片 {
            rx.push_fragment(seq as u16, i, &frags[i]);
        }
        let mut 恢复出: Vec<(usize, Vec<u8>)> = Vec::new();
        for &k in &到达冗余 {
            恢复出.extend(rx.push_redundant(&reds[k]));
        }
        fec恢复 += 恢复出.len();

        // 恢复出的分片必须内容正确（只比真实长度，尾部补齐字节不算）
        for (idx, data) in &恢复出 {
            let 真长 = HEADER_SIZE + header_payload_len(&frags[*idx]);
            assert_eq!(
                &data[..真长],
                &frags[*idx][..真长],
                "第 {} 帧分片 {} 的 FEC 恢复内容与原始不一致",
                seq,
                idx
            );
        }

        // 汇总本次可用的分片：原始到达的 + FEC 恢复的
        let mut 可用: Vec<Option<Vec<u8>>> = vec![None; frags.len()];
        for &i in &到达分片 {
            可用[i] = Some(frags[i].clone());
        }
        for (i, d) in 恢复出 {
            可用[i] = Some(d);
        }

        let 到达时刻 = f as u64 * 33_333;
        let mut 重组 = None;
        for slot in 可用.iter().flatten() {
            if let Some(r) = push_datagram(&mut asm, slot, 到达时刻) {
                重组 = Some(r);
            }
        }

        if let Some(frame) = 重组 {
            assert_eq!(
                frame.payload, payload,
                "第 {} 帧（FEC 直接救回）重组内容必须与原始载荷一致",
                seq
            );
            成功 += 1;
            直接成功 += 1;
        } else {
            // FEC 救不回来：走 NACK 点名缺失分片，模拟重传全部到达
            let mut missing = asm.missing_fragments(0, seq);
            if missing.is_empty() {
                // 极端情况：这一帧一片都没到，重组器里没有半成品 —— 整帧重传
                missing = (0..frags.len() as u16).collect();
            }
            for m in &missing {
                nack.mark_missing(*m as u32, 到达时刻);
            }
            let req = nack
                .poll(到达时刻 + 10_000, 0)
                .expect("延迟到期后应发出 NACK 请求");
            nack请求 += 1;
            assert!(!req.seqs.is_empty(), "NACK 请求不应为空");
            for m in &req.seqs {
                let idx = *m as usize;
                assert!(idx < frags.len(), "NACK 请求了越界分片 {}", idx);
                if let Some(r) = push_datagram(&mut asm, &frags[idx], 到达时刻 + 20_000) {
                    重组 = Some(r);
                }
            }
            let frame = 重组.expect("重传补齐后应能重组");
            assert_eq!(
                frame.payload, payload,
                "第 {} 帧（NACK 补齐）重组内容必须与原始载荷一致",
                seq
            );
            成功 += 1;
            nack补齐 += 1;
            nack.clear();
            assert_eq!(nack.pending_count(), 0, "帧已交付，旧请求应作废");
        }

        // 帧级清理：丢掉已处理帧的 FEC 组，避免状态无限增长
        rx.drop_before_frame(seq as u16 + 1);
        assert!(rx.tracked_groups() <= 1, "FEC 组状态应及时清理");
        assert_eq!(asm.pending_count(), 0, "每帧处理完都不应残留半成品");
    }

    let 成功率 = 成功 as f64 / 帧数 as f64;
    println!(
        "弱网统计：分片 {} 个、冗余 {} 个（送达 {} 个）、FEC 恢复 {} 片、NACK 请求 {} 次（补齐 {} 帧）、FEC 直通 {} 帧、成功率 {:.1}%",
        发送分片, 发送冗余, 收到冗余, fec恢复, nack请求, nack补齐, 直接成功, 成功率 * 100.0
    );

    assert_eq!(成功, 帧数, "每一帧最终都应重组成功（FEC 或 NACK 兜底）");
    assert!(
        成功率 > 0.90,
        "3% 丢包下重组成功率应高于 90%，实际 {:.1}%",
        成功率 * 100.0
    );
    assert!(
        fec恢复 > 0,
        "应至少有若干单丢帧被 FEC 零延迟救回，否则说明丢包模拟没生效"
    );
    assert!(
        nack请求 > 0,
        "应至少有一帧双丢超出 FEC 能力、走 NACK 兜底，否则这条综合测试只覆盖了一半链路"
    );
    assert!(
        直接成功 + nack补齐 == 帧数,
        "FEC 直通帧数 + NACK 补齐帧数应等于总帧数"
    );
    assert_eq!(
        tx.redundant_sent as usize, 发送冗余,
        "发送端冗余计数应等于实际产出"
    );
    assert_eq!(
        rx.redundant_received as usize, 收到冗余,
        "接收端冗余计数应等于实际送达"
    );
    assert_eq!(
        rx.fragments_recovered as usize, fec恢复,
        "FEC 恢复计数应与统计一致"
    );
    assert_eq!(
        asm.stats.frames_out as usize, 成功,
        "重组器吐出的帧数应等于成功帧数"
    );
}

/// FEC 配置自适应：按丢包率选冗余度，五档依次为 None/1/2/3/None，
/// 且每档给出的配置都必须能通过 validate（否则创建编码器时会直接报错）。
#[test]
fn FEC配置自适应_按丢包率选冗余度() {
    let 组大小: usize = 10;
    let 档位 = [0.0_f64, 0.02, 0.05, 0.10, 0.20];
    let 期望: [Option<usize>; 5] = [None, Some(1), Some(2), Some(3), None];

    for (i, &丢包率) in 档位.iter().enumerate() {
        let cfg = suggest_fec(丢包率, 组大小);
        assert_eq!(
            cfg.map(|c| c.redundant),
            期望[i],
            "丢包率 {:.0}% 的冗余度不符合决策表",
            丢包率 * 100.0
        );
        if let Some(c) = cfg {
            assert!(
                c.validate().is_ok(),
                "丢包率 {:.0}% 给出的配置非法：{:?}",
                丢包率 * 100.0,
                c.validate().unwrap_err()
            );
            assert_eq!(c.group_size, 组大小, "组大小应沿用调用方传入值");
            assert_eq!(c.max_packet_size, SAFE_MTU_PAYLOAD, "包上限应对齐安全 MTU");
            assert!(c.redundant <= c.group_size, "冗余数不得超过组大小");
            // 给出的配置必须能真正构造出编解码器（端到端可用，而不只是数字合法）
            assert!(FecSender::new(c).is_ok(), "配置应能创建发送端");
            assert!(FecReceiver::new(c).is_ok(), "配置应能创建接收端");
        }
    }

    // 边界：组很小的时候 redundant 必须被夹到 group_size 以内
    let 小组 = suggest_fec(0.10, 2).expect("10% 丢包应给出配置");
    assert!(
        小组.redundant <= 小组.group_size,
        "小组配置的冗余数应被夹住"
    );
    assert!(小组.validate().is_ok(), "夹住后的配置仍应合法");
}
