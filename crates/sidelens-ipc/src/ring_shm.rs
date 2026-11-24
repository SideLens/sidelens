//! 共享内存环形缓冲
//!
//! # 内存布局
//!
//!    偏移 0                    64
//!    ├─────────────────────────┼──────────────────────┬──────────────────────┬────────────────────────────┤
//!    │  RingHeader (64 字节)   │ slot_seq[0..N] ×4B   │ meta[0..N] ×40B      │ pixels slot 0..N           │
//!    │  magic/version/...      │ 每 slot 的 seqlock    │ 每帧 FrameMeta        │ 每 slot_capacity 字节       │
//!    └─────────────────────────┴──────────────────────┴──────────────────────┴────────────────────────────┘
//!
//! 元数据和像素分开存，是为了让「只想看有哪些帧」的读者不用碰 3MB 的像素区 ——
//! 读 meta 只碰 40 字节，缓存友好。
//!
//! # 为什么光有 write_seq / read_seq 不够（真实踩坑）
//!
//! 最初的实现是这样的：
//!
//!    写者：                          读者：
//!    seq = write_seq.load()          w = write_seq.load(Acquire)
//!    slot = seq & mask               seq = w - 1; slot = seq & mask
//!    写 pixels[slot]                 读 meta[slot]
//!    写 meta[slot]                   读 pixels[slot]        ← 危险！
//!    write_seq.store(seq+1,Release)  read_seq.store(w)
//!
//! 并发测试立刻抓到了撕裂帧：读端读到的 meta 说 seq=100，
//! 但像素内容是 seq=108 的。原因是读端在「读完 meta」到「读像素」
//! 之间，写端已经又写了 8 帧（slot 数刚好是 8），转了一圈回到同一个
//! slot 并把它覆盖了。**读者没有任何手段发现这件事**。
//!
//! 这不是内存序问题，是协议缺失。正确的做法是给每个 slot 加一把
//! seqlock（顺序锁）：
//!
//!    写者：                         读者：
//!    slot_seq[slot] += 1  (奇数)    s1 = slot_seq[slot].load(Acquire)
//!    写 pixels + meta               if s1 & 1 == 1 → 正在写，重试
//!    slot_seq[slot] += 1  (偶数)    拷贝 meta + pixels
//!                                   s2 = slot_seq[slot].load(Acquire)
//!                                   if s1 != s2 → 被覆盖，重试
//!
//! 奇偶是「正在写」的标志位；s1 == s2 保证拷贝期间没有写者进来。
//! 读端不加锁、不阻塞写端 —— 这正是实时视频需要的：宁可重试或丢帧，
//! 也绝不能让写端等。
//!
//! # 为什么不用信号量
//!
//! 信号量要陷入内核，一次约 1-2 微秒。60fps 下每帧两次就是 240 微秒/秒，
//! 听着不多，但它会引入不可预测的调度抖动。原子操作是纯用户态，
//! 在 x86 上是 lock 前缀指令，亚纳秒级。
//!
//! # 唯一需要的同步是「通知」
//!
//! 读者不能忙等（会烧满一个核）。序号协议只负责**数据一致性**，
//! 「有新帧了」这个通知走命名管道 —— 阻塞在内核里，不耗 CPU。

#![allow(unsafe_code)]

use std::io;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::ring::RingLayout;
use crate::shm::ShmMapping;
use crate::wire::FrameMeta;

/// 魔数 "SLNR"（小端）
pub const MAGIC: u32 = 0x534C_4E52;
/// 头部版本
///
/// v1 无 seqlock，v2 加了 per-slot seqlock。版本不匹配直接拒绝打开，
/// 而不是带着错误布局去读写。
pub const HEADER_VERSION: u32 = 2;
/// 头部字节数
pub const HEADER_SIZE: usize = 64;

/// 读者重试上限
///
/// 写端持续高速写时，读者可能连续撞上「正在写」。设上限是为了保证
/// read_latest 有确定的执行时间上界 —— 取不到就返回 None，
/// 让调用方下个节拍再来，而不是卡在这里。
const MAX_READ_RETRIES: u32 = 4;

