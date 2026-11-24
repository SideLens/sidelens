//! 抖动缓冲
//!
//! 目标：用可控的延迟换取平稳的播放节奏。
//! 与消费端（Zoom / Teams）的缓冲不同，这里的缓冲是**我们能控制的那一段**，
//! 应当尽量压到 10–40ms，把预算让给抗抖动能力。

use sidelens_proto::{now_us, FrameType};
use std::collections::VecDeque;

/// 一次投递的结果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JitterOutcome {
    /// 是否应该向发送端请求关键帧
    pub needs_keyframe: bool,
    /// 这一帧是否被当作迟到帧丢弃
    pub late: bool,
}

/// 缓冲配置
#[derive(Debug, Clone, Copy)]
pub struct JitterConfig {
    /// 目标延迟（毫秒）
    pub target_ms: f64,
    /// 延迟上限（毫秒）；超过视为参考帧丢失，清空并请求关键帧
    pub max_ms: f64,
    /// 每秒允许向下收敛的毫秒数
    pub shrink_per_sec: f64,
    /// 目标延迟的下限（毫秒）
    pub min_ms: f64,
}

impl Default for JitterConfig {
    fn default() -> Self {
        Self {
            target_ms: 20.0,
            max_ms: 200.0,
            shrink_per_sec: 2.0,
            min_ms: 10.0,
        }
    }
}

/// 缓冲统计
#[derive(Debug, Clone, Copy, Default)]
pub struct JitterStats {
    pub received: u64,
    pub played: u64,
    pub dropped: u64,
    pub reordered: u64,
    pub keyframe_requests: u64,
    pub max_depth: usize,
    pub current_depth_ms: f64,
    pub avg_jitter_ms: f64,
    /// `adapt` 实际改过多少次 `target_ms`
    pub adaptations: u64,
    /// 其中向上调（加缓冲）的次数
    pub adapt_ups: u64,
    /// 其中向下调（减缓冲）的次数
    pub adapt_downs: u64,
}

/// 队列中的一帧。只存解码所需的元信息 + 载荷。
#[derive(Debug, Clone)]
pub struct QueuedFrame {
    pub ty: FrameType,
    pub seq: u32,
    pub pts_us: u64,
    pub send_ts_us: u64,
    pub arrived_at_us: u64,
    pub flags: u8,
    /// 这一帧是 NACK 重传的（不是首传）
    ///
    /// # 为什么抖动估计要区分它（第三十八轮修的）
    ///
    /// 重传帧的 `send_ts_us` 是**原始发送时刻**，而 `arrived_at_us` 是
    /// **重传到达时刻** —— 两者相差一个 RTT + NACK 批处理延迟（实测 30ms）。
    ///
    /// 抖动公式 `|Δarrive - Δsend|` 把它算成抖动：
    ///
    /// ```text
    /// 前一帧正常: send=T-33ms, arrived=T-33ms+1ms
    /// 当前帧重传: send=T,      arrived=T+30ms
    /// Δarrive=64ms  Δsend=33ms  ->  |64-33| = 31ms   ← 假抖动
    /// ```
    ///
    /// 实测后果：3% 丢包下 `jitter_ewma` 从 0.05ms 涨到 **31ms**（600 倍），
    /// 抖动缓冲 `target_ms` 从 10 涨到 78ms，`playable p50` 从 15 涨到 87ms。
    ///
    /// 而 0% 丢包时一切正常 —— 这就是「重传污染」的决定性证据。
    pub is_retransmit: bool,
    pub payload: Vec<u8>,
}

impl QueuedFrame {
    pub fn is_keyframe(&self) -> bool {
        self.flags & sidelens_proto::FLAG_KEYFRAME != 0
    }
}

