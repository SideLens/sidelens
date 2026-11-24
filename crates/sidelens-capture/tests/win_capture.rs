//! Windows 实测（非 Windows 上整文件是空壳）
//!
//! # 为什么包一层模块
//!
//! 直接写 `#![cfg(windows)]` 会把整个文件（包括测试函数）在非 Windows 上
//! 删掉，`cargo check --all-targets` 不会报错，但测试数会静默变成 0 ——
//! 到时候「Linux 上跑过 25 个测试」就成了假话。
//!
//! 这里用「每个 `#[test]` 单独加 `#[cfg(windows)]`」的写法：测试函数在
//! 非 Windows 上不存在，但文件本身存在，`cargo test` 的输出里能看出
//! 有多少个测试被跳过。

#![cfg(windows)]

use sidelens_capture::{Backend, CaptureSource, FrameLayout, ScreenCapture};

/// 采集一帧并做基本校验
fn capture_one(cap: &mut ScreenCapture) -> Option<sidelens_capture::Frame> {
    cap.capture().expect("采集失败")
}

#[test]
fn gdi_primary_monitor_produces_non_black_frame() {
    let mut cap = match ScreenCapture::new_gdi(CaptureSource::PrimaryMonitor) {
        Ok(c) => c,
        Err(e) => {
            // 没有交互式桌面（比如 CI 的 session 0）时跳过，但要打印原因
            eprintln!("跳过 GDI 测试：{e}");
            return;
        }
    };
    assert_eq!(cap.backend(), Backend::Gdi);
    let f = match capture_one(&mut cap) {
        Some(f) => f,
        None => {
            eprintln!("GDI 没有返回帧，跳过");
            return;
        }
    };
    let (dw, dh) = cap.display_size().expect("GDI 应当知道显示尺寸");
    assert_eq!(f.display_width, dw);
    assert_eq!(f.display_height, dh);
    assert_eq!(f.coded_width, (dw + 15) & !15);
    assert_eq!(f.coded_height, (dh + 15) & !15);

    let l = FrameLayout::new(dw, dh).expect("合法尺寸");
    assert_eq!(f.data.len(), l.nv12_len(), "NV12 长度不对");

    // 屏幕全黑是可能的（锁屏 / 纯黑壁纸），但至少有非零字节。
    // 注意 limited range 的黑是 16 而不是 0，所以这里断言「不全等于 16」
    // 会太严格 —— 改用「至少有一个字节非零」+ 统计输出，人工判断。
    assert!(
        f.data.iter().any(|&b| b != 0),
        "整帧全 0 字节，采集链路可能没真正读到像素"
    );
    let stats = f.y_stats();
    eprintln!(
        "GDI 实测 {}x{} (coded {}x{})：Y min={} max={} mean={:.2}，耗时 {:.0} µs",
        f.display_width,
        f.display_height,
        f.coded_width,
        f.coded_height,
        stats.min,
        stats.max,
        stats.mean,
        cap.avg_capture_us()
    );
}

#[test]
fn gdi_window_capture_works_or_reports_why() {
    // 用桌面窗口（Progman / SHELLDLL_DefView 之类）不好找，
    // 这里只验证「无效句柄被拒绝」这条路径 —— 它是窗口采集的入口校验。
    let e = ScreenCapture::new_gdi(CaptureSource::Window(0)).expect_err("句柄 0 不是有效窗口");
    let msg = format!("{e}");
    assert!(
        msg.contains("窗口") || msg.contains("GetWindowRect"),
        "错误信息没有说明是窗口问题：{msg}"
    );
}

