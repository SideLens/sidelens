//! 音频帧封装与时间戳管理测试
//!
//! 全部用手工构造的配置与帧验证，不依赖 Opus 编码器、声卡或真实时钟。
//!
//! # 测试的重点在哪
//!
//! 1. **参数校验必须硬失败** —— 非法帧长被静默改写的后果是音画不同步，
//!    而这在音频本身听起来完全正常时极难定位
//! 2. **PTS 严格等距** —— 这是「按采样计数」原则的可观测后果，
//!    一旦有人把实现改成读墙上时间，第 7、8 个用例会立刻红
//! 3. **抖动缓冲的四种结局各有一条路径** —— Ok / Late / Overflow / Discontinuity，
//!    漏掉任何一条，线上就是一段没人负责的静音或爆音

use sidelens_codec::audio::{
    AudioFrame, AudioJitterBuffer, AudioPacer, OpusConfig, PushOutcome, DISCONTINUITY_THRESHOLD_US,
    OPUS_FRAME_MS, OPUS_SAMPLE_RATES,
};

/// 默认配置（48kHz / 单声道 / 20ms）
fn cfg() -> OpusConfig {
    OpusConfig::default()
}

/// 构造一帧（20ms 时长，与默认配置一致）
fn frame(pts_us: u64, seq: u32) -> AudioFrame {
    AudioFrame::new(pts_us, 20_000, seq, vec![0xAB; 8])
}

/// 浮点近似比较（毫秒量级用 1e-6 足够）
fn approx(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-6
}

// ---------------------------------------------------------------------------
// 1. 默认配置合法
// ---------------------------------------------------------------------------

#[test]
fn default_config_is_valid() {
    let c = cfg();
    assert_eq!(c.sample_rate, 48_000, "默认采样率应为 48kHz");
    assert_eq!(c.channels, 1, "默认应为单声道");
    assert!(approx(c.frame_ms, 20.0), "默认帧长应为 20ms");
    assert!(c.validate().is_ok(), "默认配置必须通过校验");
}

// ---------------------------------------------------------------------------
// 2. 非法采样率
// ---------------------------------------------------------------------------

#[test]
fn invalid_sample_rate_is_rejected() {
    // 44100 是音乐领域最常见的采样率，也正因如此最容易被误传进来
    let c = OpusConfig {
        sample_rate: 44_100,
        ..cfg()
    };
    let err = c.validate().expect_err("44100Hz 不是 Opus 支持的采样率");
    assert!(err.contains("44100"), "错误信息应带上非法值，实际：{err}");

    // 0 采样率不能让后续计算除零崩溃
    let zero = OpusConfig {
        sample_rate: 0,
        ..cfg()
    };
    assert!(zero.validate().is_err(), "0Hz 必须被拒绝");
    assert_eq!(zero.frame_duration_us(), 0, "非法采样率下时长应安全返回 0");
}

// ---------------------------------------------------------------------------
// 3. 非法帧长
// ---------------------------------------------------------------------------

#[test]
fn invalid_frame_ms_is_rejected() {
    // 15ms 看起来完全合理，但 Opus 会静默按 10ms 或 20ms 编码
    for bad in [15.0, 7.0, 25.0, 1.0, 0.0, -20.0] {
        let c = OpusConfig {
            frame_ms: bad,
            ..cfg()
        };
        assert!(
            c.validate().is_err(),
            "{bad}ms 不是 Opus 合法帧长，应被拒绝"
        );
    }

    // 错误信息必须点明「会被静默改写」，否则调用方不会理解为什么要硬失败
    let c = OpusConfig {
        frame_ms: 15.0,
        ..cfg()
    };
    let err = c.validate().expect_err("15ms 应被拒绝");
    assert!(
        err.contains("静默"),
        "错误信息应解释静默改写的风险，实际：{err}"
    );
}

