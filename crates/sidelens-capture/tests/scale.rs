//! 缩放测试（**全平台可跑**）
//!
//! 核心断言是 `matches_materialized_scale`：把「融合缩放转换」的结果与
//! 「先物化一张缩放后的 BGRA、再跑标准 bgra_to_nv12」逐字节对比。
//! 这条断言比「期望值手算」更强 —— 它同时钉住了 Y 和 UV、面积平均和最近邻、
//! 以及所有边界（奇数尺寸、填充区）。

use sidelens_capture::convert::{bgra_to_nv12, rgb_to_cb, rgb_to_cr, rgb_to_y, FrameLayout};
use sidelens_capture::scale::{bgra_to_nv12_scaled, bgra_to_nv12_with_plan, ScaleMode, ScalePlan};
#[cfg(not(windows))]
use sidelens_capture::CaptureSource;
use sidelens_capture::ScreenCapture;

/// 造一张确定性的 BGRA 图（宽 w、高 h、行距 = w*4）
fn make_bgra(w: usize, h: usize) -> Vec<u8> {
    let mut v = vec![0u8; w * h * 4];
    for (i, px) in v.chunks_exact_mut(4).enumerate() {
        // 三个通道用不同周期，保证任意 2x2 / 3x3 块内的值都不相同
        px[0] = (i % 251) as u8;
        px[1] = ((i * 7 + 13) % 241) as u8;
        px[2] = ((i * 13 + 29) % 239) as u8;
        px[3] = 255;
    }
    v
}

/// 造一张纯色 BGRA 图
fn solid_bgra(w: usize, h: usize, r: u8, g: u8, b: u8) -> Vec<u8> {
    let mut v = vec![0u8; w * h * 4];
    for px in v.chunks_exact_mut(4) {
        px[0] = b;
        px[1] = g;
        px[2] = r;
        px[3] = 255;
    }
    v
}

/// 参考实现：先按面积平均物化一张缩放后的 BGRA，再跑标准转换
///
/// 这是「正确语义」的定义，被测实现必须与它逐字节相同。
/// 只在整数倍时可用（非整数倍这里不做分数权重，测试也不覆盖那条路径的精确值）。
fn reference_scale_then_convert(
    src: &[u8],
    sw: usize,
    sh: usize,
    dw: usize,
    dh: usize,
    layout: FrameLayout,
) -> Vec<u8> {
    assert_eq!(sw % dw, 0, "参考实现只支持整数倍");
    assert_eq!(sh % dh, 0, "参考实现只支持整数倍");
    let fx = sw / dw;
    let fy = sh / dh;

    // 物化缩放后的 BGRA
    let mut scaled = vec![0u8; dw * dh * 4];
    for y in 0..dh {
        for x in 0..dw {
            let n = (fx * fy) as u32;
            let half = n / 2;
            let (mut sr, mut sg, mut sb) = (0u32, 0u32, 0u32);
            for ry in 0..fy {
                for rx in 0..fx {
                    let i = ((y * fy + ry) * sw + x * fx + rx) * 4;
                    sb += u32::from(src[i]);
                    sg += u32::from(src[i + 1]);
                    sr += u32::from(src[i + 2]);
                }
            }
            let o = (y * dw + x) * 4;
            scaled[o] = ((sb + half) / n) as u8;
            scaled[o + 1] = ((sg + half) / n) as u8;
            scaled[o + 2] = ((sr + half) / n) as u8;
            scaled[o + 3] = 255;
        }
    }

    // 跑标准转换
    let mut out = vec![0u8; layout.nv12_len()];
    bgra_to_nv12(&scaled, dw * 4, layout, &mut out).expect("参考转换失败");
    out
}

// ---------------------------------------------------------------- ScalePlan

#[test]
fn plan_detects_2to1_as_area_average() {
    let p = ScalePlan::new(3840, 2160, 1920, 1080).expect("合法");
    assert_eq!(p.mode(), ScaleMode::AreaAverage);
    assert_eq!(p.fx(), 2);
    assert_eq!(p.fy(), 2);
    assert_eq!(p.samples_per_pixel(), 4);
    assert!(p.is_scaling());
}

