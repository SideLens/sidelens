//! nonce 的构造
//!
//! # 为什么把这件事单独抽出来
//!
//! ChaCha20-Poly1305 的安全性完全建立在「**同一把密钥下 nonce 永不重复**」
//! 这一条上。nonce 重复一次，两份明文的异或值就直接暴露，攻击者能
//! 恢复出大量内容。
//!
//! 这条不变量横跨三个模块（握手给密钥、sealer 用它、重传复用密文），
//! 所以 nonce 的拼法必须只有**一个出处**。分散在两处各写一遍，改一处
//! 忘另一处就是静默的安全性降级 —— 而且测试仍然全绿。
//!
//! # 拼法
//!
//! ```text
//! 偏移 0  4B  方向与流号   bit31 = 方向（0 = 发起方→响应方），bit0..15 = stream_id
//! 偏移 4  4B  帧序号 seq（小端）
//! 偏移 8  4B  全 0
//! ```
//!
//! # 为什么后 4 字节留全 0
//!
//! 12 字节的 nonce 空间理论上可以用满，但**用满反而危险**：将来如果
//! 有人要加「同一个 seq 的多个分片各自加密」这类需求，后 4 字节就是
//! 现成的计数器。留白 + 注释比事后改拼法安全。
//!
//! # 为什么方向位是冗余的
//!
//! 两个方向用的是**不同的密钥**（`k_i2r` / `k_r2i`），所以方向位理论上
//! 可以省掉。保留它是纵深防御：万一将来有人把两个方向合并成一把密钥，
//! 这 1 个 bit 会让 nonce 立刻冲突（测试会红），而不是安静地重复。

use crate::handshake::Role;

/// nonce 长度
pub const NONCE_SIZE: usize = 12;

/// 方向位在第一个 u32 里的位置
const DIR_BIT: u32 = 1 << 31;

/// 构造一个 nonce
///
/// `stream_id` 只取低 16 位 —— 协议帧头里它就是 u16。
#[inline]
pub fn build(role: Role, stream_id: u16, seq: u32) -> [u8; NONCE_SIZE] {
    let head = (stream_id as u32) | if role.is_initiator() { 0 } else { DIR_BIT };
    let mut out = [0u8; NONCE_SIZE];
    out[0..4].copy_from_slice(&head.to_le_bytes());
    out[4..8].copy_from_slice(&seq.to_le_bytes());
    // out[8..12] 保持全 0，见模块文档
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 同一输入必须给出同一_nonce() {
        assert_eq!(build(Role::Initiator, 0, 42), build(Role::Initiator, 0, 42));
    }

    #[test]
    fn 方向不同则_nonce_不同() {
        assert_ne!(build(Role::Initiator, 0, 42), build(Role::Responder, 0, 42));
    }

    #[test]
    fn 流号不同则_nonce_不同() {
        assert_ne!(build(Role::Initiator, 0, 42), build(Role::Initiator, 1, 42));
    }

    #[test]
    fn seq_不同则_nonce_不同() {
        assert_ne!(build(Role::Initiator, 0, 42), build(Role::Initiator, 0, 43));
    }

    #[test]
    fn 高流号不会溢出到方向位() {
        // stream_id 是 u16，最大 0xFFFF，绝不能碰到 bit31
        let n = build(Role::Initiator, 0xFFFF, 0);
        assert_eq!(n[3] & 0x80, 0, "方向位被流号污染了");
    }

    #[test]
    fn 尾四字节恒为零() {
        let n = build(Role::Responder, 7, 0xDEAD_BEEF);
        assert_eq!(&n[8..12], &[0, 0, 0, 0]);
    }
}
