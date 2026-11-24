//! AudioJitterBuffer::adapt 的自适应目标深度测试
//!
//! # 这组测试在守什么
//!
//! target_ms 从常量变成自适应之后，最危险的失效模式**不是「没降下来」**
//! 而是「震荡」：目标深度跟着抖动 EWMA 一起抖，而目标深度抖动本身又会
//! 制造迟到帧 —— 正反馈，听感上是一串爆音。所以下面每一条都在钉死
//! 某个闸门，任何一条失效都会让 adapt 变成噪声源：
//!
//! | 闸门 | 值 | 守它的测试 |
//! | --- | --- | --- |
//! | 下限 | 20ms | jitter_zero_converges_to_floor |
//! | 升是全额 | 立即 | jitter_spike_raises_in_one_call |
//! | 单次降幅 | 10ms | down_step_is_capped |
//! | 驻留期 | 500ms | hold_period_blocks_second_adapt |
//! | 样本可信度 | >= 2 | too_few_frames_never_adapts |
//!
//! 另外几条是「改完之后缓冲还能正常工作」的回归：容量要跟着变、队列里
//! 已有的帧不能丢、不调 adapt 时行为与以前完全一致。
//!
//! # 关于构造帧的方式
//!
//! 全部用 Driver 驱动：它自己维护一条连续的 PTS 时间线，避免手写
//! 「第几帧 PTS 是多少」时算错 —— 一旦 PTS 跳变超过 500ms，缓冲会
//! 报 Discontinuity 并作废抖动样本，测试就会以一个完全不相干的
//! 原因失败。

use sidelens_codec::audio::{
    AudioFrame, AudioJitterBuffer, OpusConfig, PushOutcome, AUDIO_ADAPT_HOLD_US,
    AUDIO_ADAPT_MAX_STEP_DOWN_MS, AUDIO_JITTER_SAFETY_FACTOR, AUDIO_MAX_TARGET_MS,
    AUDIO_MIN_TARGET_MS,
};

/// 默认配置（48kHz / 单声道 / 20ms）
fn cfg() -> OpusConfig {
    OpusConfig::default()
}

const DUR_US: u64 = 20_000;
/// 到达时刻的零点（模拟一个非零的本地时钟）
const BASE_US: i64 = 1_000_000;

/// 浮点近似比较（毫秒量级用 1e-6 足够）
fn approx(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-6
}

/// 测试驱动器：维护连续的 PTS 时间线与到达时刻
struct Driver {
    jb: AudioJitterBuffer,
    pts: u64,
    seq: u32,
    /// 已推入的帧数，用来生成交替正负的抖动
    tick: u64,
    /// 最后一次到达时刻（也用作 adapt 的 now_us 基准）
    last_arrival: u64,
    /// adapt 用的虚拟时钟，保证单调且与到达时刻同量级
    clock: u64,
}

impl Driver {
    fn new(target_ms: f64) -> Self {
        Self {
            jb: AudioJitterBuffer::new(cfg(), target_ms).expect("默认配置应能创建缓冲"),
            pts: 0,
            seq: 0,
            tick: 0,
            last_arrival: BASE_US as u64,
            clock: BASE_US as u64,
        }
    }

    /// 推入一帧，到达时刻 = 理想时刻 + offset_us
    fn push_offset(&mut self, offset_us: i64) -> PushOutcome {
        let pts = self.pts;
        let arrived = (BASE_US + pts as i64 + offset_us) as u64;
        let f = AudioFrame::with_arrival(pts, DUR_US, self.seq, arrived, vec![0xAB; 8]);
        let out = self.jb.push_at(f, arrived);
        self.pts += DUR_US;
        self.seq += 1;
        self.tick += 1;
        self.last_arrival = arrived;
        self.clock = self.clock.max(arrived);
        out
    }

