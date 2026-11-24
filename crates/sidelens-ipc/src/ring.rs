//! 环形缓冲布局
//!
//! # 为什么用环形缓冲而不是队列
//!
//! 帧传输是「写端持续写、读端按需读」的生产者-消费者问题。
//! 用队列需要动态分配，而共享内存是**固定大小**的映射区，
//! 只能预先切好 slot 循环复用。
//!
//! # 为什么 slot 数必须是 2 的幂
//!
//! 取模运算 `seq % slot_count` 是除法，在热路径上比位运算慢。
//! 用 `seq & (slot_count - 1)` 替代，要求 slot_count 是 2 的幂。
//!
//! # 丢帧策略
//!
//! 读端跟不上时（比如消费端只以 15fps 取帧，而写端 60fps），
//! 环形缓冲会被写满。此时**必须丢旧帧保新帧** ——
//! 实时视频里旧帧已经没有价值，等它只会累积延迟。
//!
//! 这通过「写端覆盖最旧的未读 slot」实现。读端发现 seq 跳跃就知道丢了帧。

#![forbid(unsafe_code)]

/// 环形缓冲布局
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RingLayout {
    /// slot 数量（必须是 2 的幂）
    slot_count: u32,
    /// 每个 slot 的字节容量
    slot_capacity: usize,
}

impl RingLayout {
    /// 创建布局
    ///
    /// `slot_count` 会被向上取整到 2 的幂。
    pub fn new(slot_count: u32, slot_capacity: usize) -> Self {
        let count = slot_count.max(1).next_power_of_two();
        Self {
            slot_count: count,
            slot_capacity,
        }
    }

    pub fn slot_count(&self) -> u32 {
        self.slot_count
    }

    pub fn slot_capacity(&self) -> usize {
        self.slot_capacity
    }

    /// 共享内存总大小
    pub fn total_size(&self) -> usize {
        self.slot_count as usize * self.slot_capacity
    }

    /// 由帧序号算出 slot 下标
    ///
    /// 用位与代替取模 —— 热路径上快 3-5 倍。
    pub fn slot_of(&self, seq: u32) -> u32 {
        seq & (self.slot_count - 1)
    }

    /// slot 在共享内存中的字节偏移
    pub fn offset_of(&self, slot: u32) -> usize {
        (slot as usize) * self.slot_capacity
    }

    /// 由帧序号算出偏移（两步合一，省一次函数调用）
    pub fn offset_for_seq(&self, seq: u32) -> usize {
        self.offset_of(self.slot_of(seq))
    }

    /// 给定分辨率能否放进一个 slot
    pub fn fits(&self, required_bytes: usize) -> bool {
        required_bytes <= self.slot_capacity
    }

    /// 推荐配置：按分辨率和目标帧数算
    ///
    /// 缓冲 4 帧是常见选择：
    /// - 2 帧：读写交替，任何抖动都会撞车
    /// - 4 帧：能吸收一个完整 RTT 的抖动
    /// - 8 帧以上：内存浪费，且增加延迟
    pub fn recommended(width: u32, height: u32, frames: u32) -> Self {
        // NV12 = 1.5 字节/像素
        let frame_bytes = (width as usize) * (height as usize) * 3 / 2;
        Self::new(frames, frame_bytes)
    }
}

impl Default for RingLayout {
    fn default() -> Self {
        // 默认 720p × 4 帧
        Self::recommended(1280, 720, 4)
    }
}

/// slot 的占用状态
///
/// 用「写端序号」与「读端序号」的差值判断：
///
/// ```text
/// write_seq = 10, read_seq = 8  → 有 2 帧待读
/// write_seq = 10, read_seq = 10 → 空
/// write_seq - read_seq >= slot_count → 读端落后一圈，必然丢帧
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SlotIndex {
    /// 写端已写入的帧数（单调递增，会回绕）
    pub write_seq: u32,
    /// 读端已读走的帧数
    pub read_seq: u32,
}