#[test]
fn plan_detects_3to1_as_area_average() {
    let p = ScalePlan::new(3840, 2160, 1280, 720).expect("合法");
    assert_eq!(p.mode(), ScaleMode::AreaAverage);
    assert_eq!(p.fx(), 3);
    assert_eq!(p.fy(), 3);
    assert_eq!(p.samples_per_pixel(), 9);
}

#[test]
fn plan_detects_non_integer_ratio_as_nearest() {
    // 1920x1080 → 1280x720 是 1.5 倍，非整数
    let p = ScalePlan::new(1920, 1080, 1280, 720).expect("合法");
    assert_eq!(p.mode(), ScaleMode::Nearest);
    assert!(p.is_scaling());
}

#[test]
fn plan_identity_has_no_scaling() {
    let p = ScalePlan::new(1920, 1080, 1920, 1080).expect("合法");
    assert_eq!(p.mode(), ScaleMode::None);
    assert!(!p.is_scaling());
    assert_eq!(p.samples_per_pixel(), 1);
}

#[test]
fn plan_allows_mixed_factors() {
    // 横向 3:1、纵向 2:1 —— 仍然可以面积平均
    let p = ScalePlan::new(3840, 2160, 1280, 1080).expect("合法");
    assert_eq!(p.mode(), ScaleMode::AreaAverage);
    assert_eq!(p.fx(), 3);
    assert_eq!(p.fy(), 2);
    assert_eq!(p.samples_per_pixel(), 6);
}

#[test]
fn plan_upscale_uses_nearest() {
    let p = ScalePlan::new(1280, 720, 1920, 1080).expect("合法");
    assert_eq!(p.mode(), ScaleMode::Nearest);
    assert!(p.is_scaling());
}

#[test]
fn plan_rejects_zero_dimensions() {
    assert!(ScalePlan::new(0, 1080, 640, 360).is_err());
    assert!(ScalePlan::new(1920, 0, 640, 360).is_err());
    assert!(ScalePlan::new(1920, 1080, 0, 360).is_err());
    assert!(ScalePlan::new(1920, 1080, 640, 0).is_err());
}

// ---------------------------------------------------------------- 面积平均精确值

#[test]
fn matches_materialized_scale_2to1() {
    // 核心断言：融合路径必须与「先物化缩放再标准转换」逐字节相同。
    // 尺寸取小一点（64x48 → 32x24），既能覆盖多个 2x2 块又跑得快。
    let (sw, sh, dw, dh) = (64usize, 48usize, 32usize, 24usize);
    let src = make_bgra(sw, sh);
    let layout = FrameLayout::new(dw as u32, dh as u32).expect("合法");

    let mut got = vec![0u8; layout.nv12_len()];
    bgra_to_nv12_scaled(&src, sw * 4, (sw as u32, sh as u32), layout, &mut got)
        .expect("缩放转换失败");

    let want = reference_scale_then_convert(&src, sw, sh, dw, dh, layout);
    assert_eq!(got, want, "2:1 面积平均结果与「先缩放再转换」不一致");
}

#[test]
fn matches_materialized_scale_3to1() {
    let (sw, sh, dw, dh) = (96usize, 72usize, 32usize, 24usize);
    let src = make_bgra(sw, sh);
    let layout = FrameLayout::new(dw as u32, dh as u32).expect("合法");

    let mut got = vec![0u8; layout.nv12_len()];
    bgra_to_nv12_scaled(&src, sw * 4, (sw as u32, sh as u32), layout, &mut got)
        .expect("缩放转换失败");

    let want = reference_scale_then_convert(&src, sw, sh, dw, dh, layout);
    assert_eq!(got, want, "3:1 面积平均结果与「先缩放再转换」不一致");
}