    /// 推入一帧，抖动按 ±jitter_us 交替（最坏形态：到达间隔来回摆）
    fn push_jitter(&mut self, jitter_us: i64) -> PushOutcome {
        let offset = if self.tick % 2 == 0 {
            jitter_us
        } else {
            -jitter_us
        };
        self.push_offset(offset)
    }

    /// 推入 n 帧同样的抖动
    fn feed(&mut self, n: usize, jitter_us: i64) {
        for _ in 0..n {
            self.push_jitter(jitter_us);
        }
    }

    /// 推进虚拟时钟并 adapt
    fn adapt(&mut self) -> Option<(f64, f64)> {
        self.clock += AUDIO_ADAPT_HOLD_US;
        self.jb.adapt(self.clock)
    }

    /// 反复 adapt 直到不再变化，返回调整次数
    fn adapt_until_stable(&mut self, limit: usize) -> usize {
        let mut n = 0;
        for _ in 0..limit {
            match self.adapt() {
                Some(_) => n += 1,
                None => break,
            }
        }
        n
    }
}

// ---------------------------------------------------------------------------
// 1. 抖动稳定为 0 → 收敛到下限
// ---------------------------------------------------------------------------

#[test]
fn jitter_zero_converges_to_floor() {
    // 到达时刻与 PTS 完全同步 = 零抖动。目标深度应当一路降到下限 20ms。
    let mut d = Driver::new(100.0);
    d.feed(10, 0);
    assert!(approx(d.jb.jitter_ms(), 0.0), "零抖动下 EWMA 应为 0");
    assert_eq!(d.jb.jitter_samples(), 9, "10 帧产生 9 个样本");

    let changes = d.adapt_until_stable(40);
    assert!(changes > 0, "零抖动时应当主动往下降");
    assert!(
        approx(d.jb.target_ms(), AUDIO_MIN_TARGET_MS),
        "反复 adapt 应收敛到下限 {AUDIO_MIN_TARGET_MS}ms，实际 {}",
        d.jb.target_ms()
    );
    assert_eq!(changes, 8, "100 → 20 按 10ms 一步应正好 8 次");
    assert!(d.adapt().is_none(), "已在下限时不该再有任何调整");
    assert_eq!(d.jb.stats().adapt_downs, 8, "全部应记为向下调整");
    assert_eq!(d.jb.stats().adapt_ups, 0);
}

// ---------------------------------------------------------------------------
// 2. 抖动突然变大 → 一次 adapt 就抬上去
// ---------------------------------------------------------------------------

#[test]
fn jitter_spike_raises_in_one_call() {
    let mut d = Driver::new(20.0);
    // 抖动 ±6ms → |D| 恒为 12ms → want = 12 × 2.0 = 24ms
    d.feed(6, 6_000);

    let (old, new) = d.adapt().expect("抖动变大必须触发调整");
    assert!(approx(old, 20.0), "旧值应为构造时的 20ms，实际 {old}");

    let want = d.jb.jitter_ms() * AUDIO_JITTER_SAFETY_FACTOR;
    assert!(
        approx(new, want),
        "升是全额生效，应直接到 抖动×系数 = {want}ms，实际 {new}ms"
    );
    assert!(new > old, "抖动变大时目标深度必须变大");
    assert!(approx(d.jb.target_ms(), new), "target_ms() 必须与新值一致");
    assert_eq!(d.jb.stats().adapt_ups, 1, "应记一次向上调整");
    assert_eq!(d.jb.stats().adapt_downs, 0);
}

// ---------------------------------------------------------------------------
// 3. 单次降幅不超过 10ms
// ---------------------------------------------------------------------------

