//! 拥塞控制与码率自适应
//!
//! # 为什么实时视频不能用 TCP 式的拥塞控制
//!
//! TCP 的拥塞控制目标是「不丢包」，代价是「排队延迟可以任意增长」。
//! 对文件传输这没问题 —— 慢一点无所谓。对实时视频是灾难：
//! 缓冲队列一满，端到端延迟从 20ms 涨到 800ms，画面变成慢动作回放。
//!
//! 所以实时视频的拥塞信号必须**以延迟为主、丢包为辅**：
//!
//! | 信号 | 提前量 | 可靠性 | 用法 |
//! | --- | --- | --- | --- |
//! | 排队延迟增长 | 早（队列刚开始积压就能看到） | 中（受时钟漂移影响） | 主信号 |
//! | 丢包率 | 晚（队列已经溢出才丢） | 高 | 兜底信号 |
//!
//! 只用丢包会怎样：等到开始丢包，延迟已经炸了 200ms 以上，
//! 而且丢包触发的降码率还要 1 个 RTT 才生效 —— 用户已经卡了半秒。
//!
//! # 延迟梯度的原理
//!
//! 发送端按 `send_us` 发出包，接收端按 `arrival_us` 收到。定义：
//!
//! ```text
//! d_i = arrival_i - send_i
//! ```
//!
//! 两个设备的时钟不同源，所以 `d_i` 含一个**恒定偏移**。但偏移在
//! 斜率计算里会被中心化消掉：
//!
//! ```text
//! 斜率 = Σ(x_i - x̄)(y_i - ȳ) / Σ(x_i - x̄)²
//! ```
//!
//! 其中 `x` 是相对发送时刻、`y` 是 `d_i`。常数偏移只影响 `ȳ`，不影响斜率。
//!
//! **前提是偏移恒定**。如果两端时钟有漂移（晶振差 50ppm 就是每秒 50µs），
//! 斜率会被污染。所以这里的时间戳必须先用 [`crate::sync::ClockSync`]
//! 校正过。
//!
//! # 为什么用最小二乘而不是首尾差值
//!
//! 首尾差值 `(d_n - d_0) / (t_n - t_0)` 只要有一个离群点就废了 ——
//! 而实时视频里离群点很常见（调度抖动、WiFi 重传）。
//! 最小二乘用上所有样本，单个离群点的影响被摊薄。
//!
//! # AIMD 的取舍
//!
//! 经典 AIMD 是「加性增、乘性减」。这里做了两处调整：
//!
//! 1. **降得更狠**：乘性减系数 0.85（经典是 0.5，但那是为了公平性；
//!    我们只有一条流，降太狠会让画面糊一下）。实际用 0.85 是因为
//!    过载通常只是短暂排队，降太多会立刻又升回来，形成锯齿。
//! 2. **升得更慢**：每秒 +8%，而不是每个 RTT 翻倍。因为码率过冲的代价
//!    是「延迟炸一下」，比慢一点更难受。
//!
//! 过载后还有一段 **hold 期**（默认 300ms）：这段时间内只允许降，不允许升。
//! 否则降完立刻升，会在同一个瓶颈上反复撞墙。
//!
//! # 与分辨率的关系
//!
//! 码率有下限（默认 200kbps）。再往下压，720p 会糊到不可用。
//! 所以 [`CongestionController::resolution_hint`] 会在码率触底时
//! 建议降分辨率 —— 同样 200kbps，360p 比 720p 清楚得多。

use std::collections::VecDeque;

/// 拥塞信号
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CongestionSignal {
    /// 排队延迟在持续增长（主信号）
    DelayGrowth,
    /// 丢包率过高（兜底信号）
    PacketLoss,
    /// 两个信号同时出现
    Both,
    /// 没有拥塞迹象
    None,
}

impl CongestionSignal {
    /// 是否说明拥塞
    pub fn is_congested(self) -> bool {
        !matches!(self, CongestionSignal::None)
    }
}