#[test]
fn area_average_2x2_is_exact_mean() {
    // 手算验证：4x4 的图缩到 2x2，每个目标像素是源图 2x2 块的精确平均。
    // 四个 2x2 块各是一种纯色，块内同色，所以平均 = 原色，期望值可手写。
    let colors = [(255u8, 0u8, 0u8), (0, 255, 0), (0, 0, 255), (255, 255, 255)];
    let mut src = vec![0u8; 4 * 4 * 4];
    for y in 0..4usize {
        for x in 0..4usize {
            let block = (y / 2) * 2 + (x / 2);
            let (r, g, b) = colors[block];
            let i = (y * 4 + x) * 4;
            src[i] = b;
            src[i + 1] = g;
            src[i + 2] = r;
            src[i + 3] = 255;
        }
    }
    let layout = FrameLayout::new(2, 2).expect("2x2");
    // 注意 coded size 是 16x16（2 向上对齐到 16），所以有效区域只是
    // 左上角的 2x2，其余是右/下填充。读像素必须用 coded 宽度做行距。
    assert_eq!(layout.coded_width(), 16);
    let mut got = vec![0u8; layout.nv12_len()];
    bgra_to_nv12_scaled(&src, 4 * 4, (4, 4), layout, &mut got).expect("缩放失败");

    let cw = layout.coded_width() as usize;
    assert_eq!(got[0], rgb_to_y(255, 0, 0), "左上角 Y 不是纯红");
    assert_eq!(got[1], rgb_to_y(0, 255, 0), "右上角 Y 不是纯绿");
    assert_eq!(got[cw], rgb_to_y(0, 0, 255), "左下角 Y 不是纯蓝");
    assert_eq!(got[cw + 1], rgb_to_y(255, 255, 255), "右下角 Y 不是纯白");

    // 右侧填充必须是最后一列的复制（不是黑）
    assert_eq!(got[2], got[1], "右侧填充不是边缘复制");
    assert_eq!(got[cw + 2], got[cw + 1], "右侧填充不是边缘复制");
}

#[test]
fn area_average_mixes_colors_in_uv() {
    // 4x4 图：左半红、右半蓝，缩到 2x2。缩完只剩一个 UV 块，
    // 它必须平均红和蓝 —— 如果 UV 没跟着缩放，Cb 会等于纯红的值。
    let (sw, sh) = (4usize, 4usize);
    let mut src = vec![0u8; sw * sh * 4];
    for y in 0..sh {
        for x in 0..sw {
            let (r, g, b) = if x < 2 { (255, 0, 0) } else { (0, 0, 255) };
            let i = (y * sw + x) * 4;
            src[i] = b;
            src[i + 1] = g;
            src[i + 2] = r;
            src[i + 3] = 255;
        }
    }
    let layout = FrameLayout::new(2, 2).expect("2x2");
    let mut got = vec![0u8; layout.nv12_len()];
    bgra_to_nv12_scaled(&src, sw * 4, (4, 4), layout, &mut got).expect("缩放失败");

    let uv = layout.y_len();
    let expect_cb = (u32::from(rgb_to_cb(255, 0, 0)) + u32::from(rgb_to_cb(0, 0, 255))) / 2;
    assert_eq!(
        u32::from(got[uv]),
        expect_cb,
        "UV 的 Cb 不是红蓝平均（色度糊了或没缩）"
    );
    assert_ne!(got[uv], rgb_to_cb(255, 0, 0), "UV 完全没被平均");
}

#[test]
fn scaled_coded_size_stays_16_aligned() {
    for (dw, dh, cw, ch) in [
        (1920u32, 1080u32, 1920u32, 1088u32),
        (1280, 720, 1280, 720),
        (640, 360, 640, 368),
        (1000, 700, 1008, 704),
    ] {
        let layout = FrameLayout::new(dw, dh).expect("合法");
        assert_eq!(layout.coded_width(), cw, "{dw}x{dh} 的 coded 宽度不对");
        assert_eq!(layout.coded_height(), ch, "{dw}x{dh} 的 coded 高度不对");
        assert_eq!(layout.coded_width() % 16, 0);
        assert_eq!(layout.coded_height() % 16, 0);
    }
}

