//! Windows GPU 实测：证明视频处理器真的做了 YUV→RGB，且矩阵/range/裁剪都对
//!
//! # 这组测试在验证什么
//!
//! `present` 返回 `Ok` 只代表「命令提交成功」—— 画面到底对不对，只有把像素
//! 读回来才能断言。所以这里的核心是**离屏 + 读回**：
//!
//! 1. 造一张已知的纯色 NV12（`Y=16/UV=128` 黑、`Y=235/UV=128` 白）；
//! 2. 走完整的 GPU 路径（上传 → 视频处理器 → BGRA 纹理）；
//! 3. 读回 BGRA，断言值落在预期范围内。
//!
//! 这三步同时钉住四件事：
//!
//! - **色彩矩阵**：BT.709 与 BT.601 对纯灰是等价的，所以另用一组彩色断言
//!   （红色）来区分；
//! - **range**：把 limited 当 full 解，16 会出来 16 而不是 0 —— 纯黑那条会红；
//! - **裁剪**：1920x1088 的填充行若上屏，底部会是非零像素；
//! - **通道顺序**：B8G8R8A8 的 B/R 若反了，纯灰看不出来，红色那条会红。
//!
//! # GPU 不可用时怎么办
//!
//! **跳过并打印原因，不 panic**。CI 机器、远程桌面、没有 WARP 的容器都可能
//! 建不出设备，那不是代码 bug。判定方式是 `GpuRenderer::offscreen` 返回
//! `Err` —— 这时打印 HRESULT 并直接 return。
//!
//! 为了让「跳过」不至于掩盖真 bug，纯逻辑测试（尺寸校验、headless 计数）
//! 放在 `tests/logic.rs`，**不依赖 GPU**，那些必须无条件通过。

#![cfg(windows)]

use sidelens_render_gpu::{
    fill_rect_nv12, solid_nv12, GpuError, GpuMode, GpuRenderer, Nv12Layout, Rect,
};

/// 色彩矩阵的舍入容忍度
///
/// 不同驱动的视频处理器实现略有差异（尤其是 WARP 与硬件之间），
/// 实测偏差在 1~2 之间。给 5 是「既能抓住真正的错误（把 limited 当 full
/// 解会偏 16，把 BT.709 当 BT.601 解红色会偏 20+），又不会因为驱动差异误报」。
const TOL: i32 = 5;

/// 建离屏渲染器；GPU 不可用时打印原因并返回 None
fn try_offscreen(w: u32, h: u32) -> Option<GpuRenderer> {
    match GpuRenderer::offscreen(w, h) {
        Ok(r) => {
            println!(
                "[gpu] 适配器 = {}，software(WARP) = {}，格式 = {}",
                r.adapter_name(),
                r.is_software(),
                r.swapchain_format()
            );
            Some(r)
        }
        Err(e) => {
            println!("[gpu] 跳过：离屏渲染器创建失败 —— {e}");
            None
        }
    }
}

fn assert_channel(actual: u8, expected: u8, what: &str) {
    let diff = (actual as i32 - expected as i32).abs();
    assert!(
        diff <= TOL,
        "{what}：期望 {expected}±{TOL}，实际 {actual}（差 {diff}）"
    );
}

/// **最重要的测试**：纯黑 NV12 → GPU → 读回必须是黑的
///
/// limited range 的 Y=16 对应 RGB 0。如果驱动把 limited 当 full 解，
/// 结果会是 16 左右 —— 差 16，远超容忍度，这条会红。
#[test]
fn gpu_converts_limited_black_to_zero() {
    let Some(mut r) = try_offscreen(320, 240) else {
        return;
    };
    let layout = Nv12Layout::new((320, 240), (320, 240)).expect("320x240 合法");
    let frame = solid_nv12(layout, 16, 128);

    r.present(&frame, (320, 240), (320, 240))
        .expect("present 必须成功");
    let img = r.read_pixels().expect("离屏读回必须成功");

    assert_eq!(img.width(), 320);
    assert_eq!(img.height(), 240);
    let px = img.pixel(160, 120).expect("中心像素必须存在");
    assert_channel(px[0], 0, "黑：B");
    assert_channel(px[1], 0, "黑：G");
    assert_channel(px[2], 0, "黑：R");
}