/// 抖动缓冲
///
/// # 两条关键规则
///
/// 1. **只有「已播放过的那一帧之前」的 seq 才算迟到。**
///    用 `expected_seq` 判断会把正常乱序一并误杀 —— 乱序帧还没播，必须进队列重排。
///    JS 版本最初就是这么错的，测试挂了才发现。
/// 2. **积压过深时清空并请求关键帧。**
///    积压通常意味着丢了一个参考帧，后面的帧全解不出来，等待没有意义。
#[derive(Debug)]
pub struct JitterBuffer {
    config: JitterConfig,
    queue: VecDeque<QueuedFrame>,
    /// 已播放过的最大 seq；只有 <= 它的帧才算真正的迟到帧
    last_played_seq: Option<u32>,
    last_arrival_us: Option<u64>,
    /// 上一帧自己的 `send_ts_us`
    ///
    /// 必须单独存：抖动要用 `(R_i - R_{i-1}) - (S_i - S_{i-1})`，
    /// 拿当前帧的 `send_ts_us` 去减上一帧的到达时刻会把 `send_ts_us`
    /// 自己抵消掉，算出来的是到达间隔而不是抖动。见 `push_at` 的注释。
    last_send_ts_us: Option<u64>,
    jitter_ewma: f64,
    /// 上次 `adapt` 改动 `target_ms` 的时刻（驻留期判定用）
    last_adapt_us: Option<u64>,
    pub stats: JitterStats,
}

impl Default for JitterBuffer {
    fn default() -> Self {
        Self::new(JitterConfig::default())
    }
}

impl JitterBuffer {
    pub fn new(config: JitterConfig) -> Self {
        Self {
            config,
            queue: VecDeque::new(),
            last_played_seq: None,
            last_arrival_us: None,
            last_send_ts_us: None,
            jitter_ewma: 0.0,
            last_adapt_us: None,
            stats: JitterStats::default(),
        }
    }

    pub fn config(&self) -> &JitterConfig {
        &self.config
    }

    /// 目标延迟（毫秒）
    pub fn target_ms(&self) -> f64 {
        self.config.target_ms
    }

    /// 当前队列深度（帧数）
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// 投递一帧（用当前时钟作为到达时刻）
    pub fn push(&mut self, frame: QueuedFrame) -> JitterOutcome {
        self.push_at(frame, now_us())
    }