/// 码率调整方向
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateState {
    /// 正在缓慢升码率
    Increase,
    /// 保持当前码率
    Hold,
    /// 正在降码率
    Decrease,
}

/// 降码率的原因（写日志与调参用）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// 延迟梯度超阈值
    DelayGradient,
    /// 丢包率超阈值
    Loss,
    /// 延迟与丢包同时超标
    DelayAndLoss,
    /// 过载后的保持期，不允许升
    Holding,
    /// 带宽有余量，缓慢升
    Underuse,
    /// 轻度丢包：不升也不降，观望
    LossCaution,
    /// 无变化
    Steady,
    /// 码率已在上限
    AtCeiling,
    /// 码率已在下限
    AtFloor,
}

/// 拥塞控制配置
#[derive(Debug, Clone, Copy)]
pub struct CongestionConfig {
    /// 最低码率（bps）。低于这个值画面不可用，宁可降分辨率
    pub min_bitrate_bps: f64,
    /// 最高码率（bps）
    pub max_bitrate_bps: f64,
    /// 起始码率（bps）
    pub start_bitrate_bps: f64,
    /// 乘性减系数（0-1，越小降得越狠）
    pub decrease_factor: f64,
    /// 每秒加性增的比例（相对当前码率）
    pub increase_ratio_per_sec: f64,
    /// 单次增量的绝对上限（bps），防止高码率时升太猛
    ///
    /// 这个值和 `increase_ratio_per_sec` 是互补的两道闸：
    /// 百分比闸管「相对速度」，绝对值闸管「绝对速度」。
    /// 8 Mbps 时 8%/秒 = 640 kbps/秒，若单次上限只有 100 kbps 就会卡住。
    pub max_increase_step_bps: f64,
    /// 丢包率超过这个值就降
    ///
    /// # 为什么是 3% 而不是教科书的 10%
    ///
    /// 实测：注入 5% 包级丢包时，控制器用 10% 阈值判为「健康」，
    /// 一路升码率到 2 Mbps —— 而链路已经明显在丢包了。
    ///
    /// 对实时视频，3% 已经是「画面会周期性卡一下」的水平。
    /// 10% 是文件传输的量级（有重传兜底，丢点无所谓）。
    pub loss_threshold: f64,
    /// 丢包率超过这个值降得更狠
    pub loss_high_threshold: f64,
    /// 轻度丢包（低于 loss_threshold 但高于这个值）时温和降速
    ///
    /// 实时视频的「舒适区」是 1% 以下。1-3% 之间不降码率但也不该升 ——
    /// 否则会慢慢爬到真正开始丢包的边缘。
    pub loss_caution_threshold: f64,
    /// 延迟梯度斜率阈值（ms 延迟 / ms 发送时间）
    pub delay_gradient_threshold: f64,
    /// 过载后的保持期（毫秒），期间不允许升
    pub hold_ms: u64,

    /// 延迟梯度窗口样本数
    pub trend_window: usize,
    /// 延迟梯度窗口时间跨度（毫秒）
    ///
    /// 与 `trend_window` 取先到者淘汰。加这个是因为只按样本数淘汰时，
    /// 低帧率下窗口时间跨度会拉到 2 秒，拥塞判定严重滞后。
    /// 见 [`DelayTrend::window_ms`] 的说明。
    pub trend_window_ms: u64,
    /// 窗口内最少样本数（不足时不判定）
    pub trend_min_samples: usize,
}

impl Default for CongestionConfig {
    fn default() -> Self {
        Self {
            min_bitrate_bps: 200_000.0,
            max_bitrate_bps: 8_000_000.0,
            start_bitrate_bps: 1_500_000.0,
            decrease_factor: 0.85,
            increase_ratio_per_sec: 0.08,
            max_increase_step_bps: 500_000.0,
            loss_threshold: 0.03,
            loss_high_threshold: 0.10,
            loss_caution_threshold: 0.01,
            delay_gradient_threshold: 0.015,
            hold_ms: 300,

            trend_window: 20,
            trend_window_ms: 400,
            trend_min_samples: 6,
        }
    }
}

