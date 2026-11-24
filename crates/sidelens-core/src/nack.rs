//! NACK：丢包重传请求
//!
//! # 为什么需要它
//!
//! 视频帧之间有依赖关系（P 帧依赖前面的 I/P 帧）。丢一个包，后面所有帧都解不出来，
//! 只能等下一个关键帧 —— 那可能是 2 秒后。NACK 让接收端能说"我缺第 12、13 帧"，
//! 发送端从缓存里重发，把 2 秒的卡顿压到 1 个 RTT。
//!
//! # 设计取舍
//!
//! | 方案 | 延迟开销 | 带宽开销 | 适用 |
//! | --- | --- | --- | --- |
//! | 每个包都确认（ACK） | 1 RTT | 小 | 可靠传输 |
//! | 只在缺失时请求（NACK） | 1 RTT | 小 | **实时视频** |
//! | 前向纠错（FEC） | 0 | 大（+20-50%） | 高丢包、低延迟 |
//!
//! 选 NACK：实时视频的大多数包都能到，为少数丢包付出 1 RTT 比牺牲 30% 带宽划算。
//! FEC 留作后续（`FrameType::Fec` 已预留）。
//!
//! # 请求的三种生成策略
//!
//! 1. **即时**：发现序号跳跃立刻请求（延迟最低，但可能因乱序误报）
//! 2. **延迟**：等 `nack_delay_ms` 再请求（给乱序留时间，误报少）
//! 3. **批量**：攒一小段时间的缺失一起请求（请求数少）
//!
//! 本实现用 **延迟 + 批量**：等 5ms 攒一批，一次请求多个序号。

use serde::{Deserialize, Serialize};

/// NACK 请求载荷
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NackRequest {
    /// 流的标识（0=视频，1=音频）
    pub stream: u16,
    /// 缺失的序号列表（升序）
    pub seqs: Vec<u32>,
}

impl NackRequest {
    pub fn new(stream: u16, seqs: Vec<u32>) -> Self {
        let mut seqs = seqs;
        seqs.sort_unstable();
        seqs.dedup();
        Self { stream, seqs }
    }

    pub fn is_empty(&self) -> bool {
        self.seqs.is_empty()
    }

    pub fn len(&self) -> usize {
        self.seqs.len()
    }

    /// 编码为 JSON 字节
    pub fn to_bytes(&self) -> Vec<u8> {
        // 这个结构不会序列化失败（全是整数），失败时返回空对象而非 panic
        serde_json::to_vec(self).unwrap_or_else(|_| b"{\"stream\":0,\"seqs\":[]}".to_vec())
    }

    /// 从 JSON 字节解析
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        serde_json::from_slice(bytes).ok()
    }
}

/// 缺失序号的状态
#[derive(Debug, Clone, Copy)]
struct MissingEntry {
    /// 首次发现时刻（微秒）
    first_seen_us: u64,
    /// 上次请求时刻（微秒）；0 表示还没请求过
    last_requested_us: u64,
    /// 已被请求的次数
    request_count: u32,
}

/// 缺失序号跟踪器（接收端用）
///
/// 记录"哪些 seq 还没到"，在 `poll` 时吐出到期的请求。
///
/// # 为什么要延迟而不是立刻请求
///
/// 网络乱序会让 seq 3 比 seq 2 先到。如果立刻请求 seq 2，等重传到达时
/// 原来的 seq 2 可能已经到了 —— 白费一次 RTT 和一份带宽。
/// 等 `delay_ms` 让乱序的包先到齐，能大幅减少误报。
///
/// # 为什么请求后不删记录
///
/// 早期实现里 `poll` 一发出请求就把记录删了，导致两个问题：
///
/// 1. **重传包再丢就没人管了**：那个 seq 从此消失，接收端永远等不到。
/// 2. **`healed_before_request` 永远是 0**：原包如果后来到了，
///    找不到记录，无法统计"延迟策略到底救回了多少误报"。
///
/// 现在改成保留记录，按 `retry_ms` 间隔重发，直到收到或超出重试次数。
#[derive(Debug)]
pub struct NackGenerator {
    /// 缺失的 seq → 状态
    pending: std::collections::BTreeMap<u32, MissingEntry>,
    /// 延迟多久后发出首次请求（毫秒）
    delay_ms: f64,
    /// 同一序号两次请求之间的间隔（毫秒）
    retry_ms: f64,
    /// 单次请求最多包含多少个序号
    max_batch: usize,
    /// 同一序号最多请求几次
    max_requests_per_seq: u32,
    /// 统计
    pub total_missing: u64,
    pub total_requests: u64,
    pub total_seqs_requested: u64,
    /// 被乱序"自愈"的误报数（请求发出前原包就到了）
    pub healed_before_request: u64,
    /// 请求多次仍未收到，最终放弃的序号数
    pub gave_up: u64,
}