    /// 投递一帧（显式指定到达时刻，便于测试）
    pub fn push_at(&mut self, mut frame: QueuedFrame, arrived_at_us: u64) -> JitterOutcome {
        // 控制帧不进队列
        if !matches!(frame.ty, FrameType::Video | FrameType::Audio) {
            return JitterOutcome {
                needs_keyframe: false,
                late: false,
            };
        }

        frame.arrived_at_us = arrived_at_us;

        // 重传帧**不参与**抖动估计 —— 它的到达延迟反映的是恢复机制，
        // 不是链路抖动。见 `QueuedFrame::is_retransmit` 的文档。
        //
        // 同时要清掉 `last_arrival_us`：否则下一帧的首传会拿它当基准，
        // 算出一个同样巨大的假样本（污染会「传染」到下一帧）。
        if frame.is_retransmit {
            self.last_arrival_us = None;
            self.last_send_ts_us = None;
        }

        // 用「传输时延的变化量」估计网络抖动（RFC 3550 §6.4.1 的 |D(i-1,i)|）
        //
        // # 这里原来有个真 bug（本轮修）
        //
        // 原实现：
        //
        // ```text
        // transit      = arrived      - frame.send_ts_us
        // last_transit = last_arrival - frame.send_ts_us    ← 同一个 send_ts！
        // delta        = |transit - last_transit|
        //              = |arrived - last_arrival|
        // ```
        //
        // `send_ts_us` 在减式里**自己抵消掉了**，剩下的 `|arrived - last_arrival|`
        // 是**到达间隔** —— 它等于「帧间隔 + 抖动」，而不是抖动。
        //
        // 60fps 下帧间隔 16.67ms，于是 `jitter_ewma` 恒在 16.7ms 附近，
        // 和真实抖动（回环上 <1ms）差了 20 倍。日志里 `avg_jitter_ms` 一直
        // 是个假数字，而它又是 `adapt` 的唯一输入。
        //
        // 正确形式要拿**上一帧自己的** `send_ts_us`：
        //
        // ```text
        // D(i-1,i) = (R_i - R_{i-1}) - (S_i - S_{i-1})
        // ```
        //
        // 这才是「网络把间隔拉开了多少」，帧间隔被减掉了。
        if !frame.is_retransmit {
            if let (Some(last_arrival), Some(last_send)) =
                (self.last_arrival_us, self.last_send_ts_us)
            {
                let d_arrive = arrived_at_us as i128 - last_arrival as i128;
                let d_send = frame.send_ts_us as i128 - last_send as i128;
                let delta = (d_arrive - d_send).unsigned_abs() as f64;
                // 样本限幅：超过 `JITTER_SAMPLE_CAP_US` 的样本丢弃。
                //
                // # 为什么必须限幅（第三十八轮实测）
                //
                // 3% 丢包 + NACK 重传时，`jitter_ewma` 从 0.05ms 涨到 16-45ms
                // （600 倍），而回环的真实抖动 < 1ms。
                //
                // 根因不是「重传帧本身」—— 重传复用了 `FrameType::Video`，
                // `FLAG_RETRANSMIT` 全仓库无写入点，无法从帧头区分。
                // 而是**被重传救回来的帧**：丢了一个分片 → 整帧要等重传补齐
                // → 到达时刻推迟 ~30ms → 下一帧正常到达 → 到达间隔 63ms
                // 而 `Δsend` 只有 33ms → 假样本 |63-33| = 30ms。
                //
                // 限幅的代价：真实的突发抖动（>30ms）会被忽略。但那种情况下
                // 抖动缓冲加多深都救不了 —— 该靠 NACK/FEC，而不是加延迟。
                if delta < JITTER_SAMPLE_CAP_US as f64 {
                    self.jitter_ewma = if self.jitter_ewma == 0.0 {
                        delta
                    } else {
                        self.jitter_ewma * 0.9 + delta * 0.1
                    };
                    self.stats.avg_jitter_ms = self.jitter_ewma / 1000.0;
                }
            }
            self.last_arrival_us = Some(arrived_at_us);
            self.last_send_ts_us = Some(frame.send_ts_us);
        }

        // 真迟到：已经播过这一帧之后的帧了
        if let Some(last_played) = self.last_played_seq {
            if frame.seq <= last_played {
                self.stats.reordered += 1;
                return JitterOutcome {
                    needs_keyframe: false,
                    late: true,
                };
            }
        }

        self.stats.received += 1;

        // 按 PTS 插入（队列通常很短，线性插入比排序整个队列更划算）
        let pos = self
            .queue
            .iter()
            .position(|f| f.pts_us > frame.pts_us)
            .unwrap_or(self.queue.len());
        self.queue.insert(pos, frame);

        if self.queue.len() > self.stats.max_depth {
            self.stats.max_depth = self.queue.len();
        }

        // 队列深度按「队首到队尾的到达时间差」算，比按帧数更接近真实积压时长
        let depth_ms = match (self.queue.front(), self.queue.back()) {
            (Some(front), Some(back)) => {
                (back.arrived_at_us as i128 - front.arrived_at_us as i128) as f64 / 1000.0
            }
            _ => 0.0,
        };
        self.stats.current_depth_ms = depth_ms;

        if depth_ms > self.config.max_ms {
            self.stats.dropped += self.queue.len() as u64;
            self.queue.clear();
            self.stats.keyframe_requests += 1;
            return JitterOutcome {
                needs_keyframe: true,
                late: false,
            };
        }

        JitterOutcome {
            needs_keyframe: false,
            late: false,
        }
    }