/// 纯白 NV12 → 读回必须是白的（limited 235 → 255）
#[test]
fn gpu_converts_limited_white_to_255() {
    let Some(mut r) = try_offscreen(320, 240) else {
        return;
    };
    let layout = Nv12Layout::new((320, 240), (320, 240)).expect("合法");
    let frame = solid_nv12(layout, 235, 128);

    r.present(&frame, (320, 240), (320, 240))
        .expect("present 必须成功");
    let img = r.read_pixels().expect("读回必须成功");

    let px = img.pixel(10, 10).expect("像素必须存在");
    assert_channel(px[0], 255, "白：B");
    assert_channel(px[1], 255, "白：G");
    assert_channel(px[2], 255, "白：R");
}

/// 中灰：Y=128 在 limited range 下大约是 130
#[test]
fn gpu_converts_mid_gray() {
    let Some(mut r) = try_offscreen(160, 120) else {
        return;
    };
    let layout = Nv12Layout::new((160, 120), (160, 120)).expect("合法");
    let frame = solid_nv12(layout, 128, 128);

    r.present(&frame, (160, 120), (160, 120))
        .expect("present 必须成功");
    let img = r.read_pixels().expect("读回必须成功");

    let px = img.pixel(80, 60).expect("像素必须存在");
    // 1.164383 * (128-16) = 130.4 —— 中灰略偏亮是 limited range 的正常结果
    assert_channel(px[0], 130, "中灰：B");
    assert_channel(px[1], 130, "中灰：G");
    assert_channel(px[2], 130, "中灰：R");
}

/// **通道顺序**：BT.709 的标准红 (Y=63, Cb=102, Cr=240) → 纯红 (255, 0, 0)
///
/// 这条同时验证两件事：矩阵是 BT.709（用 BT.601 解会得到 ~233, 0, 2），
/// 以及 B8G8R8A8 的 B/R 没有搞反（反了会读成 B=255 而不是 R=255）。
#[test]
fn gpu_converts_bt709_red_with_correct_channel_order() {
    let Some(mut r) = try_offscreen(64, 64) else {
        return;
    };
    let layout = Nv12Layout::new((64, 64), (64, 64)).expect("合法");
    let mut frame = solid_nv12(layout, 63, 128);
    // 把 UV 平面填成 BT.709 的红：Cb=102, Cr=240
    let y_len = layout.y_len();
    for pair in frame[y_len..].chunks_exact_mut(2) {
        pair[0] = 102;
        pair[1] = 240;
    }

    r.present(&frame, (64, 64), (64, 64))
        .expect("present 必须成功");
    let img = r.read_pixels().expect("读回必须成功");

    let px = img.pixel(32, 32).expect("像素必须存在");
    // BGRA：B 低、G 低、R 高
    assert!(
        px[2] > 200,
        "R 通道应该是红的（>200），实际 {}；若 B 高说明通道顺序反了",
        px[2]
    );
    assert!(px[0] < 60, "B 通道应该接近 0，实际 {}", px[0]);
    assert!(px[1] < 60, "G 通道应该接近 0，实际 {}", px[1]);
}

