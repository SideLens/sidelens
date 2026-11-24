//! 重传缓存（发送端用）
//!
//! # 设计要点
//!
//! 1. **按 seq 索引，容量有界**：视频帧 8KB × 300 帧 ≈ 2.4MB，足够覆盖 5 秒（60fps）。
//! 2. **关键帧永不淘汰**：请求关键帧后如果那个关键帧被淘汰了，接收端就永远恢复不了。
//! 3. **按时间淘汰**：超过 `max_age_ms` 的帧即使没超容量也丢掉 ——
//!    太旧的重传到达时早已过了播放时刻，重传没有意义，只会浪费带宽。
//! 4. **重传次数上限**：同一帧被请求超过 N 次说明链路极差，继续重传是徒劳。

use std::collections::VecDeque;

use sidelens_proto::FrameType;

/// 缓存里的一帧
#[derive(Debug, Clone)]
pub struct CachedFrame {
    pub ty: FrameType,
    pub flags: u8,
    pub stream_id: u16,
    pub seq: u32,
    pub pts_us: u64,
    /// 原始编码字节（含 32 字节头，重传时直接发出去，不重新编码）
    pub bytes: Vec<u8>,
    /// 入缓存时刻（微秒）
    pub cached_at_us: u64,
    /// 已被重传的次数
    pub retransmit_count: u32,
}

impl CachedFrame {
    pub fn is_keyframe(&self) -> bool {
        self.flags & sidelens_proto::FLAG_KEYFRAME != 0
    }

    pub fn size(&self) -> usize {
        self.bytes.len()
    }
}

/// 重传缓存配置
#[derive(Debug, Clone, Copy)]
pub struct RetransmitConfig {
    /// 最多缓存多少帧
    pub max_frames: usize,
    /// 最多缓存多少字节（软上限，关键帧可以突破）
    pub max_bytes: usize,
    /// 超过这么久就淘汰（毫秒）
    pub max_age_ms: f64,
    /// 同一帧最多重传几次
    pub max_retransmits: u32,
}

impl Default for RetransmitConfig {
    fn default() -> Self {
        Self {
            max_frames: 300,
            max_bytes: 8 * 1024 * 1024,
            max_age_ms: 5000.0,
            max_retransmits: 3,
        }
    }
}

/// 存入重传缓存的一帧
#[derive(Debug, Clone)]
pub struct StoreSpec {
    pub ty: FrameType,
    pub flags: u8,
    pub stream_id: u16,
    pub seq: u32,
    pub pts_us: u64,
    /// 完整编码字节（含 32 字节头）
    pub bytes: Vec<u8>,
}

impl StoreSpec {
    /// 最常用的构造：视频帧、无特殊 flags
    pub fn video(seq: u32, pts_us: u64, bytes: Vec<u8>) -> Self {
        Self {
            ty: FrameType::Video,
            flags: 0,
            stream_id: 0,
            seq,
            pts_us,
            bytes,
        }
    }

    /// 带 flags 的构造（关键帧等）
    pub fn with_flags(
        ty: FrameType,
        flags: u8,
        stream_id: u16,
        seq: u32,
        pts_us: u64,
        bytes: Vec<u8>,
    ) -> Self {
        Self {
            ty,
            flags,
            stream_id,
            seq,
            pts_us,
            bytes,
        }
    }
}

/// 重传缓存统计
#[derive(Debug, Clone, Copy, Default)]
pub struct RetransmitStats {
    pub stored: u64,
    pub evicted_by_count: u64,
    pub evicted_by_age: u64,
    pub served: u64,
    pub refused_expired: u64,
    pub refused_over_limit: u64,
    pub bytes_served: u64,
}

/// 发送端重传缓存
///
/// # 用法
///
/// ```
/// # use sidelens_core::retransmit::{RetransmitCache, RetransmitConfig, StoreSpec};
/// let mut cache = RetransmitCache::new(RetransmitConfig::default());
/// // 发送一帧时顺手存进去
/// cache.store(StoreSpec::video(1, 1_000_000, vec![0u8; 100]), 1_000_000);
/// // 收到 NACK 时按 seq 取出（now_us 用于判断是否过期）
/// let frame = cache.take_for_retransmit(1, 2_000_000);
/// assert!(frame.is_some());
/// ```
#[derive(Debug)]
pub struct RetransmitCache {
    config: RetransmitConfig,
    /// 按 seq 索引（同一流内 seq 唯一，暂不支持多流共存于一个缓存）
    frames: std::collections::HashMap<u32, CachedFrame>,
    /// 淘汰顺序（先进先出；关键帧被淘汰后从中间移除，用 retain 清理）
    order: VecDeque<u32>,
    bytes: usize,
    pub stats: RetransmitStats,
}