#[test]
fn down_step_is_capped() {
    // 从 100ms 开始、抖动只有 1ms 量级 → want 远低于当前值，
    // 但一次最多只能降 AUDIO_ADAPT_MAX_STEP_DOWN_MS
    let mut d = Driver::new(100.0);
    d.feed(4, 1_000);
    let want = d.jb.jitter_ms() * AUDIO_JITTER_SAFETY_FACTOR;
    assert!(
        want < 100.0 - AUDIO_ADAPT_MAX_STEP_DOWN_MS,
        "前提：want 远低于当前值"
    );

    let mut last = d.jb.target_ms();
    for step in 0..3 {
        let (old, new) = d.adapt().expect("应能降");
        let drop = old - new;
        assert!(
            drop <= AUDIO_ADAPT_MAX_STEP_DOWN_MS + 1e-9,
            "第 {step} 次降幅 {drop}ms 超过上限 {AUDIO_ADAPT_MAX_STEP_DOWN_MS}ms"
        );
        assert!(drop > 0.0, "降幅必须为正");
        assert!(
            approx(new, old - AUDIO_ADAPT_MAX_STEP_DOWN_MS),
            "应按满步长下降"
        );
        last = new;
    }
    assert!(last < 100.0, "多次 adapt 后应确实降下来了，实际 {last}");
    assert_eq!(d.jb.stats().adapt_downs, 3, "应记三次向下调整");
}

// ---------------------------------------------------------------------------
// 4. 驻留期内不再调整
// ---------------------------------------------------------------------------

#[test]
fn hold_period_blocks_second_adapt() {
    let mut d = Driver::new(100.0);
    d.feed(4, 1_000);

    let (_, first) = d.adapt().expect("第一次应调整");
    let now = d.clock;

    // 驻留期内：无论怎么调都不动
    assert!(
        d.jb.adapt(now + AUDIO_ADAPT_HOLD_US - 1).is_none(),
        "驻留期内不该再调整"
    );
    assert!(
        approx(d.jb.target_ms(), first),
        "驻留期内 target_ms 必须不变"
    );
    assert_eq!(d.jb.stats().adaptations, 1);

    // 刚好到期：可以再调
    assert!(
        d.jb.adapt(now + AUDIO_ADAPT_HOLD_US).is_some(),
        "驻留期一过就该允许调整"
    );
    assert_eq!(d.jb.stats().adaptations, 2);
}

// ---------------------------------------------------------------------------
// 5. 0 帧 / 1 帧时不动
// ---------------------------------------------------------------------------

#[test]
fn too_few_frames_never_adapts() {
    // 0 帧：连样本都没有
    let mut empty = Driver::new(100.0);
    assert!(empty.jb.adapt(0).is_none(), "空缓冲不该调整");
    assert!(
        empty.jb.adapt(u64::MAX).is_none(),
        "空缓冲任何时刻都不该调整"
    );
    assert_eq!(empty.jb.stats(), Default::default(), "不该产生任何统计");

    // 1 帧：算不出 |D|（需要相邻两帧）
    let mut one = Driver::new(100.0);
    one.feed(1, 5_000);
    assert_eq!(one.jb.jitter_samples(), 0, "单帧不产生抖动样本");
    assert!(one.adapt().is_none(), "1 帧时样本不可信，不该调整");
    assert!(approx(one.jb.target_ms(), 100.0), "不该改动 target_ms");

    // 2 帧：只有一个样本，仍不够
    let mut two = Driver::new(100.0);
    two.feed(2, 5_000);
    assert_eq!(two.jb.jitter_samples(), 1, "两帧产生一个样本");
    assert!(
        two.adapt().is_none(),
        "只有一个样本时不该调整（分不清尖峰和稳定抖动）"
    );

    // 3 帧：两个样本，够了
    let mut three = Driver::new(100.0);
    three.feed(3, 5_000);
    assert_eq!(three.jb.jitter_samples(), 2);
    assert!(three.adapt().is_some(), "两个样本起就该能调整");
}

// ---------------------------------------------------------------------------
// 6. 改变 target 后 capacity_frames() 跟着变
// ---------------------------------------------------------------------------

