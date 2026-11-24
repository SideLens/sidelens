//! 跨设备时钟同步
//!
//! # 问题
//!
//! 手机和电脑的时钟起点完全不同（`SystemTime` 虽然都以 Unix 纪元为基准，
//! 但两个设备的晶振偏差 + NTP 对时误差会让它们差几十毫秒）。
//!
//! 如果直接用「接收端 now() - 发送端 send_ts」算延迟，得到的是
//! **真实延迟 + 时钟偏移**，可能有几百毫秒的误差。
//!
//! # 方案：NTP 式的四时间戳协商
//!
//! ```text
//! 发送端                        接收端
//!   T1 ──── RTT_PROBE ────────→
//!                                T2 = 收到时刻
//!   T4 ←─── RTT_REPLY ─────────  T3 = 发出时刻（含 T1、T2）
//! ```
//!
//! 网络往返：`RTT = (T4 - T1) - (T3 - T2)`
//! 时钟偏移：`offset = ((T2 - T1) + (T3 - T4)) / 2`
//!
//! 之后所有时间戳都加 `offset` 换算到发送端时基。
//!
//! # 为什么取多次的最小 RTT
//!
//! 单次测量的误差来源是**路径不对称**（去程和回程走的不是同一条路）。
//! RTT 越小，说明这一趟越"直"，不对称性越小，offset 估计越准。
//! 所以保留滑动窗口里 RTT 最小的那次的 offset。
//!
//! # 一个无法消除的偏差（务必知道）
//!
//! 对端记录的 `T2` 是**用户态读到包的时刻**，不是**网卡收到包的时刻**。
//! 中间隔着内核调度：从包进队列到 tokio 唤醒任务，实测有 5ms 左右。
//!
//! 这部分会**整段计入去程**，导致 offset 估计偏负（本端看起来"更快"）。
//! 实测同机回环时 offset 显示 -5.5ms，而真实值应是 0。
//!
//! 后果：
//!
//! - **单向延迟会被高估 5ms 左右**（因为 offset 偏负，`to_local` 把对端时刻往早算）
//! - 这个偏差在真机上会与真实网络不对称混在一起，无法分离
//!
//! NTP 用内核时间戳（`SO_TIMESTAMPNS`）避免这个问题；tokio 没暴露这个能力。
//! 当前接受这个 5ms 级别的偏差 —— 相对 20ms 的抖动缓冲来说是 25% 误差，
//! 但**只影响"延迟数字的绝对值"，不影响抖动缓冲的行为**（后者只看相对间隔）。
//!
//! # 与抖动缓冲的关系
//!
//! 没有同步时，`send_ts` 只能用来算"相对抖动"（相邻帧的间隔变化）。
//! 有了同步，才能算**绝对单程延迟**，这决定了：
//!
//! - 抖动缓冲的目标值该设多少（延迟大的链路需要更大缓冲）
//! - 码率自适应该降多少（延迟持续增长说明在排队）
//! - 是否该丢帧（延迟超过阈值说明追不上了）

use std::collections::VecDeque;

/// 一次测量的结果
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SyncSample {
    /// 网络往返（微秒，已扣除接收端处理时间）
    pub rtt_us: u64,
    /// 发送端时钟 = 接收端时钟 + offset（微秒）
    pub offset_us: i64,
}

/// 四时间戳协商中的本地状态
#[derive(Debug, Clone, Copy)]
pub struct ProbeState {
    /// 探测序号
    pub nonce: u32,
    /// T1：本端发出时刻
    pub t1_us: u64,
}

/// RTT_PROBE 的 JSON 载荷
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RttProbe {
    pub nonce: u32,
    /// 发送端时刻（微秒）
    pub t1_us: u64,
}

/// RTT_REPLY 的 JSON 载荷
///
/// # 为什么它顺便携带码率建议
///
/// 拥塞控制器（[`crate::congestion::CongestionController`]）在**接收端**跑 ——
/// 它需要延迟梯度样本，而那些样本只有收到包的一方才拿得到。
/// 但真正执行码率的是**发送端**的编码器。
///
/// 中间缺一条反馈回路。协议里已经有现成的反向通道
/// （`RTT_REPLY` 每 2 秒一次），再开一个新帧类型不如复用它 ——
/// 两者语义上都是「接收端告诉发送端链路的状况」。
///
/// # 向后兼容
///
/// 新增字段全部是 `Option` + `#[serde(default)]`：
/// 老版本发来的 JSON 缺这些键时反序列化成 `None`，
/// 新版本发给老版本时老版本会忽略未知键（serde 默认行为）。
/// 所以混合版本互通不会崩，只是码率自适应不生效。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RttReply {
    pub nonce: u32,
    /// 回显 T1
    pub t1_us: u64,
    /// 接收端收到时刻
    pub t2_us: u64,
    /// 接收端发出时刻
    pub t3_us: u64,

    /// 接收端拥塞控制器算出的目标码率（bps）
    ///
    /// `None` 表示接收端没有拥塞控制器（老版本，或未启用），
    /// 发送端应保持当前码率不变。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_bps: Option<u64>,
    /// 接收端实测丢包率（千分之一，即 1000 = 100%）
    ///
    /// 用整数而不是 f64：JSON 里的浮点在跨语言/跨版本解析时容易出精度问题，
    /// 而千分之一的精度对码率决策完全够（0.1% 的差别不影响任何决策）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loss_permille: Option<u16>,
}

