//! BGRA→NV12 与 16 对齐的纯逻辑测试（**全平台可跑**）
//!
//! 这里的每一条断言都不碰平台 API，所以在 Linux / macOS CI 上同样执行。
//! Windows 相关的实测在 `win_capture.rs` 里。

use sidelens_capture::convert::{
    align16, bgra_to_nv12, fill_black_nv12, rgb_to_cb, rgb_to_cr, rgb_to_y, FrameLayout, C_MAX,
    C_MIN, C_NEUTRAL, Y_BLACK, Y_WHITE,
};

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

// ---------------------------------------------------------------- 对齐

#[test]
fn align16_matches_known_values() {
    assert_eq!(align16(0), 0);
    assert_eq!(align16(1), 16);
    assert_eq!(align16(15), 16);
    assert_eq!(align16(16), 16);
    assert_eq!(align16(17), 32);
    assert_eq!(align16(1080), 1088);
    assert_eq!(align16(1920), 1920);
    assert_eq!(align16(720), 720);
    assert_eq!(align16(640), 640);
    assert_eq!(align16(360), 368);
}

#[test]
fn align16_is_idempotent_and_monotonic() {
    for v in 0u32..4096 {
        let a = align16(v);
        assert!(a >= v, "align16({v}) = {a} 小于输入");
        assert_eq!(a % 16, 0, "align16({v}) = {a} 不是 16 的倍数");
        assert_eq!(align16(a), a, "align16 不是幂等的：{v}");
    }
}

#[test]
fn layout_1080p_pads_height_to_1088() {
    let l = FrameLayout::new(1920, 1080).expect("1920x1080 是合法尺寸");
    assert_eq!(l.display_width(), 1920);
    assert_eq!(l.display_height(), 1080);
    assert_eq!(l.coded_width(), 1920);
    assert_eq!(l.coded_height(), 1088);
    assert_eq!(l.padding(), (0, 8));
    assert!(!l.is_exact());
    // 1920*1088*3/2 = 3133440
    assert_eq!(l.nv12_len(), 1920 * 1088 * 3 / 2);
}

#[test]
fn layout_720p_needs_no_padding() {
    let l = FrameLayout::new(1280, 720).expect("1280x720 是合法尺寸");
    assert!(l.is_exact());
    assert_eq!(l.padding(), (0, 0));
    assert_eq!(l.nv12_len(), 1280 * 720 * 3 / 2);
}

#[test]
fn layout_640x360_pads_to_640x368() {
    let l = FrameLayout::new(640, 360).expect("640x360 是合法尺寸");
    assert_eq!(l.coded_height(), 368);
    assert_eq!(l.padding(), (0, 8));
    assert_eq!(l.nv12_len(), 640 * 368 * 3 / 2);
}

#[test]
fn layout_rejects_zero_and_huge() {
    assert!(FrameLayout::new(0, 100).is_err());
    assert!(FrameLayout::new(100, 0).is_err());
    assert!(FrameLayout::new(0, 0).is_err());
    assert!(FrameLayout::new(20_000, 100).is_err());
    assert!(FrameLayout::new(100, 20_000).is_err());
    // 边界值本身合法
    assert!(FrameLayout::new(16_384, 16).is_ok());
}

// ---------------------------------------------------------------- BT.709

#[test]
fn bt709_red_roundtrip_pins_matrix() {
    // 标准红在 BT.709 limited range 下是 (Y=63, Cb=102, Cr=240)。
    // 接收端 sidelens-render 的 bt709_red_pins_matrix 用同一组数字做解码断言，
    // 两端一起钉住 —— 谁改了矩阵，两个 crate 的测试同时红。
    assert_eq!(rgb_to_y(255, 0, 0), 63);
    assert_eq!(rgb_to_cb(255, 0, 0), 102);
    assert_eq!(rgb_to_cr(255, 0, 0), 240);
}

#[test]
fn bt709_white_maps_to_limited_white() {
    assert_eq!(rgb_to_y(255, 255, 255), Y_WHITE);
    assert_eq!(rgb_to_cb(255, 255, 255), C_NEUTRAL);
    assert_eq!(rgb_to_cr(255, 255, 255), C_NEUTRAL);
}

#[test]
fn bt709_black_maps_to_limited_black() {
    assert_eq!(rgb_to_y(0, 0, 0), Y_BLACK);
    assert_eq!(rgb_to_cb(0, 0, 0), C_NEUTRAL);
    assert_eq!(rgb_to_cr(0, 0, 0), C_NEUTRAL);
}

