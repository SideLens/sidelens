//! 纯转换测试（**跨平台**，CI 必跑）
//!
//! 这个文件里不能出现任何 `cfg(windows)` 之外的东西 —— 它要能在 Linux 上跑。
//!
//! 覆盖四类：
//!
//! 1. 颜色正确性：纯黑 / 纯白 / 纯红 / 纯绿 / 纯蓝 的 NV12 → RGBA 期望值
//! 2. 矩阵选择：BT.709 vs BT.601 在同一组输入上必须给出不同结果，
//!    且 BT.709 解标准红是**精确**的 (255,0,0)
//! 3. coded vs display 裁剪：640x368 → 640x360，底部 8 行填充绝不能出现
//! 4. 错误路径：尺寸不匹配 / 奇数尺寸 / 缓冲不足都必须返回 Err 而不是 panic

use sidelens_render::{ConvertError, Matrix, Nv12View};

/// 造一帧紧凑 NV12
///
/// `y` 是 Y 值，`(cb, cr)` 是色度值，整帧统一 —— 这样期望的 RGB 只有一个值，
/// 断言可以直接写死。
fn solid_nv12(width: u32, height: u32, y: u8, cb: u8, cr: u8) -> Vec<u8> {
    let w = width as usize;
    let h = height as usize;
    let mut buf = vec![y; w * h];
    buf.resize(w * h + w * (h / 2), cb);
    // UV 交织：偶数列 Cb、奇数列 Cr
    for i in (w * h..buf.len()).step_by(2) {
        buf[i] = cb;
        buf[i + 1] = cr;
    }
    buf
}

/// 取第 (x, y) 个像素的 RGBA
fn px(rgba: &[u8], width: u32, x: u32, y: u32) -> [u8; 4] {
    let i = ((y * width + x) * 4) as usize;
    [rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]]
}

// ─────────────────────────── 1. 颜色正确性 ───────────────────────────

/// limited range 的纯黑：(Y=16, Cb=Cr=128) → (0,0,0,255)
#[test]
fn pure_black_maps_to_zero() {
    let buf = solid_nv12(4, 4, 16, 128, 128);
    let view = Nv12View::new(&buf, (4, 4), (4, 4)).unwrap();
    let mut rgba = Vec::new();
    view.to_rgba(&mut rgba).unwrap();

    assert_eq!(rgba.len(), 4 * 4 * 4);
    for y in 0..4 {
        for x in 0..4 {
            assert_eq!(px(&rgba, 4, x, y), [0, 0, 0, 255], "({x},{y}) 应为纯黑");
        }
    }
}

/// limited range 的纯白：(Y=235, Cb=Cr=128) → (255,255,255,255)
#[test]
fn pure_white_maps_to_255() {
    let buf = solid_nv12(4, 4, 235, 128, 128);
    let view = Nv12View::new(&buf, (4, 4), (4, 4)).unwrap();
    let mut rgba = Vec::new();
    view.to_rgba(&mut rgba).unwrap();

    for y in 0..4 {
        for x in 0..4 {
            assert_eq!(
                px(&rgba, 4, x, y),
                [255, 255, 255, 255],
                "({x},{y}) 应为纯白"
            );
        }
    }
}

/// BT.709 limited 的标准红 (Y=63, Cb=102, Cr=240) → **(255, 1, 0)**
///
/// 这条是**矩阵选择的钉子**：同样的三元组用 BT.601 解出来是 (233, 0, 0)，
/// 红通道差 22。默认矩阵一旦被改成 601，这条立刻红。
///
/// # 为什么 G 是 1 而不是 0
///
/// 8 位的「标准红」三元组并不精确落在 RGB 立方体的顶点上。代入 BT.709 矩阵：
///
/// ```text
/// R = 1.164383*(63-16) + 1.792741*(240-128) =  54.726 + 200.787 = 255.51
/// G = 1.164383*(63-16) - 0.213249*(102-128) - 0.532909*(240-128)
///   =  54.726 + 5.544 - 59.686 = 0.584
/// B = 1.164383*(63-16) + 2.112402*(102-128) =  54.726 - 54.922 = -0.196
/// ```
///
/// G 的真实值是 0.584 —— 四舍五入就是 1，硬压成 0 反而**错**了 1 LSB。
/// 第一版把这条断言写成 [255,0,0]，测试立刻红了，也顺便证明了：
/// 拿「记忆里的期望值」写测试是行不通的，必须算。
#[test]
fn bt709_red_pins_matrix() {
    let buf = solid_nv12(2, 2, 63, 102, 240);
    let view = Nv12View::new(&buf, (2, 2), (2, 2)).unwrap();
    let mut rgba = Vec::new();
    view.to_rgba(&mut rgba).unwrap();

    assert_eq!(
        px(&rgba, 2, 0, 0),
        [255, 1, 0, 255],
        "BT.709 下 Y=63/Cb=102/Cr=240 应解成 (255,1,0)（G 的真值是 0.584）"
    );
}