#[test]
fn all_six_standard_frame_lengths_pass() {
    for ms in OPUS_FRAME_MS {
        let c = OpusConfig {
            frame_ms: ms,
            ..cfg()
        };
        assert!(c.validate().is_ok(), "{ms}ms 是标准帧长，应通过校验");
    }
}

// ---------------------------------------------------------------------------
// 4. 声道数
// ---------------------------------------------------------------------------

#[test]
fn invalid_channel_count_is_rejected() {
    for bad in [0u8, 3, 6] {
        let c = OpusConfig {
            channels: bad,
            ..cfg()
        };
        assert!(c.validate().is_err(), "{bad} 声道应被拒绝");
    }

    for ok in [1u8, 2] {
        let c = OpusConfig {
            channels: ok,
            ..cfg()
        };
        assert!(c.validate().is_ok(), "{ok} 声道应被接受");
    }
}

// ---------------------------------------------------------------------------
// 5、6. 采样数与时长计算
// ---------------------------------------------------------------------------

#[test]
fn frame_samples_is_computed_correctly() {
    // 48000Hz × 20ms = 960 采样，这是 Opus 最经典的帧尺寸
    assert_eq!(cfg().frame_samples(), 960, "48kHz 20ms 应为 960 采样");

    let stereo_10ms = OpusConfig {
        sample_rate: 48_000,
        channels: 2,
        frame_ms: 10.0,
    };
    assert_eq!(stereo_10ms.frame_samples(), 480, "声道数不影响每帧采样数");

    let narrow = OpusConfig {
        sample_rate: 8_000,
        channels: 1,
        frame_ms: 60.0,
    };
    assert_eq!(narrow.frame_samples(), 480, "8kHz 60ms 应为 480 采样");
}