impl RttProbe {
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_default()
    }

    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        serde_json::from_slice(bytes).ok()
    }
}

impl RttReply {
    /// 只带时钟同步字段的回复（码率建议为 `None`）
    pub fn new(nonce: u32, t1_us: u64, t2_us: u64, t3_us: u64) -> Self {
        Self {
            nonce,
            t1_us,
            t2_us,
            t3_us,
            target_bps: None,
            loss_permille: None,
        }
    }

    /// 附带码率建议的回复
    pub fn with_rate_hint(mut self, target_bps: u64, loss_ratio: f64) -> Self {
        self.target_bps = Some(target_bps);
        // 钳到 0..=1000：100% = 1000，留出余量避免溢出
        let permille = (loss_ratio.clamp(0.0, 1.0) * 1000.0).round() as u16;
        self.loss_permille = Some(permille.min(1000));
        self
    }

    /// 码率建议（bps），`None` 表示对端没给
    pub fn rate_hint(&self) -> Option<u64> {
        self.target_bps
    }

    /// 丢包率（0-1），`None` 表示对端没给
    pub fn loss_ratio(&self) -> Option<f64> {
        self.loss_permille.map(|p| p as f64 / 1000.0)
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_default()
    }

    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        serde_json::from_slice(bytes).ok()
    }
}

/// 时钟同步器
///
/// # 用法（发送端）
///
/// ```
/// # use sidelens_core::sync::ClockSync;
/// let mut sync = ClockSync::new();
/// let probe = sync.start_probe(1_000_000, 42);
/// // ... 发出 RTT_PROBE(t1 = probe.t1_us, nonce = probe.nonce) ...
/// // 收到 RTT_REPLY 后，用本端 t4 与对端的 t1/t2/t3 结算：
/// let t4 = 1_050_000;      // 本端收到回复的时刻
/// let t2 = 1_020_000;      // 对端收到探测的时刻
/// let t3 = 1_021_000;      // 对端发出回复的时刻
/// let sample = sync.finish_probe(t4, probe.t1_us, t2, t3);
/// assert!(sample.is_some());
/// assert!(sync.is_synced());
/// ```
#[derive(Debug)]
pub struct ClockSync {
    samples: VecDeque<SyncSample>,
    window: usize,
    next_nonce: u32,
    /// 当前估计的偏移（微秒）；None 表示还没同步过
    current_offset_us: Option<i64>,
    /// 当前估计的 RTT（微秒）
    current_rtt_us: u64,
}

impl Default for ClockSync {
    fn default() -> Self {
        Self::new()
    }
}

impl ClockSync {
    pub fn new() -> Self {
        Self {
            samples: VecDeque::with_capacity(16),
            window: 16,
            next_nonce: 1,
            current_offset_us: None,
            current_rtt_us: 0,
        }
    }

    pub fn with_window(mut self, window: usize) -> Self {
        self.window = window.max(1);
        self
    }

    /// 开始一次探测，返回本端 T1 与配对的 nonce
    pub fn start_probe(&mut self, now_us: u64, _peer_hint: u32) -> ProbeState {
        let nonce = self.next_nonce;
        self.next_nonce = self.next_nonce.wrapping_add(1);
        ProbeState {
            nonce,
            t1_us: now_us,
        }
    }