/// 头部字段偏移
mod off {
    pub const MAGIC: usize = 0;
    pub const VERSION: usize = 4;
    pub const SLOT_COUNT: usize = 8;
    pub const SLOT_CAPACITY: usize = 12;
    pub const WRITE_SEQ: usize = 16;
    pub const READ_SEQ: usize = 20;
    pub const DROPPED: usize = 24;
    pub const RESERVED0: usize = 28;
    pub const TOTAL_WRITTEN: usize = 32;
    pub const TOTAL_READ: usize = 40;
    pub const RESERVED1: usize = 48;
    pub const RESERVED2: usize = 56;
}

/// 共享内存里的头部视图
///
/// 所有字段都是原子类型 —— 这是唯一安全的跨进程并发访问方式。
#[derive(Debug)]
struct Header {
    base: *mut u8,
}

// SAFETY: 头部字段全是原子类型，跨线程跨进程访问都是良定义的。
unsafe impl Send for Header {}
unsafe impl Sync for Header {}

impl Header {
    fn atomic_u32(&self, offset: usize) -> &AtomicU32 {
        // SAFETY: base 指向共享内存起点，头部至少 HEADER_SIZE 字节，
        // AtomicU32 的对齐要求是 4 字节，而我们的偏移都是 4 的倍数
        unsafe { &*(self.base.add(offset) as *const AtomicU32) }
    }

    fn atomic_u64(&self, offset: usize) -> &AtomicU64 {
        // SAFETY: 同上；u64 偏移都是 8 的倍数
        unsafe { &*(self.base.add(offset) as *const AtomicU64) }
    }

    fn plain_u32(&self, offset: usize) -> u32 {
        self.atomic_u32(offset).load(Ordering::Relaxed)
    }

    fn write_seq(&self) -> &AtomicU32 {
        self.atomic_u32(off::WRITE_SEQ)
    }

    fn read_seq(&self) -> &AtomicU32 {
        self.atomic_u32(off::READ_SEQ)
    }

    fn dropped(&self) -> &AtomicU32 {
        self.atomic_u32(off::DROPPED)
    }

    fn total_written(&self) -> &AtomicU64 {
        self.atomic_u64(off::TOTAL_WRITTEN)
    }

    fn total_read(&self) -> &AtomicU64 {
        self.atomic_u64(off::TOTAL_READ)
    }

    fn magic(&self) -> u32 {
        self.plain_u32(off::MAGIC)
    }

    fn version(&self) -> u32 {
        self.plain_u32(off::VERSION)
    }

    fn slot_count(&self) -> u32 {
        self.plain_u32(off::SLOT_COUNT)
    }

    fn slot_capacity(&self) -> u32 {
        self.plain_u32(off::SLOT_CAPACITY)
    }

    /// 初始化头部（只有创建者调用，且必须在任何读者打开前完成）
    fn init(&self, layout: &RingLayout) {
        self.atomic_u32(off::MAGIC).store(MAGIC, Ordering::Relaxed);
        self.atomic_u32(off::VERSION)
            .store(HEADER_VERSION, Ordering::Relaxed);
        self.atomic_u32(off::SLOT_COUNT)
            .store(layout.slot_count(), Ordering::Relaxed);
        self.atomic_u32(off::SLOT_CAPACITY)
            .store(layout.slot_capacity() as u32, Ordering::Relaxed);
        self.atomic_u32(off::WRITE_SEQ).store(0, Ordering::Relaxed);
        self.atomic_u32(off::READ_SEQ).store(0, Ordering::Relaxed);
        self.atomic_u32(off::DROPPED).store(0, Ordering::Relaxed);
        self.atomic_u32(off::RESERVED0).store(0, Ordering::Relaxed);
        self.atomic_u64(off::TOTAL_WRITTEN)
            .store(0, Ordering::Relaxed);
        self.atomic_u64(off::TOTAL_READ).store(0, Ordering::Relaxed);
        self.atomic_u64(off::RESERVED1).store(0, Ordering::Relaxed);
        self.atomic_u64(off::RESERVED2).store(0, Ordering::Relaxed);
    }
}