#[test]
fn capacity_follows_target() {
    // 20ms 帧：容量 = ceil(target/20) + 1
    let mut d = Driver::new(100.0);
    assert_eq!(d.jb.capacity_frames(), 6, "100ms 目标应为 5+1 帧");

    d.feed(4, 1_000);
    let (_, first) = d.adapt().expect("应能降");
    // 降幅 10ms → 90ms → ceil(90/20)+1 = 6（90/20=4.5 向上取整 5）
    assert!(approx(first, 90.0));
    assert_eq!(d.jb.capacity_frames(), 6, "90ms → ceil(4.5)+1 = 6");

    // 继续降到 20ms：容量应缩到 2。注意每轮都要补抖动样本，
    // 否则链路一安静 EWMA 就按 0.9 衰减、几帧后归零。
    let mut seen = vec![d.jb.capacity_frames()];
    for _ in 0..20 {
        d.feed(2, 1_000);
        if d.adapt().is_none() {
            continue;
        }
        let c = d.jb.capacity_frames();
        if seen.last() != Some(&c) {
            seen.push(c);
        }
    }
    assert!(approx(d.jb.target_ms(), 20.0), "最终应收敛到下限");
    assert_eq!(
        d.jb.capacity_frames(),
        2,
        "20ms 目标 → ceil(1)+1 = 2，容量必须跟着变"
    );
    assert!(
        seen.windows(2).all(|w| w[0] >= w[1]),
        "容量应单调下降，实际路径 {seen:?}"
    );
    assert!(seen.len() >= 3, "容量应当真的变过，实际路径 {seen:?}");
}

#[test]
fn capacity_grows_with_target() {
    let mut d = Driver::new(20.0);
    assert_eq!(d.jb.capacity_frames(), 2, "20ms → 2 帧");

    // 大抖动把目标顶上去：|D| = 40ms → want = 80ms → ceil(4)+1 = 5
    d.feed(6, 20_000);
    let (_, new) = d.adapt().expect("大抖动必须抬升");
    assert!(new > 20.0, "目标应变大，实际 {new}");
    assert!(
        d.jb.capacity_frames() > 2,
        "容量必须随目标变大，实际 {}",
        d.jb.capacity_frames()
    );
    assert_eq!(
        d.jb.capacity_frames(),
        ((new / 20.0).ceil() as usize) + 1,
        "容量公式必须与 new 一致"
    );
}

// ---------------------------------------------------------------------------
// 7. 调小 target 不丢队列里已有的帧
// ---------------------------------------------------------------------------

#[test]
fn shrinking_keeps_queued_frames() {
    // 从 100ms 开始（容量 6），塞满 6 帧
    let mut d = Driver::new(100.0);
    d.feed(6, 0);
    assert_eq!(d.jb.len(), 6, "六帧都应在队");
    assert!(approx(d.jb.depth_ms(), 120.0));

    // 反复 adapt 把目标压到 20ms（容量 2）—— 队列里仍是 6 帧
    d.adapt_until_stable(40);
    assert!(approx(d.jb.target_ms(), 20.0), "目标应已降到下限");
    assert_eq!(d.jb.capacity_frames(), 2, "容量应已降到 2");
    assert_eq!(d.jb.len(), 6, "调小容量绝不能丢掉队列里已有的有效帧");
    assert!(approx(d.jb.depth_ms(), 120.0), "深度不应因 adapt 改变");

    // 已有的帧仍然能正常播出，顺序不变
    let mut got = Vec::new();
    for _ in 0..6 {
        got.push(d.jb.pop_due(10_000_000).expect("已有帧应可取出").seq);
    }
    assert_eq!(got, vec![0, 1, 2, 3, 4, 5], "已有帧必须按原顺序播出");
}