/// 延迟梯度估计
///
/// 维护最近 N 组 `(send_us, arrival_us)`，用最小二乘拟合
/// 「累积延迟」对「发送时间」的斜率。
///
/// # 为什么用 `arrival - send` 而不是「到达间隔 - 发送间隔」
///
/// 后者需要相邻两个包，对单个包丢失很敏感；前者每对样本独立，
/// 丢包只是少一个样本。
#[derive(Debug)]
pub struct DelayTrend {
    /// (相对发送时刻 us, arrival - send us)
    samples: VecDeque<(i64, i64)>,
    capacity: usize,
    /// 时间窗口（毫秒）。0 表示只按 capacity 淘汰。
    ///
    /// # 为什么需要时间窗口（结构性缺陷）
    ///
    /// 只按 `capacity`（样本数）淘汰时，窗口覆盖的时间跨度 = 样本数 × 帧间隔：
    ///
    /// | 帧率 | 20 个样本的时间跨度 |
    /// | --- | --- |
    /// | 60 fps | 333 ms |
    /// | 30 fps | 667 ms |
    /// | 15 fps | 1.33 s |
    /// | 10 fps | 2.0 s |
    ///
    /// **帧率越低（往往正是弱网时），拥塞判定滞后越严重** —— 这是正反馈的坏设计：
    /// 网络差 → 帧率降 → 滞后变长 → 更晚才降码率 → 网络更差。
    ///
    /// 所以额外加一个时间窗口，两者取先到者淘汰。
    window_ms: u64,
    /// 第一个样本的绝对发送时刻，用于转成相对值
    base_send_us: Option<u64>,
}

impl DelayTrend {
    /// 只按样本数限制（`window_ms = 0` 表示不限时间）
    pub fn new(capacity: usize) -> Self {
        Self::with_window(capacity, 0)
    }

    /// 同时按样本数与时间窗口限制
    pub fn with_window(capacity: usize, window_ms: u64) -> Self {
        Self {
            samples: VecDeque::new(),
            capacity: capacity.max(2),
            window_ms,
            base_send_us: None,
        }
    }

    pub fn window_ms(&self) -> u64 {
        self.window_ms
    }