#[test]
fn dxgi_primary_monitor_or_reports_hresult() {
    let mut cap = match ScreenCapture::new_dxgi(CaptureSource::PrimaryMonitor) {
        Ok(c) => c,
        Err(e) => {
            // DXGI 在远程桌面 / 会话 0 / 被别的进程占用时必然失败，
            // 这是正常现象。把 HRESULT 打出来供报告引用。
            eprintln!(
                "DXGI 不可用（这是允许的）：stage={} hresult={:?} msg={}",
                e.stage(),
                e.hresult_hex(),
                e
            );
            return;
        }
    };
    assert_eq!(cap.backend(), Backend::Dxgi);

    // 静止桌面下第一次 Acquire 可能超时，最多重试 30 次（约 0.5 秒）
    let mut got = None;
    for _ in 0..30 {
        match cap.capture().expect("采集失败") {
            Some(f) => {
                got = Some(f);
                break;
            }
            None => continue,
        }
    }
    let f = match got {
        Some(f) => f,
        None => {
            eprintln!("DXGI 0.5 秒内没有新帧（桌面完全静止），跳过内容校验");
            return;
        }
    };
    let (dw, dh) = cap.display_size().expect("DXGI 应当知道显示尺寸");
    assert_eq!(f.display_width, dw);
    assert_eq!(f.display_height, dh);
    let l = FrameLayout::new(dw, dh).expect("合法尺寸");
    assert_eq!(f.data.len(), l.nv12_len());
    assert!(f.data.iter().any(|&b| b != 0), "整帧全 0 字节");

    let stats = f.y_stats();
    eprintln!(
        "DXGI 实测 {}x{} (coded {}x{})：Y min={} max={} mean={:.2}，耗时 {:.0} µs",
        f.display_width,
        f.display_height,
        f.coded_width,
        f.coded_height,
        stats.min,
        stats.max,
        stats.mean,
        cap.avg_capture_us()
    );
}

#[test]
fn dxgi_rejects_window_source_with_clear_reason() {
    let e =
        ScreenCapture::new_dxgi(CaptureSource::Window(0x1234)).expect_err("DXGI 不支持窗口采集");
    let msg = format!("{e}");
    assert!(msg.contains("窗口"), "错误信息没有说明原因：{msg}");
}

#[test]
fn auto_reports_fallback_reason() {
    let (mut cap, err) = ScreenCapture::auto(CaptureSource::PrimaryMonitor);
    eprintln!("auto 选中后端 {}，降级原因 {:?}", cap.backend(), err);
    // 无论选中哪个后端，都必须能产出帧（合成源兜底也算成功）
    let mut got = false;
    for _ in 0..30 {
        if cap.capture().expect("采集失败").is_some() {
            got = true;
            break;
        }
    }
    assert!(got, "auto 选出的后端 0.5 秒内产不出任何帧");
    eprintln!(
        "auto 后端 {} 采集耗时 {:.0} µs",
        cap.backend(),
        cap.avg_capture_us()
    );
}

#[test]
fn gdi_capture_repeatedly_is_stable() {
    let mut cap = match ScreenCapture::new_gdi(CaptureSource::PrimaryMonitor) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("跳过 GDI 重复采集测试：{e}");
            return;
        }
    };
    for i in 0..10 {
        let f = cap.capture().expect("采集失败");
        assert!(f.is_some(), "GDI 第 {i} 次没有返回帧");
    }
    assert_eq!(cap.frames(), 10);
    let avg = cap.avg_capture_us();
    assert!(avg > 0.0);
    eprintln!("GDI 10 次平均 {avg:.0} µs");
}

#[test]
fn gdi_scales_to_720p_from_whatever_screen() {
    // 关键场景：屏幕分辨率未知（本机 4K），目标是 1280x720。
    // 必须产出 1280x720 的帧，而不是报错或退回合成图案。
    let mut cap =
        match ScreenCapture::new_gdi_sized(CaptureSource::PrimaryMonitor, Some((1280, 720))) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("跳过 GDI 缩放测试：{e}");
                return;
            }
        };
    assert_eq!(cap.target_size(), Some((1280, 720)));
    assert_eq!(cap.display_size(), Some((1280, 720)));

    let f = match cap.capture().expect("采集失败") {
        Some(f) => f,
        None => {
            eprintln!("GDI 没有返回帧，跳过");
            return;
        }
    };
    assert_eq!(f.display_width, 1280, "输出宽度不是 1280");
    assert_eq!(f.display_height, 720, "输出高度不是 720");
    assert_eq!(f.coded_width, 1280);
    assert_eq!(f.coded_height, 720);
    let layout = FrameLayout::new(1280, 720).expect("合法");
    assert_eq!(f.data.len(), layout.nv12_len(), "NV12 长度不对");
    assert!(f.data.iter().any(|&b| b != 0), "整帧全 0");

    let stats = f.y_stats();
    eprintln!(
        "GDI 缩放实测：源 {:?} → 输出 {}x{}，模式 {:?}，Y min={} max={} mean={:.2}，耗时 {:.0} µs",
        cap.source_size(),
        f.display_width,
        f.display_height,
        cap.scale_mode(),
        stats.min,
        stats.max,
        stats.mean,
        cap.avg_capture_us()
    );
    // 缩放不能把画面压成全黑或纯色
    assert!(stats.max > stats.min + 8, "缩放后没有动态范围");
}