#[test]
fn frame_duration_is_computed_correctly() {
    assert_eq!(cfg().frame_duration_us(), 20_000, "20ms 应为 20000 微秒");

    // 全部合法组合都要能整除，否则 PTS 会逐帧累积误差
    for rate in OPUS_SAMPLE_RATES {
        for ms in OPUS_FRAME_MS {
            let c = OpusConfig {
                sample_rate: rate,
                channels: 1,
                frame_ms: ms,
            };
            let expect = (ms * 1000.0).round() as u64;
            assert_eq!(
                c.frame_duration_us(),
                expect,
                "{rate}Hz {ms}ms 的时长应与毫秒值一致"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 7、8. Pacer 的 PTS 严格等距
// ---------------------------------------------------------------------------

#[test]
fn pacer_pts_is_strictly_evenly_spaced() {
    let mut pacer = AudioPacer::new(cfg(), 0).expect("默认配置应能创建 pacer");
    let dur = pacer.frame_duration_us();

    let mut prev = pacer.next_frame().0;
    for i in 1..100u64 {
        let (pts, _) = pacer.next_frame();
        assert!(pts > prev, "第 {i} 帧的 PTS 必须严格递增：{prev} → {pts}");
        assert_eq!(pts - prev, dur, "第 {i} 帧的间隔必须恒等于帧时长");
        prev = pts;
    }
}

#[test]
fn pacer_interval_equals_frame_duration() {
    for ms in OPUS_FRAME_MS {
        let c = OpusConfig {
            frame_ms: ms,
            ..cfg()
        };
        let mut pacer = AudioPacer::new(c, 0).expect("标准帧长应能创建 pacer");
        let expect_dur = (ms * 1000.0).round() as u64;

        let (p0, d0) = pacer.next_frame();
        let (p1, d1) = pacer.next_frame();
        assert_eq!(d0, expect_dur, "{ms}ms 配置下返回的时长不对");
        assert_eq!(d1, expect_dur, "同一 pacer 的时长必须恒定");
        assert_eq!(p1 - p0, expect_dur, "{ms}ms 配置下 PTS 间隔应等于时长");
    }
}

#[test]
fn pacer_starts_from_non_zero_origin() {
    let mut pacer = AudioPacer::new(cfg(), 1_000_000).expect("默认配置应能创建 pacer");
    assert_eq!(pacer.current_pts_us(), 1_000_000, "初始 PTS 应等于起点");
    assert_eq!(pacer.next_frame().0, 1_000_000, "首帧应从起点开始");
    assert_eq!(
        pacer.current_pts_us(),
        1_020_000,
        "取帧后应推进到下一帧位置"
    );
}

// ---------------------------------------------------------------------------
// 9. frames_emitted
// ---------------------------------------------------------------------------

#[test]
fn pacer_frame_counter_is_accurate() {
    let mut pacer = AudioPacer::new(cfg(), 0).expect("默认配置应能创建 pacer");
    assert_eq!(pacer.frames_emitted(), 0, "初始帧数应为 0");

    for expect in 1..=50u64 {
        pacer.next_frame();
        assert_eq!(pacer.frames_emitted(), expect, "帧计数应逐帧加一");
    }

    // current_pts_us 只是读，不能顺手把计数改掉
    let _ = pacer.current_pts_us();
    assert_eq!(pacer.frames_emitted(), 50, "读当前 PTS 不应影响帧计数");
}

// ---------------------------------------------------------------------------
// 10. reset
// ---------------------------------------------------------------------------

#[test]
fn pacer_reset_restarts_from_new_pts() {
    let mut pacer = AudioPacer::new(cfg(), 0).expect("默认配置应能创建 pacer");
    for _ in 0..10 {
        pacer.next_frame();
    }
    assert_eq!(pacer.frames_emitted(), 10);

    pacer.reset(5_000_000);
    assert_eq!(pacer.current_pts_us(), 5_000_000, "重置后应停在新起点");
    assert_eq!(pacer.frames_emitted(), 0, "重置应把帧计数归零");
    assert_eq!(pacer.next_frame().0, 5_000_000, "重置后首帧应在新起点");
    assert_eq!(pacer.frames_emitted(), 1, "重置后计数从 1 重新开始");
}

#[test]
fn pacer_rejects_invalid_config() {
    let bad = OpusConfig {
        frame_ms: 15.0,
        ..cfg()
    };
    assert!(AudioPacer::new(bad, 0).is_err(), "非法帧长不应能创建 pacer");
}

// ---------------------------------------------------------------------------
// 11. 抖动缓冲：顺序推入、按时间取出
// ---------------------------------------------------------------------------

#[test]
fn jitter_buffer_pops_in_time_order() {
    // 目标 100ms → 容量 = ceil(100/20) + 1 = 6 帧，够放 5 帧不溢出
    let mut jb = AudioJitterBuffer::new(cfg(), 100.0).expect("默认配置应能创建缓冲");
    for i in 0..5u32 {
        let out = jb.push(frame(i as u64 * 20_000, i));
        assert_eq!(out, PushOutcome::Ok, "顺序推入应为 Ok");
    }
    assert_eq!(jb.len(), 5, "五帧都应入队");

    let f0 = jb.pop_due(0).expect("第一帧应立即到期");
    assert_eq!(f0.pts_us, 0, "第一帧 PTS 应为 0");
    assert_eq!(f0.seq, 0, "第一帧序号应为 0");

    assert!(jb.pop_due(19_999).is_none(), "第二帧在 20ms 前不该到期");

    let f1 = jb.pop_due(20_000).expect("第二帧在 20ms 时到期");
    assert_eq!(f1.pts_us, 20_000);
    assert_eq!(f1.seq, 1, "取出顺序必须严格按 PTS");
}

// ---------------------------------------------------------------------------
// 12. 抖动缓冲：乱序推入
// ---------------------------------------------------------------------------

#[test]
fn jitter_buffer_reorders_out_of_order_frames() {
    let mut jb = AudioJitterBuffer::new(cfg(), 100.0).expect("默认配置应能创建缓冲");
    // 模拟网络到达顺序：60 / 0 / 40 / 20
    for (pts, seq) in [(60_000u64, 3u32), (0, 0), (40_000, 2), (20_000, 1)] {
        assert_eq!(
            jb.push(frame(pts, seq)),
            PushOutcome::Ok,
            "预缓冲阶段的乱序帧不应被判迟到"
        );
    }
    assert_eq!(jb.len(), 4);

    let mut got = Vec::new();
    for _ in 0..4 {
        got.push(jb.pop_due(1_000_000).expect("全部到期后应能取出").seq);
    }
    assert_eq!(got, vec![0, 1, 2, 3], "取出顺序必须按 PTS 而不是到达顺序");
}

// ---------------------------------------------------------------------------
// 13. 抖动缓冲：迟到帧
// ---------------------------------------------------------------------------

#[test]
fn jitter_buffer_drops_late_frames() {
    // 目标 40ms → 迟到窗口 40ms，容量 3 帧
    let mut jb = AudioJitterBuffer::new(cfg(), 40.0).expect("默认配置应能创建缓冲");
    for i in 0..3u32 {
        jb.push(frame(i as u64 * 20_000, i));
    }

    // 播放时钟推到 60ms：此时 0ms 那帧已经彻底过期
    let f = jb.pop_due(60_000).expect("应取出最早一帧");
    assert_eq!(f.pts_us, 0);

    // 0ms 帧的重传到达：时钟早已越过它
    let out = jb.push(frame(0, 0));
    assert_eq!(out, PushOutcome::Late, "播放时钟已越过的帧必须判迟到");
    assert_eq!(jb.dropped_late(), 1, "迟到计数应加一");
    assert_eq!(jb.len(), 2, "迟到帧不能入队污染后续播放");

    // 45ms 帧：45000 + 20000 = 65000 > 60000，还在迟到窗口内，应能补播
    assert_eq!(
        jb.push(frame(45_000, 9)),
        PushOutcome::Ok,
        "落后不超过目标深度的帧仍可补播"
    );

    // 20ms 帧：20000 + 20000 = 40000 <= 60000，超出窗口，只能丢
    assert_eq!(
        jb.push(frame(20_000, 1)),
        PushOutcome::Late,
        "落后超过目标深度的帧应被丢弃"
    );
    assert_eq!(jb.dropped_late(), 2, "迟到计数应累计");
}

// ---------------------------------------------------------------------------
// 14. 抖动缓冲：溢出
// ---------------------------------------------------------------------------

#[test]
fn jitter_buffer_overflow_drops_oldest() {
    // 20ms 帧 + 40ms 目标深度 → 容量 = ceil(40/20) + 1 = 3 帧
    let mut jb = AudioJitterBuffer::new(cfg(), 40.0).expect("默认配置应能创建缓冲");
    assert_eq!(jb.capacity_frames(), 3, "容量应为目标帧数加一帧余量");

    assert_eq!(jb.push(frame(0, 0)), PushOutcome::Ok);
    assert_eq!(jb.push(frame(20_000, 1)), PushOutcome::Ok);
    assert_eq!(jb.push(frame(40_000, 2)), PushOutcome::Ok);

    // 第 4 帧把队列顶到 4，超出容量
    assert_eq!(
        jb.push(frame(60_000, 3)),
        PushOutcome::Overflow,
        "超容量应报溢出"
    );
    assert_eq!(jb.dropped_overflow(), 1, "溢出计数应加一");
    assert_eq!(jb.len(), 3, "溢出后队列长度应回到容量");

    let f = jb.pop_due(60_000).expect("应取出剩余最早一帧");
    assert_eq!(f.pts_us, 20_000, "被丢掉的是最旧的 0ms 帧");

    // 取走一帧后队列只剩 2，再推两帧才会再次溢出
    assert_eq!(jb.push(frame(80_000, 4)), PushOutcome::Ok);
    assert_eq!(jb.push(frame(100_000, 5)), PushOutcome::Overflow);
    assert_eq!(jb.dropped_overflow(), 2, "溢出计数应累计");
    assert_eq!(jb.len(), 3, "再次溢出后队列长度仍应等于容量");
}

// ---------------------------------------------------------------------------
// 15. 抖动缓冲：时间戳跳变
// ---------------------------------------------------------------------------

#[test]
fn jitter_buffer_reports_discontinuity_on_jump() {
    let mut jb = AudioJitterBuffer::new(cfg(), 40.0).expect("默认配置应能创建缓冲");
    jb.push(frame(0, 0));
    jb.push(frame(20_000, 1));
    assert_eq!(jb.len(), 2);

    // 跳变 +600ms：超过 500ms 阈值，只可能是采集重启或换设备
    let jumped = 20_000 + DISCONTINUITY_THRESHOLD_US + 100_000;
    assert_eq!(
        jb.push(frame(jumped, 2)),
        PushOutcome::Discontinuity,
        "超过阈值的跳变必须报不连续"
    );
    assert_eq!(jb.len(), 1, "跳变后旧队列应被清空，只留新起点那一帧");
    assert_eq!(jb.dropped_late(), 0, "跳变不算迟到");

    // 新时间线可以继续正常工作
    assert_eq!(jb.push(frame(jumped + 20_000, 3)), PushOutcome::Ok);
    let f = jb.pop_due(jumped).expect("新时间线的第一帧应可取出");
    assert_eq!(f.pts_us, jumped, "跳变帧应成为新时间线起点");
}

// ---------------------------------------------------------------------------
// 16. depth_ms
// ---------------------------------------------------------------------------

#[test]
fn jitter_buffer_depth_is_sum_of_durations() {
    let mut jb = AudioJitterBuffer::new(cfg(), 40.0).expect("默认配置应能创建缓冲");
    assert!(approx(jb.depth_ms(), 0.0), "空缓冲深度应为 0");

    for i in 0..3u32 {
        jb.push(frame(i as u64 * 20_000, i));
    }
    assert!(
        approx(jb.depth_ms(), 60.0),
        "三帧 20ms 应为 60ms，实际 {}",
        jb.depth_ms()
    );

    jb.pop_due(0);
    assert!(
        approx(jb.depth_ms(), 40.0),
        "取走一帧后应剩 40ms，实际 {}",
        jb.depth_ms()
    );
}

// ---------------------------------------------------------------------------
// 17. 空缓冲
// ---------------------------------------------------------------------------

#[test]
fn empty_jitter_buffer_pops_none() {
    let mut jb = AudioJitterBuffer::new(cfg(), 40.0).expect("默认配置应能创建缓冲");
    assert!(jb.is_empty(), "新建缓冲应为空");
    assert!(jb.pop_due(0).is_none(), "空缓冲任何时刻都应返回 None");
    assert!(jb.pop_due(u64::MAX).is_none(), "极端时钟值也不应 panic");
    assert_eq!(jb.dropped_late(), 0);
    assert_eq!(jb.dropped_overflow(), 0);
}

// ---------------------------------------------------------------------------
// 18. 2.5ms 边界
// ---------------------------------------------------------------------------

#[test]
fn minimum_frame_length_2_5ms_works() {
    let c = OpusConfig {
        sample_rate: 48_000,
        channels: 1,
        frame_ms: 2.5,
    };
    assert!(c.validate().is_ok(), "2.5ms 是 Opus 最小合法帧长");
    assert_eq!(c.frame_samples(), 120, "48kHz 2.5ms 应为 120 采样");
    assert_eq!(c.frame_duration_us(), 2_500, "2.5ms 应为 2500 微秒");

    let mut pacer = AudioPacer::new(c, 0).expect("2.5ms 配置应能创建 pacer");
    let (p0, d0) = pacer.next_frame();
    let (p1, _) = pacer.next_frame();
    let (p2, _) = pacer.next_frame();
    assert_eq!(d0, 2_500);
    assert_eq!(
        (p0, p1, p2),
        (0, 2_500, 5_000),
        "2.5ms 帧的 PTS 也必须严格等距"
    );

    // 比它更小的值必须拒绝
    let too_small = OpusConfig { frame_ms: 2.0, ..c };
    assert!(too_small.validate().is_err(), "2ms 不是合法帧长");
}

// ---------------------------------------------------------------------------
// 额外覆盖：帧结构、静音帧、缓冲参数校验、清空语义
// ---------------------------------------------------------------------------

#[test]
fn frame_end_pts_saturates() {
    let f = AudioFrame::new(1_000, 20_000, 0, vec![1, 2, 3]);
    assert_eq!(f.end_pts_us(), 21_000, "结束时刻应为 pts + duration");
    assert_eq!(f.len(), 3);

    let overflow = AudioFrame::new(u64::MAX - 10, 20_000, 1, Vec::new());
    assert_eq!(
        overflow.end_pts_us(),
        u64::MAX,
        "贴近上界时应饱和而不是回绕成 0"
    );
    assert!(overflow.is_empty(), "空负载应被识别");
}

#[test]
fn silent_frame_keeps_flag_through_buffer() {
    let mut jb = AudioJitterBuffer::new(cfg(), 40.0).expect("默认配置应能创建缓冲");
    let mut silent = AudioFrame::new(0, 20_000, 0, Vec::new());
    silent.silent = true;

    assert_eq!(jb.push(silent), PushOutcome::Ok, "DTX 静音帧也应入队");
    let out = jb.pop_due(0).expect("静音帧应可取出");
    assert!(out.silent, "静音标记必须原样保留");
    assert!(out.payload.is_empty(), "静音帧负载为空");
}

#[test]
fn jitter_buffer_rejects_invalid_params() {
    let bad_frame = OpusConfig {
        frame_ms: 15.0,
        ..cfg()
    };
    assert!(
        AudioJitterBuffer::new(bad_frame, 40.0).is_err(),
        "非法帧长不应能创建缓冲"
    );

    assert!(
        AudioJitterBuffer::new(cfg(), -1.0).is_err(),
        "负的目标深度应被拒绝"
    );
    assert!(
        AudioJitterBuffer::new(cfg(), f64::NAN).is_err(),
        "NaN 目标深度应被拒绝"
    );
    assert!(
        AudioJitterBuffer::new(cfg(), f64::INFINITY).is_err(),
        "无穷大目标深度应被拒绝"
    );
    assert!(
        AudioJitterBuffer::new(cfg(), 0.0).is_ok(),
        "0ms 目标深度是合法的极端值"
    );
}

#[test]
fn clear_keeps_drop_counters() {
    let mut jb = AudioJitterBuffer::new(cfg(), 40.0).expect("默认配置应能创建缓冲");
    jb.push(frame(0, 0));
    jb.push(frame(20_000, 1));
    jb.pop_due(60_000);
    jb.push(frame(0, 0));
    assert_eq!(jb.dropped_late(), 1);

    jb.clear();
    assert!(jb.is_empty(), "清空后队列应为空");
    assert_eq!(jb.dropped_late(), 1, "清空不应抹掉诊断计数");

    // 清空后播放时钟基准也一并丢掉，可以从任意时基重新确立
    jb.push(frame(0, 0));
    assert!(jb.pop_due(0).is_some(), "清空后应能重新开始播放");
}

#[test]
fn push_outcome_is_readable() {
    assert_eq!(PushOutcome::Ok.to_string(), "OK");
    assert_eq!(PushOutcome::Late.to_string(), "LATE");
    assert_eq!(PushOutcome::Overflow.to_string(), "OVERFLOW");
    assert_eq!(PushOutcome::Discontinuity.to_string(), "DISCONTINUITY");
}