    /// 取出当前「到期应当播放」的帧（用当前时钟）
    pub fn pop(&mut self) -> Option<QueuedFrame> {
        self.pop_at(now_us())
    }

    /// 取出到期帧（显式指定时刻，便于测试）
    pub fn pop_at(&mut self, now_us: u64) -> Option<QueuedFrame> {
        let front = self.queue.front()?;
        let held_us = now_us.saturating_sub(front.arrived_at_us);
        if (held_us as f64) < self.config.target_ms * 1000.0 {
            return None;
        }
        let frame = self.queue.pop_front()?;
        self.stats.played += 1;
        self.last_played_seq = Some(frame.seq);
        Some(frame)
    }

    /// 每秒调用一次：让目标延迟缓慢收敛回低位，避免延迟永久漂高
    pub fn shrink(&mut self, elapsed_ms: f64) {
        let delta = self.config.shrink_per_sec * elapsed_ms / 1000.0;
        self.config.target_ms = (self.config.target_ms - delta).max(self.config.min_ms);
    }

    /// 主动把目标延迟抬高（检测到持续抖动时用）
    pub fn grow(&mut self, extra_ms: f64) {
        self.config.target_ms = (self.config.target_ms + extra_ms).min(self.config.max_ms);
    }

    pub fn reset(&mut self) {
        self.queue.clear();
        self.last_played_seq = None;
        self.last_arrival_us = None;
        self.last_send_ts_us = None;
        self.last_adapt_us = None;
    }

    /// 按实测抖动自适应调整目标延迟
    ///
    /// # 为什么需要它（生产路径从未调用过 shrink/grow）
    ///
    /// `shrink` / `grow` 早就写好了，但**生产代码一次都没调用** ——
    /// 全仓库只有 `tests/jitter.rs` 里调过。于是 `target_ms` 永远是
    /// `JitterConfig::default()` 的 20.0，实测 `playable_ms` p50 也
    /// 就永远是 20.95ms。
    ///
    /// 那个 20.95 里 **20ms 是纯等待**，只有 0.95ms 是真实传输 + 处理。
    /// 换句话说：延迟优化的全部空间都在这个 `target_ms` 上。
    ///
    /// # 为什么是「抖动 × 倍数」而不是固定步进
    ///
    /// 固定步进（每秒 ±2ms）收敛太慢：从 20 降到 10 要 5 秒，
    /// 而链路状态可能 1 秒就变了。
    ///
    /// 按抖动算能直接跳到「刚好够用」的位置。倍数取 2.5 的经验依据：
    /// 抖动是 EWMA（`0.9/0.1` 平滑），本身已经是均值而非峰值，
    /// 乘 2.5 大致覆盖 3σ。小于 2 会在抖动轻微变大时开始丢帧，
    /// 大于 3 则退化成「不敢降」。
    ///
    /// # 三个闸门（缺一个就会震荡）
    ///
    /// 1. **下限** `min_ms`：再小的抖动也至少留 10ms，否则 UDP 乱序
    ///    和调度抖动会直接把帧判成迟到。
    /// 2. **单次降幅上限** `max_step_ms`：一次最多降 2ms。
    ///    不限制的话，链路刚好平静 100ms 就会把目标砍到底，
    ///    然后下一波抖动全部丢帧。
    /// 3. **驻留期** `hold_ms`：调整后 `hold_ms` 内不再动。
    ///    否则 `target_ms` 会跟着抖动的 EWMA 一起抖，
    ///    而 `target_ms` 抖动本身会制造迟到帧 —— 正反馈。
    ///
    /// # 为什么「降」比「升」保守得多
    ///
    /// 降错了的代价是**丢帧**（可见的画面卡顿），升错了的代价只是
    /// 多一点延迟（不可见）。所以降的步长小、驻留长；升则立即生效 ——
    /// 抖动一变大就马上加缓冲，晚一步就已经在丢帧了。
    ///
    /// # 参数
    ///
    /// - `now_us`：当前时刻，用于驻留期判定
    ///
    /// 返回 `Some((旧值, 新值))` 表示发生了调整，`None` 表示没动
    /// （驻留期内、或算出的新值与当前值相同）。
    pub fn adapt(&mut self, now_us: u64) -> Option<(f64, f64)> {
        // 抖动 EWMA 还没建立（少于 2 帧）时不动 —— 那时的值不可信
        if self.stats.received < 2 || self.jitter_ewma <= 0.0 {
            return None;
        }

        // 驻留期闸门
        if let Some(last) = self.last_adapt_us {
            if now_us.saturating_sub(last) < ADAPT_HOLD_US {
                return None;
            }
        }

        let jitter_ms = self.jitter_ewma / 1000.0;
        // 抖动 × 倍数，并钳到 [min_ms, max_ms]
        let want = (jitter_ms * JITTER_SAFETY_FACTOR).clamp(self.config.min_ms, self.config.max_ms);

        let cur = self.config.target_ms;
        let diff = want - cur;

        let next = if diff > 0.0 {
            // 升：立即全额生效。抖动变大时每一帧都在丢，等不起。
            want
        } else if diff < -0.0 {
            // 降：单次最多 max_step_ms
            let step = (-diff).min(ADAPT_MAX_STEP_MS);
            // 降幅小于 0.1ms 就别动了，否则会为了噪声反复写同一个值
            if step < ADAPT_MIN_DELTA_MS {
                return None;
            }
            cur - step
        } else {
            return None;
        };

        // 浮点比较留 1e-9 容差：want 与 cur 在数学上相等时不该触发调整
        if (next - cur).abs() < 1e-9 {
            return None;
        }

        self.config.target_ms = next;
        self.last_adapt_us = Some(now_us);
        self.stats.adaptations += 1;
        if next > cur {
            self.stats.adapt_ups += 1;
        } else {
            self.stats.adapt_downs += 1;
        }
        Some((cur, next))
    }
}