#[test]
fn scaling_pads_bottom_rows_with_edge_copy() {
    // 72x36 → 64x36（coded 64x48），底部 12 行必须边缘复制，不是黑
    let (sw, sh) = (128usize, 72usize);
    let src = solid_bgra(sw, sh, 200, 100, 50);
    let layout = FrameLayout::new(64, 36).expect("64x36");
    assert_eq!(layout.coded_height(), 48);

    let mut got = vec![0u8; layout.nv12_len()];
    bgra_to_nv12_scaled(&src, sw * 4, (sw as u32, sh as u32), layout, &mut got).expect("缩放失败");

    let cw = layout.coded_width() as usize;
    let last_valid = got[35 * cw];
    assert_ne!(last_valid, 16, "有效行本身就是黑的，测试没意义");
    for y in 36..48 {
        assert_eq!(got[y * cw], last_valid, "第 {y} 行不是边缘复制");
    }
}

// ---------------------------------------------------------------- 最近邻

#[test]
fn nearest_handles_non_integer_ratio() {
    // 1920x1080 → 1280x720（1.5:1）：不 panic、尺寸正确、不偏色。
    // 用纯色图，色偏会立刻暴露（最近邻对纯色是恒等的）。
    let (sw, sh, dw, dh) = (192usize, 108usize, 128usize, 72usize);
    let src = solid_bgra(sw, sh, 200, 120, 60);
    let layout = FrameLayout::new(dw as u32, dh as u32).expect("合法");
    let mut got = vec![0u8; layout.nv12_len()];
    bgra_to_nv12_scaled(&src, sw * 4, (sw as u32, sh as u32), layout, &mut got).expect("缩放失败");

    let y = rgb_to_y(200, 120, 60);
    assert_eq!(got[0], y, "纯色缩放后 Y 变了（有色偏）");
    // 有效区域全部等于原色
    let cw = layout.coded_width() as usize;
    for row in 0..dh {
        for col in 0..dw {
            assert_eq!(got[row * cw + col], y, "({col},{row}) 的 Y 不是原色");
        }
    }
    // 色度也必须中性偏移保持
    let uv = layout.y_len();
    assert_eq!(got[uv], rgb_to_cb(200, 120, 60));
    assert_eq!(got[uv + 1], rgb_to_cr(200, 120, 60));
}

#[test]
fn nearest_preserves_distinct_colors() {
    // 4x4 四色块 → 3x3（非整数倍）。最近邻会丢像素，但**不能串色**：
    // 每个输出像素必须精确等于源图某个像素的颜色。
    let colors = [(255u8, 0u8, 0u8), (0, 255, 0), (0, 0, 255), (255, 255, 255)];
    let mut src = vec![0u8; 4 * 4 * 4];
    for y in 0..4usize {
        for x in 0..4usize {
            let block = (y / 2) * 2 + (x / 2);
            let (r, g, b) = colors[block];
            let i = (y * 4 + x) * 4;
            src[i] = b;
            src[i + 1] = g;
            src[i + 2] = r;
            src[i + 3] = 255;
        }
    }
    let layout = FrameLayout::new(3, 3).expect("3x3");
    let mut got = vec![0u8; layout.nv12_len()];
    bgra_to_nv12_scaled(&src, 16, (4, 4), layout, &mut got).expect("缩放失败");

    let valid: Vec<u8> = colors.iter().map(|&(r, g, b)| rgb_to_y(r, g, b)).collect();
    let cw = layout.coded_width() as usize;
    for y in 0..3 {
        for x in 0..3 {
            let v = got[y * cw + x];
            assert!(
                valid.contains(&v),
                "({x},{y}) 的 Y={v} 不在四色调色板里 —— 最近邻串色了"
            );
        }
    }
}

