//! headless sink 的行为测试（Windows 专属文件，但只测 headless 路径）
//!
//! 之所以放在 `cfg(windows)` 下：本 crate 的 `AudioSink` 在非 Windows 上
//! 也编译，但这些断言里的缓冲容量算法与真实 WASAPI 的 20ms 语义对齐，
//! 在 Windows 上验证才有意义。

#![cfg(windows)]

use sidelens_audio_out::{AudioSink, PcmFormat, SinkMode};

#[test]
fn headless_mode_is_reported() {
    let sink = AudioSink::headless(PcmFormat::default());
    assert_eq!(sink.mode(), SinkMode::Headless);
    assert_eq!(sink.mode().as_str(), "headless");
    assert_eq!(sink.format(), PcmFormat::default());
}

#[test]
fn headless_write_100_frames_does_not_panic() {
    let mut sink = AudioSink::headless(PcmFormat::default());
    let frame = vec![0i16; 960]; // 20ms
    for _ in 0..100 {
        let n = sink.write(&frame).expect("headless 不该失败");
        assert!(n <= frame.len());
    }
    assert_eq!(sink.write_calls(), 100);
    // 写入 + 丢弃 = 提交的总量
    assert_eq!(
        sink.samples_written() + sink.samples_dropped(),
        100 * 960,
        "写入与丢弃之和必须等于总提交量（不允许凭空消失）"
    );
}

#[test]
fn headless_accounts_every_sample() {
    let mut sink = AudioSink::headless(PcmFormat::default());
    let mut submitted = 0u64;
    for i in 0..50u32 {
        // 帧长在 2.5ms - 60ms 之间变化（Opus 允许的范围）
        let n = match i % 3 {
            0 => 120,   // 2.5ms
            1 => 960,   // 20ms
            _ => 2_880, // 60ms
        };
        let pcm = vec![0i16; n];
        submitted += n as u64;
        let _ = sink.write(&pcm).unwrap();
    }
    assert_eq!(sink.samples_written() + sink.samples_dropped(), submitted);
    assert_eq!(sink.write_calls(), 50);
}

#[test]
fn headless_latency_is_sane() {
    let mut sink = AudioSink::headless(PcmFormat::default());
    // 未写入时延迟为 0
    assert_eq!(sink.latency_ms(), 0.0);

    let pcm = vec![0i16; 960];
    let _ = sink.write(&pcm).unwrap();
    let lat = sink.latency_ms();
    // 队列最多 20ms（模拟缓冲容量），所以延迟落在 [0, 20] 且有限
    assert!(lat.is_finite(), "latency 必须是有限值，实际 {lat}");
    assert!(
        (0.0..=20.0).contains(&lat),
        "headless 延迟应在 0-20ms，实际 {lat}"
    );
}

#[test]
fn headless_reports_stalls_when_buffer_full() {
    let mut sink = AudioSink::headless(PcmFormat::default());
    // 20ms 容量的缓冲；连写 3 个 20ms 帧，至少有一帧写不进去
    let frame = vec![0i16; 960];
    let mut wrote = 0;
    for _ in 0..3 {
        wrote += sink.write(&frame).unwrap();
    }
    assert!(wrote < 3 * 960, "缓冲满时应拒收，实际全收下了");
    assert!(sink.write_stalls() >= 1, "应有至少一次 Ok(0) 卡顿");
    assert!(sink.samples_dropped() > 0, "拒收的采样应计入丢弃");
}

#[test]
fn headless_timing_metrics_are_populated() {
    // # 为什么断言的是「有调用」而不是「耗时 > 0」
    //
    // 第一版断言 `write_ns() > 0`，单独跑 8/8 过，全量并行跑偶发失败。
    // 根因：headless 的 `write` 只做计数，路径短到 `Instant::elapsed()`
    // 真能测出 **0 纳秒**（QPC 分辨率约 100ns，一次计数远低于它）。
    //
    // 「测出 0」是**合法结果**，不是缺陷。断言它严格大于 0 等于断言
    // 「机器一定够慢」，这是错的 —— 机器越快越容易失败。
    //
    // 正确的断言：
    // 1. 调用次数确实累加了（这是计数的职责）
    // 2. 耗时字段可读且非负（0 是合法值）
    // 3. `avg = total / calls` 的关系成立（这是唯一有意义的等式）
    let mut sink = AudioSink::headless(PcmFormat::default());
    let pcm = vec![0i16; 960];
    for _ in 0..10 {
        let _ = sink.write(&pcm).unwrap();
    }
    assert!(sink.write_calls() > 0, "调用次数应累加，实际为 0");
    assert!(sink.samples_written() > 0, "写入采样数应累加");
    // 0 是合法值：headless 路径可能快到测不出时间。
    // `avg_write_ns()` 返回 u128（纳秒），恒 >= 0，所以只需断言它可读
    // 且与「总量 / 次数」的关系成立。
    let calls = sink.write_calls() as u128;
    assert!(calls > 0, "调用次数必须 > 0 才能算平均");
    let avg = sink.avg_write_ns();
    let expect = sink.write_ns() / calls;
    assert_eq!(avg, expect, "平均耗时应等于 总量/次数");
}

#[test]
fn headless_played_ms_matches_samples() {
    let mut sink = AudioSink::headless(PcmFormat::new(48_000, 1));
    let pcm = vec![0i16; 960];
    let _ = sink.write(&pcm).unwrap();
    let played = sink.played_ms();
    assert!(
        (0.0..=20.0).contains(&played),
        "played_ms 应与写入量一致，实际 {played}"
    );
}

#[test]
fn empty_write_is_a_noop() {
    let mut sink = AudioSink::headless(PcmFormat::default());
    assert_eq!(sink.write(&[]).unwrap(), 0);
    assert_eq!(sink.samples_written(), 0);
    assert_eq!(sink.samples_dropped(), 0);
}

#[test]
fn real_device_open_result_is_reported_not_panicked() {
    // 有设备就成功、没设备就 Err —— 两种都不允许 panic。
    // 这条测试的意义是：CI 容器里没有声卡时，降级路径必须走得通。
    match AudioSink::new(PcmFormat::default()) {
        Ok(sink) => {
            assert_eq!(sink.mode(), SinkMode::Device);
            println!(
                "真实设备：{:?}，缓冲区 {} 帧，流延迟 {:.2}ms",
                sink.device_format(),
                sink.latency_ms(),
                0.0
            );
        }
        Err(e) => {
            println!("没有可用音频设备，降级 headless：{e}");
            let sink = AudioSink::headless(PcmFormat::default());
            assert_eq!(sink.mode(), SinkMode::Headless);
        }
    }
}
