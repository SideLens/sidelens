//! 合成源与 FrameLayout/Frame 的测试（**全平台可跑**）

use sidelens_capture::convert::{align16, Y_BLACK};
use sidelens_capture::synthetic::{render_pattern, SyntheticSource, SYNTH_BAR_STEP};
use sidelens_capture::{Backend, CaptureSource, Frame, FrameLayout, ScreenCapture};

// ---------------------------------------------------------------- 合成源

#[test]
fn synthetic_frame_has_exact_nv12_length() {
    for (w, h) in [
        (640u32, 360u32),
        (1280, 720),
        (1920, 1080),
        (16, 16),
        (17, 17),
    ] {
        let mut cap = ScreenCapture::synthetic(w, h);
        let f = cap
            .capture()
            .expect("合成源不会失败")
            .expect("合成源永远有帧");
        let l = FrameLayout::new(w, h).expect("合法尺寸");
        assert_eq!(f.coded_width, l.coded_width(), "{w}x{h} coded_width 不对");
        assert_eq!(
            f.coded_height,
            l.coded_height(),
            "{w}x{h} coded_height 不对"
        );
        assert_eq!(
            f.data.len(),
            l.nv12_len(),
            "{w}x{h} 数据长度不是 coded_width*coded_height*3/2"
        );
    }
}

#[test]
fn synthetic_frame_is_not_black() {
    let mut cap = ScreenCapture::synthetic(1920, 1080);
    let f = cap.capture().expect("合成源不会失败").expect("有帧");
    let stats = f.y_stats();
    assert!(!f.is_black(), "合成源产出了全黑帧");
    assert!(stats.max > stats.min, "Y 平面没有动态范围");
    // 渐变 + 白条：max 必须到 235（limited range 白）
    assert_eq!(stats.max, 235, "没有出现 limited-range 白");
    // 单帧的 min 取决于竖条位置（seq=0 时竖条压在 x=0，最暗处被盖住），
    // 所以动态范围用「max - min」判，而不是钉死 min。
    assert!(
        stats.max - stats.min > 150,
        "动态范围太窄：{}..{}",
        stats.min,
        stats.max
    );
    assert!(
        stats.mean > 60.0 && stats.mean < 200.0,
        "均值 {} 不合理",
        stats.mean
    );
}

#[test]
fn synthetic_gradient_reaches_limited_black_across_frames() {
    // 竖条每帧右移 7 像素，跑够 1920/7 ≈ 275 帧一定会有「竖条不压 x=0」的帧，
    // 那时 x=0 处的渐变值就是 16 + 0 - 3 → 钳到 16。
    let mut cap = ScreenCapture::synthetic(1920, 1080);
    let mut min_seen = u8::MAX;
    for _ in 0..40 {
        let f = cap.capture().expect("不会失败").expect("有帧");
        min_seen = min_seen.min(f.y_stats().min);
    }
    assert_eq!(
        min_seen, 16,
        "40 帧内没有出现 limited-range 黑（最小 {min_seen}）"
    );
}

#[test]
fn synthetic_uv_plane_is_neutral() {
    let mut cap = ScreenCapture::synthetic(320, 240);
    let f = cap.capture().expect("不会失败").expect("有帧");
    assert!(
        f.uv_plane().iter().all(|&v| v == 128),
        "合成源的 UV 不是恒 128"
    );
}

#[test]
fn synthetic_bar_moves_between_frames() {
    let mut cap = ScreenCapture::synthetic(640, 360);
    let f0 = cap.capture().expect("不会失败").expect("有帧");
    let f1 = cap.capture().expect("不会失败").expect("有帧");
    assert_ne!(f0.y_plane(), f1.y_plane(), "相邻两帧内容相同，竖条没有移动");
}

#[test]
fn synthetic_bar_step_is_coprime_with_common_widths() {
    // 竖条位置 = (seq * STEP) % width。如果 STEP 与 width 有公因子，
    // 竖条只会在少数几个位置循环，压测时码率会呈现虚假周期性。
    for w in [640usize, 1280, 1920, 2560, 3840] {
        let g = gcd(SYNTH_BAR_STEP, w);
        assert_eq!(g, 1, "STEP={SYNTH_BAR_STEP} 与宽度 {w} 有公因子 {g}");
    }
}