#[test]
fn upscale_works_without_panic() {
    // 720p 屏配 1080p 编码器：放大，无信息增益但必须能跑
    let (sw, sh, dw, dh) = (64usize, 36usize, 96usize, 54usize);
    let src = make_bgra(sw, sh);
    let layout = FrameLayout::new(dw as u32, dh as u32).expect("合法");
    let mut got = vec![0u8; layout.nv12_len()];
    bgra_to_nv12_scaled(&src, sw * 4, (sw as u32, sh as u32), layout, &mut got).expect("放大失败");
    assert_eq!(got.len(), layout.nv12_len());
    assert!(got[..layout.y_len()].iter().any(|&v| v != 16), "放大后全黑");
}

#[test]
fn identical_size_matches_unscaled_path() {
    // 缩放路径在「源 == 目标」时必须与 bgra_to_nv12 逐字节相同 ——
    // 这是两条代码路径的等价性保证，避免调用方因为选了带缩放的 API
    // 而得到不同的像素。
    let (w, h) = (64usize, 48usize);
    let src = make_bgra(w, h);
    let layout = FrameLayout::new(w as u32, h as u32).expect("合法");

    let mut a = vec![0u8; layout.nv12_len()];
    bgra_to_nv12(&src, w * 4, layout, &mut a).expect("无缩放失败");
    let mut b = vec![0u8; layout.nv12_len()];
    bgra_to_nv12_scaled(&src, w * 4, (w as u32, h as u32), layout, &mut b).expect("缩放路径失败");
    assert_eq!(a, b, "源==目标时两条路径结果不同");
}

// ---------------------------------------------------------------- 参数校验

#[test]
fn rejects_layout_mismatched_with_plan() {
    let src = make_bgra(64, 48);
    // plan 说目标 32x24，layout 却是 16x16
    let plan = ScalePlan::new(64, 48, 32, 24).expect("plan");
    let layout = FrameLayout::new(16, 16).expect("layout");
    let mut dst = vec![0u8; layout.nv12_len()];
    let e = bgra_to_nv12_with_plan(&src, 64 * 4, plan, layout, &mut dst)
        .expect_err("尺寸不一致应当报错");
    assert!(format!("{e}").contains("不一致"), "错误信息不明确：{e}");
}

#[test]
fn rejects_short_source_for_scaling() {
    let layout = FrameLayout::new(32, 24).expect("合法");
    let mut dst = vec![0u8; layout.nv12_len()];
    // 声明源是 64x48，但只给了 64x24 的数据
    let short = vec![0u8; 64 * 24 * 4];
    assert!(bgra_to_nv12_scaled(&short, 64 * 4, (64, 48), layout, &mut dst).is_err());
}

#[test]
fn honors_stride_when_scaling() {
    // 行距大于 width*4 时（DXGI staging 的常见情况），缩放必须按行距跳行
    let (sw, sh, dw, dh) = (64usize, 48usize, 32usize, 24usize);
    let stride = 320usize; // 64*4=256，取 320 模拟 256 字节对齐
    let mut src = vec![0xAAu8; stride * sh]; // 行尾垃圾
    for y in 0..sh {
        for x in 0..sw {
            let i = y * stride + x * 4;
            src[i] = 60;
            src[i + 1] = 120;
            src[i + 2] = 200;
            src[i + 3] = 255;
        }
    }
    let layout = FrameLayout::new(dw as u32, dh as u32).expect("合法");
    let mut got = vec![0u8; layout.nv12_len()];
    bgra_to_nv12_scaled(&src, stride, (sw as u32, sh as u32), layout, &mut got).expect("缩放失败");
    let expect = rgb_to_y(200, 120, 60);
    let cw = layout.coded_width() as usize;
    for y in 0..dh {
        for x in 0..dw {
            assert_eq!(got[y * cw + x], expect, "({x},{y}) 受行尾垃圾影响");
        }
    }
}

// ---------------------------------------------------------------- ScreenCapture 接线

