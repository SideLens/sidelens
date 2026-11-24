//! DPI 换算（纯逻辑，任何平台可测）
//!
//! # 为什么单独一层
//!
//! 「逻辑像素 → 物理像素」的换算错一次，界面在 200% 缩放下就会小一半或者糊成一片，
//! 而这种错误**在开发机的 100% 屏幕上永远看不出来**。把它抽成不依赖 Win32 的纯函数，
//! 就能在 Linux CI 上用一组 DPI 值把边界钉死。

/// 设计基准 DPI。Win32 的所有坐标、字体尺寸都以 96 DPI 为 100% 缩放。
pub const BASE_DPI: u32 = 96;

/// UI 字号（磅）。9pt 在 96 DPI 下是 12px，和 Windows 自带对话框的观感一致。
pub const UI_FONT_POINT: i32 = 9;

/// 把 DPI 归一到合理区间
///
/// 下限 48（50%）、上限 960（1000%）：`GetDpiForWindow` 在窗口尚未关联显示器时
/// 会返回 0，直接拿去乘会让整个界面塌成 0x0，所以 0 必须映射成 96。
pub fn clamp_dpi(dpi: u32) -> u32 {
    if dpi == 0 {
        BASE_DPI
    } else {
        dpi.clamp(48, 960)
    }
}

/// 逻辑像素 → 物理像素（四舍五入）
///
/// 负数对称处理：`scale_len(-x)` 必须等于 `-scale_len(x)`，
/// 否则用「负边距」表达的对齐在不同 DPI 下会偏向不同方向。
pub fn scale_len(logical: i32, dpi: u32) -> i32 {
    let dpi = i64::from(clamp_dpi(dpi));
    let base = i64::from(BASE_DPI);
    let v = i64::from(logical) * dpi;
    let half = base / 2;
    let rounded = if v >= 0 {
        (v + half) / base
    } else {
        -((-v + half) / base)
    };
    rounded.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}

/// 物理像素 → 逻辑像素
///
/// [`scale_len`] 的逆运算，用于「拿到客户区物理尺寸后反推设计坐标系」。
/// 和正向一样按四舍五入，保证 `unscale_len(scale_len(v, d), d) == v`
/// 在 d 为 96 的整数倍时恒成立（120 / 144 / 168 / 192 都满足）。
pub fn unscale_len(physical: i32, dpi: u32) -> i32 {
    let dpi = i64::from(clamp_dpi(dpi));
    let base = i64::from(BASE_DPI);
    let v = i64::from(physical) * base;
    let half = dpi / 2;
    let rounded = if v >= 0 {
        (v + half) / dpi
    } else {
        -((-v + half) / dpi)
    };
    rounded.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}

/// 字号（磅）→ `CreateFontW` 的 `lfHeight`
///
/// 返回值是**负数**：Win32 里负的 `lfHeight` 表示「字符高度（em 高）」，
/// 正的表示「单元格高度」。用字符高度才能让不同字体的视觉大小一致，
/// 也才能和设计稿上的磅值对上。
pub fn font_height(point: i32, dpi: u32) -> i32 {
    let dpi = i64::from(clamp_dpi(dpi));
    // px = pt * dpi / 72（1pt = 1/72 英寸）
    let num = i64::from(point) * dpi;
    let px = (num + 36) / 72;
    -(px.max(1).min(i64::from(i32::MAX)) as i32)
}

/// 是否处于高 DPI（> 100%）
pub fn is_high_dpi(dpi: u32) -> bool {
    clamp_dpi(dpi) > BASE_DPI
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dpi_为零时退回基准值() {
        assert_eq!(clamp_dpi(0), BASE_DPI);
    }

    #[test]
    fn dpi_超出区间被夹住() {
        assert_eq!(clamp_dpi(1), 48);
        assert_eq!(clamp_dpi(10_000), 960);
    }

    #[test]
    fn 一百百分比不缩放() {
        for v in [-100, -1, 0, 1, 17, 1024] {
            assert_eq!(scale_len(v, 96), v, "dpi=96 时 {v} 不该被改变");
        }
    }

    #[test]
    fn 两百百分比翻倍() {
        assert_eq!(scale_len(16, 192), 32);
        assert_eq!(scale_len(1, 192), 2);
    }

    #[test]
    fn 一百五十百分比四舍五入() {
        // 96 → 144：x1.5
        assert_eq!(scale_len(1, 144), 2); // 1.5 → 2
        assert_eq!(scale_len(3, 144), 5); // 4.5 → 5
        assert_eq!(scale_len(5, 144), 8); // 7.5 → 8
    }

    #[test]
    fn 负数对称缩放() {
        for dpi in [96, 120, 144, 192, 240] {
            for v in [1, 7, 33, 320] {
                assert_eq!(
                    scale_len(-v, dpi),
                    -scale_len(v, dpi),
                    "dpi={dpi} v={v} 正负不对称"
                );
            }
        }
    }

    #[test]
    fn 九磅字在常见dpi下的像素高() {
        assert_eq!(font_height(9, 96), -12);
        assert_eq!(font_height(9, 120), -15);
        assert_eq!(font_height(9, 144), -18);
        assert_eq!(font_height(9, 192), -24);
    }

    #[test]
    fn 字号永远为负且非零() {
        for dpi in [48, 96, 144, 960] {
            for pt in [1, 9, 24] {
                let h = font_height(pt, dpi);
                assert!(h < 0, "pt={pt} dpi={dpi} 得到 {h}");
            }
        }
        // 极小字号也不能塌成 0，否则 CreateFontW 会退回系统默认字体
        assert!(font_height(0, 48) < 0);
    }

    #[test]
    fn 正反换算可逆() {
        for dpi in [96, 120, 144, 168, 192, 240, 288] {
            for v in [0, 1, 16, 320, 1920] {
                let round_trip = unscale_len(scale_len(v, dpi), dpi);
                assert!(
                    (round_trip - v).abs() <= 1,
                    "dpi={dpi} v={v} 往返得到 {round_trip}"
                );
            }
        }
    }

    #[test]
    fn 高dpi判定() {
        assert!(!is_high_dpi(96));
        assert!(is_high_dpi(120));
        assert!(!is_high_dpi(0));
    }
}