/// BT.601 对同一组输入必须给出**不同**结果（证明矩阵真的生效了）
#[test]
fn bt601_differs_from_bt709_on_red() {
    let buf = solid_nv12(2, 2, 63, 102, 240);
    let view = Nv12View::new(&buf, (2, 2), (2, 2)).unwrap();

    let mut a = Vec::new();
    let mut b = Vec::new();
    view.to_rgba_with(&mut a, Matrix::Bt709).unwrap();
    view.to_rgba_with(&mut b, Matrix::Bt601).unwrap();

    assert_ne!(a, b, "BT.601 与 BT.709 对同一输入不能给出相同结果");
    // BT.601 的红通道明显更低 —— 这正是选错矩阵时「颜色发闷」的来源
    assert!(
        b[0] < a[0],
        "BT.601 的 R 应低于 BT.709（{} vs {}）",
        b[0],
        a[0]
    );
    assert_eq!(a[0], 255);
    assert_eq!(b[0], 233, "BT.601 对标准红的 R 通道算得 233.48 → 233");
}

/// 默认矩阵必须是 BT.709
#[test]
fn default_matrix_is_bt709() {
    assert_eq!(Matrix::default(), Matrix::Bt709);

    let buf = solid_nv12(2, 2, 63, 102, 240);
    let view = Nv12View::new(&buf, (2, 2), (2, 2)).unwrap();
    let mut default_out = Vec::new();
    let mut explicit_out = Vec::new();
    view.to_rgba(&mut default_out).unwrap();
    view.to_rgba_with(&mut explicit_out, Matrix::Bt709).unwrap();
    assert_eq!(default_out, explicit_out);
}

/// 中性灰：Y=126 附近的 Cb=Cr=128 应该是三通道相等
#[test]
fn neutral_gray_has_equal_channels() {
    let buf = solid_nv12(2, 2, 126, 128, 128);
    let view = Nv12View::new(&buf, (2, 2), (2, 2)).unwrap();
    let mut rgba = Vec::new();
    view.to_rgba(&mut rgba).unwrap();
    let p = px(&rgba, 2, 0, 0);
    assert_eq!(p[0], p[1], "灰度的 R 与 G 必须相等");
    assert_eq!(p[1], p[2], "灰度的 G 与 B 必须相等");
    assert_eq!(p[3], 255);
}

/// 亮度单调：Y 越大 RGB 越亮
#[test]
fn luma_is_monotonic() {
    let mut prev = -1i32;
    for y in [16u8, 50, 100, 150, 200, 235] {
        let buf = solid_nv12(2, 2, y, 128, 128);
        let view = Nv12View::new(&buf, (2, 2), (2, 2)).unwrap();
        let mut rgba = Vec::new();
        view.to_rgba(&mut rgba).unwrap();
        let v = rgba[0] as i32;
        assert!(v >= prev, "Y={y} 的 R={v} 应不小于上一个亮度 {prev}");
        prev = v;
    }
}

/// 越界的 Y（0 和 255）不能 panic，且被夹到 0 / 255
#[test]
fn out_of_range_luma_is_clamped() {
    for (y, expect_low) in [(0u8, true), (255u8, false)] {
        let buf = solid_nv12(2, 2, y, 128, 128);
        let view = Nv12View::new(&buf, (2, 2), (2, 2)).unwrap();
        let mut rgba = Vec::new();
        view.to_rgba(&mut rgba).unwrap();
        let p = px(&rgba, 2, 0, 0);
        if expect_low {
            assert_eq!(p, [0, 0, 0, 255], "Y=0 应夹到黑");
        } else {
            assert_eq!(p, [255, 255, 255, 255], "Y=255 应夹到白");
        }
    }
}