/// 共享内存环形缓冲
///
/// 写端（sidelensd）调 write_frame，读端（vcam DLL）调 read_latest。
///
/// # 容量计算
///
/// 总大小 = HEADER_SIZE + N*4(seqlock) + N*FrameMeta::SIZE + N*slot_capacity
#[derive(Debug)]
pub struct ShmRing {
    shm: ShmMapping,
    layout: RingLayout,
    header: Header,
}

// SAFETY: 所有可变状态都在原子字段里，像素区的并发访问由 seqlock 保护
unsafe impl Send for ShmRing {}
unsafe impl Sync for ShmRing {}

impl ShmRing {
    /// 计算给定布局需要的共享内存总字节数
    pub fn required_size(layout: &RingLayout) -> usize {
        let n = layout.slot_count() as usize;
        HEADER_SIZE + n * 4 + n * FrameMeta::SIZE + layout.total_size()
    }

    /// 创建（写端）
    ///
    /// 同名对象已存在时直接接管 —— 上一轮 sidelensd 崩溃留下的映射会被复用，
    /// 但头部会被重新初始化，序号从 0 开始。
    pub fn create(name: &str, layout: RingLayout) -> io::Result<Self> {
        let size = Self::required_size(&layout);
        let shm = ShmMapping::create(name, size)?;
        let header = Header { base: shm.as_ptr() };
        header.init(&layout);

        let this = Self {
            shm,
            layout,
            header,
        };
        // 清空所有 slot 的 seqlock（全部为偶数的 0）
        for slot in 0..this.layout.slot_count() {
            this.slot_seq(slot).store(0, Ordering::Relaxed);
        }
        Ok(this)
    }