impl SlotIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// 待读帧数
    pub fn pending(&self) -> u32 {
        self.write_seq.wrapping_sub(self.read_seq)
    }

    pub fn is_empty(&self) -> bool {
        self.pending() == 0
    }

    /// 读端落后了多少帧（用于判断是否发生了丢帧）
    ///
    /// `slot_count` 是环形缓冲的容量。超过它说明写端已经覆盖过
    /// 读端还没读的 slot。
    pub fn dropped(&self, slot_count: u32) -> u32 {
        self.pending().saturating_sub(slot_count)
    }

    /// 写端推进一步
    pub fn advance_write(&mut self) {
        self.write_seq = self.write_seq.wrapping_add(1);
    }

    /// 读端推进一步
    pub fn advance_read(&mut self) {
        self.read_seq = self.read_seq.wrapping_add(1);
    }

    /// 读端跳到最新（丢弃中间所有帧）
    ///
    /// 消费端要求"给我最新一帧"时用 —— 这是实时视频的正确行为。
    pub fn skip_to_latest(&mut self) {
        self.read_seq = self.write_seq;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_count_rounds_up_to_power_of_two() {
        assert_eq!(RingLayout::new(3, 1024).slot_count(), 4);
        assert_eq!(RingLayout::new(5, 1024).slot_count(), 8);
        assert_eq!(RingLayout::new(4, 1024).slot_count(), 4);
        assert_eq!(RingLayout::new(1, 1024).slot_count(), 1);
    }

    #[test]
    fn slot_index_wraps_correctly() {
        let layout = RingLayout::new(4, 1024);
        assert_eq!(layout.slot_of(0), 0);
        assert_eq!(layout.slot_of(3), 3);
        assert_eq!(layout.slot_of(4), 0, "序号 4 应回到 slot 0");
        assert_eq!(layout.slot_of(7), 3);
    }

    #[test]
    fn offset_is_slot_times_capacity() {
        let layout = RingLayout::new(4, 1000);
        assert_eq!(layout.offset_of(0), 0);
        assert_eq!(layout.offset_of(2), 2000);
        assert_eq!(layout.offset_for_seq(5), 1000, "seq 5 → slot 1 → 偏移 1000");
    }

    #[test]
    fn pending_counts_unread_frames() {
        let mut idx = SlotIndex::new();
        assert!(idx.is_empty());

        idx.advance_write();
        idx.advance_write();
        assert_eq!(idx.pending(), 2);

        idx.advance_read();
        assert_eq!(idx.pending(), 1);
    }

    #[test]
    fn dropped_detects_overrun() {
        let mut idx = SlotIndex::new();
        // 写端跑了 10 帧，读端一帧没读，容量 4
        for _ in 0..10 {
            idx.advance_write();
        }
        assert_eq!(idx.pending(), 10);
        assert_eq!(idx.dropped(4), 6, "10 帧待读但只能存 4 帧，丢 6 帧");

        // 容量足够时不丢
        assert_eq!(idx.dropped(16), 0);
    }

    #[test]
    fn skip_to_latest_clears_backlog() {
        let mut idx = SlotIndex::new();
        for _ in 0..8 {
            idx.advance_write();
        }
        assert_eq!(idx.pending(), 8);
        idx.skip_to_latest();
        assert!(idx.is_empty(), "跳过后不应有积压");
    }

    #[test]
    fn recommended_layout_fits_720p() {
        let layout = RingLayout::recommended(1280, 720, 4);
        assert_eq!(layout.slot_count(), 4);
        // 1280*720*1.5 = 1,382,400
        assert_eq!(layout.slot_capacity(), 1_382_400);
        assert!(layout.fits(1_382_400));
        assert!(!layout.fits(1_382_401));
    }

    #[test]
    fn total_size_is_slot_count_times_capacity() {
        let layout = RingLayout::new(4, 1000);
        assert_eq!(layout.total_size(), 4000);
    }
}
