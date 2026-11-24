//! Opus 包结构的最小安全解析
//!
//! # 为什么需要这个
//!
//! 「这一帧是不是 DTX」不能用长度阈值判断。实测（48kHz 单声道，DTX 开）：
//!
//! | 帧长 | DTX 帧字节数 | TOC |
//! | --- | --- | --- |
//! | 20ms | **1** | `0x78` |
//! | 40ms | **1** | `0x79` |
//! | 60ms | **2** | `0x7B` |
//!
//! 按 `n <= channels` 判会在 60ms 下**漏判全部 DTX 帧**（2 > 1），
//! 于是「静音期不发包」的统计完全失真。
//!
//! 正确判据是「包里有没有真正的数据字节」—— TOC 之外的、不属于
//! 帧数/长度/padding 字段的字节。DTX 包的数据字节恰好为 0。
//!
//! # 为什么不直接用 `opus_packet_parse`
//!
//! `audiopus` 没有包装它（只包装了 bandwidth / samples_per_frame /
//! nb_samples / nb_channels），直接调 `ffi::opus_packet_parse` 需要
//! `unsafe`，而本 crate 是 `#![forbid(unsafe_code)]`。
//!
//! 这里只解析 TOC 与帧头，不碰帧内容 —— 那是 RFC 6716 §3 里定义得
//! 最清楚的一段，二十行就能写完。

/// 包的第一个字节（TOC）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Toc {
    /// 配置号 0-31：决定模式、带宽、内部帧长
    pub config: u8,
    /// 立体声标志
    pub stereo: bool,
    /// 帧数编码 0-3
    pub code: u8,
}

/// 解析 TOC 字节
pub fn toc(packet: &[u8]) -> Option<Toc> {
    let b = *packet.first()?;
    Some(Toc {
        config: b >> 3,
        stereo: (b >> 2) & 1 == 1,
        code: b & 0b11,
    })
}

/// TOC 里 config 对应的**内部帧长**（微秒）
///
/// # 这张表解释了「40ms / 60ms 帧」的真相
///
/// 实测 32kbps Voip 下：
///
/// - 20ms 包 → code=0，config=15 → 1 × 20ms
/// - 40ms 包 → code=1，config=15 → **2 × 20ms**
/// - 60ms 包 → code=3，config=15 → **3 × 20ms**
///
/// 也就是说 libopus 并没有「40ms 模式」—— 它把 2 个 20ms 帧打进一个包。
/// 60ms 同理。这直接影响丢包容错：丢一个 60ms 包 = 丢 3 个内部帧。
pub fn config_frame_us(config: u8) -> u64 {
    match config {
        // SILK：NB / MB / WB，帧长 10 / 20 / 40 / 60
        0..=11 => [10_000, 20_000, 40_000, 60_000][(config % 4) as usize],
        // Hybrid：SWB / FB，帧长 10 / 20
        12..=15 => [10_000, 20_000][(config % 2) as usize],
        // CELT：NB / WB / SWB / FB，帧长 2.5 / 5 / 10 / 20
        _ => [2_500, 5_000, 10_000, 20_000][(config % 4) as usize],
    }
}

/// 一个包里装了几个内部帧
pub fn frame_count(packet: &[u8]) -> Option<usize> {
    let t = toc(packet)?;
    Some(match t.code {
        0 => 1,
        1 | 2 => 2,
        _ => {
            // code 3：第二字节低 6 位是帧数
            let b = *packet.get(1)?;
            (b & 0x3f) as usize
        }
    })
}

/// 包里「真正的数据字节数」（扣掉 TOC、帧数、长度字段、padding）
///
/// DTX 包返回 0。
pub fn data_bytes(packet: &[u8]) -> Option<usize> {
    let t = toc(packet)?;
    let len = packet.len();
    let header = match t.code {
        0 => 1,
        1 => 1,
        2 => {
            // 第二字节是第 1 帧长度；>=252 表示用 2 字节编码
            if *packet.get(1)? < 252 {
                2
            } else {
                3
            }
        }
        _ => {
            let b = *packet.get(1)?;
            let vbr = b & 0x80 != 0;
            let pad = b & 0x40 != 0;
            let count = (b & 0x3f) as usize;
            let mut pos = 2;
            let mut pad_bytes = 0usize;
            if pad {
                loop {
                    let v = *packet.get(pos)?;
                    pos += 1;
                    pad_bytes += v as usize;
                    if v != 255 {
                        break;
                    }
                }
            }
            if vbr {
                for _ in 0..count.saturating_sub(1) {
                    let v = *packet.get(pos)?;
                    pos += 1;
                    if v >= 252 {
                        pos += 1;
                    }
                }
            }
            // padding 字节在包尾，不占数据
            let _ = pad_bytes;
            return Some(len.saturating_sub(pos).saturating_sub(pad_bytes));
        }
    };
    Some(len.saturating_sub(header))
}

/// 这个包是不是 DTX（不含任何音频数据）
pub fn is_dtx(packet: &[u8]) -> bool {
    matches!(data_bytes(packet), Some(0))
}

/// 包声明的总时长（微秒）= 内部帧长 × 帧数
pub fn packet_duration_us(packet: &[u8]) -> Option<u64> {
    let t = toc(packet)?;
    let n = frame_count(packet)?;
    Some(config_frame_us(t.config) * n as u64)
}