#[test]
fn shrink_does_not_drop_and_overflow_keeps_pace() {
    // 调小容量后队列**不会**被立刻砍掉 —— 已入队的帧仍然有效。
    //
    // # 这里量到一个必须写清楚的语义
    //
    // 溢出判定是 len > capacity 就「丢最旧一帧」，而且**每次 push 只丢一帧**。
    // 所以调小容量之后队列长度会**稳定在原长度**（push 加一帧、溢出减一帧，
    // 收支平衡），不会一路收敛到新容量。
    //
    // 这是刻意的：调小 target 只该影响「之后还能囤多少」，不该惩罚已经
    // 在队里的有效帧。代价是队列会暂时比 capacity 长，直到自然播出。
    // 谁要改这条语义，必须同时改 jitter_buffer_overflow_drops_oldest。
    let mut d = Driver::new(100.0);
    d.feed(5, 0);
    assert_eq!(d.jb.len(), 5);

    d.adapt_until_stable(40);
    assert_eq!(d.jb.capacity_frames(), 2);
    assert_eq!(d.jb.len(), 5, "adapt 本身一帧都不丢");
    assert_eq!(d.jb.dropped_overflow(), 0, "adapt 不产生溢出丢弃");

    // 第 6 帧：len 6 > 容量 2 → 淘汰最旧的一帧，len 回到 5
    assert_eq!(d.push_offset(0), PushOutcome::Overflow);
    assert_eq!(d.jb.len(), 5, "每次溢出只淘汰一帧，不是一次砍到底");
    assert_eq!(d.jb.dropped_overflow(), 1);

    // 队首必须是被推到最前面的那帧（原 seq 0 已淘汰）
    let front = d.jb.pop_due(10_000_000).expect("应能取出");
    assert_eq!(front.seq, 1, "丢的必须是最旧的，队首应是原第 2 帧");

    // 自然播出几帧后，队列长度就回到容量以内
    while d.jb.len() > d.jb.capacity_frames() {
        assert!(d.jb.pop_due(10_000_000).is_some(), "队列非空，必须能取出");
    }
    assert_eq!(d.jb.len(), 2, "播出之后长度应回到容量");
}

// ---------------------------------------------------------------------------
// 8. 向后兼容：不调 adapt 就永远是构造时的值
// ---------------------------------------------------------------------------

#[test]
fn without_adapt_target_never_changes() {
    let mut d = Driver::new(40.0);
    d.feed(50, 8_000); // 剧烈抖动
    assert!(approx(d.jb.target_ms(), 40.0), "不调 adapt 就该一直是 40ms");
    assert_eq!(d.jb.capacity_frames(), 3, "容量也该一直是构造时的值");
    assert_eq!(d.jb.stats().adaptations, 0, "不该有任何调整");
    assert_eq!(d.jb.stats().adapt_ups, 0);
    assert_eq!(d.jb.stats().adapt_downs, 0);
    // avg_jitter_ms 会随 push 更新（那是量测，不是调整），
    // 所以这里只校验「没有任何调整动作」，不要求整个 stats 归零。
    assert!(d.jb.stats().avg_jitter_ms > 0.0, "抖动仍在量测中");
}

#[test]
fn arrived_at_us_defaults_to_zero_and_is_ignored() {
    // AudioFrame::new 不设到达时刻，默认 0 表示「调用方没填」
    let f = AudioFrame::new(0, 20_000, 0, vec![1]);
    assert_eq!(f.arrived_at_us, 0, "默认到达时刻应为 0");

    // 全部 arrived_at_us = 0 → 一律当作「没填」，不产生任何抖动样本。
    // 这是老调用点（不填 arrived_at_us 也不调 adapt）行为完全不变的保证。
    let mut jb = AudioJitterBuffer::new(cfg(), 40.0).expect("缓冲");
    for i in 0..20u32 {
        jb.push(AudioFrame::new(i as u64 * 20_000, 20_000, i, vec![1; 8]));
    }
    assert_eq!(jb.jitter_samples(), 0, "没填到达时刻就不该造出假样本");
    assert!(approx(jb.jitter_ms(), 0.0), "EWMA 应保持 0");
    assert!(jb.adapt(10_000_000).is_none(), "没有样本就不该调整");
    assert!(approx(jb.target_ms(), 40.0), "target 必须保持构造值");
}