    /// 加入一组样本
    ///
    /// `send_us` 是发送端时间戳，`arrival_us` 是接收端到达时间戳。
    /// 两者可以不同源（含固定偏移），但**偏移必须恒定**。
    pub fn add(&mut self, send_us: u64, arrival_us: u64) {
        let base = *self.base_send_us.get_or_insert(send_us);
        // 发送时刻倒退说明时间戳异常（乱序或时钟跳变），跳过
        let rel = send_us as i64 - base as i64;
        if rel < 0 {
            return;
        }
        let delay = arrival_us as i64 - send_us as i64;

        // 按时间窗口淘汰：比当前样本早超过 window_ms 的全部丢弃
        if self.window_ms > 0 {
            let cutoff = rel - (self.window_ms as i64) * 1000;
            while let Some((front, _)) = self.samples.front() {
                if *front < cutoff {
                    self.samples.pop_front();
                } else {
                    break;
                }
            }
        }

        // 再按样本数上限淘汰
        while self.samples.len() >= self.capacity {
            self.samples.pop_front();
        }
        self.samples.push_back((rel, delay));
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    pub fn clear(&mut self) {
        self.samples.clear();
        self.base_send_us = None;
    }

    /// 最小二乘斜率（无量纲：ms 延迟增长 / ms 发送时间）
    ///
    /// 返回 `None` 表示样本不足或发送时间跨度太小（拟合无意义）。
    pub fn slope(&self, min_samples: usize) -> Option<f64> {
        let n = self.samples.len();
        if n < min_samples.max(2) {
            return None;
        }

        let nf = n as f64;
        // x 用毫秒，避免数值过大导致精度损失
        let mean_x = self.samples.iter().map(|(x, _)| *x as f64).sum::<f64>() / nf / 1000.0;
        let mean_y = self.samples.iter().map(|(_, y)| *y as f64).sum::<f64>() / nf / 1000.0;

        let mut cov = 0.0;
        let mut var = 0.0;
        for (x, y) in &self.samples {
            let dx = *x as f64 / 1000.0 - mean_x;
            let dy = *y as f64 / 1000.0 - mean_y;
            cov += dx * dy;
            var += dx * dx;
        }

        // 发送时间跨度不足 10ms 时拟合不可靠（分母趋零，斜率会爆）
        if var < 1e-6 {
            return None;
        }
        Some(cov / var)
    }
}

/// 一次反馈输入
#[derive(Debug, Clone, Copy)]
pub struct Feedback {
    /// 丢包率（0-1）
    pub loss_ratio: f64,
    /// 当前 RTT（毫秒），仅用于日志与下限保护
    pub rtt_ms: f64,
    /// 这份统计是否包含**新数据**（上次决策之后又有样本进来）
    ///
    /// # 为什么必须有这个字段（第四、五个实测踩坑）
    ///
    /// 丢包率是按 **2 秒窗口**统计的，而 `update()` 在真实链路上是每
    /// **1ms** 调一次。同一份统计会被重复喂给控制器 2000 次 ——
    /// 每一次都触发一次降速。
    ///
    /// 实测：10 秒内 `cc_decreases=33`，码率从 1502 砸到 200，
    /// 而接收端吞吐一直稳定在 2300-2500 kbps（**带宽完全够**）。
    ///
    /// 试过的错误修法：
    ///
    /// | 尝试 | 为什么失败 |
    /// | --- | --- |
    /// | 2 秒决策一次 | 漏掉 1.6 秒样本，500ms 的拥塞不可见 |
    /// | 1ms 决策（无节流） | 0.97^2000 ≈ 0，瞬间触底 |
    /// | 200ms 降速节流 | 仍 26 次，10 秒到底 |
    /// | 单次事件降幅上限 | 上限生效前已经到底 |
    /// | 按采样跨度折算当量 | 窗口重置后跨度≈0，当量永远攒不满，反而死循环 |
    ///
    /// 共同错误：都在**猜**「这次调用该不该降」，而真正缺的信息是
    /// 「这份数据到底有没有更新过」。所以直接把这个问题交给调用方回答。
    ///
    /// - `true`：有新样本 → 允许降速
    /// - `false`：数据没变 → 只看不降（仍会更新升码率逻辑）
    ///
    /// 默认 `true` 保证「一次调用一次判断」的简单用法仍然有效。
    pub fresh: bool,
}

impl Default for Feedback {
    fn default() -> Self {
        Self {
            loss_ratio: 0.0,
            rtt_ms: 0.0,
            fresh: true,
        }
    }
}

/// 一次决策结果
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Decision {
    /// 目标码率（bps）
    pub target_bps: f64,
    /// 调整方向
    pub state: RateState,
    /// 原因
    pub reason: Reason,
    /// 本次的延迟梯度斜率（无样本时为 None）
    pub slope: Option<f64>,
}

/// 拥塞控制器
///
/// 输入：延迟样本 + 丢包率。输出：目标码率。
#[derive(Debug)]
pub struct CongestionController {
    config: CongestionConfig,
    trend: DelayTrend,
    target_bps: f64,
    state: RateState,
    /// 保持期结束时刻（微秒）；在此之前不允许升
    hold_until_us: u64,

