//! 渲染层测试（Windows 专属）
//!
//! 只测 **headless** 模式 —— 它不建窗口，所以在 CI / 无交互桌面的
//! Session 0 里也能跑。真窗口的测试会卡在消息循环上，不适合放进 `cargo test`。
//!
//! # 这里到底验了什么
//!
//! 1. headless 模式能连喂 10 帧不 panic、帧数对得上
//! 2. `pump_events()` 在 headless 下恒为 `true`（没有窗口可关）
//! 3. 耗时统计真的在动（不是恒 0 的摆设）
//! 4. coded/display 不一致的帧也能喂进去（走的是和真窗口同一条转换路径）
//! 5. 非法尺寸返回 Err 而不是 panic

#![cfg(windows)]

use sidelens_render::{Nv12View, RenderError, Renderer, RendererMode};

/// 造一帧带内容的 NV12（渐变 Y + 中性色度）
fn make_frame(width: u32, height: u32) -> Vec<u8> {
    let w = width as usize;
    let h = height as usize;
    let mut buf = Vec::with_capacity(w * h * 3 / 2);
    for y in 0..h {
        for x in 0..w {
            buf.push((((x * 200) / w.max(1) + (y * 40) / h.max(1)) % 236 + 16) as u8);
        }
    }
    // 色度平面整体填中性灰
    buf.resize(w * h + w * h / 2, 128);
    buf
}

/// **核心**：headless 模式下 present 10 帧不 panic，帧数 / 耗时统计正确
#[test]
fn headless_presents_ten_frames_without_panic() {
    let (w, h) = (320u32, 240u32);
    let buf = make_frame(w, h);
    let view = Nv12View::new(&buf, (w, h), (w, h)).expect("构造视图");

    let mut r = Renderer::headless(w, h).expect("建 headless 渲染器");
    assert_eq!(r.mode(), RendererMode::Headless);
    assert_eq!(r.mode().as_str(), "headless");
    assert_eq!(r.size(), (w, h));
    assert_eq!(r.frames(), 0);

    for i in 0..10 {
        r.present(&view)
            .unwrap_or_else(|e| panic!("第 {i} 帧 present 失败：{e}"));
    }

    assert_eq!(r.frames(), 10, "10 次 present 必须记 10 帧");
    assert!(r.convert_ns() > 0, "转换耗时应大于 0");
    assert!(r.last_convert_ns() > 0, "最近一帧耗时应大于 0");
    assert_eq!(r.avg_convert_ns(), r.convert_ns() / 10);
    assert_eq!(r.blit_ns(), 0, "headless 不 blit，blit 耗时应恒为 0");
}

/// headless 的 pump_events 恒为 true
#[test]
fn headless_pump_events_never_signals_quit() {
    let mut r = Renderer::headless(16, 16).unwrap();
    for _ in 0..100 {
        assert!(r.pump_events(), "headless 没有窗口可关，必须恒为 true");
    }
}

/// coded 640x368 / display 640x360 的帧能喂进 headless
#[test]
fn headless_accepts_cropped_frame() {
    let buf = make_frame(640, 368);
    let view = Nv12View::new(&buf, (640, 368), (640, 360)).unwrap();
    assert!(view.needs_crop());

    let mut r = Renderer::headless(640, 360).unwrap();
    r.present(&view).expect("带裁剪的帧也要能 present");
    assert_eq!(r.frames(), 1);
}

/// 非法帧尺寸返回 Err，不是 panic
#[test]
fn headless_rejects_invalid_frame() {
    let buf = vec![16u8; 100];
    // 缓冲远不够 640x368
    let err = Nv12View::new(&buf, (640, 368), (640, 360));
    assert!(err.is_err(), "缓冲不足必须报错");
}

/// 零尺寸建渲染器返回 Err
#[test]
fn zero_size_renderer_is_err() {
    let e = Renderer::headless(0, 100).unwrap_err();
    assert!(matches!(e, RenderError::ZeroSize { .. }), "实际 {e:?}");
    let e2 = Renderer::headless(100, 0).unwrap_err();
    assert!(matches!(e2, RenderError::ZeroSize { .. }));
}

/// 尺寸变化（分辨率切换）时 headless 也能继续工作
#[test]
fn headless_survives_resolution_change() {
    let mut r = Renderer::headless(640, 360).unwrap();

    let small = make_frame(160, 120);
    let v1 = Nv12View::new(&small, (160, 120), (160, 120)).unwrap();
    r.present(&v1).unwrap();

    let big = make_frame(640, 360);
    let v2 = Nv12View::new(&big, (640, 360), (640, 360)).unwrap();
    r.present(&v2).unwrap();

    assert_eq!(r.frames(), 2);
}

/// RenderError 的 Display 可读
#[test]
fn render_error_display_is_readable() {
    let e = RenderError::ZeroSize {
        width: 0,
        height: 0,
    };
    assert!(!e.to_string().is_empty());
    let e2 = RenderError::WindowGone;
    assert!(!e2.to_string().is_empty());
    // Convert 变体要能把内层错误带出来
    let inner = Nv12View::new(&[0u8; 4], (0, 0), (0, 0)).unwrap_err();
    let e3 = RenderError::from(inner);
    assert!(e3.to_string().contains("0"), "应包含内层错误描述");
}
