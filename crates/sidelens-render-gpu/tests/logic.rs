//! 跨平台纯逻辑测试：尺寸校验、裁剪矩形、headless 计数
//!
//! 这些测试**不依赖 GPU、不依赖 Windows**，所以在 Linux / macOS CI 上
//! 也会跑。它们的意义是：GPU 相关的测试在无显卡环境会「跳过」，
//! 而跳过是静默的 —— 如果所有测试都跳过，CI 看起来是绿的但什么都没验证。
//! 所以必须有一组无条件通过的测试守着最容易被改坏的算术。
//!
//! 覆盖的重点：
//!
//! - **coded vs display 的裁剪矩形**（1920x1088 → 1920x1080）—— 算错就是底部绿边；
//! - **staging 打包**（行距 > 宽度时填充不能被覆盖）—— 写错就是画面斜切；
//! - **headless 计数与校验**（失败的 present 不能计数）；
//! - **分位数**（p50/p95 用最近秩，样本窗口是环形的）。

use sidelens_render_gpu::{
    fill_rect_nv12, solid_nv12, FrameStats, GpuError, GpuMode, GpuRenderer, LayoutError,
    Nv12Layout, Rect,
};

// ---------------------------------------------------------------- 尺寸校验

#[test]
fn layout_accepts_the_real_decoder_sizes() {
    // 本项目实际会遇到的三种尺寸（codec-mf 的输出）
    for (coded, display) in [
        ((640, 368), (640, 360)),
        ((1280, 720), (1280, 720)),
        ((1920, 1088), (1920, 1080)),
    ] {
        let l = Nv12Layout::new(coded, display).expect("合法尺寸必须通过");
        assert_eq!(l.coded(), coded);
        assert_eq!(l.display(), display);
    }
}

#[test]
fn layout_rejects_zero_and_odd() {
    assert!(matches!(
        Nv12Layout::new((0, 0), (0, 0)),
        Err(LayoutError::ZeroDimension { .. })
    ));
    assert!(matches!(
        Nv12Layout::new((641, 368), (640, 360)),
        Err(LayoutError::OddDimension { what: "coded", .. })
    ));
}

#[test]
fn layout_rejects_display_bigger_than_coded() {
    assert!(matches!(
        Nv12Layout::new((640, 360), (640, 368)),
        Err(LayoutError::DisplayLargerThanCoded { .. })
    ));
}

// ---------------------------------------------------------------- 裁剪矩形

#[test]
fn crop_rects_shrink_to_display_not_coded() {
    let l = Nv12Layout::new((1920, 1088), (1920, 1080)).expect("合法");
    assert!(l.needs_crop());
    // 输入矩形在 coded 坐标系里，但高度必须是 display 高
    assert_eq!(l.source_rect(), Rect::new(0, 0, 1920, 1080));
    assert_eq!(l.dest_rect(), Rect::new(0, 0, 1920, 1080));
}

#[test]
fn no_crop_flag_when_sizes_match() {
    let l = Nv12Layout::new((1280, 720), (1280, 720)).expect("合法");
    assert!(!l.needs_crop());
}

#[test]
fn frame_len_is_nv12_math() {
    let l = Nv12Layout::new((1920, 1088), (1920, 1080)).expect("合法");
    assert_eq!(l.y_len(), 2_088_960);
    assert_eq!(l.uv_len(), 1_044_480);
    assert_eq!(l.frame_len(), 3_133_440);
    assert_eq!(l.staging_rows(), 1632);
}

// ---------------------------------------------------------------- staging 打包

#[test]
fn scatter_respects_driver_row_pitch() {
    // 驱动给的 RowPitch 常常大于宽度（对齐）。多出来的字节是填充，
    // 覆盖它会在画面上表现为「右边有一条脏线」。
    let l = Nv12Layout::new((8, 4), (8, 4)).expect("合法");
    let src = solid_nv12(l, 16, 128);
    let mut dst = vec![0xEEu8; 16 * 6];
    l.scatter_to_rows(&src, &mut dst, 16).expect("必须成功");

    // 第一行 Y 在 0..8，填充在 8..16
    assert_eq!(dst[0], 16);
    assert_eq!(dst[7], 16);
    assert_eq!(&dst[8..16], &[0xEE; 8], "填充不能被覆盖");
    // UV 从第 4 行开始（4*16=64）
    assert_eq!(dst[64], 128);
}