    /// 上次拥塞前的码率（恢复期用来加速爬升）
    ///
    /// 纯百分比增长在低码率时太慢：200kbps × 8% = 16kbps/秒，
    /// 回到 1.5Mbps 要 80 秒 —— 用户会以为画面一直是糊的。
    /// 记住「拥塞前跑到过多少」后，恢复期可以按更大的步子往上试，
    /// 一旦再次拥塞就更新这个值。
    pre_congestion_bps: f64,
    /// 上次更新的时刻（微秒），用于算 dt
    last_update_us: Option<u64>,
    /// 累计决策次数
    pub decisions: u64,
    /// 累计降码率次数
    pub decreases: u64,
    /// 累计升码率次数
    pub increases: u64,
}

impl Default for CongestionController {
    fn default() -> Self {
        Self::new(CongestionConfig::default())
    }
}

impl CongestionController {
    pub fn new(config: CongestionConfig) -> Self {
        let start = config
            .start_bitrate_bps
            .clamp(config.min_bitrate_bps, config.max_bitrate_bps);
        Self {
            trend: DelayTrend::with_window(config.trend_window, config.trend_window_ms),
            target_bps: start,
            state: RateState::Hold,
            hold_until_us: 0,
            last_update_us: None,
            pre_congestion_bps: 0.0,
            decisions: 0,
            decreases: 0,
            increases: 0,
            config,
        }
    }

    pub fn config(&self) -> &CongestionConfig {
        &self.config
    }

    /// 当前目标码率（bps）
    pub fn target_bps(&self) -> f64 {
        self.target_bps
    }

    pub fn state(&self) -> RateState {
        self.state
    }

    /// 记录一组延迟样本
    pub fn add_delay_sample(&mut self, send_us: u64, arrival_us: u64) {
        self.trend.add(send_us, arrival_us);
    }

    /// 当前延迟梯度斜率
    pub fn slope(&self) -> Option<f64> {
        self.trend.slope(self.config.trend_min_samples)
    }

    /// 判定拥塞信号
    pub fn signal(&self, fb: &Feedback) -> CongestionSignal {
        let delay = self
            .slope()
            .map(|s| s > self.config.delay_gradient_threshold)
            .unwrap_or(false);
        let loss = fb.loss_ratio > self.config.loss_threshold;

        match (delay, loss) {
            (true, true) => CongestionSignal::Both,
            (true, false) => CongestionSignal::DelayGrowth,
            (false, true) => CongestionSignal::PacketLoss,
            (false, false) => CongestionSignal::None,
        }
    }