#[test]
fn gray_stays_neutral_at_every_level() {
    // R=G=B 时色度必须精确等于 128，否则画面整体会偏色。
    // 这一条依赖「色度三项系数和恰好为 0」，改系数时最容易破坏。
    for v in 0u8..=255 {
        assert_eq!(rgb_to_cb(v, v, v), C_NEUTRAL, "灰色 {v} 的 Cb 不是中性值");
        assert_eq!(rgb_to_cr(v, v, v), C_NEUTRAL, "灰色 {v} 的 Cr 不是中性值");
    }
}

#[test]
fn y_never_escapes_limited_range() {
    // 全 8-bit RGB 空间的角点 + 一批采样点
    let mut samples: Vec<(u8, u8, u8)> = Vec::new();
    for &r in &[0u8, 1, 64, 128, 192, 254, 255] {
        for &g in &[0u8, 1, 64, 128, 192, 254, 255] {
            for &b in &[0u8, 1, 64, 128, 192, 254, 255] {
                samples.push((r, g, b));
            }
        }
    }
    for (r, g, b) in samples {
        let y = rgb_to_y(r, g, b);
        assert!(
            (Y_BLACK..=Y_WHITE).contains(&y),
            "Y({r},{g},{b}) = {y} 越界 [{Y_BLACK},{Y_WHITE}]"
        );
        let cb = rgb_to_cb(r, g, b);
        let cr = rgb_to_cr(r, g, b);
        assert!((C_MIN..=C_MAX).contains(&cb), "Cb({r},{g},{b}) = {cb} 越界");
        assert!((C_MIN..=C_MAX).contains(&cr), "Cr({r},{g},{b}) = {cr} 越界");
    }
}

#[test]
fn bgra_channel_order_is_not_rgba() {
    // 纯红：BGRA 内存里是 [0, 0, 255, 255]。如果按 RGBA 解释，
    // 会读成 (R=0, G=0, B=255) 也就是蓝 —— 红蓝互换。
    let w = 4usize;
    let h = 4usize;
    let src = solid_bgra(w, h, 255, 0, 0);
    let l = FrameLayout::new(w as u32, h as u32).expect("4x4");
    let mut dst = vec![0u8; l.nv12_len()];
    bgra_to_nv12(&src, w * 4, l, &mut dst).expect("转换应当成功");

    assert_eq!(dst[0], 63, "Y 不是 BT.709 红");
    let uv = l.y_len();
    assert_eq!(dst[uv], 102, "Cb 不是 BT.709 红");
    assert_eq!(dst[uv + 1], 240, "Cr 不是 BT.709 红");
}

// ---------------------------------------------------------------- UV 布局

#[test]
fn uv_is_interleaved_not_planar() {
    // 左半屏纯红、右半屏纯蓝。交织 UV 下，每行的 UV 是 [Cb_red, Cr_red,
    // Cb_blue, Cr_blue, ...]；如果写成 U 平面后跟 V 平面，第二个字节
    // 会是另一个像素的 Cb 而不是同一个像素的 Cr。
    let w = 16usize;
    let h = 16usize;
    let mut src = vec![0u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let i = (y * w + x) * 4;
            let (r, g, b) = if x < w / 2 { (255, 0, 0) } else { (0, 0, 255) };
            src[i] = b;
            src[i + 1] = g;
            src[i + 2] = r;
            src[i + 3] = 255;
        }
    }
    let l = FrameLayout::new(w as u32, h as u32).expect("16x16");
    let mut dst = vec![0u8; l.nv12_len()];
    bgra_to_nv12(&src, w * 4, l, &mut dst).expect("转换应当成功");

    let uv = l.y_len();
    // 第 0 个 2x2 块 = 纯红
    assert_eq!(dst[uv], rgb_to_cb(255, 0, 0));
    assert_eq!(dst[uv + 1], rgb_to_cr(255, 0, 0));
    // 第 4 个 2x2 块（x=8，右半屏）= 纯蓝
    assert_eq!(dst[uv + 8], rgb_to_cb(0, 0, 255));
    assert_eq!(dst[uv + 9], rgb_to_cr(0, 0, 255));
    // 红蓝的 Cr 相差极大，足以证明第 2 个字节是「同一个块的 Cr」
    assert_ne!(dst[uv + 1], dst[uv + 9]);
}

// ---------------------------------------------------------------- 填充