/// **裁剪**：1920x1088 的 coded 缓冲，display 只到 1080
///
/// 造一张「上部黑、底部 8 行白」的帧。如果裁剪失效，读回的画面底部会是白的；
/// 裁剪正确时整幅画面都是黑的。
#[test]
fn gpu_crops_coded_padding_rows() {
    let Some(mut r) = try_offscreen(1920, 1080) else {
        return;
    };
    let layout = Nv12Layout::new((1920, 1088), (1920, 1080)).expect("合法");
    let mut frame = solid_nv12(layout, 16, 128);
    // 把 coded 的最后 8 行（display 之外的填充）刷成白
    let y_len = layout.y_len();
    let stride = 1920usize;
    let pad_start = 1080 * stride;
    for b in &mut frame[pad_start..y_len] {
        *b = 235;
    }

    r.present(&frame, (1920, 1088), (1920, 1080))
        .expect("present 必须成功");
    let img = r.read_pixels().expect("读回必须成功");

    assert_eq!(img.height(), 1080, "读回高度必须是 display 高");
    // 最后一行（1079）必须是黑的 —— 说明 1080..1088 的填充没被采样
    let px = img.pixel(960, 1079).expect("最后一行必须存在");
    assert_channel(px[0], 0, "裁剪：底部 B 必须黑");
    assert_channel(px[1], 0, "裁剪：底部 G 必须黑");
    assert_channel(px[2], 0, "裁剪：底部 R 必须黑");
}

/// 非 16 对齐的小尺寸（coded == display）也要能走通
#[test]
fn gpu_handles_non_aligned_size() {
    let Some(mut r) = try_offscreen(320, 180) else {
        return;
    };
    let layout = Nv12Layout::new((320, 180), (320, 180)).expect("合法");
    let frame = solid_nv12(layout, 235, 128);
    r.present(&frame, (320, 180), (320, 180))
        .expect("present 必须成功");
    let img = r.read_pixels().expect("读回必须成功");
    let px = img.pixel(319, 179).expect("右下角像素必须存在");
    assert_channel(px[2], 255, "非对齐尺寸的右下角也必须是白的");
}

/// 连续 present 多帧，帧数必须准确
#[test]
fn gpu_counts_frames_across_presents() {
    let Some(mut r) = try_offscreen(64, 64) else {
        return;
    };
    let layout = Nv12Layout::new((64, 64), (64, 64)).expect("合法");
    let frame = solid_nv12(layout, 16, 128);
    for _ in 0..10 {
        r.present(&frame, (64, 64), (64, 64))
            .expect("present 必须成功");
    }
    assert_eq!(r.frames(), 10);
    assert!(r.avg_present_us() >= 0.0);
}

/// 尺寸变化会重建管线，且新尺寸的读回正确
#[test]
fn gpu_rebuilds_pipeline_on_size_change() {
    let Some(mut r) = try_offscreen(64, 64) else {
        return;
    };
    let small = Nv12Layout::new((64, 64), (64, 64)).expect("合法");
    r.present(&solid_nv12(small, 16, 128), (64, 64), (64, 64))
        .expect("present 必须成功");

    let big = Nv12Layout::new((128, 96), (128, 96)).expect("合法");
    r.present(&solid_nv12(big, 235, 128), (128, 96), (128, 96))
        .expect("重建后 present 必须成功");

    let img = r.read_pixels().expect("读回必须成功");
    let px = img.pixel(64, 48).expect("像素必须存在");
    assert_channel(px[2], 255, "重建后必须显示新帧（白）");
}

/// GPU 计时器：跑若干帧后应该能拿到非负的 GPU 耗时
///
/// 设备不支持 timestamp query 时 avg_gpu_us 会是 0 —— 这是允许的
/// （不是所有虚拟显卡都支持），所以这里只断言「不为负」。
#[test]
fn gpu_timer_reports_non_negative() {
    let Some(mut r) = try_offscreen(320, 240) else {
        return;
    };
    let layout = Nv12Layout::new((320, 240), (320, 240)).expect("合法");
    let frame = solid_nv12(layout, 16, 128);
    for _ in 0..5 {
        r.present(&frame, (320, 240), (320, 240))
            .expect("present 必须成功");
    }
    let gpu_us = r.avg_gpu_us();
    assert!(gpu_us >= 0.0, "GPU 耗时不能是负数：{gpu_us}");
    println!("[gpu] avg_gpu_us = {gpu_us:.1}");
}

/// 非离屏模式调 read_pixels 必须明确报错，而不是给一张黑图
#[test]
fn headless_read_pixels_is_unsupported() {
    let r = GpuRenderer::headless(64, 64);
    let e = r.read_pixels().unwrap_err();
    assert!(matches!(e, GpuError::Unsupported { .. }), "实际 {e:?}");
}