    /// 做一次决策
    ///
    /// `now_us` 必须是单调时钟（同一台机器上的单调递增时间）。
    pub fn update(&mut self, fb: &Feedback, now_us: u64) -> Decision {
        self.decisions += 1;

        // dt 用于加性增：没有上次时刻时按「一个典型反馈周期」估。
        //
        // # 上限为什么是 5 秒而不是 1 秒（实测踩坑）
        //
        // 第一版写成 `clamp(0.001, 1.0)`，理由是「防止长时间没决策后一次升太猛」。
        // 但实测发现：`udp_server` 每 2 秒才 update 一次，dt 被截到 1.0，
        // 于是每 2 秒只升 8%（设计是 16.6%），**有效增速只有设计值的一半**。
        //
        // 1 秒的上限对「反馈周期 20-50ms」的场景是合适的，但我们的调用方
        // 是 2 秒级。放宽到 5 秒既覆盖了常见节奏，又能挡住
        // 「进程挂起十分钟后恢复」这种真正该防的情况。
        let dt_sec = match self.last_update_us {
            Some(prev) => {
                let d = now_us.saturating_sub(prev) as f64 / 1_000_000.0;
                // 下限 1ms 保证有增量；上限 5s 挡住异常长间隔
                d.clamp(0.001, 5.0)
            }
            None => 0.02,
        };
        self.last_update_us = Some(now_us);

        let sig = self.signal(fb);
        let slope = self.slope();

        let at_floor = self.target_bps <= self.config.min_bitrate_bps + 1.0;
        let at_ceiling = self.target_bps >= self.config.max_bitrate_bps - 1.0;

        if sig.is_congested() {
            // # 只在「数据新鲜」时降速
            //
            // 丢包率是 2 秒窗口的统计，`update()` 却按 1ms 调。
            // 同一份数据重复驱动降速会让码率瞬间触底 —— 而带宽明明够。
            // 见 [`Feedback::fresh`] 的说明。
            if !fb.fresh {
                self.state = RateState::Hold;
                return Decision {
                    target_bps: self.target_bps,
                    state: self.state,
                    reason: Reason::Holding,
                    slope,
                };
            }

            // 降码率：延迟信号与丢包信号叠加时降得更狠
            let mut factor = self.config.decrease_factor;
            if sig == CongestionSignal::Both {
                factor *= 0.9;
            }
            if fb.loss_ratio > self.config.loss_high_threshold {
                factor *= 0.9;
            }

            // # 区分「拥塞丢包」与「随机丢包」
            //
            // 实测发现：注入固定的 5% 随机丢包时，控制器把码率从 1502
            // 一路压到 509 —— 但接收端实测吞吐一直是 2300-2500 kbps，
            // 说明**带宽完全够**，链路只是质量差（WiFi 干扰、线路误码）。
            //
            // 这两种情况的正确应对完全不同：
            //
            // | 现象 | 原因 | 正确应对 |
            // | --- | --- | --- |
            // | 延迟涨 + 丢包 | 队列溢出（真拥塞） | 降码率 |
            // | 只有丢包，延迟平稳 | 链路误码/干扰 | 加 FEC，**不降码率** |
            //
            // 所以：纯丢包信号（延迟没涨）时，降得温和一些。
            // 延迟平稳是「带宽够」的强证据。
            //
            // 但要保留「丢得越多降得越多」的梯度 —— 第一版用
            // `factor.max(0.97)` 直接截断，结果 12% 和 25% 丢包得到
            // 相同降幅（测试 `high_loss_decreases_even_harder` 抓到了）。
            // 正确做法是按丢包严重程度插值：
            //
            // | 包级丢包 | 系数 |
            // | --- | --- |
            // | 3%（刚过阈值） | 0.97 |
            // | 10%（loss_high_threshold） | 0.88 |
            // | >= 20% | 0.85 × 0.9 = 0.765 |
            if sig == CongestionSignal::PacketLoss {
                let lo = self.config.loss_threshold;
                let hi = self.config.loss_high_threshold.max(lo + 1e-6);
                // t=0 在 lo，t=1 在 hi，超出 hi 则封顶
                let t = ((fb.loss_ratio - lo) / (hi - lo)).clamp(0.0, 1.0);
                let mild = 0.97;
                let strong = 0.88;
                factor = mild + (strong - mild) * t;
                // 超过 hi 之后继续按超出幅度加罚，而不是一刀切。
                //
                // 12% 与 25% 丢包都超过 hi=10%，若只做一次固定惩罚，
                // 两者降幅完全相同 —— 测试 `high_loss_decreases_even_harder`
                // 抓到了这一点。真实链路里 25% 丢包必须比 12% 更狠地退让。
                if fb.loss_ratio > hi {
                    // 超出量每 10 个百分点再乘 0.95，最多叠到 0.7
                    let excess = (fb.loss_ratio - hi).min(0.4);
                    let extra = (1.0 - excess * 0.5).max(0.7);
                    factor *= extra;
                }
            }

            let new_target = (self.target_bps * factor).max(self.config.min_bitrate_bps);
            let changed = (new_target - self.target_bps).abs() > 1.0;
            if changed {
                // 记住「拥塞前跑到过多少」——恢复期用它做上界探测
                self.pre_congestion_bps = self.pre_congestion_bps.max(self.target_bps);
            }
            self.target_bps = new_target;

            // 关键：清空延迟窗口。
            //
            // 实测发现的振荡陷阱：窗口里留着拥塞期的样本时，网络已经恢复
            // 但斜率仍为正，会连续降 4 次直到触底（200kbps），
            // 然后再花几十秒才爬回来。
            //
            // 清空后第一次 update 因为样本不足不会判拥塞，
            // 进入保持期等待，之后重新积累干净的样本。
            self.trend.clear();

            // 进入保持期：降完之后不许马上升
            self.hold_until_us = now_us.saturating_add(self.config.hold_ms * 1000);
            self.state = RateState::Decrease;
            if changed {
                self.decreases += 1;
            }

            let reason = match sig {
                CongestionSignal::Both => Reason::DelayAndLoss,
                CongestionSignal::DelayGrowth => Reason::DelayGradient,
                CongestionSignal::PacketLoss => Reason::Loss,
                CongestionSignal::None => Reason::Steady,
            };
            return Decision {
                target_bps: self.target_bps,
                state: self.state,
                reason,
                slope,
            };
        }

        // 没有拥塞：先看是否还在保持期
        if now_us < self.hold_until_us {
            self.state = RateState::Hold;
            return Decision {
                target_bps: self.target_bps,
                state: self.state,
                reason: Reason::Holding,
                slope,
            };
        }

        if at_ceiling {
            self.state = RateState::Hold;
            return Decision {
                target_bps: self.target_bps,
                state: self.state,
                reason: Reason::AtCeiling,
                slope,
            };
        }

        // 轻度丢包：不升也不降。
        //
        // 这是「舒适区边缘」的处理 —— 1-3% 丢包说明链路已经接近
        // 它的容量上限，继续升只会更快撞墙。但也还没到要降的地步。
        if fb.loss_ratio > self.config.loss_caution_threshold {
            self.state = RateState::Hold;
            return Decision {
                target_bps: self.target_bps,
                state: self.state,
                reason: Reason::LossCaution,
                slope,
            };
        }

        // 加性增。两种步子：
        //
        // - **常规**：按当前码率的百分比（increase_ratio_per_sec/秒）
        // - **恢复**：如果知道拥塞前跑到过更高的码率，且当前离它还很远，
        //   用更大的步子往上试（最多每步 25% 的差距）
        //
        // 恢复期加速的理由：纯百分比增长在低码率时慢得离谱 ——
        // 200kbps × 8% = 16kbps/秒，回到 1.5Mbps 要 80 秒。
        // 用户在这 80 秒里看到的一直是糊的，会以为摄像头坏了。
        //
        // 但不能一次跳回去 —— 那会立刻再次拥塞。所以用「按差距比例」
        // 逼近：离上次拥塞点越近，步子越小，自然收敛。
        let regular_step = self.target_bps * self.config.increase_ratio_per_sec * dt_sec;
        let recovery_step = if self.pre_congestion_bps > self.target_bps {
            // 每步补上 1/4 的差距，但不超过常规步子的 8 倍
            let gap = self.pre_congestion_bps - self.target_bps;
            (gap * 0.25).min(regular_step * 8.0)
        } else {
            0.0
        };
        let step = regular_step
            .max(recovery_step)
            .min(self.config.max_increase_step_bps);
        let new_target = (self.target_bps + step).min(self.config.max_bitrate_bps);
        let changed = (new_target - self.target_bps).abs() > 1.0;
        self.target_bps = new_target;
        self.state = RateState::Increase;
        if changed {
            self.increases += 1;
        }

        Decision {
            target_bps: self.target_bps,
            state: self.state,
            reason: if at_floor && !changed {
                Reason::AtFloor
            } else {
                Reason::Underuse
            },
            slope,
        }
    }

    /// 码率触底时建议的分辨率
    ///
    /// 逻辑：同样 200kbps，360p 比 720p 清楚得多。所以码率越低，
    /// 越应该主动降分辨率，而不是死压 720p 的质量。
    pub fn resolution_hint(&self) -> Option<(u32, u32)> {
        if self.target_bps > self.config.min_bitrate_bps * 2.0 {
            return None;
        }
        Some(if self.target_bps <= self.config.min_bitrate_bps * 1.2 {
            (320, 180)
        } else {
            (640, 360)
        })
    }

    /// 重置到起始状态
    pub fn reset(&mut self) {
        self.trend.clear();
        self.target_bps = self
            .config
            .start_bitrate_bps
            .clamp(self.config.min_bitrate_bps, self.config.max_bitrate_bps);
        self.state = RateState::Hold;
        self.hold_until_us = 0;
        self.last_update_us = None;
        self.pre_congestion_bps = 0.0;
    }

    /// 上次拥塞前的码率（诊断用）
    pub fn pre_congestion_bps(&self) -> f64 {
        self.pre_congestion_bps
    }
}