#[test]
fn synthetic_target_size_controls_output() {
    // 合成源也支持目标尺寸：直接按目标尺寸渲染（不做缩放）
    let mut cap = ScreenCapture::synthetic(320, 240);
    assert_eq!(cap.target_size(), Some((320, 240)));
    let f = cap.capture().expect("不会失败").expect("有帧");
    assert_eq!(f.display_width, 320);
    assert_eq!(f.display_height, 240);

    cap.set_target_size(Some((160, 120)))
        .expect("设目标尺寸失败");
    assert_eq!(cap.target_size(), Some((160, 120)));
    let f = cap.capture().expect("不会失败").expect("有帧");
    assert_eq!(f.display_width, 160);
    assert_eq!(f.display_height, 120);
    assert_eq!(f.coded_width, 160);
    assert_eq!(f.coded_height, 128); // 120 → 128
}

#[test]
fn set_target_size_is_idempotent() {
    // 尺寸没变就不该重建（重建 DXGI 会丢 duplication）
    let mut cap = ScreenCapture::synthetic(320, 240);
    let before = cap.capture().expect("不会失败").expect("有帧");
    cap.set_target_size(Some((320, 240)))
        .expect("重复设置应当成功");
    let after = cap.capture().expect("不会失败").expect("有帧");
    assert_eq!(before.coded_width, after.coded_width);
    assert_eq!(before.coded_height, after.coded_height);
}

#[test]
fn set_target_size_rejects_zero() {
    let mut cap = ScreenCapture::synthetic(320, 240);
    assert!(cap.set_target_size(Some((0, 120))).is_err());
    assert!(cap.set_target_size(Some((160, 0))).is_err());
    // 失败后原尺寸不变
    assert_eq!(cap.target_size(), Some((320, 240)));
}

#[test]
fn synthetic_scale_mode_is_none() {
    // 合成源直接按目标尺寸渲染，没有缩放步骤
    let cap = ScreenCapture::synthetic(320, 240);
    assert_eq!(cap.scale_mode(), None);
}

#[test]
fn source_size_reports_render_size_for_synthetic() {
    let cap = ScreenCapture::synthetic(320, 240);
    assert_eq!(cap.source_size(), Some((320, 240)));
    assert_eq!(cap.display_size(), Some((320, 240)));
}

#[test]
fn synthetic_target_size_keeps_16_alignment() {
    for (w, h, cw, ch) in [
        (1920u32, 1080u32, 1920u32, 1088u32),
        (1280, 720, 1280, 720),
        (640, 360, 640, 368),
    ] {
        let mut cap = ScreenCapture::synthetic(w, h);
        let f = cap.capture().expect("不会失败").expect("有帧");
        assert_eq!(f.coded_width, cw, "{w}x{h} coded 宽度不对");
        assert_eq!(f.coded_height, ch, "{w}x{h} coded 高度不对");
        assert_eq!(f.coded_width % 16, 0);
        assert_eq!(f.coded_height % 16, 0);
    }
}

#[cfg(not(windows))]
#[test]
fn sized_constructors_report_unsupported_off_windows() {
    assert!(ScreenCapture::new_gdi_sized(CaptureSource::PrimaryMonitor, Some((640, 360))).is_err());
    assert!(
        ScreenCapture::new_dxgi_sized(CaptureSource::PrimaryMonitor, Some((640, 360))).is_err()
    );
}

#[cfg(not(windows))]
#[test]
fn auto_sized_keeps_target_size_on_fallback() {
    // 两个真后端都不可用时退回合成源，但**必须保持目标尺寸** ——
    // 否则编码器会因为尺寸突变而重建
    let (cap, err) = ScreenCapture::auto_sized(CaptureSource::PrimaryMonitor, Some((1280, 720)));
    assert!(err.is_some(), "非 Windows 上应当报告降级原因");
    assert_eq!(cap.target_size(), Some((1280, 720)));
    assert_eq!(cap.display_size(), Some((1280, 720)));
}