// ---------------------------------------------------------------------------
// 9. 统计字段
// ---------------------------------------------------------------------------

#[test]
fn stats_track_ups_and_downs() {
    let mut d = Driver::new(100.0);
    d.feed(4, 1_000);
    assert_eq!(d.jb.stats().adaptations, 0);

    // 先降两次：100 → 80
    for _ in 0..2 {
        d.adapt().expect("应能降");
    }
    assert_eq!(d.jb.stats().adapt_downs, 2);
    assert_eq!(d.jb.stats().adapt_ups, 0);
    assert_eq!(d.jb.stats().adaptations, 2);
    assert!(approx(d.jb.target_ms(), 80.0));

    // 再来一波大抖动：|D| = 60ms → want = 120ms > 80 → 记为向上。
    // 帧数要给够，让 EWMA 从 2ms 爬到 60ms 附近（0.9/0.1 约 10 个样本
    // 跟上一次阶跃，这里给 40 帧留足余量）。
    d.feed(40, 30_000);
    assert!(
        d.jb.jitter_ms() * AUDIO_JITTER_SAFETY_FACTOR > 80.0,
        "前提：want 必须大于当前 80ms，实际 jitter={}",
        d.jb.jitter_ms()
    );
    d.adapt().expect("大抖动应触发抬升");
    assert_eq!(d.jb.stats().adapt_ups, 1);
    assert_eq!(d.jb.stats().adaptations, 3);
    assert!(
        d.jb.stats().avg_jitter_ms > 0.0,
        "avg_jitter_ms 应反映实测抖动"
    );
    assert!(
        approx(d.jb.stats().avg_jitter_ms, d.jb.jitter_ms()),
        "stats.avg_jitter_ms 应与 jitter_ms() 一致"
    );
}

// ---------------------------------------------------------------------------
// 10. clear 重置抖动估计但不重置目标深度与计数
// ---------------------------------------------------------------------------

#[test]
fn clear_resets_jitter_but_keeps_target() {
    let mut d = Driver::new(100.0);
    d.feed(6, 5_000);
    d.adapt().expect("应先调整一次");
    let target_after = d.jb.target_ms();
    assert!(target_after < 100.0);

    d.jb.clear();
    assert_eq!(d.jb.jitter_samples(), 0, "清空后抖动样本应作废");
    assert!(approx(d.jb.stats().avg_jitter_ms, 0.0), "EWMA 应清零");
    assert!(
        approx(d.jb.target_ms(), target_after),
        "清空队列不代表链路变好，target 不该回退"
    );
    assert_eq!(d.jb.stats().adaptations, 1, "累计计数应保留");

    // 清空后必须重新攒够样本才敢动
    assert!(d.adapt().is_none(), "样本不足不该调整");
}

// ---------------------------------------------------------------------------
// 11. 抖动估计本身：|D| 的正确性
// ---------------------------------------------------------------------------

#[test]
fn jitter_estimate_matches_transit_deviation() {
    let mut d = Driver::new(40.0);
    d.push_offset(0); // 第 0 帧正点到达，无样本
    assert_eq!(d.jb.jitter_samples(), 0);

    // 第 1 帧晚到 4ms → |D| = |(20000+4000) - 20000| = 4ms
    d.push_offset(4_000);
    assert_eq!(d.jb.jitter_samples(), 1);
    assert!(
        approx(d.jb.jitter_ms(), 4.0),
        "首样本应等于 |Δ到达 - ΔPTS| = 4ms，实际 {}",
        d.jb.jitter_ms()
    );

    // 第 2 帧又晚到 4ms → 到达间隔回到 20ms、PTS 间隔 20ms → |D| = 0
    // EWMA 从 4ms 按 0.9/0.1 回落到 3.6ms
    d.push_offset(4_000);
    assert!(
        approx(d.jb.jitter_ms(), 3.6),
        "恒定偏移的抖动会衰减，实际 {}",
        d.jb.jitter_ms()
    );

    // 交替 ±4ms：到达间隔在 12/28ms 之间摆 → |D| 恒为 8ms，EWMA 收敛到 8ms
    let mut d2 = Driver::new(40.0);
    for _ in 0..60 {
        d2.push_jitter(4_000);
    }
    assert!(
        approx(d2.jb.jitter_ms(), 8.0),
        "交替 ±4ms 的 |D| 应恒为 8ms，实际 {}",
        d2.jb.jitter_ms()
    );
}

