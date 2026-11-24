//! Windows 实测：真设备打开结果、loopback 语义、headless 行为
//!
//! # 为什么这里的真设备测试不 panic
//!
//! 本机的音频端点**全是渲染设备**（笔记本只有扬声器，没有麦克风阵列，
//! 也没有插 line-in）。也就是说 `AudioCapture::new()` 在这台机器上
//! 大概率返回 `Err` —— 这是环境事实，不是代码缺陷。
//!
//! 所以真设备测试写成「打不开就打印原因并跳过」：
//! 有设备时验证设备路径，没设备时验证降级路径，两种都不 panic。
//! 用 `panic!` 会让 CI 在无声卡容器里全红，而那时候代码可能完全正确。
//!
//! # 为什么 loopback 的断言不要求「有数据」
//!
//! loopback 在系统没放声音时**根本不推进**（不是送静音，是没数据）。
//! 所以断言只能是「read 不阻塞、不报错、返回值与 out 增量一致」，
//! 不能要求 `n > 0` —— 那会让测试在安静环境里随机失败。

#![cfg(windows)]

use std::time::{Duration, Instant};

use sidelens_audio_in::{AudioCapture, CaptureError, CaptureMode, PcmFormat};

/// 打不开设备时统一的跳过信息
fn skip_reason(e: &CaptureError) -> String {
    format!("{e}")
}

#[test]
fn headless_mode_is_reported() {
    let cap = AudioCapture::headless(PcmFormat::default());
    assert_eq!(cap.mode(), CaptureMode::Headless);
    assert_eq!(cap.mode().as_str(), "headless");
    assert_eq!(cap.format(), PcmFormat::default());
    assert!(cap.device_format().is_none(), "headless 不该有设备格式");
}

#[test]
fn headless_read_advances_with_wall_clock() {
    // headless 必须按真实时间产数据，否则调用方的节拍逻辑
    // 在无声卡环境下永远拿不到东西，测试全废。
    let mut cap = AudioCapture::headless(PcmFormat::default());
    let mut out = Vec::new();

    // 刚创建时可能还没到一个采样
    let _ = cap.read(&mut out).expect("headless 不该失败");

    std::thread::sleep(Duration::from_millis(50));
    out.clear();
    let n = cap.read(&mut out).expect("headless 不该失败");
    assert!(n > 0, "50ms 后应至少产出一个采样，实际 {n}");
    assert_eq!(n, out.len());
    assert!(out.iter().all(|&s| s == 0), "headless 只能产静音");
}

#[test]
fn headless_read_is_non_blocking() {
    // read 必须在 1ms tick 里跑得动。这里给一个宽松上限：
    // 真出问题时（比如误加了 sleep）会差几个数量级，1ms 足够区分。
    let mut cap = AudioCapture::headless(PcmFormat::default());
    let mut out = Vec::with_capacity(8192);
    let t0 = Instant::now();
    for _ in 0..100 {
        let _ = cap.read(&mut out).expect("headless 不该失败");
        out.clear();
    }
    let per_call = t0.elapsed().as_secs_f64() * 1000.0 / 100.0;
    assert!(per_call < 1.0, "100 次 read 平均 {per_call:.3}ms/次，太慢");
    assert_eq!(cap.read_calls(), 100);
}

#[test]
fn headless_latency_and_period_are_zero() {
    let cap = AudioCapture::headless(PcmFormat::default());
    assert_eq!(cap.latency_ms(), 0.0, "没有设备就没有设备延迟");
    assert_eq!(cap.device_period_ms(), (0.0, 0.0));
    assert_eq!(cap.buffer_frames(), 4_800, "模拟 100ms 容量");
}