#[test]
fn bottom_padding_replicates_last_row() {
    let dw = 32u32;
    let dh = 20u32; // coded 32x32，底部 12 行是 padding
    let l = FrameLayout::new(dw, dh).expect("32x20");
    assert_eq!(l.coded_height(), 32);

    // 每行用不同的亮度，便于区分
    let mut src = vec![0u8; (dw * dh * 4) as usize];
    for y in 0..dh as usize {
        for x in 0..dw as usize {
            let i = (y * dw as usize + x) * 4;
            let v = (y * 10) as u8;
            src[i] = v;
            src[i + 1] = v;
            src[i + 2] = v;
            src[i + 3] = 255;
        }
    }
    let mut dst = vec![0u8; l.nv12_len()];
    bgra_to_nv12(&src, (dw * 4) as usize, l, &mut dst).expect("转换应当成功");

    let cw = l.coded_width() as usize;
    let last_valid = dst[(dh as usize - 1) * cw];
    for y in dh as usize..l.coded_height() as usize {
        for x in 0..cw {
            assert_eq!(
                dst[y * cw + x],
                last_valid,
                "第 {y} 行第 {x} 列不是最后一行有效像素的复制"
            );
        }
    }
}

#[test]
fn right_padding_replicates_last_column() {
    let dw = 40u32; // coded 48，右侧 8 列 padding
    let dh = 16u32;
    let l = FrameLayout::new(dw, dh).expect("40x16");
    assert_eq!(l.coded_width(), 48);

    // 水平渐变，每列不同
    let mut src = vec![0u8; (dw * dh * 4) as usize];
    for y in 0..dh as usize {
        for x in 0..dw as usize {
            let i = (y * dw as usize + x) * 4;
            let v = (x * 6) as u8;
            src[i] = v;
            src[i + 1] = v;
            src[i + 2] = v;
            src[i + 3] = 255;
        }
    }
    let mut dst = vec![0u8; l.nv12_len()];
    bgra_to_nv12(&src, (dw * 4) as usize, l, &mut dst).expect("转换应当成功");

    let cw = l.coded_width() as usize;
    for y in 0..l.coded_height() as usize {
        let last = dst[y * cw + dw as usize - 1];
        for x in dw as usize..cw {
            assert_eq!(
                dst[y * cw + x],
                last,
                "第 {y} 行第 {x} 列不是最后一列的复制"
            );
        }
    }
}

#[test]
fn odd_size_last_block_uses_edge_pixels() {
    // 3x1 的图：红、绿、蓝。宽度 3 是奇数，最后一个 2x2 块只有 1 列有效像素，
    // 采样坐标钳到 x=2（蓝），所以整个块的色度应当**精确等于纯蓝**。
    let w = 3usize;
    let h = 1usize;
    let mut src = vec![0u8; w * h * 4];
    let colors = [(255u8, 0u8, 0u8), (0, 255, 0), (0, 0, 255)];
    for (x, &(r, g, b)) in colors.iter().enumerate() {
        let i = x * 4;
        src[i] = b;
        src[i + 1] = g;
        src[i + 2] = r;
        src[i + 3] = 255;
    }
    let l = FrameLayout::new(w as u32, h as u32).expect("3x1");
    let mut dst = vec![0u8; l.nv12_len()];
    bgra_to_nv12(&src, w * 4, l, &mut dst).expect("转换应当成功");

    let uv = l.y_len();
    // 第 0 块 = 红+绿 各两次 → 平均
    let cb0 = (rgb_to_cb(255, 0, 0) as u32 + rgb_to_cb(0, 255, 0) as u32) / 2;
    let cr0 = (rgb_to_cr(255, 0, 0) as u32 + rgb_to_cr(0, 255, 0) as u32) / 2;
    assert_eq!(dst[uv] as u32, cb0, "第 0 块的 Cb 不是红绿平均");
    assert_eq!(dst[uv + 1] as u32, cr0, "第 0 块的 Cr 不是红绿平均");

    // 第 1 块 = 纯蓝（边缘复制）
    assert_eq!(
        dst[uv + 2],
        rgb_to_cb(0, 0, 255),
        "奇数宽的最后一块没有钳到边缘像素"
    );
    assert_eq!(dst[uv + 3], rgb_to_cr(0, 0, 255), "同上（Cr）");
}