    /// 收到 RTT_REPLY 后结算
    ///
    /// - `t4_us`：本端收到回复的时刻
    /// - `t2_us` / `t3_us`：对端记录的收到时刻 / 发出时刻
    ///
    /// 返回 None 表示时间戳不自洽（RTT 为负或异常大），样本被丢弃。
    pub fn finish_probe(
        &mut self,
        t4_us: u64,
        t1_us: u64,
        t2_us: u64,
        t3_us: u64,
    ) -> Option<SyncSample> {
        // 对端处理时间（同一台机器上的两个时刻，相减永远安全）
        let peer_processing = t3_us.checked_sub(t2_us)?;

        // # 四个时间戳的时基归属（搞混过，写清楚）
        //
        // | 字段 | 谁打的 | 时基 |
        // | --- | --- | --- |
        // | `t1` | 本端发出探测时 | **本端** |
        // | `t2` | 对端收到探测时 | **对端** |
        // | `t3` | 对端发出回复时 | **对端** |
        // | `t4` | 本端收到回复时 | **本端** |
        //
        // 于是：
        //
        // - `t3 - t2`：同源，就是对端处理时间
        // - `t4 - t1`：同源，就是本端看到的往返总时长
        // - `t2 - t1` / `t3 - t4`：**跨时基**，各带一个时钟差
        //
        // NTP 的 offset 公式正是利用「跨时基的两项各带同一个差、相加后
        // 差被平分出来」：`offset = ((t2-t1) + (t3-t4)) / 2`。
        //
        // # 一个实测踩到的坑（第五十六轮）
        //
        // 我一度以为 `t1` 是对端时基，给 `--clock-bias-us` 也加了偏移，
        // 结果 `t4 - t1` 变成负数 → `checked_sub` 返回 None → **整次探测
        // 被静默丢弃**，表现为 `synced=false` 一次都同步不上。
        //
        // 教训：**测试工具本身不能破坏被测语义**。要模拟时基差异，
        // 应该只偏移载荷里的 PTS（视频/音频时间戳），不能偏移探测用的
        // t1/t4 —— 那是本端自己的计时。

        let a = t2_us as i128 - t1_us as i128;
        let b = t3_us as i128 - t4_us as i128;
        let offset_us = ((a + b) / 2) as i64;

        // # RTT 用 i128 相减，不用 checked_sub
        //
        // `checked_sub` 在 `t4 < t1` 时静默返回 None —— 而那个分支
        // 在正常情况永远不会走到（t4 一定晚于 t1）。真走到时说明本端
        // 时钟被 NTP 往回调了，那是**值得知道的事**，不该无声吞掉。
        //
        // 用 i128 相减，负 RTT 显式判为不自洽（下面统一处理）。
        let total = t4_us as i128 - t1_us as i128;
        let rtt_i128 = total - peer_processing as i128;

        // RTT 为负说明时间戳不自洽（时钟跳变 / 恶意包 / 驱动延迟异常）
        if !(0..=2_000_000).contains(&rtt_i128) {
            return None;
        }
        let rtt_us = rtt_i128 as u64;

        let sample = SyncSample { rtt_us, offset_us };
        self.add_sample(sample);
        Some(sample)
    }

    /// 直接加入样本（用于测试与批量导入）
    pub fn add_sample(&mut self, sample: SyncSample) {
        if self.samples.len() == self.window {
            self.samples.pop_front();
        }
        self.samples.push_back(sample);
        self.recompute();
    }

    /// 选出 RTT 最小的样本作为当前估计
    ///
    /// 理由见模块文档：RTT 越小，路径越"直"，不对称误差越小。
    fn recompute(&mut self) {
        let Some(best) = self.samples.iter().min_by_key(|s| s.rtt_us) else {
            return;
        };
        self.current_offset_us = Some(best.offset_us);
        self.current_rtt_us = best.rtt_us;
    }

    /// 当前时钟偏移（微秒）
    pub fn offset_us(&self) -> Option<i64> {
        self.current_offset_us
    }

    /// 当前 RTT 估计（微秒）
    pub fn rtt_us(&self) -> u64 {
        self.current_rtt_us
    }

    /// 是否已同步过
    pub fn is_synced(&self) -> bool {
        self.current_offset_us.is_some()
    }

    /// 把对端时间戳换算到本端时基
    ///
    /// 未同步时原样返回（此时延迟数字不可信，但至少不会 panic）。
    pub fn to_local(&self, peer_us: u64) -> u64 {
        match self.current_offset_us {
            Some(off) => (peer_us as i128 + off as i128).clamp(0, u64::MAX as i128) as u64,
            None => peer_us,
        }
    }

    /// 用本端时钟计算对端某时刻的真实单程延迟
    ///
    /// `peer_send_us` 是发送端打的 send_ts；`local_now_us` 是本端当前时刻。
    /// 未同步时返回 None —— 宁可说"不知道"也不要给错数字。
    pub fn one_way_latency_us(&self, peer_send_us: u64, local_now_us: u64) -> Option<u64> {
        let off = self.current_offset_us?;
        let peer_in_local = (peer_send_us as i128 + off as i128).max(0) as u64;
        local_now_us.checked_sub(peer_in_local)
    }

    pub fn sample_count(&self) -> usize {
        self.samples.len()
    }

    /// 手动校准掉的系统性偏差（微秒）
    ///
    /// 用于扣除"对端用户态收到包到打时间戳"的固定延迟。
    /// 正数表示把估计的 offset 往正方向调（补偿负偏）。
    pub fn apply_bias_correction(&mut self, bias_us: i64) {
        if let Some(off) = self.current_offset_us {
            self.current_offset_us = Some(off.saturating_add(bias_us));
        }
    }

    pub fn clear(&mut self) {
        self.samples.clear();
        self.current_offset_us = None;
        self.current_rtt_us = 0;
    }
}