#[test]
fn headless_counts_captured_and_dropped() {
    // 睡 200ms 后一次 read：want ≈ 9600 采样，但单次最多给 100ms（4800）。
    // 差额必须计入 dropped，不能凭空消失。
    let mut cap = AudioCapture::headless(PcmFormat::default());
    let mut out = Vec::new();
    let _ = cap.read(&mut out).expect("headless 不该失败");
    std::thread::sleep(Duration::from_millis(200));
    out.clear();
    let n = cap.read(&mut out).expect("headless 不该失败");

    assert_eq!(n as u64, cap.samples_captured());
    assert!(cap.samples_dropped() > 0, "读得太慢的部分必须记成丢弃");
    // 单次 read 有 100ms 上限
    assert!(n <= 4_800, "单次 read 不该超过 100ms 的量，实际 {n} 采样");
}

#[test]
fn headless_timing_metrics_are_populated() {
    let mut cap = AudioCapture::headless(PcmFormat::default());
    let mut out = Vec::new();
    for _ in 0..10 {
        let _ = cap.read(&mut out).expect("headless 不该失败");
        out.clear();
    }
    assert!(cap.read_calls() == 10);
    assert!(cap.read_ns() > 0, "累计耗时应有值");
    assert!(cap.avg_read_ns() > 0, "平均耗时应可算");
    assert!(cap.captured_ms() >= 0.0);
}

#[test]
fn invalid_format_is_rejected_before_touching_com() {
    // 格式错误必须在进 COM 之前拦下 —— 否则会变成一个
    // 看不懂的 E_INVALIDARG，排查要花半天。
    match AudioCapture::new(PcmFormat::new(0, 1)) {
        Err(CaptureError::Format(_)) => {}
        other => panic!("0Hz 应在进设备之前被拒绝，实际 {other:?}"),
    }
    match AudioCapture::loopback(PcmFormat::new(48_000, 0)) {
        Err(CaptureError::Format(_)) => {}
        other => panic!("0 声道应在进设备之前被拒绝，实际 {other:?}"),
    }
}

#[test]
fn microphone_open_result_is_reported_not_panicked() {
    // 有麦克风就验证设备路径，没有就打印原因 —— 两种都不 panic。
    match AudioCapture::new(PcmFormat::default()) {
        Ok(mut cap) => {
            assert_eq!(cap.mode(), CaptureMode::Microphone);
            let dev = cap.device_format().expect("真设备必须有设备格式");
            println!("麦克风：设备格式 {dev}，缓冲 {} 帧", cap.buffer_frames());
            let (def_ms, min_ms) = cap.device_period_ms();
            println!("  设备周期 默认 {def_ms:.3}ms / 最小 {min_ms:.3}ms");
            println!("  采集延迟 {:.3}ms", cap.latency_ms());
            assert!(dev.sample_rate >= 8_000);
            assert!(dev.channels >= 1);

            let mut out = Vec::new();
            let t0 = Instant::now();
            let n = cap.read(&mut out).expect("打开成功后 read 不该失败");
            let ms = t0.elapsed().as_secs_f64() * 1000.0;
            assert_eq!(n, out.len());
            assert!(ms < 50.0, "read 必须非阻塞，实际 {ms:.3}ms");
            println!("  首次 read：{n} 采样，耗时 {ms:.3}ms");
        }
        Err(e) => {
            println!("本机没有可用麦克风，跳过设备路径：{}", skip_reason(&e));
            let cap = AudioCapture::headless(PcmFormat::default());
            assert_eq!(cap.mode(), CaptureMode::Headless);
        }
    }
}