#[test]
fn late_frames_do_not_pollute_jitter() {
    let mut d = Driver::new(40.0);
    d.feed(3, 0);
    let samples_before = d.jb.jitter_samples();

    // 推进播放时钟，让旧帧变成迟到帧
    d.jb.pop_due(BASE_US as u64 + 60_000);
    // 重传一帧早就该播的（PTS 0）
    let arrived = d.clock + 500_000;
    let out = d.jb.push_at(
        AudioFrame::with_arrival(0, DUR_US, 99, arrived, vec![1]),
        arrived,
    );
    assert_eq!(out, PushOutcome::Late);
    assert_eq!(
        d.jb.jitter_samples(),
        samples_before,
        "迟到帧的到达时刻反映的是重传/绕路，不该喂进抖动估计"
    );
}

#[test]
fn discontinuity_invalidates_jitter_sample() {
    let mut d = Driver::new(40.0);
    d.feed(2, 0);
    assert_eq!(d.jb.jitter_samples(), 1);

    // 跳变 620ms：时间线断了，这一对帧不产生样本
    let jumped = 620_000u64;
    let arrived = BASE_US as u64 + jumped;
    let out = d.jb.push_at(
        AudioFrame::with_arrival(jumped, DUR_US, 2, arrived, vec![1]),
        arrived,
    );
    assert_eq!(out, PushOutcome::Discontinuity);
    assert_eq!(d.jb.jitter_samples(), 1, "跨断点的那一对帧不产生样本");

    // 断点后的第一帧：没有可比的上一到达时刻，仍不产生样本
    let arrived = BASE_US as u64 + jumped + DUR_US;
    d.jb.push_at(
        AudioFrame::with_arrival(jumped + DUR_US, DUR_US, 3, arrived, vec![1]),
        arrived,
    );
    assert_eq!(d.jb.jitter_samples(), 1, "断点后第一帧仍无样本");

    // 断点后的第二帧：重新开始累计
    let arrived = BASE_US as u64 + jumped + 2 * DUR_US;
    d.jb.push_at(
        AudioFrame::with_arrival(jumped + 2 * DUR_US, DUR_US, 4, arrived, vec![1]),
        arrived,
    );
    assert_eq!(d.jb.jitter_samples(), 2, "断点后应重新开始累计");
}

// ---------------------------------------------------------------------------
// 12. 上限
// ---------------------------------------------------------------------------

#[test]
fn target_never_exceeds_max() {
    let mut d = Driver::new(20.0);
    // 极端抖动：到达间隔与 PTS 差 400ms
    d.feed(6, 400_000);
    let (_, new) = d.adapt().expect("极端抖动应触发调整");
    assert!(
        new <= AUDIO_MAX_TARGET_MS + 1e-9,
        "目标深度不能超过上限，实际 {new}"
    );
    assert!(
        approx(new, AUDIO_MAX_TARGET_MS),
        "极端抖动应直接顶到上限，实际 {new}"
    );
    assert_eq!(
        d.jb.capacity_frames(),
        ((AUDIO_MAX_TARGET_MS / 20.0).ceil() as usize) + 1
    );
}