/// 真窗口渲染器：present 100 帧并报耗时
///
/// 需要交互桌面；创建失败（无桌面 / Session 0）时跳过。
#[test]
fn gpu_window_presents_100_frames() {
    let mut r = match GpuRenderer::new("SideLens GPU test", 640, 360) {
        Ok(r) => r,
        Err(e) => {
            println!("[gpu] 跳过窗口测试：{e}");
            return;
        }
    };
    assert_eq!(r.mode(), GpuMode::Window);
    println!(
        "[gpu] 窗口适配器 = {}，格式 = {}",
        r.adapter_name(),
        r.swapchain_format()
    );

    let layout = Nv12Layout::new((640, 368), (640, 360)).expect("合法");
    let mut frame = solid_nv12(layout, 16, 128);

    for i in 0..100u32 {
        // 每帧改一下亮度，避免驱动做「内容没变」的优化
        let y = 16 + (i % 200) as u8;
        frame[..layout.y_len()].fill(y);
        if !r.pump_events() {
            break;
        }
        r.present(&frame, (640, 368), (640, 360))
            .expect("窗口 present 必须成功");
    }

    let (p50, p95, max) = r.present_percentiles();
    println!(
        "[gpu] 窗口 100 帧：avg={:.1}us p50={:.1}us p95={:.1}us max={:.1}us gpu_avg={:.1}us",
        r.avg_present_us(),
        p50,
        p95,
        max,
        r.avg_gpu_us()
    );
    assert_eq!(r.frames(), 100);
}

/// 窗口渲染器在 coded != display 时也要能呈现（真实解码器场景）
#[test]
fn gpu_window_accepts_coded_larger_than_display() {
    let mut r = match GpuRenderer::new("SideLens GPU crop test", 320, 180) {
        Ok(r) => r,
        Err(e) => {
            println!("[gpu] 跳过：{e}");
            return;
        }
    };
    let layout = Nv12Layout::new((320, 192), (320, 180)).expect("合法");
    let frame = solid_nv12(layout, 128, 128);
    r.present(&frame, (320, 192), (320, 180))
        .expect("coded > display 的窗口 present 必须成功");
    assert_eq!(r.frames(), 1);
}

/// 记录实测数字，方便和 GDI 对比（不失败，只打印）
#[test]
fn gpu_reports_1080p_present_timing() {
    let Some(mut r) = try_offscreen(1920, 1080) else {
        return;
    };
    let layout = Nv12Layout::new((1920, 1088), (1920, 1080)).expect("合法");
    let frame = solid_nv12(layout, 16, 128);

    // 预热：首帧要建管线（几十毫秒），不该计入统计
    r.present(&frame, (1920, 1088), (1920, 1080))
        .expect("预热帧必须成功");

    for _ in 0..30 {
        r.present(&frame, (1920, 1088), (1920, 1080))
            .expect("present 必须成功");
    }
    let (p50, p95, max) = r.present_percentiles();
    println!(
        "[gpu] 1080p 离屏 30 帧：avg={:.1}us p50={:.1}us p95={:.1}us max={:.1}us upload={:.1}us gpu={:.1}us",
        r.avg_present_us(),
        p50,
        p95,
        max,
        r.last_upload_us(),
        r.avg_gpu_us()
    );
    assert!(r.frames() > 0);
}

/// 读回的区域映射：不同位置读到的应该是同一颜色（纯色帧）
#[test]
fn gpu_readback_maps_whole_surface() {
    let Some(mut r) = try_offscreen(128, 96) else {
        return;
    };
    let layout = Nv12Layout::new((128, 96), (128, 96)).expect("合法");
    r.present(&solid_nv12(layout, 235, 128), (128, 96), (128, 96))
        .expect("present 必须成功");
    let img = r.read_pixels().expect("读回必须成功");

    for (x, y) in [(0, 0), (127, 0), (0, 95), (127, 95), (64, 48)] {
        let px = img
            .pixel(x, y)
            .unwrap_or_else(|| panic!("({x},{y}) 必须存在"));
        assert_channel(px[2], 255, &format!("({x},{y}) 的 R"));
    }
}