    /// 打开（读端）
    ///
    /// 会校验魔数与版本 —— 版本不匹配直接失败，而不是带着错误布局去读写。
    pub fn open(name: &str) -> io::Result<Self> {
        // 先按最小头部映射一次，读出真实布局，再按完整大小重新映射
        let (slot_count, slot_capacity) = {
            let probe = ShmMapping::open(name, HEADER_SIZE)?;
            let header = Header {
                base: probe.as_ptr(),
            };

            if header.magic() != MAGIC {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "共享内存魔数不匹配：期望 0x{MAGIC:08X} 实际 0x{:08X}",
                        header.magic()
                    ),
                ));
            }
            if header.version() != HEADER_VERSION {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "共享内存版本不匹配：期望 {HEADER_VERSION} 实际 {}（请重启 sidelensd）",
                        header.version()
                    ),
                ));
            }
            (header.slot_count(), header.slot_capacity() as usize)
        };

        let layout = RingLayout::new(slot_count, slot_capacity);
        let size = Self::required_size(&layout);
        let shm = ShmMapping::open(name, size)?;
        let header = Header { base: shm.as_ptr() };
        Ok(Self {
            shm,
            layout,
            header,
        })
    }

    pub fn layout(&self) -> &RingLayout {
        &self.layout
    }

    /// 实际使用的完整对象名
    pub fn name(&self) -> &str {
        self.shm.name()
    }

    /// 写者已写入的帧数
    pub fn written(&self) -> u64 {
        self.header.total_written().load(Ordering::Relaxed)
    }

    /// 读者已读走的帧数
    pub fn read_count(&self) -> u64 {
        self.header.total_read().load(Ordering::Relaxed)
    }

    /// 因写者覆盖而丢失的帧数
    pub fn dropped(&self) -> u32 {
        self.header.dropped().load(Ordering::Relaxed)
    }

    /// 待读帧数
    pub fn pending(&self) -> u32 {
        self.header
            .write_seq()
            .load(Ordering::Relaxed)
            .wrapping_sub(self.header.read_seq().load(Ordering::Relaxed))
    }

    // ---- 分区寻址 ----

    fn slot_seq_offset(slot: u32) -> usize {
        HEADER_SIZE + (slot as usize) * 4
    }

    fn meta_region_offset(&self) -> usize {
        HEADER_SIZE + (self.layout.slot_count() as usize) * 4
    }

    fn pixel_region_offset(&self) -> usize {
        self.meta_region_offset() + (self.layout.slot_count() as usize) * FrameMeta::SIZE
    }

    fn slot_seq(&self, slot: u32) -> &AtomicU32 {
        // SAFETY: slot < slot_count，seqlock 区在头部之后连续排布
        unsafe { &*(self.shm.as_ptr().add(Self::slot_seq_offset(slot)) as *const AtomicU32) }
    }

    fn meta_ptr(&self, slot: u32) -> *mut u8 {
        // SAFETY: 已按 slot_count 校验
        unsafe {
            self.shm
                .as_ptr()
                .add(self.meta_region_offset() + (slot as usize) * FrameMeta::SIZE)
        }
    }

    fn pixel_ptr(&self, slot: u32) -> *mut u8 {
        // SAFETY: 已按 slot_count 校验
        unsafe {
            self.shm
                .as_ptr()
                .add(self.pixel_region_offset() + self.layout.offset_of(slot))
        }
    }

    // ---- 写 ----

    /// 写一帧
    ///
    /// 返回实际使用的 slot 下标。
    ///
    /// # 越界
    ///
    /// pixels.len() > slot_capacity 时返回 InvalidInput —— 宁可报错也不要
    /// 静默截断，截断的视频帧只会让人以为是编解码问题。
    pub fn write_frame(&self, pixels: &[u8], meta: &FrameMeta) -> io::Result<u32> {
        if pixels.len() > self.layout.slot_capacity() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "帧太大：{} 字节，slot 容量 {} 字节",
                    pixels.len(),
                    self.layout.slot_capacity()
                ),
            ));
        }

        let seq = self.header.write_seq().load(Ordering::Relaxed);
        let slot = self.layout.slot_of(seq);
        let lock = self.slot_seq(slot);

        // seqlock 进入：变奇数表示「正在写」
        let s = lock.load(Ordering::Relaxed);
        lock.store(s.wrapping_add(1), Ordering::Release);

        // 先写像素，再写 meta —— meta 里的 payload_len 是读者判断数据有效性的依据
        // SAFETY: slot 由 seq & mask 得出必然 < slot_count；像素长度已校验
        unsafe {
            let dst = self.pixel_ptr(slot);
            std::ptr::copy_nonoverlapping(pixels.as_ptr(), dst, pixels.len());

            let mut m = *meta;
            m.seq = seq;
            m.payload_len = pixels.len() as u32;
            std::ptr::copy_nonoverlapping(
                m.encode().as_ptr(),
                self.meta_ptr(slot),
                FrameMeta::SIZE,
            );
        }

        // seqlock 退出：变回偶数
        lock.store(s.wrapping_add(2), Ordering::Release);

        // Release：保证 slot 内容对读者可见后，才公开新的 write_seq
        self.header
            .write_seq()
            .store(seq.wrapping_add(1), Ordering::Release);
        self.header.total_written().fetch_add(1, Ordering::Relaxed);

        // 读端落后超过一圈 → 记录丢帧
        let read = self.header.read_seq().load(Ordering::Relaxed);
        if seq.wrapping_add(1).wrapping_sub(read) > self.layout.slot_count() {
            self.header.dropped().fetch_add(1, Ordering::Relaxed);
        }

        Ok(slot)
    }

    // ---- 读 ----

    /// 读最新一帧到 buf，返回元数据
    ///
    /// 没有新帧时返回 None。中间积压的旧帧会被**跳过** ——
    /// 实时视频里补播旧帧只会让延迟越滚越大。
    ///
    /// # 撕裂防护
    ///
    /// 用 per-slot seqlock：拷贝前后各读一次 slot 的序号，
    /// 不一致说明拷贝期间写者进来覆盖过，这次结果作废。
    pub fn read_latest(&self, buf: &mut [u8]) -> Option<FrameMeta> {
        let w = self.header.write_seq().load(Ordering::Acquire);
        let r = self.header.read_seq().load(Ordering::Relaxed);
        if w == r {
            return None;
        }

        let seq = w.wrapping_sub(1);
        let slot = self.layout.slot_of(seq);
        let lock = self.slot_seq(slot);

        for _ in 0..MAX_READ_RETRIES {
            let s1 = lock.load(Ordering::Acquire);
            if s1 & 1 == 1 {
                // 写者正在写这个 slot，稍后重试
                continue;
            }

            // SAFETY: slot 在范围内；s1 是偶数说明此刻没有写者
            let meta = unsafe {
                let raw = std::slice::from_raw_parts(self.meta_ptr(slot), FrameMeta::SIZE);
                FrameMeta::decode(raw)?
            };

            let len = (meta.payload_len as usize).min(buf.len());
            if len > 0 {
                // SAFETY: payload_len 写入时已按 slot_capacity 校验；这里再按 buf 截断
                unsafe {
                    std::ptr::copy_nonoverlapping(self.pixel_ptr(slot), buf.as_mut_ptr(), len);
                }
            }

            let s2 = lock.load(Ordering::Acquire);
            if s1 != s2 {
                // 拷贝期间被覆盖，这次结果作废
                continue;
            }

            // 成功：推进 read_seq，告诉写者这些 slot 可以复用了
            self.header.read_seq().store(w, Ordering::Release);
            self.header.total_read().fetch_add(1, Ordering::Relaxed);
            return Some(meta);
        }

        // 重试耗尽：让调用方下个节拍再来，不在这里死等
        None
    }

    /// 丢弃所有积压（读者刚连上时用，避免从陈旧帧开始播）
    pub fn skip_to_latest(&self) {
        let w = self.header.write_seq().load(Ordering::Acquire);
        self.header.read_seq().store(w, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::PixelFormat;

    fn unique(tag: &str) -> String {
        format!(
            "sidelens-ring-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        )
    }

    fn layout() -> RingLayout {
        // 4 个 slot，每个 64 字节 —— 小到能一眼看穿
        RingLayout::new(4, 64)
    }

    fn meta() -> FrameMeta {
        let mut m = FrameMeta::new(64, 1, PixelFormat::Nv12);
        m.pts_us = 1_700_000_000_000;
        m
    }

    #[test]
    fn required_size_accounts_for_all_regions() {
        let l = layout();
        // 64 头 + 4*4 seqlock + 4*40 meta + 4*64 像素 = 64 + 16 + 160 + 256 = 496
        assert_eq!(ShmRing::required_size(&l), 496);
    }

    #[test]
    fn create_then_open_sees_same_layout() {
        let name = unique("open");
        let writer = ShmRing::create(&name, layout()).expect("创建");
        let reader = ShmRing::open(&name).expect("打开");

        assert_eq!(reader.layout().slot_count(), 4);
        assert_eq!(reader.layout().slot_capacity(), 64);
        assert_eq!(reader.name(), writer.name());
    }

    #[test]
    fn single_frame_roundtrip() {
        let name = unique("one");
        let writer = ShmRing::create(&name, layout()).expect("创建");
        let reader = ShmRing::open(&name).expect("打开");

        let pixels = [7u8; 64];
        let slot = writer.write_frame(&pixels, &meta()).expect("写");
        assert_eq!(slot, 0, "第一帧应落在 slot 0");

        let mut buf = [0u8; 64];
        let got = reader.read_latest(&mut buf).expect("应读到帧");
        assert_eq!(got.seq, 0);
        assert_eq!(got.payload_len, 64);
        assert_eq!(got.pts_us, 1_700_000_000_000);
        assert_eq!(buf, pixels);
    }

    #[test]
    fn empty_ring_returns_none() {
        let name = unique("empty");
        let _writer = ShmRing::create(&name, layout()).expect("创建");
        let reader = ShmRing::open(&name).expect("打开");
        let mut buf = [0u8; 64];
        assert!(reader.read_latest(&mut buf).is_none());
    }

    #[test]
    fn reader_gets_latest_and_skips_backlog() {
        let name = unique("skip");
        let writer = ShmRing::create(&name, layout()).expect("创建");
        let reader = ShmRing::open(&name).expect("打开");

        for i in 0..3u8 {
            writer.write_frame(&[i; 64], &meta()).expect("写");
        }

        let mut buf = [0u8; 64];
        let got = reader.read_latest(&mut buf).expect("应读到帧");
        assert_eq!(got.seq, 2, "应跳过前两帧，直接取最新的 seq=2");
        assert_eq!(buf[0], 2);

        assert!(reader.read_latest(&mut buf).is_none());
    }

    #[test]
    fn sequence_wraps_around_slots() {
        let name = unique("wrap");
        let writer = ShmRing::create(&name, layout()).expect("创建");
        let reader = ShmRing::open(&name).expect("打开");

        let mut buf = [0u8; 64];
        for i in 0..10u8 {
            writer.write_frame(&[i; 64], &meta()).expect("写");
            let got = reader.read_latest(&mut buf).expect("读");
            assert_eq!(got.seq, i as u32);
            assert_eq!(buf[0], i, "slot 复用后内容必须是新帧");
        }
        assert_eq!(writer.written(), 10);
        assert_eq!(reader.read_count(), 10);
    }

    #[test]
    fn oversized_frame_is_rejected() {
        let name = unique("big");
        let writer = ShmRing::create(&name, layout()).expect("创建");
        let err = writer.write_frame(&[0u8; 65], &meta()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn read_into_small_buffer_truncates_safely() {
        let name = unique("small");
        let writer = ShmRing::create(&name, layout()).expect("创建");
        let reader = ShmRing::open(&name).expect("打开");

        writer.write_frame(&[9u8; 64], &meta()).expect("写");
        let mut small = [0u8; 10];
        let got = reader.read_latest(&mut small).expect("读");
        assert_eq!(got.payload_len, 64);
        assert_eq!(small, [9u8; 10]);
    }

    #[test]
    fn open_rejects_wrong_magic() {
        let name = unique("magic");
        let shm = ShmMapping::create(&name, ShmRing::required_size(&layout())).expect("建");
        drop(shm);
        assert!(ShmRing::open(&name).is_err());
    }

    #[test]
    fn pending_tracks_unread_frames() {
        let name = unique("pending");
        let writer = ShmRing::create(&name, layout()).expect("创建");
        let reader = ShmRing::open(&name).expect("打开");

        writer.write_frame(&[1u8; 64], &meta()).unwrap();
        writer.write_frame(&[2u8; 64], &meta()).unwrap();
        assert_eq!(writer.pending(), 2);

        let mut buf = [0u8; 64];
        reader.read_latest(&mut buf).unwrap();
        assert_eq!(reader.pending(), 0);
    }

    #[test]
    fn skip_to_latest_discards_backlog() {
        let name = unique("skipto");
        let writer = ShmRing::create(&name, layout()).expect("创建");
        let reader = ShmRing::open(&name).expect("打开");

        for _ in 0..3 {
            writer.write_frame(&[1u8; 64], &meta()).unwrap();
        }
        assert_eq!(writer.pending(), 3);
        reader.skip_to_latest();
        assert_eq!(reader.pending(), 0);
    }

    #[test]
    fn seqlock_is_even_when_idle() {
        let name = unique("seq");
        let writer = ShmRing::create(&name, layout()).expect("创建");
        for slot in 0..4 {
            assert_eq!(
                writer.slot_seq(slot).load(Ordering::Relaxed) & 1,
                0,
                "slot {slot} 空闲时 seqlock 应为偶数"
            );
        }
        writer.write_frame(&[1u8; 64], &meta()).unwrap();
        for slot in 0..4 {
            assert_eq!(
                writer.slot_seq(slot).load(Ordering::Relaxed) & 1,
                0,
                "写完后 seqlock 必须回到偶数"
            );
        }
    }

    #[test]
    fn reader_detects_in_progress_write() {
        let name = unique("busy");
        let writer = ShmRing::create(&name, layout()).expect("创建");
        let reader = ShmRing::open(&name).expect("打开");

        writer.write_frame(&[1u8; 64], &meta()).unwrap();

        // 手工把 slot 0 的 seqlock 变成奇数，模拟「写者正在写」
        writer.slot_seq(0).store(1, Ordering::Release);

        let mut buf = [0u8; 64];
        assert!(
            reader.read_latest(&mut buf).is_none(),
            "写者正在写时必须放弃这次读取，而不是返回半新半旧的数据"
        );

        writer.slot_seq(0).store(2, Ordering::Release);
        assert!(reader.read_latest(&mut buf).is_some(), "写完后应能读到");
    }
}