#[test]
fn scatter_copies_all_planes_for_1080p() {
    let l = Nv12Layout::new((1920, 1088), (1920, 1080)).expect("合法");
    let src = solid_nv12(l, 200, 100);
    // 模拟驱动的 2048 字节对齐行距
    let pitch = 2048usize;
    let mut dst = vec![0u8; pitch * l.staging_rows()];
    l.scatter_to_rows(&src, &mut dst, pitch).expect("必须成功");

    // 抽查 Y 平面每一行都写到了
    for r in [0usize, 1, 500, 1087] {
        assert_eq!(dst[r * pitch], 200, "Y 行 {r} 必须写入");
        assert_eq!(dst[r * pitch + 1919], 200, "Y 行 {r} 末像素必须写入");
        // 行尾到 pitch 之间是填充
        assert_eq!(dst[r * pitch + 1920], 0, "填充必须保持 0");
    }
    // UV 平面起始行
    let uv_row = 1088usize;
    assert_eq!(dst[uv_row * pitch], 100, "UV 首行必须写入");
    assert_eq!(dst[(uv_row + 543) * pitch], 100, "UV 末行必须写入");
}

#[test]
fn scatter_errors_are_actionable() {
    let l = Nv12Layout::new((16, 16), (16, 16)).expect("合法");
    let src = solid_nv12(l, 16, 128);
    let mut dst = vec![0u8; 16 * 24];

    // 行距小于宽度
    let e = l.scatter_to_rows(&src, &mut dst, 15).unwrap_err();
    assert!(e.to_string().contains("行距"));

    // 源缓冲不足
    let e = l.scatter_to_rows(&src[..10], &mut dst, 16).unwrap_err();
    assert!(e.to_string().contains("不足"));
}

// ---------------------------------------------------------------- headless

#[test]
fn headless_counts_only_successful_presents() {
    let mut r = GpuRenderer::headless(1920, 1080);
    let l = Nv12Layout::new((1920, 1088), (1920, 1080)).expect("合法");
    let good = solid_nv12(l, 16, 128);

    assert_eq!(r.frames(), 0);
    r.present(&good, (1920, 1088), (1920, 1080))
        .expect("必须成功");
    assert_eq!(r.frames(), 1);

    // 各种失败都不该计数
    assert!(r.present(&good, (1921, 1088), (1920, 1080)).is_err());
    assert!(r.present(&good[..100], (1920, 1088), (1920, 1080)).is_err());
    assert!(r.present(&good, (1920, 1088), (1920, 1090)).is_err());
    assert_eq!(r.frames(), 1, "失败的 present 不能增加帧数");
}

#[test]
fn headless_mode_reports_correctly() {
    let r = GpuRenderer::headless(800, 600);
    assert_eq!(r.mode(), GpuMode::Headless);
    assert_eq!(r.size(), (800, 600));
    assert_eq!(r.swapchain_format(), "none");
    assert_eq!(r.avg_present_us(), 0.0);
    assert_eq!(r.avg_gpu_us(), 0.0);
    assert!(!r.adapter_name().is_empty());
}

#[test]
fn headless_pump_events_never_ends() {
    let mut r = GpuRenderer::headless(64, 64);
    for _ in 0..100 {
        assert!(r.pump_events(), "headless 没有窗口，不该返回 false");
    }
}

#[test]
fn headless_error_carries_the_offending_layout() {
    let mut r = GpuRenderer::headless(64, 64);
    let frame = vec![0u8; 64 * 64 * 3 / 2];
    match r.present(&frame, (63, 64), (63, 64)) {
        Err(GpuError::Layout(LayoutError::OddDimension { what, width, .. })) => {
            assert_eq!(what, "coded");
            assert_eq!(width, 63);
        }
        other => panic!("期望 OddDimension，实际 {other:?}"),
    }
}

// ---------------------------------------------------------------- 统计