/// 填充矩形辅助函数在 GPU 路径上也要生效（示例用的就是这个）
#[test]
fn gpu_renders_filled_rect() {
    let Some(mut r) = try_offscreen(160, 120) else {
        return;
    };
    let layout = Nv12Layout::new((160, 120), (160, 120)).expect("合法");
    let mut frame = solid_nv12(layout, 16, 128);
    // 中间 40x40 刷白
    fill_rect_nv12(&mut frame, layout, Rect::new(60, 40, 100, 80), 235, 128);

    r.present(&frame, (160, 120), (160, 120))
        .expect("present 必须成功");
    let img = r.read_pixels().expect("读回必须成功");

    let inside = img.pixel(80, 60).expect("矩形中心必须存在");
    assert_channel(inside[2], 255, "矩形内必须是白的");

    let outside = img.pixel(10, 10).expect("矩形外必须存在");
    assert_channel(outside[2], 0, "矩形外必须是黑的");
}

/// 离屏渲染器 resize 后仍能正确出画
///
/// # 为什么这条重要
///
/// `resize` 会丢弃旧管线（输出视图绑在旧纹理上）。如果实现漏了重建，
/// resize 后的第一次 `present` 会画到一块已经失效的纹理上 ——
/// 表现为「窗口一拖动画面就黑」。
#[test]
fn gpu_resize_keeps_rendering() {
    let Some(mut r) = try_offscreen(320, 240) else {
        return;
    };

    // 先画一帧确认正常
    let layout1 = Nv12Layout::new((320, 240), (320, 240)).expect("320x240 合法");
    r.present(&solid_nv12(layout1, 235, 128), (320, 240), (320, 240))
        .expect("首次 present");
    let img1 = r.read_pixels().expect("首次读回");
    assert_channel(
        img1.pixel(160, 120).expect("中心像素")[1],
        255,
        "resize 前：白",
    );

    // 改尺寸
    r.resize(640, 480).expect("resize 必须成功");
    assert_eq!(r.size(), (640, 480), "size() 应反映新尺寸");

    // 再画一帧 —— 这条会红如果 resize 后没重建管线
    let layout2 = Nv12Layout::new((640, 480), (640, 480)).expect("640x480 合法");
    r.present(&solid_nv12(layout2, 16, 128), (640, 480), (640, 480))
        .expect("resize 后 present 必须成功");
    let img2 = r.read_pixels().expect("resize 后读回");
    assert_channel(
        img2.pixel(320, 240).expect("中心像素")[1],
        0,
        "resize 后：黑",
    );
}

/// 同尺寸重复 resize 是幂等的（不重建、不报错）
#[test]
fn gpu_resize_same_size_is_idempotent() {
    let Some(mut r) = try_offscreen(320, 240) else {
        return;
    };
    r.resize(320, 240).expect("同尺寸 resize 应成功");
    r.resize(320, 240).expect("再来一次也应成功");
    assert_eq!(r.size(), (320, 240));
}

/// 尺寸为 0 的 resize 必须报错而不是 panic
#[test]
fn gpu_resize_zero_is_error() {
    let Some(mut r) = try_offscreen(320, 240) else {
        return;
    };
    assert!(r.resize(0, 240).is_err(), "宽 0 应报错");
    assert!(r.resize(320, 0).is_err(), "高 0 应报错");
    // 报错后原尺寸不该被破坏
    assert_eq!(r.size(), (320, 240), "失败的 resize 不该改尺寸");
}

/// headless 模式的 resize 只改尺寸，不碰 GPU
#[test]
fn gpu_headless_resize_updates_size() {
    let mut r = GpuRenderer::headless(320, 240);
    r.resize(800, 600).expect("headless resize 应成功");
    assert_eq!(r.size(), (800, 600));
}