#[test]
fn odd_height_last_block_uses_edge_pixels() {
    // 1x3 的图：红、绿、蓝。高度 3 是奇数，最后一行 2x2 块只有 1 行有效。
    let w = 1usize;
    let h = 3usize;
    let mut src = vec![0u8; w * h * 4];
    let colors = [(255u8, 0u8, 0u8), (0, 255, 0), (0, 0, 255)];
    for (y, &(r, g, b)) in colors.iter().enumerate() {
        let i = y * 4;
        src[i] = b;
        src[i + 1] = g;
        src[i + 2] = r;
        src[i + 3] = 255;
    }
    let l = FrameLayout::new(w as u32, h as u32).expect("1x3");
    let mut dst = vec![0u8; l.nv12_len()];
    bgra_to_nv12(&src, w * 4, l, &mut dst).expect("转换应当成功");

    let cw = l.coded_width() as usize;
    let uv = l.y_len();
    // UV 第 0 行 = 红+绿；第 1 行（vy=1，对应 y=2..3，钳到 2）= 纯蓝
    let cb0 = (rgb_to_cb(255, 0, 0) as u32 + rgb_to_cb(0, 255, 0) as u32) / 2;
    assert_eq!(dst[uv] as u32, cb0);
    let row1 = uv + cw;
    assert_eq!(
        dst[row1],
        rgb_to_cb(0, 0, 255),
        "奇数高的最后一块没有钳到边缘像素"
    );
}

// ---------------------------------------------------------------- 参数校验

#[test]
fn rejects_short_stride() {
    let l = FrameLayout::new(16, 16).expect("16x16");
    let src = vec![0u8; 16 * 16 * 4];
    let mut dst = vec![0u8; l.nv12_len()];
    let e = bgra_to_nv12(&src, 16 * 4 - 1, l, &mut dst).expect_err("行距不足应当报错");
    assert!(
        format!("{e}").contains("行距"),
        "错误信息里应当提到行距：{e}"
    );
}

#[test]
fn rejects_short_source_buffer() {
    let l = FrameLayout::new(16, 16).expect("16x16");
    let src = vec![0u8; 16 * 16 * 4 - 64];
    let mut dst = vec![0u8; l.nv12_len()];
    assert!(bgra_to_nv12(&src, 16 * 4, l, &mut dst).is_err());
}

#[test]
fn rejects_short_destination_buffer() {
    let l = FrameLayout::new(16, 16).expect("16x16");
    let src = vec![0u8; 16 * 16 * 4];
    let mut dst = vec![0u8; l.nv12_len() - 1];
    let e = bgra_to_nv12(&src, 16 * 4, l, &mut dst).expect_err("目标缓冲不足应当报错");
    assert!(
        format!("{e}").contains("NV12"),
        "错误信息里应当提到 NV12：{e}"
    );
}

#[test]
fn honors_stride_larger_than_width() {
    // D3D11 的 staging 纹理行距会按 256 字节对齐。1080p 正好是 7680，
    // 但 1366 宽会是 5504（1366*4 = 5464）。这里用 40 宽 + 96 字节行距模拟。
    let dw = 40usize;
    let dh = 8usize;
    // 40*4 = 160，取 192 模拟 256 字节对齐后的行距
    let stride = 192usize;
    let mut src = vec![0xAAu8; stride * dh]; // 行尾填垃圾，转换必须忽略
    for y in 0..dh {
        for x in 0..dw {
            let i = y * stride + x * 4;
            src[i] = 0; // B
            src[i + 1] = 0; // G
            src[i + 2] = 255; // R
            src[i + 3] = 255;
        }
    }
    let l = FrameLayout::new(dw as u32, dh as u32).expect("40x8");
    let mut dst = vec![0u8; l.nv12_len()];
    bgra_to_nv12(&src, stride, l, &mut dst).expect("转换应当成功");
    assert_eq!(dst[0], 63, "行尾 padding 被当成了像素");
}

#[test]
fn fill_black_uses_limited_range_values() {
    let l = FrameLayout::new(16, 16).expect("16x16");
    let mut dst = vec![0xFFu8; l.nv12_len()];
    fill_black_nv12(l, &mut dst);
    let y_len = l.y_len();
    assert!(dst[..y_len].iter().all(|&v| v == Y_BLACK), "Y 平面不是 16");
    assert!(
        dst[y_len..].iter().all(|&v| v == C_NEUTRAL),
        "UV 平面不是 128"
    );
}

#[test]
fn conversion_is_deterministic() {
    // 同一个输入跑两次必须逐字节相同 —— 定点实现的意义就在这里，
    // 浮点版本在不同 CPU 上可能差 1，测试没法写死。
    let w = 64usize;
    let h = 64usize;
    let mut src = vec![0u8; w * h * 4];
    for (i, px) in src.chunks_exact_mut(4).enumerate() {
        px[0] = (i % 251) as u8;
        px[1] = ((i * 7) % 241) as u8;
        px[2] = ((i * 13) % 239) as u8;
        px[3] = 255;
    }
    let l = FrameLayout::new(w as u32, h as u32).expect("64x64");
    let mut a = vec![0u8; l.nv12_len()];
    let mut b = vec![0u8; l.nv12_len()];
    bgra_to_nv12(&src, w * 4, l, &mut a).expect("第一次转换");
    bgra_to_nv12(&src, w * 4, l, &mut b).expect("第二次转换");
    assert_eq!(a, b, "两次转换结果不一致");
}