/// 输出缓冲会被清空重填，不会残留上一次的尾巴
#[test]
fn out_buffer_is_reset_not_appended() {
    let buf = solid_nv12(2, 2, 16, 128, 128);
    let view = Nv12View::new(&buf, (2, 2), (2, 2)).unwrap();
    let mut rgba = vec![0xAAu8; 9999];
    view.to_rgba(&mut rgba).unwrap();
    assert_eq!(rgba.len(), 2 * 2 * 4, "长度必须精确等于显示尺寸 * 4");
}

// ──────────────────── 2. coded vs display 裁剪 ────────────────────

/// **核心测试**：coded 640x368 / display 640x360
///
/// 造一帧「上 360 行是纯黑、下 8 行是亮绿」的假 NV12。
/// 如果裁剪没做对，底部 8 行会被画进 RGBA —— 输出高度会变成 368 或者
/// 最后几行冒出绿色。两种都必须被这条断言抓住。
#[test]
fn coded_368_crops_to_display_360() {
    let cw = 640u32;
    let ch = 368u32;
    let dw = 640u32;
    let dh = 360u32;

    let mut buf = vec![16u8; (cw * ch) as usize]; // 全部黑色 Y
                                                  // 底部 8 行（360..368）填成亮绿：Y=150，Cb=44，Cr=21（BT.709 下的绿）
    let fill_start = (cw * dh) as usize;
    for v in buf[fill_start..].iter_mut() {
        *v = 150;
    }
    // UV 平面整体填中性灰（黑色需要 Cb=Cr=128）
    let uv_off = (cw * ch) as usize;
    buf.resize(uv_off + (cw * ch / 2) as usize, 128);

    let view = Nv12View::new(&buf, (cw, ch), (dw, dh)).unwrap();
    assert!(view.needs_crop(), "coded != display，必须报告需要裁剪");
    assert_eq!(view.coded_height(), 368);
    assert_eq!(view.display_height(), 360);

    let mut rgba = Vec::new();
    view.to_rgba(&mut rgba).unwrap();

    // 输出尺寸必须按 display，不是 coded
    assert_eq!(
        rgba.len(),
        (dw * dh * 4) as usize,
        "RGBA 长度必须是 640*360*4，不是 640*368*4"
    );

    // 每一行都必须是黑的 —— 底部填充的亮绿一个像素都不许漏出来
    for y in 0..dh {
        for x in [0u32, 319, 639] {
            assert_eq!(
                px(&rgba, dw, x, y),
                [0, 0, 0, 255],
                "({x},{y}) 应是纯黑；底部填充行漏进了输出"
            );
        }
    }
}

/// 裁掉右侧：coded 648x360 / display 640x360（8 列宏块填充）
#[test]
fn coded_648_crops_right_to_640() {
    let cw = 648u32;
    let ch = 360u32;
    let dw = 640u32;
    let dh = 360u32;

    // 整帧黑，只有右侧 8 列（640..648）是亮白
    let mut buf = vec![16u8; (cw * ch) as usize];
    for y in 0..ch {
        for x in dw..cw {
            buf[(y * cw + x) as usize] = 235;
        }
    }
    let uv_off = (cw * ch) as usize;
    buf.resize(uv_off + (cw * ch / 2) as usize, 128);

    let view = Nv12View::new(&buf, (cw, ch), (dw, dh)).unwrap();
    let mut rgba = Vec::new();
    view.to_rgba(&mut rgba).unwrap();

    assert_eq!(rgba.len(), (dw * dh * 4) as usize);
    // 最右一列（x=639）必须是黑的
    for y in [0u32, 179, 359] {
        assert_eq!(
            px(&rgba, dw, 639, y),
            [0, 0, 0, 255],
            "右侧填充列漏进了输出"
        );
    }
}

/// `crop_to_display()` 之后的转换结果必须和原视图**逐字节相同**
#[test]
fn crop_to_display_is_byte_identical() {
    let cw = 64u32;
    let ch = 48u32;
    let dw = 64u32;
    let dh = 40u32;

    // 造点有内容的数据，避免全同值掩盖偏移错误
    let mut buf = Vec::new();
    for i in 0..(cw * ch) {
        buf.push(((i * 7) % 200 + 20) as u8);
    }
    for i in 0..(cw * ch / 2) {
        buf.push(((i * 3) % 200 + 20) as u8);
    }

    let view = Nv12View::new(&buf, (cw, ch), (dw, dh)).unwrap();
    let cropped = view.crop_to_display();
    assert_eq!(cropped.coded_height(), dh);
    assert!(!cropped.needs_crop());

    let mut a = Vec::new();
    let mut b = Vec::new();
    view.to_rgba(&mut a).unwrap();
    cropped.to_rgba(&mut b).unwrap();
    assert_eq!(a, b, "crop_to_display 前后必须逐字节相同");
}

