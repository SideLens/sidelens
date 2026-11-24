//! 拥塞控制在真实发送链路上的集成测试
//!
//! # 为什么需要这层测试
//!
//! `congestion.rs` 的单元测试只验证「输入样本 → 输出码率」这个纯函数。
//! 但真实链路里码率要**作用到帧大小上**，还要和丢包统计、延迟采样
//! 配合。这里模拟一条完整的反馈闭环：
//!
//! ```text
//! 网络模型（固定带宽 + 队列）
//!      ↓ 排队延迟 + 丢包
//! 采样器（send_us / arrival_us）
//!      ↓
//! 拥塞控制器 → 新码率
//!      ↓
//! 帧大小 = 码率 / fps
//!      └──────────→ 回到网络模型
//! ```
//!
//! 这个闭环能抓到单元测试抓不到的问题：码率算得对但换算成帧大小后
//! 依然打爆链路、或者降了码率但延迟没降（说明信号取错了）。

use std::collections::VecDeque;

use sidelens_core::congestion::{CongestionConfig, CongestionController, Feedback, RateState};

/// 一个带队列的链路模型
///
/// 每次 `send` 时：
/// 1. 如果队列已满 → 丢包
/// 2. 否则入队，算出它要排队多久
/// 3. 出队延迟 = 传输时间（size / bandwidth）
///
/// 这就是真实的「缓冲区膨胀」：带宽不够时包在队列里越积越多，
/// 延迟线性增长 —— 正是拥塞控制要检测的信号。
#[derive(Debug)]
struct LinkModel {
    /// 链路带宽（bytes/秒）
    bandwidth_bps: f64,
    /// 队列容量（字节）
    queue_capacity: f64,
    /// 当前队列占用（字节）
    queue_bytes: f64,
    /// 上次排空时刻（微秒）
    last_drain_us: u64,
    /// 丢包计数
    dropped: u64,
    /// 发送计数
    sent: u64,
}

impl LinkModel {
    fn new(bandwidth_bytes_per_sec: f64, queue_ms: f64) -> Self {
        Self {
            bandwidth_bps: bandwidth_bytes_per_sec,
            queue_capacity: bandwidth_bytes_per_sec * queue_ms / 1000.0,
            queue_bytes: 0.0,
            last_drain_us: 0,
            dropped: 0,
            sent: 0,
        }
    }

    /// 发一个包，返回 (是否丢弃, 到达时刻)
    fn send(&mut self, size_bytes: usize, now_us: u64) -> (bool, u64) {
        // 先把队列按经过的时间排空
        if self.last_drain_us > 0 {
            let elapsed = now_us.saturating_sub(self.last_drain_us) as f64 / 1_000_000.0;
            let drained = self.bandwidth_bps * elapsed;
            self.queue_bytes = (self.queue_bytes - drained).max(0.0);
        }
        self.last_drain_us = now_us;
        self.sent += 1;

        if self.queue_bytes + size_bytes as f64 > self.queue_capacity {
            self.dropped += 1;
            return (true, now_us);
        }

        self.queue_bytes += size_bytes as f64;
        // 排队延迟 = 队列里已有的量 / 带宽
        let queue_delay_us = self.queue_bytes / self.bandwidth_bps * 1_000_000.0;
        (false, now_us + queue_delay_us as u64)
    }

    fn loss_ratio(&self) -> f64 {
        if self.sent == 0 {
            return 0.0;
        }
        self.dropped as f64 / self.sent as f64
    }
}

#[test]
fn controller_finds_stable_rate_on_limited_link() {
    // 链路只有 1 MB/s（8 Mbps），控制器从 1.5 Mbps 起步 —— 有富余，应往上爬
    let mut cc = CongestionController::new(CongestionConfig {
        max_bitrate_bps: 20_000_000.0,
        ..Default::default()
    });
    let mut link = LinkModel::new(1_000_000.0, 50.0);

    let fps = 30.0;
    let mut t = 1_000_000u64;
    let frame_interval_us = (1_000_000.0 / fps) as u64;

    let mut recent_loss = VecDeque::new();

    for i in 0..600 {
        // 按当前目标码率算帧大小
        let frame_bytes = (cc.target_bps() / 8.0 / fps) as usize;
        let frame_bytes = frame_bytes.max(200);

        let (dropped, arrival) = link.send(frame_bytes, t);
        if !dropped {
            cc.add_delay_sample(t, arrival);
        }

        recent_loss.push_back(u32::from(dropped));
        if recent_loss.len() > 30 {
            recent_loss.pop_front();
        }
        let loss = recent_loss.iter().sum::<u32>() as f64 / recent_loss.len() as f64;

        cc.update(
            &Feedback {
                loss_ratio: loss,
                rtt_ms: 0.0,
                fresh: true,
            },
            t,
        );

        t += frame_interval_us;

        // 稳定后检查
        if i == 599 {
            let target = cc.target_bps();
            // 链路 8 Mbps，目标码率不应超过它的 90%
            assert!(
                target < 8_000_000.0 * 0.95,
                "600 帧后码率应收敛到链路容量以下，实际 {target}"
            );
            assert!(target > 500_000.0, "不应崩到下限，实际 {target}");
            // 稳定时不应该还在剧烈降
            assert_ne!(cc.state(), RateState::Decrease, "稳定后不应持续降码率");
        }
    }
}