// ---------------------------------------------------------------- 真实尺寸

#[test]
fn layout_1920x1080_uses_1088_coded_height() {
    // 编码器的真实约束：1920x1080 → 1920x1088。
    // 这条与 sidelens-codec-mf 的 EncoderConfig 必须对得上，
    // 否则 encode_nv12 会因为「NV12 数据不足」直接报错。
    let l = FrameLayout::new(1920, 1080).expect("1920x1080");
    assert_eq!((l.coded_width(), l.coded_height()), (1920, 1088));
    assert_eq!(l.nv12_len(), 1920 * 1088 * 3 / 2);
    assert_eq!(l.nv12_len(), 3_133_440);
}

#[test]
fn layout_3840x2160_is_already_aligned() {
    // 4K 本来就 16 对齐，不应有任何填充
    let l = FrameLayout::new(3840, 2160).expect("3840x2160");
    assert!(l.is_exact());
    assert_eq!(l.padding(), (0, 0));
    assert_eq!(l.nv12_len(), 3840 * 2160 * 3 / 2);
    assert_eq!(l.nv12_len(), 12_441_600);
}

#[test]
fn layout_1366x768_pads_only_width() {
    // 最常见的笔记本分辨率：宽度不是 16 的倍数，高度是
    let l = FrameLayout::new(1366, 768).expect("1366x768");
    assert_eq!(l.coded_width(), 1376);
    assert_eq!(l.coded_height(), 768);
    assert_eq!(l.padding(), (10, 0));
    assert!(!l.is_exact());
}

#[test]
fn layout_1440x900_pads_only_height() {
    let l = FrameLayout::new(1440, 900).expect("1440x900");
    assert_eq!(l.coded_width(), 1440); // 1440 = 90 * 16
    assert_eq!(l.coded_height(), 912);
    assert_eq!(l.padding(), (0, 12));
}

#[test]
fn layout_2560x1440_is_aligned() {
    let l = FrameLayout::new(2560, 1440).expect("2560x1440");
    assert!(l.is_exact());
}

#[test]
fn conversion_preserves_valid_area_bytes() {
    // 有效区域的像素值必须与「无填充转换」完全一致 ——
    // 填充只应该写 padding 区，绝不能碰有效像素。
    let dw = 32u32;
    let dh = 20u32;
    let l = FrameLayout::new(dw, dh).expect("32x20");
    let mut src = vec![0u8; (dw * dh * 4) as usize];
    for (i, px) in src.chunks_exact_mut(4).enumerate() {
        px[0] = (i % 251) as u8;
        px[1] = ((i * 3) % 241) as u8;
        px[2] = ((i * 7) % 239) as u8;
        px[3] = 255;
    }
    let mut dst = vec![0u8; l.nv12_len()];
    bgra_to_nv12(&src, (dw * 4) as usize, l, &mut dst).expect("转换应当成功");

    // 逐像素重算有效区域的 Y
    let cw = l.coded_width() as usize;
    for y in 0..dh as usize {
        for x in 0..dw as usize {
            let i = (y * dw as usize + x) * 4;
            let expect = rgb_to_y(src[i + 2], src[i + 1], src[i]);
            assert_eq!(
                dst[y * cw + x],
                expect,
                "有效区域 ({x},{y}) 的 Y 被填充污染了"
            );
        }
    }
}

#[test]
fn conversion_pads_uv_plane_too() {
    // UV 平面也要按 coded 尺寸填充，否则编码器会读到未初始化的尾部
    let dw = 32u32;
    let dh = 20u32;
    let l = FrameLayout::new(dw, dh).expect("32x20");
    let src = vec![128u8; (dw * dh * 4) as usize];
    let mut dst = vec![0xAAu8; l.nv12_len()]; // 用非零初值，能查出漏写
    bgra_to_nv12(&src, (dw * 4) as usize, l, &mut dst).expect("转换应当成功");
    let uv = l.y_len();
    let uv_end = uv + l.y_len() / 2;
    assert!(
        !dst[uv..uv_end].contains(&0xAA),
        "UV 平面有未写入的字节（还是初始值 0xAA）"
    );
}