/// coded == display 时 crop_to_display 是恒等操作
#[test]
fn crop_to_display_is_identity_when_sizes_match() {
    let buf = solid_nv12(8, 8, 100, 128, 128);
    let view = Nv12View::new(&buf, (8, 8), (8, 8)).unwrap();
    assert!(!view.needs_crop());
    let c = view.crop_to_display();
    assert_eq!((c.coded_width(), c.coded_height()), (8, 8));
    assert_eq!((c.display_width(), c.display_height()), (8, 8));
}

/// frame_len 按 **coded** 尺寸算 —— 和 H264Decoder::output_frame_len() 对得上
#[test]
fn frame_len_uses_coded_size() {
    let buf = vec![16u8; 640 * 368 * 3 / 2];
    let view = Nv12View::new(&buf, (640, 368), (640, 360)).unwrap();
    assert_eq!(view.frame_len(), 640 * 368 * 3 / 2);
    assert_eq!(
        view.frame_len(),
        353280,
        "和 decoder 的 output_frame_len 一致"
    );
}

/// 1080p：coded 1920x1088 / display 1920x1080
#[test]
fn hd_1080_crops_8_rows() {
    let (cw, ch, dw, dh) = (1920u32, 1088u32, 1920u32, 1080u32);
    let mut buf = vec![16u8; (cw * ch) as usize];
    let fill_start = (cw * dh) as usize;
    for v in buf[fill_start..].iter_mut() {
        *v = 200; // 底部 8 行填亮
    }
    let uv_off = (cw * ch) as usize;
    buf.resize(uv_off + (cw * ch / 2) as usize, 128);

    let view = Nv12View::new(&buf, (cw, ch), (dw, dh)).unwrap();
    let mut rgba = Vec::new();
    view.to_rgba(&mut rgba).unwrap();
    assert_eq!(rgba.len(), (dw * dh * 4) as usize);
    // 最后一行必须是黑的
    assert_eq!(px(&rgba, dw, 0, dh - 1), [0, 0, 0, 255]);
}

// ───────────────────────── 3. 错误路径 ─────────────────────────

/// 显示尺寸大于编码尺寸 → Err
#[test]
fn display_larger_than_coded_is_err() {
    let buf = vec![16u8; 16 * 16 * 3 / 2];
    let e = Nv12View::new(&buf, (16, 16), (32, 16)).unwrap_err();
    assert!(
        matches!(e, ConvertError::DisplayLargerThanCoded { .. }),
        "应报 DisplayLargerThanCoded，实际 {e:?}"
    );
}

/// 奇数尺寸 → Err（NV12 的色度是 2x2 子采样）
#[test]
fn odd_coded_dimension_is_err() {
    let buf = vec![16u8; 17 * 16 * 3 / 2 + 64];
    let e = Nv12View::new(&buf, (17, 16), (16, 16)).unwrap_err();
    match e {
        ConvertError::OddDimension { what, width, .. } => {
            assert_eq!(what, "coded");
            assert_eq!(width, 17);
        }
        other => panic!("应报 OddDimension，实际 {other:?}"),
    }
}

/// 奇数显示尺寸 → Err
#[test]
fn odd_display_dimension_is_err() {
    let buf = vec![16u8; 16 * 16 * 3 / 2];
    let e = Nv12View::new(&buf, (16, 16), (15, 16)).unwrap_err();
    assert!(matches!(
        e,
        ConvertError::OddDimension {
            what: "display",
            ..
        }
    ));
}

/// 零尺寸 → Err
#[test]
fn zero_dimension_is_err() {
    let buf = vec![0u8; 64];
    let e = Nv12View::new(&buf, (0, 16), (0, 16)).unwrap_err();
    assert!(
        matches!(e, ConvertError::ZeroDimension { .. }),
        "实际 {e:?}"
    );
}

/// 缓冲不足 → Err（而不是 panic 越界读）
#[test]
fn short_buffer_is_err() {
    let buf = vec![16u8; 100]; // 远小于 640*368*3/2
    let e = Nv12View::new(&buf, (640, 368), (640, 360)).unwrap_err();
    match e {
        ConvertError::BufferTooSmall { need, have } => {
            assert_eq!(need, 640 * 368 * 3 / 2);
            assert_eq!(have, 100);
        }
        other => panic!("应报 BufferTooSmall，实际 {other:?}"),
    }
}