fn gcd(mut a: usize, mut b: usize) -> usize {
    while b != 0 {
        let t = b;
        b = a % b;
        a = t;
    }
    a
}

#[test]
fn synthetic_padding_replicates_edge() {
    // 640x360 → coded 640x368。第 360..367 行必须等于第 359 行。
    let l = FrameLayout::new(640, 360).expect("640x360");
    let mut dst = vec![0u8; l.nv12_len()];
    render_pattern(0, 640, 360, l.coded_width(), l.coded_height(), &mut dst);

    let cw = l.coded_width() as usize;
    let last_valid_row = &dst[359 * cw..360 * cw];
    for y in 360..l.coded_height() as usize {
        let row = &dst[y * cw..(y + 1) * cw];
        assert_eq!(row, last_valid_row, "第 {y} 行不是边缘复制");
    }
}

#[test]
fn synthetic_respects_display_size_when_padded() {
    let src = SyntheticSource::new(40, 20);
    assert_eq!(src.width(), 40);
    assert_eq!(src.height(), 20);
    let mut cap = ScreenCapture::synthetic(40, 20);
    let f = cap.capture().expect("不会失败").expect("有帧");
    assert_eq!(f.display_width, 40);
    assert_eq!(f.display_height, 20);
    assert_eq!(f.coded_width, 48);
    assert_eq!(f.coded_height, 32);
}

#[test]
fn synthetic_zero_size_is_clamped() {
    // 0 尺寸不该 panic，也不该返回错误 —— 构造期就夹到 1x1（对齐后 16x16）
    let mut cap = ScreenCapture::synthetic(0, 0);
    let f = cap.capture().expect("不会失败").expect("有帧");
    assert_eq!(f.display_width, 1);
    assert_eq!(f.display_height, 1);
    assert_eq!(f.coded_width, 16);
    assert_eq!(f.coded_height, 16);
    assert_eq!(f.data.len(), 16 * 16 * 3 / 2);
}

// ---------------------------------------------------------------- ScreenCapture

#[test]
fn synthetic_reports_backend_and_source() {
    let cap = ScreenCapture::synthetic(320, 240);
    assert_eq!(cap.backend(), Backend::Synthetic);
    assert_eq!(*cap.source(), CaptureSource::Synthetic);
    assert_eq!(cap.frames(), 0);
    assert_eq!(cap.frames_dropped(), 0);
    assert_eq!(cap.avg_capture_us(), 0.0);
}

#[test]
fn synthetic_counts_frames_and_time() {
    let mut cap = ScreenCapture::synthetic(320, 240);
    for _ in 0..10 {
        let _ = cap.capture().expect("不会失败");
    }
    assert_eq!(cap.frames(), 10);
    assert_eq!(cap.empty_polls(), 0);
    assert!(cap.avg_capture_us() > 0.0, "平均耗时应当大于 0");
    assert_eq!(cap.stats().frames, 10);
}

#[test]
fn captured_at_us_is_monotonic() {
    let mut cap = ScreenCapture::synthetic(64, 64);
    let mut prev = 0u64;
    for _ in 0..20 {
        let f = cap.capture().expect("不会失败").expect("有帧");
        assert!(
            f.captured_at_us >= prev,
            "时间戳倒退了：{} -> {}",
            prev,
            f.captured_at_us
        );
        prev = f.captured_at_us;
    }
}

#[test]
fn display_size_matches_construction() {
    let cap = ScreenCapture::synthetic(800, 600);
    assert_eq!(cap.display_size(), Some((800, 600)));
}

// ---------------------------------------------------------------- Frame