impl RetransmitCache {
    pub fn new(config: RetransmitConfig) -> Self {
        Self {
            config,
            frames: std::collections::HashMap::new(),
            order: VecDeque::new(),
            bytes: 0,
            stats: RetransmitStats::default(),
        }
    }

    pub fn config(&self) -> &RetransmitConfig {
        &self.config
    }

    pub fn len(&self) -> usize {
        self.frames.len()
    }

    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// 存入一帧（发送时调用）
    ///
    /// 参数用 [`StoreSpec`] 打包而不是散列成 8 个形参：
    /// 调用点更易读，也不会因为参数顺序传错而静默出错。
    pub fn store(&mut self, spec: StoreSpec, now_us: u64) {
        let StoreSpec {
            ty,
            flags,
            stream_id,
            seq,
            pts_us,
            bytes,
        } = spec;

        let frame = CachedFrame {
            ty,
            flags,
            stream_id,
            seq,
            pts_us,
            bytes,
            cached_at_us: now_us,
            retransmit_count: 0,
        };
        self.bytes += frame.size();
        self.frames.insert(seq, frame);
        self.order.push_back(seq);
        self.stats.stored += 1;

        self.evict(now_us);
    }

    /// 淘汰过期与超量的帧
    fn evict(&mut self, now_us: u64) {
        let max_age_us = (self.config.max_age_ms * 1000.0) as u64;
        let mut removed = Vec::new();

        // 1) 按时间淘汰
        for (&seq, f) in &self.frames {
            if now_us.saturating_sub(f.cached_at_us) > max_age_us {
                removed.push(seq);
            }
        }
        for seq in removed {
            if let Some(f) = self.frames.remove(&seq) {
                self.bytes = self.bytes.saturating_sub(f.size());
                self.stats.evicted_by_age += 1;
            }
        }

        // 2) 按数量 / 字节数淘汰（从最老的开始，关键帧跳过）
        while self.frames.len() > self.config.max_frames || self.bytes > self.config.max_bytes {
            let Some(seq) = self.order.pop_front() else {
                break;
            };
            let Some(f) = self.frames.get(&seq) else {
                continue;
            };
            // 关键帧不淘汰，放回队首之后的策略：
            // 直接把关键帧留在 frames 里，只是从 order 里丢弃引用。
            // 下次 evict 不会再看到它，除非它重新 store（那会重新入队）。
            if f.is_keyframe() {
                continue;
            }
            if let Some(f) = self.frames.remove(&seq) {
                self.bytes = self.bytes.saturating_sub(f.size());
                self.stats.evicted_by_count += 1;
            }
        }
    }

    /// 取一帧用于重传
    ///
    /// 返回 `None` 的情况：不存在、已过期、重传次数超限。
    pub fn take_for_retransmit(&mut self, seq: u32, now_us: u64) -> Option<CachedFrame> {
        let max_age_us = (self.config.max_age_ms * 1000.0) as u64;

        let frame = self.frames.get_mut(&seq)?;

        if now_us.saturating_sub(frame.cached_at_us) > max_age_us {
            self.stats.refused_expired += 1;
            return None;
        }
        if frame.retransmit_count >= self.config.max_retransmits {
            self.stats.refused_over_limit += 1;
            return None;
        }

        frame.retransmit_count += 1;
        let snapshot = frame.clone();
        self.stats.served += 1;
        self.stats.bytes_served += snapshot.size() as u64;
        Some(snapshot)
    }

    /// 查询是否存在（不消耗重传配额）
    pub fn contains(&self, seq: u32) -> bool {
        self.frames.contains_key(&seq)
    }

    pub fn clear(&mut self) {
        self.frames.clear();
        self.order.clear();
        self.bytes = 0;
    }
}