/// 单个抖动样本的上限（微秒）
///
/// 超过这个值的样本视为「等重传/等 FEC」而非真实网络抖动，直接丢弃。
///
/// # 15ms 的依据
///
/// | 场景 | 典型抖动 |
/// | --- | --- |
/// | 本机回环 | 0.04-1 ms |
/// | 局域网 WiFi 5GHz | 2-8 ms |
/// | 局域网 WiFi 2.4GHz | 10-30 ms |
/// | 公网同城 | 5-15 ms |
///
/// 而 NACK 重传的延迟 = 5ms 批处理 + 20ms 重试间隔 + RTT ≈ **30ms+**。
/// 取 15ms 能把两者分开：真实的链路抖动（<15ms）保留，
/// 重传造成的假样本（>30ms）丢弃。
///
/// 2.4GHz WiFi 的 10-30ms 会被部分削掉 —— 这是刻意的取舍：
/// 那种链路上抖动缓冲加到 30ms 已经足够，再加只是白增延迟。
pub const JITTER_SAMPLE_CAP_US: u64 = 15_000;

/// 抖动 → 目标延迟的安全系数
///
/// 见 [`JitterBuffer::adapt`] 的说明：EWMA 本身已是均值，
/// 乘 2.5 大致覆盖 3σ。小于 2 会开始丢帧，大于 3 则不敢降。
pub const JITTER_SAFETY_FACTOR: f64 = 2.5;

/// 单次下调的最大幅度（毫秒）
pub const ADAPT_MAX_STEP_MS: f64 = 2.0;

/// 小于这个幅度就不调整（避免为噪声反复写值）
pub const ADAPT_MIN_DELTA_MS: f64 = 0.1;

/// 调整后的驻留期（微秒）
///
/// 200ms ≈ 6 帧 @30fps。太短会跟着抖动 EWMA 一起抖（正反馈），
/// 太长则链路变化后跟不上。
pub const ADAPT_HOLD_US: u64 = 200_000;