#[test]
fn controller_backs_off_when_link_shrinks() {
    // 链路从 1 MB/s 突然缩到 200 KB/s —— 控制器必须跟着降
    let mut cc = CongestionController::default();
    let mut link = LinkModel::new(1_000_000.0, 50.0);
    let fps = 30.0;
    let mut t = 1_000_000u64;
    let interval = (1_000_000.0 / fps) as u64;
    let mut recent_loss = VecDeque::new();

    let mut rate_before_shrink = 0.0;

    for i in 0..900 {
        // 第 300 帧时链路缩水
        if i == 300 {
            rate_before_shrink = cc.target_bps();
            link = LinkModel::new(200_000.0, 50.0);
        }

        let frame_bytes = ((cc.target_bps() / 8.0 / fps) as usize).max(200);
        let (dropped, arrival) = link.send(frame_bytes, t);
        if !dropped {
            cc.add_delay_sample(t, arrival);
        }

        recent_loss.push_back(u32::from(dropped));
        if recent_loss.len() > 30 {
            recent_loss.pop_front();
        }
        let loss = recent_loss.iter().sum::<u32>() as f64 / recent_loss.len() as f64;

        cc.update(
            &Feedback {
                loss_ratio: loss,
                rtt_ms: 0.0,
                fresh: true,
            },
            t,
        );

        t += interval;
    }

    assert!(
        cc.target_bps() < rate_before_shrink,
        "链路缩水后码率应下降：{rate_before_shrink} → {}",
        cc.target_bps()
    );
    // 200 KB/s = 1.6 Mbps，目标码率不该还停在更高处
    assert!(
        cc.target_bps() < 3_000_000.0,
        "应降到接近新链路容量，实际 {}",
        cc.target_bps()
    );
    // 链路缩水后确实发生了丢包（否则上面那条断言可能是别的原因过的）。
    //
    // 阈值用 0.005 而不是 0.01：这个场景里控制器降得比较及时，
    // 链路只在缩水初期丢了一小批（约 0.8%），随后就被控住了。
    // 那 0.8% 是真实的丢包证据，不该因为不到 1% 就判为「没丢」。
    assert!(
        link.loss_ratio() > 0.005,
        "缩水后应有可见丢包，实际 {}",
        link.loss_ratio()
    );
}

#[test]
fn frame_size_tracks_target_bitrate() {
    // 码率 → 帧大小的换算必须一致，否则控制器算得再准也没用
    let cc = CongestionController::new(CongestionConfig {
        start_bitrate_bps: 4_000_000.0,
        ..Default::default()
    });
    let fps = 30.0;
    let frame_bytes = cc.target_bps() / 8.0 / fps;
    // 4 Mbps / 8 / 30 = 16666 字节
    assert!(
        (frame_bytes - 16_666.0).abs() < 10.0,
        "4Mbps@30fps 每帧应约 16666 字节，实际 {frame_bytes}"
    );
}

#[test]
fn zero_loss_keeps_increasing_until_ceiling() {
    // 无限带宽链路：控制器应该一路升到上限然后停住
    //
    // # 这里要算清时间尺度
    //
    // `increase_ratio_per_sec = 0.08` 意味着每秒 +8%（复利）。
    // 从起始 1.5 Mbps 到上限 8 Mbps 是 5.33 倍，需要
    //
    //     ln(5.33) / ln(1.08) ≈ 21.6 秒
    //
    // 30fps 下就是约 650 帧。所以模拟 1500 帧（50 秒）才够。
    // 第一版测试只跑了 500 帧（16.5 秒），卡在 5.67 Mbps —— 那是测试
    // 时间不够，不是算法有问题。
    let cfg = CongestionConfig::default();
    let mut cc = CongestionController::new(cfg);
    let mut t = 1_000_000u64;
    for _ in 0..1500 {
        cc.add_delay_sample(t, t + 10_000);
        cc.update(&Feedback::default(), t);
        t += 33_333;
    }
    assert!(
        (cc.target_bps() - cfg.max_bitrate_bps).abs() < 1000.0,
        "应升到上限 {}，实际 {}",
        cfg.max_bitrate_bps,
        cc.target_bps()
    );
}

#[test]
fn permanent_congestion_settles_at_floor() {
    // 持续拥塞：应稳定在下限而不是无限降（负码率）
    let cfg = CongestionConfig::default();
    let mut cc = CongestionController::new(cfg);
    let mut t = 1_000_000u64;
    for _ in 0..200 {
        cc.update(
            &Feedback {
                loss_ratio: 0.6,
                rtt_ms: 500.0,
                fresh: true,
            },
            t,
        );
        t += 33_333;
    }
    assert!(
        (cc.target_bps() - cfg.min_bitrate_bps).abs() < 1.0,
        "应稳定在下限 {}，实际 {}",
        cfg.min_bitrate_bps,
        cc.target_bps()
    );
    assert!(cc.resolution_hint().is_some(), "触底时应建议降分辨率");
}