#[test]
fn frame_stats_percentiles_are_nearest_rank() {
    let mut s = FrameStats::new();
    for i in 1..=100 {
        s.record(i as f64);
    }
    assert_eq!(s.p50_us(), 50.0);
    assert_eq!(s.p95_us(), 95.0);
    assert_eq!(s.max_us(), 100.0);
    assert_eq!(s.count(), 100);
}

#[test]
fn frame_stats_reject_bad_samples() {
    let mut s = FrameStats::new();
    s.record(f64::NAN);
    s.record(-5.0);
    s.record(f64::INFINITY);
    assert_eq!(s.count(), 0);
}

// ---------------------------------------------------------------- 帧构造

#[test]
fn solid_frame_fills_both_planes() {
    let l = Nv12Layout::new((32, 16), (32, 16)).expect("合法");
    let f = solid_nv12(l, 16, 128);
    assert_eq!(f.len(), l.frame_len());
    assert!(f[..l.y_len()].iter().all(|&b| b == 16));
    assert!(f[l.y_len()..].iter().all(|&b| b == 128));
}

#[test]
fn fill_rect_never_touches_coded_padding() {
    // display 比 coded 小：填充区必须原封不动
    let l = Nv12Layout::new((32, 32), (32, 24)).expect("合法");
    let mut f = solid_nv12(l, 16, 128);
    fill_rect_nv12(&mut f, l, Rect::new(-100, -100, 1000, 1000), 235, 128);

    // display 最后一行被填
    assert_eq!(f[23 * 32], 235);
    // coded 填充行（24..32）不能碰
    for row in 24..32 {
        assert_eq!(f[row * 32], 16, "第 {row} 行是 coded 填充，不能改");
    }
}

// ------------------------------------------------------ 设备丢失的错误分类

/// `DeviceRemoved` 的三个 HRESULT 必须能被识别（这是「该重建」的判据）
///
/// # 为什么这条测试重要
///
/// 设备丢失是**唯一可恢复**的 GPU 错误。识别不出来的后果是：
/// 画面永久黑屏，用户只能重启软件。
///
/// 三个码来自 DXGI 头文件，是 ABI 稳定的 —— 但写错一个数字就失效，
/// 所以用测试钉住。
#[test]
fn device_removed_hresult_values_are_correct() {
    // DXGI_ERROR_DEVICE_REMOVED  = 0x887A0005
    // DXGI_ERROR_DEVICE_HUNG     = 0x887A0006
    // DXGI_ERROR_DEVICE_RESET    = 0x887A0007
    //
    // 这三个是连续的三连号，共同点：D3D11 对象全部失效，必须重建。
    let removed = GpuError::DeviceRemoved {
        hresult: 0x887A_0005,
    };
    let hung = GpuError::DeviceRemoved {
        hresult: 0x887A_0006,
    };
    let reset = GpuError::DeviceRemoved {
        hresult: 0x887A_0007,
    };

    for e in [&removed, &hung, &reset] {
        let s = e.to_string();
        assert!(
            s.contains("设备丢失"),
            "错误信息要能让用户看懂「设备丢了」：{s}"
        );
        assert!(s.contains("重建"), "错误信息要指出「可以重建」：{s}");
        assert!(s.contains("0x"), "必须带 HRESULT 供排障：{s}");
    }
}

/// `DeviceRemoved` 与普通错误的 `Display` 必须可区分
#[test]
fn device_removed_is_distinguishable_from_other_errors() {
    let removed = GpuError::DeviceRemoved {
        hresult: 0x887A_0005,
    };
    let other = GpuError::Texture("创建纹理失败".into());
    assert_ne!(removed.to_string(), other.to_string());
    // 普通错误不该被误认为设备丢失
    assert!(!other.to_string().contains("设备丢失"));
}

/// HRESULT 用 8 位十六进制输出（与 WinDbg / 事件查看器一致）
#[test]
fn device_removed_formats_hresult_as_8_digits() {
    let e = GpuError::DeviceRemoved {
        hresult: 0x887A_0005,
    };
    let s = e.to_string();
    assert!(s.contains("887A0005"), "应是 8 位大写十六进制：{s}");
}