impl Default for NackGenerator {
    fn default() -> Self {
        Self::new(5.0, 32)
    }
}

impl NackGenerator {
    /// `delay_ms`：首次请求前的等待（给乱序留时间）
    /// `max_batch`：单次请求最多带多少序号
    pub fn new(delay_ms: f64, max_batch: usize) -> Self {
        Self {
            pending: std::collections::BTreeMap::new(),
            delay_ms,
            retry_ms: 20.0,
            max_batch: max_batch.max(1),
            max_requests_per_seq: 3,
            total_missing: 0,
            total_requests: 0,
            total_seqs_requested: 0,
            healed_before_request: 0,
            gave_up: 0,
        }
    }

    /// 设置重发间隔与重试上限
    pub fn with_retry(mut self, retry_ms: f64, max_requests: u32) -> Self {
        self.retry_ms = retry_ms.max(1.0);
        self.max_requests_per_seq = max_requests.max(1);
        self
    }

    /// 报告"seq 缺失"（发现序号跳跃时调用）
    pub fn mark_missing(&mut self, seq: u32, now_us: u64) {
        self.pending.entry(seq).or_insert_with(|| {
            self.total_missing += 1;
            MissingEntry {
                first_seen_us: now_us,
                last_requested_us: 0,
                request_count: 0,
            }
        });
    }

    /// 报告"seq 到了"（乱序补上时调用，撤销缺失记录）
    pub fn mark_received(&mut self, seq: u32) {
        if self.pending.remove(&seq).is_some() {
            self.healed_before_request += 1;
        }
    }

    /// 当前待请求的缺失数
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// 取出到期的请求。没有到期项时返回 None。
    ///
    /// 判定"到期"两种情况：
    /// - 首次请求：`now - first_seen >= delay_ms`
    /// - 重发请求：`now - last_requested >= retry_ms` 且未超重试上限
    ///
    /// 注意 `saturating_sub` 在这里会出错：`now < delay` 时它返回 0，
    /// 而记录时刻通常也接近 0，会让刚标记的缺失立刻被判为到期。
    pub fn poll(&mut self, now_us: u64, stream: u16) -> Option<NackRequest> {
        let delay_us = (self.delay_ms * 1000.0) as u64;
        let retry_us = (self.retry_ms * 1000.0) as u64;
        let max_req = self.max_requests_per_seq;

        // 先清理超出重试次数的
        let exhausted: Vec<u32> = self
            .pending
            .iter()
            .filter(|(_, e)| e.request_count >= max_req)
            .map(|(s, _)| *s)
            .collect();
        for s in exhausted {
            self.pending.remove(&s);
            self.gave_up += 1;
        }

        let due: Vec<u32> = self
            .pending
            .iter()
            .filter(|(_, e)| {
                let ready_first =
                    e.request_count == 0 && now_us.saturating_sub(e.first_seen_us) >= delay_us;
                let ready_retry =
                    e.request_count > 0 && now_us.saturating_sub(e.last_requested_us) >= retry_us;
                ready_first || ready_retry
            })
            .map(|(s, _)| *s)
            .take(self.max_batch)
            .collect();

        if due.is_empty() {
            return None;
        }

        // 保留记录（可能重发），只更新请求时刻与计数
        for s in &due {
            if let Some(e) = self.pending.get_mut(s) {
                e.last_requested_us = now_us;
                e.request_count += 1;
            }
        }
        self.total_requests += 1;
        self.total_seqs_requested += due.len() as u64;

        Some(NackRequest::new(stream, due))
    }

    /// 丢弃过旧的缺失记录（比如已经等到关键帧，重传没意义了）
    pub fn forget_before(&mut self, seq: u32) {
        self.pending = self.pending.split_off(&seq);
    }

    pub fn clear(&mut self) {
        self.pending.clear();
    }
}
