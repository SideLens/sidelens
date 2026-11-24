//! 重放防护
//!
//! # 为什么加密之外还要这一层
//!
//! 加密保证「看不懂」和「改不了」，但**不保证「不是旧的」**。
//! 攻击者把 30 秒前录到的一整段密文原样重发，每一帧都能通过认证 ——
//! 因为密钥没变、密文没变、标签也没变。
//!
//! 对摄像头的后果很具体：把一段「用户在电脑前」的旧画面循环重放，
//! 通话对方看到的是活人，实际桌前已经没人。
//!
//! # 为什么必须按流分开计数
//!
//! 协议里视频是 `stream_id=0`、音频是 `stream_id=1`，两套 `seq`
//! **各自从 0 累加**。用一个窗口管所有流会出现：
//!
//! ```text
//! 视频 seq=100 → 窗口 [0, 100]
//! 音频 seq=3   → 3 < 100 - window，判为重放 → 音频全被丢掉
//! ```
//!
//! 实测症状：音频一帧都进不了抖动缓冲，而日志只说「重放」，
//! 排查方向会被完全带偏。所以窗口是 `HashMap<stream_id, ReplayWindow>`。
//!
//! # 窗口开多大
//!
//! 太小会误杀**正常的乱序包**。UDP 在 WiFi 上乱序 10-30 个包很常见，
//! NACK 重传更是可能晚到几百个 seq。开 1024：覆盖任何合理乱序，
//! 内存只是一个 `[u64; 16]` 位图。

use std::collections::HashMap;

/// 重放检测失败
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayError {
    /// 这个 seq 已经收过了
    Duplicate(u32),
    /// seq 太旧，已滑出窗口
    TooOld {
        /// 收到的 seq
        seq: u32,
        /// 当前窗口下界
        lowest: u32,
    },
}

impl std::fmt::Display for ReplayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReplayError::Duplicate(seq) => write!(f, "seq={seq} 重复"),
            ReplayError::TooOld { seq, lowest } => {
                write!(f, "seq={seq} 过旧（窗口下界 {lowest}）")
            }
        }
    }
}

impl std::error::Error for ReplayError {}

/// 默认窗口大小（位）
pub const DEFAULT_WINDOW: u32 = 1024;

/// 单个流的重放窗口
///
/// # 实现方式：最高 seq + 位图
///
/// 记录「见过的最大 seq」以及它往前 `window` 个位置里哪些见过。
/// 比最高 seq 还新的直接接受并滑动；比下界还旧的直接拒绝；
/// 中间的查位图。
///
/// # 为什么用 u32 回绕比较而不是 i64
///
/// `seq` 是 u32，跑到 2^32 会回绕到 0。直接比大小会把回绕后的新包
/// 判成「过旧」。用有符号差值（`a.wrapping_sub(b) as i32`）比较：
/// 只要两个 seq 相差不到 2^31，差值符号就是正确的 —— 对实时媒体
/// （每秒几十到几百个包）来说，这个前提永远成立。
#[derive(Debug, Clone)]
pub struct ReplayWindow {
    /// 见过的最大 seq；`None` = 还没收到任何包
    highest: Option<u32>,
    /// 位图：bit 0 对应 `highest`，bit i 对应 `highest - i`
    bits: Vec<u64>,
    /// 窗口大小（位）
    window: u32,
    /// 累计拒绝的重放包
    pub rejected: u64,
    /// 累计接受的包
    pub accepted: u64,
}

impl Default for ReplayWindow {
    fn default() -> Self {
        Self::new(DEFAULT_WINDOW)
    }
}

impl ReplayWindow {
    /// 构造指定窗口大小的窗口
    ///
    /// 窗口向上取整到 64 的倍数（位图按 u64 存）。
    pub fn new(window: u32) -> Self {
        let w = window.max(64).div_ceil(64) * 64;
        Self {
            highest: None,
            bits: vec![0u64; (w / 64) as usize],
            window: w,
            rejected: 0,
            accepted: 0,
        }
    }

    /// 实际窗口大小（已向上取整）
    pub fn window(&self) -> u32 {
        self.window
    }

    /// 见过的最大 seq
    pub fn highest(&self) -> Option<u32> {
        self.highest
    }