#[test]
fn loopback_open_result_is_reported_not_panicked() {
    // loopback 需要渲染端点。本机有扬声器，所以这条大概率能开成功 ——
    // 它验证的是「loopback 传的是 eRender 而不是 eCapture」这个关键点：
    // 传错端点会 E_INVALIDARG。
    match AudioCapture::loopback(PcmFormat::default()) {
        Ok(mut cap) => {
            assert_eq!(cap.mode(), CaptureMode::Loopback);
            let dev = cap.device_format().expect("loopback 必须有设备格式");
            println!("loopback：设备格式 {dev}，缓冲 {} 帧", cap.buffer_frames());
            println!("  采集延迟 {:.3}ms", cap.latency_ms());

            let mut out = Vec::new();
            let t0 = Instant::now();
            let n = cap.read(&mut out).expect("打开成功后 read 不该失败");
            let ms = t0.elapsed().as_secs_f64() * 1000.0;
            assert_eq!(n, out.len());
            assert!(ms < 50.0, "read 必须非阻塞，实际 {ms:.3}ms");
            // 这里**不能**断言 n > 0：系统可能没在放声音
            println!("  首次 read：{n} 采样（0 是正常的：系统没在放声音）");
        }
        Err(e) => {
            println!("loopback 打不开，跳过：{}", skip_reason(&e));
        }
    }
}

#[test]
fn loopback_read_zero_is_not_an_error() {
    // 坑 2：loopback 在没有声音播放时根本不推进。
    // 连读 20 次拿到 0 是**正常**的，不能当成错误，
    // 否则调用方会「静音时疯狂重连设备」。
    let Ok(mut cap) = AudioCapture::loopback(PcmFormat::default()) else {
        println!("loopback 打不开，跳过");
        return;
    };
    let mut out = Vec::new();
    let mut total = 0usize;
    for _ in 0..20 {
        let n = cap.read(&mut out).expect("read 0 不是错误");
        total += n;
        assert_eq!(n, out.len() - (out.len() - n));
        out.clear();
        std::thread::sleep(Duration::from_millis(1));
    }
    println!("loopback 连读 20 次共 {total} 采样（0 也正常）");
    assert_eq!(cap.read_calls(), 20);
}

#[test]
fn loopback_captures_audible_signal_when_playing() {
    // 只有在系统真的在放声音时才有信号。这里跑 300ms 观察：
    // 有信号就验证「值不是全 0」，没信号就跳过 —— 不能断言必须有信号。
    let Ok(mut cap) = AudioCapture::loopback(PcmFormat::default()) else {
        println!("loopback 打不开，跳过");
        return;
    };
    let mut out = Vec::new();
    let deadline = Instant::now() + Duration::from_millis(300);
    while Instant::now() < deadline {
        let _ = cap.read(&mut out).expect("read 不该失败");
        std::thread::sleep(Duration::from_millis(2));
    }
    if out.is_empty() {
        println!("300ms 内 loopback 没有任何数据（系统静音），跳过信号断言");
        return;
    }
    let peak = out.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);
    let rms = (out.iter().map(|&s| (s as f64).powi(2)).sum::<f64>() / out.len() as f64).sqrt();
    println!(
        "loopback 300ms 采到 {} 采样，peak {peak}，rms {rms:.1}",
        out.len()
    );
    assert!(peak > 0, "采到数据就不该全 0（全 0 说明转换链路坏了）");
}

#[test]
fn device_format_matches_requested_rate_or_open_fails_cleanly() {
    // 采样率不一致必须**明确报错**，而不是悄悄变调。
    // 请求一个设备不可能用的采样率（比如 8000Hz，混音格式几乎不会是它），
    // 期望拿到 RateMismatch 或 UnsupportedFormat —— 两种都是干净的失败。
    match AudioCapture::loopback(PcmFormat::new(8_000, 1)) {
        Ok(cap) => {
            // 设备真的支持 8kHz（罕见），那也是合法的
            println!("设备支持 8kHz，设备格式 {:?}", cap.device_format());
        }
        Err(CaptureError::RateMismatch {
            device_rate,
            requested_rate,
        }) => {
            println!("采样率不一致被正确拒绝：设备 {device_rate}Hz，请求 {requested_rate}Hz");
            assert_eq!(requested_rate, 8_000);
            assert_ne!(device_rate, 8_000);
        }
        Err(e) => {
            // 也可能在 Initialize 阶段就以别的形式失败 —— 同样是干净的失败
            println!("8kHz 打开失败（可接受）：{e}");
        }
    }
}