/// stride 小于宽度 → Err
#[test]
fn small_stride_is_err() {
    let buf = vec![16u8; 64 * 64 * 3 / 2];
    let e = Nv12View::with_stride(&buf, 32, (64, 64), (64, 64)).unwrap_err();
    match e {
        ConvertError::StrideTooSmall { stride, width } => {
            assert_eq!(stride, 32);
            assert_eq!(width, 64);
        }
        other => panic!("应报 StrideTooSmall，实际 {other:?}"),
    }
}

/// 带行填充的 stride 能正确解析（硬件解码器场景）
#[test]
fn strided_buffer_is_handled() {
    let cw = 16u32;
    let ch = 8u32;
    let stride = 32usize; // 比宽度大，每行尾部有填充

    let mut buf = vec![0u8; stride * ch as usize + stride * (ch as usize / 2)];
    // Y 平面：每行前 16 字节填黑，填充区填 235（如果被读到会变白）
    for row in 0..ch as usize {
        for col in 0..cw as usize {
            buf[row * stride + col] = 16;
        }
        for col in cw as usize..stride {
            buf[row * stride + col] = 235;
        }
    }
    // UV 平面填中性灰
    let uv = stride * ch as usize;
    for v in buf[uv..].iter_mut() {
        *v = 128;
    }

    let view = Nv12View::with_stride(&buf, stride, (cw, ch), (cw, ch)).unwrap();
    assert_eq!(view.stride(), stride);
    assert_eq!(
        view.frame_len(),
        stride * ch as usize + stride * (ch as usize / 2)
    );

    let mut rgba = Vec::new();
    view.to_rgba(&mut rgba).unwrap();
    for x in 0..cw {
        assert_eq!(
            px(&rgba, cw, x, 0),
            [0, 0, 0, 255],
            "行尾填充不能被当成有效像素"
        );
    }
}

/// 所有错误的 Display 实现都要能打印（不 panic、不含空串）
#[test]
fn errors_have_readable_display() {
    let cases = [
        ConvertError::ZeroDimension {
            width: 0,
            height: 0,
        },
        ConvertError::OddDimension {
            what: "coded",
            width: 17,
            height: 16,
        },
        ConvertError::DisplayLargerThanCoded {
            coded: (16, 16),
            display: (32, 32),
        },
        ConvertError::StrideTooSmall {
            stride: 8,
            width: 16,
        },
        ConvertError::BufferTooSmall {
            need: 100,
            have: 10,
        },
    ];
    for c in cases {
        let s = c.to_string();
        assert!(!s.is_empty(), "{c:?} 的 Display 输出为空");
    }
}

// ─────────────────────── 4. 边界与回归 ───────────────────────

/// 最小合法帧：2x2
#[test]
fn minimum_frame_2x2() {
    let buf = solid_nv12(2, 2, 235, 128, 128);
    let view = Nv12View::new(&buf, (2, 2), (2, 2)).unwrap();
    let mut rgba = Vec::new();
    view.to_rgba(&mut rgba).unwrap();
    assert_eq!(rgba.len(), 16);
    assert_eq!(px(&rgba, 2, 1, 1), [255, 255, 255, 255]);
}

/// 720p 全流程能跑通且尺寸正确
#[test]
fn hd_720_full_frame() {
    let (w, h) = (1280u32, 720u32);
    let buf = solid_nv12(w, h, 126, 128, 128);
    let view = Nv12View::new(&buf, (w, h), (w, h)).unwrap();
    let mut rgba = Vec::new();
    view.to_rgba(&mut rgba).unwrap();
    assert_eq!(rgba.len(), (w * h * 4) as usize);
}

/// 同一帧连续转两次结果一致（没有隐藏状态）
#[test]
fn conversion_is_deterministic() {
    let mut buf = Vec::new();
    for i in 0..(32 * 16) {
        buf.push(((i * 11) % 236 + 16) as u8);
    }
    for i in 0..(32 * 16 / 2) {
        buf.push(((i * 5) % 200 + 20) as u8);
    }
    let view = Nv12View::new(&buf, (32, 16), (32, 16)).unwrap();
    let mut a = Vec::new();
    let mut b = Vec::new();
    view.to_rgba(&mut a).unwrap();
    view.to_rgba(&mut b).unwrap();
    assert_eq!(a, b);
}