#[test]
fn frame_black_is_limited_range_black() {
    let l = FrameLayout::new(64, 64).expect("64x64");
    let f = Frame::black(l, 12345);
    assert!(f.is_black(), "Frame::black 产出的不是全黑");
    assert_eq!(f.captured_at_us, 12345);
    assert_eq!(f.y_plane().len(), l.y_len());
    assert_eq!(f.uv_plane().len(), l.y_len() / 2);
    assert!(f.uv_plane().iter().all(|&v| v == 128));
    assert_eq!(f.y_stats().min, Y_BLACK);
    assert_eq!(f.y_stats().max, Y_BLACK);
}

#[test]
fn frame_layout_roundtrips() {
    let l = FrameLayout::new(1920, 1080).expect("1920x1080");
    let f = Frame::black(l, 0);
    let back = f.layout();
    assert_eq!(back.coded_width(), l.coded_width());
    assert_eq!(back.coded_height(), l.coded_height());
    assert_eq!(back.display_width(), l.display_width());
    assert_eq!(back.display_height(), l.display_height());
}

#[test]
fn frame_debug_does_not_dump_pixels() {
    // Debug 实现必须只打印摘要。Vec<u8> 的默认 Debug 会把 3 MB 打出来，
    // 一条 dbg! 就能刷爆终端 —— 这里断言输出长度在合理范围内。
    let mut cap = ScreenCapture::synthetic(1920, 1080);
    let f = cap.capture().expect("不会失败").expect("有帧");
    let s = format!("{f:?}");
    assert!(s.len() < 400, "Debug 输出过长（{} 字节）：{s}", s.len());
    assert!(s.contains("1920"), "Debug 输出里没有宽度：{s}");
    assert!(s.contains("1088"), "Debug 输出里没有 coded 高度：{s}");
}

// ---------------------------------------------------------------- 类型

#[test]
fn source_display_is_readable() {
    assert_eq!(format!("{}", CaptureSource::PrimaryMonitor), "主显示器");
    assert_eq!(format!("{}", CaptureSource::Monitor(2)), "显示器 #2");
    assert_eq!(format!("{}", CaptureSource::Window(0x1234)), "窗口 0x1234");
    assert_eq!(format!("{}", CaptureSource::Synthetic), "合成源");
    assert!(CaptureSource::Synthetic.is_synthetic());
    assert!(!CaptureSource::PrimaryMonitor.is_synthetic());
}

#[test]
fn backend_names_are_stable() {
    // 日志解析脚本依赖这三个字符串，改名要同步改脚本
    assert_eq!(Backend::Gdi.as_str(), "GDI");
    assert_eq!(Backend::Dxgi.as_str(), "DXGI");
    assert_eq!(Backend::Synthetic.as_str(), "Synthetic");
}

#[test]
fn align16_agrees_with_frame_layout() {
    for (w, h) in [(1u32, 1u32), (16, 16), (17, 16), (1920, 1080), (1366, 768)] {
        let l = FrameLayout::new(w, h).expect("合法");
        assert_eq!(l.coded_width(), align16(w));
        assert_eq!(l.coded_height(), align16(h));
    }
}

// ---------------------------------------------------------------- 非 Windows 平台

#[cfg(not(windows))]
#[test]
fn platform_backends_report_unsupported() {
    // 非 Windows 上必须返回 UnsupportedPlatform 而不是 panic 或 panic!()
    let e = ScreenCapture::new_gdi(CaptureSource::PrimaryMonitor)
        .expect_err("非 Windows 上 GDI 应当不可用");
    assert!(
        format!("{e}").contains("GDI"),
        "错误信息里应当提到后端名：{e}"
    );
    let e = ScreenCapture::new_dxgi(CaptureSource::PrimaryMonitor)
        .expect_err("非 Windows 上 DXGI 应当不可用");
    assert!(
        format!("{e}").contains("DXGI"),
        "错误信息里应当提到后端名：{e}"
    );
}

#[cfg(not(windows))]
#[test]
fn auto_falls_back_to_synthetic() {
    let (cap, err) = ScreenCapture::auto(CaptureSource::PrimaryMonitor);
    assert!(err.is_some(), "非 Windows 上 auto 应当报告降级原因");
    assert_eq!(cap.backend(), Backend::Synthetic);
}
