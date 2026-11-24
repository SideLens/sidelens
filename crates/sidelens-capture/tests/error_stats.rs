//! 错误类型与统计的测试（**全平台可跑**）
//!
//! 错误信息的格式被日志解析脚本依赖，这里把每一条 Display 都钉住 ——
//! 改文案就是改接口。

use std::error::Error;

use sidelens_capture::convert::{FrameLayout, Y_BLACK};
use sidelens_capture::{CaptureError, CaptureSource, Frame, ScreenCapture};

#[test]
fn unsupported_platform_names_the_backend() {
    let e = CaptureError::UnsupportedPlatform { backend: "DXGI" };
    assert_eq!(e.stage(), "DXGI");
    assert_eq!(e.hresult(), None);
    let msg = format!("{e}");
    assert!(msg.contains("DXGI"), "缺少后端名：{msg}");
    assert!(msg.contains("当前平台"), "缺少平台说明：{msg}");
}

#[test]
fn unsupported_source_includes_reason() {
    let e = CaptureError::UnsupportedSource {
        backend: "DXGI",
        reason: "只能按显示器复制".into(),
    };
    assert_eq!(e.stage(), "DXGI");
    let msg = format!("{e}");
    assert!(msg.contains("DXGI"), "缺少后端名：{msg}");
    assert!(msg.contains("只能按显示器复制"), "缺少原因：{msg}");
}

#[test]
fn invalid_argument_keeps_the_message() {
    let e = CaptureError::InvalidArgument("显示器序号 3 越界".into());
    assert_eq!(e.stage(), "validate");
    assert!(format!("{e}").contains("显示器序号 3 越界"));
}

#[test]
fn win32_error_formats_hresult_as_hex() {
    // 报告里必须贴十六进制 —— 十进制码值没法直接和 MSDN 对照。
    // 用 DXGI_ERROR_ACCESS_LOST (0x887A0026) 这个真实值。
    let e = CaptureError::Win32 {
        stage: "AcquireNextFrame",
        hresult: Some(0x887A0026u32 as i32),
        message: "访问丢失".into(),
    };
    assert_eq!(e.hresult(), Some(0x887A0026u32 as i32));
    assert_eq!(e.hresult_hex().as_deref(), Some("0x887A0026"));
    let msg = format!("{e}");
    assert!(msg.contains("0x887A0026"), "Display 里没有十六进制：{msg}");
    assert!(
        msg.contains("AcquireNextFrame"),
        "Display 里没有阶段：{msg}"
    );
}

#[test]
fn win32_error_without_hresult_omits_the_code() {
    let e = CaptureError::new("GetDC", "拿不到设备上下文");
    assert_eq!(e.hresult(), None);
    assert_eq!(e.hresult_hex(), None);
    let msg = format!("{e}");
    assert!(
        !msg.contains("HRESULT"),
        "没有码值却打了 HRESULT 标签：{msg}"
    );
    assert!(msg.contains("拿不到设备上下文"));
}

#[test]
fn capture_error_implements_std_error() {
    // 必须能塞进 anyhow / Box<dyn Error>，否则调用方没法统一处理
    let e: Box<dyn Error> = Box::new(CaptureError::InvalidArgument("测试".into()));
    assert!(e.to_string().contains("测试"));
}

// ---------------------------------------------------------------- 统计

#[test]
fn stats_average_is_zero_before_any_frame() {
    let cap = ScreenCapture::synthetic(64, 64);
    let s = cap.stats();
    assert_eq!(s.frames, 0);
    assert_eq!(s.avg_us(), 0.0, "0 帧时平均耗时必须是 0，不能是 NaN");
    assert!(s.avg_us().is_finite());
}

#[test]
fn stats_track_empty_polls_separately_from_frames() {
    // 合成源永远有帧，所以 empty_polls 恒为 0。
    // 这个测试保证两个计数器不会互相污染（DXGI 的 Ok(None) 只能加 empty_polls）。
    let mut cap = ScreenCapture::synthetic(32, 32);
    for _ in 0..5 {
        assert!(cap.capture().expect("不会失败").is_some());
    }
    assert_eq!(cap.frames(), 5);
    assert_eq!(cap.empty_polls(), 0);
    assert_eq!(cap.frames_dropped(), 0);
}

// ---------------------------------------------------------------- 源

#[test]
fn window_source_keeps_the_handle_in_display() {
    // HWND 转 isize 之后要能原样打印回来，否则排查「采错了窗口」时没线索
    let h = 0x0000_0000_00AB_CDEFisize;
    let s = CaptureSource::Window(h);
    let msg = format!("{s}");
    assert!(msg.contains("ABCDEF"), "句柄没有以十六进制出现：{msg}");
}

#[test]
fn source_is_copy_and_send() {
    // 发送端要把 CaptureSource 传进工作线程，必须是 Send + Copy
    fn assert_send_copy<T: Send + Copy>() {}
    assert_send_copy::<CaptureSource>();
    let a = CaptureSource::Monitor(1);
    let b = a; // Copy，不是 move
    assert_eq!(a, b);
}

#[test]
fn frame_black_respects_padding() {
    // 1080p 的兜底帧必须是 1920x1088，且填充行也是黑（16），不是 0
    let l = FrameLayout::new(1920, 1080).expect("1920x1080");
    let f = Frame::black(l, 0);
    assert_eq!(f.coded_height, 1088);
    assert_eq!(f.data.len(), 1920 * 1088 * 3 / 2);
    let cw = 1920usize;
    for y in 1080..1088 {
        for x in [0usize, 100, 1919] {
            assert_eq!(f.data[y * cw + x], Y_BLACK, "填充行 ({x},{y}) 不是 16");
        }
    }
}

#[test]
fn frame_y_stats_on_black_frame() {
    let l = FrameLayout::new(16, 16).expect("16x16");
    let f = Frame::black(l, 0);
    let s = f.y_stats();
    assert_eq!(s.min, Y_BLACK);
    assert_eq!(s.max, Y_BLACK);
    assert!((s.mean - f64::from(Y_BLACK)).abs() < 1e-9);
    assert_eq!(s.samples, 16 * 16);
}