    /// 窗口下界（含）
    pub fn lowest(&self) -> Option<u32> {
        self.highest.map(|h| h.wrapping_sub(self.window - 1))
    }

    /// 检查并标记一个 seq
    ///
    /// 接受返回 `Ok(())`，重放返回 `Err`。
    pub fn check_and_mark(&mut self, seq: u32) -> Result<(), ReplayError> {
        let Some(highest) = self.highest else {
            // 第一个包：直接接受
            self.highest = Some(seq);
            self.bits[0] = 1;
            self.accepted += 1;
            return Ok(());
        };

        // 有符号差值比较，正确处理 u32 回绕
        let diff = seq.wrapping_sub(highest) as i32;

        if diff > 0 {
            // 更新的包：整体滑动
            let shift = diff as u32;
            self.slide(shift);
            self.highest = Some(seq);
            self.bits[0] |= 1;
            self.accepted += 1;
            return Ok(());
        }

        // 比最高还旧（或相等）：查位图
        let back = diff.unsigned_abs();
        if back >= self.window {
            self.rejected += 1;
            return Err(ReplayError::TooOld {
                seq,
                lowest: highest.wrapping_sub(self.window - 1),
            });
        }

        let word = (back / 64) as usize;
        let bit = back % 64;
        if self.bits[word] & (1u64 << bit) != 0 {
            self.rejected += 1;
            return Err(ReplayError::Duplicate(seq));
        }

        self.bits[word] |= 1u64 << bit;
        self.accepted += 1;
        Ok(())
    }

    /// 位图整体左移 `shift` 位（朝"更旧"的方向）
    fn slide(&mut self, shift: u32) {
        let words = (shift / 64) as usize;
        let bits = shift % 64;

        if words >= self.bits.len() {
            // 整个窗口都被冲掉
            self.bits.iter_mut().for_each(|w| *w = 0);
            return;
        }

        if words > 0 {
            // 从旧端往新端搬：bits[i] = bits[i - words]
            for i in (0..self.bits.len()).rev() {
                self.bits[i] = if i >= words { self.bits[i - words] } else { 0 };
            }
        }

        if bits > 0 {
            // 再按位左移，低位补 0
            let mut carry = 0u64;
            for w in self.bits.iter_mut() {
                let next = *w >> (64 - bits);
                *w = (*w << bits) | carry;
                carry = next;
            }
        }
    }

    /// 复位（新会话开始时调用）
    pub fn reset(&mut self) {
        self.highest = None;
        self.bits.iter_mut().for_each(|w| *w = 0);
        self.rejected = 0;
        self.accepted = 0;
    }
}

/// 按流分开的重放防护
///
/// 见模块文档「为什么必须按流分开计数」。
#[derive(Debug)]
pub struct ReplayGuard {
    windows: HashMap<u16, ReplayWindow>,
    window_size: u32,
}

impl Default for ReplayGuard {
    fn default() -> Self {
        Self::new(DEFAULT_WINDOW)
    }
}

impl ReplayGuard {
    /// 每个流用多大的窗口
    pub fn new(window_size: u32) -> Self {
        Self {
            windows: HashMap::new(),
            window_size,
        }
    }

    /// 检查并标记
    pub fn check_and_mark(&mut self, stream_id: u16, seq: u32) -> Result<(), ReplayError> {
        self.windows
            .entry(stream_id)
            .or_insert_with(|| ReplayWindow::new(self.window_size))
            .check_and_mark(seq)
    }

    /// 某个流的窗口（只读）
    pub fn window(&self, stream_id: u16) -> Option<&ReplayWindow> {
        self.windows.get(&stream_id)
    }

    /// 已知的流数量
    pub fn stream_count(&self) -> usize {
        self.windows.len()
    }

    /// 累计拒绝数（所有流）
    pub fn total_rejected(&self) -> u64 {
        self.windows.values().map(|w| w.rejected).sum()
    }

    /// 累计接受数（所有流）
    pub fn total_accepted(&self) -> u64 {
        self.windows.values().map(|w| w.accepted).sum()
    }

    /// 全部复位
    pub fn reset(&mut self) {
        self.windows.values_mut().for_each(|w| w.reset());
    }
}