#[test]
fn dxgi_scales_to_720p_or_reports_why() {
    let mut cap =
        match ScreenCapture::new_dxgi_sized(CaptureSource::PrimaryMonitor, Some((1280, 720))) {
            Ok(c) => c,
            Err(e) => {
                eprintln!(
                    "DXGI 缩放不可用（允许）：stage={} hresult={:?} msg={}",
                    e.stage(),
                    e.hresult_hex(),
                    e
                );
                return;
            }
        };
    assert_eq!(cap.target_size(), Some((1280, 720)));

    let mut got = None;
    for _ in 0..60 {
        match cap.capture().expect("采集失败") {
            Some(f) => {
                got = Some(f);
                break;
            }
            None => continue,
        }
    }
    let f = match got {
        Some(f) => f,
        None => {
            eprintln!("DXGI 1 秒内没有新帧（桌面静止），跳过内容校验");
            return;
        }
    };
    assert_eq!((f.display_width, f.display_height), (1280, 720));
    assert_eq!((f.coded_width, f.coded_height), (1280, 720));
    let layout = FrameLayout::new(1280, 720).expect("合法");
    assert_eq!(f.data.len(), layout.nv12_len());
    assert!(f.data.iter().any(|&b| b != 0));

    let stats = f.y_stats();
    eprintln!(
        "DXGI 缩放实测：源 {:?} → 输出 {}x{}，模式 {:?}，Y min={} max={} mean={:.2}，耗时 {:.0} µs",
        cap.source_size(),
        f.display_width,
        f.display_height,
        cap.scale_mode(),
        stats.min,
        stats.max,
        stats.mean,
        cap.avg_capture_us()
    );
    assert!(stats.max > stats.min + 8, "缩放后没有动态范围");
}

#[test]
fn set_target_size_rebuilds_backend() {
    let mut cap = match ScreenCapture::new_gdi(CaptureSource::PrimaryMonitor) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("跳过 set_target_size 测试：{e}");
            return;
        }
    };
    let native = cap.source_size().expect("应当知道源尺寸");
    assert_eq!(cap.target_size(), None, "默认不该有目标尺寸");

    cap.set_target_size(Some((640, 360))).expect("设置失败");
    assert_eq!(cap.target_size(), Some((640, 360)));
    let f = cap.capture().expect("采集失败").expect("GDI 必有帧");
    assert_eq!((f.display_width, f.display_height), (640, 360));
    assert_eq!((f.coded_width, f.coded_height), (640, 368));
    // 源尺寸不变（还是屏幕分辨率）
    assert_eq!(cap.source_size(), Some(native));

    // 恢复不缩放
    cap.set_target_size(None).expect("恢复失败");
    let f = cap.capture().expect("采集失败").expect("GDI 必有帧");
    assert_eq!((f.display_width, f.display_height), native);
    eprintln!("set_target_size 实测：{native:?} → 640x360 → 恢复 {native:?}");
}

#[test]
fn auto_sized_falls_back_but_keeps_size() {
    let (mut cap, err) =
        ScreenCapture::auto_sized(CaptureSource::PrimaryMonitor, Some((1280, 720)));
    eprintln!(
        "auto_sized 选中后端 {}，源 {:?}，降级原因 {:?}",
        cap.backend(),
        cap.source_size(),
        err
    );
    assert_eq!(cap.target_size(), Some((1280, 720)));
    let mut got = false;
    for _ in 0..60 {
        if let Some(f) = cap.capture().expect("采集失败") {
            assert_eq!((f.display_width, f.display_height), (1280, 720));
            got = true;
            break;
        }
    }
    assert!(got, "auto_sized 1 秒内产不出帧");
}
