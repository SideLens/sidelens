//! 画面效果：自动取景、背景虚化、人像平滑。
//!
//! 全部作用在 NV12 的 Y 平面（亮度）与 UV 平面上，纯 CPU，没有模型依赖。
//! 逻辑与调用方解耦，因此可以在 Linux CI 上把每个边界跑一遍。
//!
//! # 三个效果的实现取舍
//!
//! | 效果 | rng-lens 做法 | 本模块做法 | 为什么这样够用 |
//! | --- | --- | --- | --- |
//! | 自动取景 | 人脸/主体检测网络 | 亮度梯度显著性 + 积分图找最优窗口 | 视频会议里的人是画面中细节最多的区域 |
//! | 背景虚化 | 语义分割逐像素 alpha | 中心椭圆 + 羽化 + 可分离盒式模糊 | 摄像头视角固定时人总在中间 |
//! | 人像平滑 | 皮肤分割 | 双边滤波近似（保边降噪） | 肤色平滑的需求本质是「降噪但别糊掉五官」 |
//!
//! 每一条都有对应的断言。效果不如模型，但可测、可关、开销可算。
//! 默认全关，用户开了才走这些路径。

use sidelens_frame_transform::{Frame, Geometry};

/// 自动取景参数
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AutoFrameConfig {
    /// 输出窗口相对输入的比例（0.5 = 裁一半）
    pub scale: f32,
    /// 显著性图的下采样倍数（越大越快、越粗）
    pub downsample: u32,
    /// 平滑系数：0 = 窗口瞬移，越接近 1 越稳
    pub smoothing: f32,
}

impl Default for AutoFrameConfig {
    fn default() -> Self {
        Self {
            scale: 0.75,
            downsample: 8,
            // 0.85 是折中：再高跟不住人移动，再低会有可见的抖动
            smoothing: 0.85,
        }
    }
}

/// 自动取景的窗口（输入坐标系里，偶数对齐）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FocusWindow {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// 找出画面最值得保留的窗口。
///
/// # 算法
///
/// 1. 把 Y 平面按 downsample 缩小成一张显著性图 —— 用局部梯度而不是
///    亮度绝对值：一盏顶灯很亮但没有细节，人脸上有大量边缘；
/// 2. 对显著性图做积分图，任意窗口的和是 O(1)；
/// 3. 枚举所有合法窗口，取和最大的那个；
/// 4. 与上一帧的窗口按 smoothing 做指数平滑。
///
/// 第 4 步是必需的：不做平滑时，画面里两只手交替移动会让窗口来回跳，
/// 观感比不裁还差。
pub fn find_focus_window(
    frame: &Frame,
    config: AutoFrameConfig,
    previous: Option<FocusWindow>,
) -> Option<FocusWindow> {
    let geometry = frame.geometry;
    let width = geometry.width();
    let height = geometry.height();
    if width < 16 || height < 16 {
        return None;
    }

    let scale = config.scale.clamp(0.1, 1.0);
    // 窗口尺寸必须偶数（NV12 约束），且至少 16 像素以免退化成噪点
    let mut window_width = (((width as f32 * scale) as u32) / 2 * 2).max(16).min(width);
    let mut window_height = (((height as f32 * scale) as u32) / 2 * 2)
        .max(16)
        .min(height);
    window_width -= window_width % 2;
    window_height -= window_height % 2;

    let step = config.downsample.clamp(1, 64);
    let map_width = (width / step).max(1) as usize;
    let map_height = (height / step).max(1) as usize;
    let raw = saliency_map(frame, step, map_width, map_height);
    let integral = integral_image(&raw, map_width, map_height);

    let win_w = (window_width / step).max(1) as usize;
    let win_h = (window_height / step).max(1) as usize;
    if win_w > map_width || win_h > map_height {
        return None;
    }

    let mut best = (0u64, 0usize, 0usize);
    for y in 0..=(map_height - win_h) {
        for x in 0..=(map_width - win_w) {
            let sum = window_sum(&integral, map_width, x, y, win_w, win_h);
            if sum > best.0 {
                best = (sum, x, y);
            }
        }
    }

    let mut target_x = (best.1 * step as usize) as u32;
    let mut target_y = (best.2 * step as usize) as u32;
    // 偶数对齐：裁切必须从偶数坐标开始，否则色度会错位
    target_x -= target_x % 2;
    target_y -= target_y % 2;
    target_x = target_x.min(width - window_width);
    target_y = target_y.min(height - window_height);

    let smoothing = config.smoothing.clamp(0.0, 0.99);
    let (x, y) = match previous {
        Some(previous) if previous.width == window_width && previous.height == window_height => {
            let blend = |old: u32, new: u32| -> u32 {
                let value = old as f32 * smoothing + new as f32 * (1.0 - smoothing);
                let value = (value.round() as u32).min(u32::MAX - 1);
                value - value % 2
            };
            (blend(previous.x, target_x), blend(previous.y, target_y))
        }
        _ => (target_x, target_y),
    };

    Some(FocusWindow {
        x,
        y,
        width: window_width,
        height: window_height,
    })
}

/// 下采样后的显著性图：每格是 Y 平面上该块内相邻像素差的绝对值之和
fn saliency_map(frame: &Frame, step: u32, map_width: usize, map_height: usize) -> Vec<u64> {
    let width = frame.geometry.width() as usize;
    let height = frame.geometry.height() as usize;
    let step = step as usize;
    let mut map = vec![0u64; map_width * map_height];

    for cell_y in 0..map_height {
        for cell_x in 0..map_width {
            let mut energy = 0u64;
            let start_y = cell_y * step;
            let start_x = cell_x * step;
            for y in start_y..(start_y + step).min(height) {
                for x in start_x..(start_x + step).min(width) {
                    let index = y * width + x;
                    let value = frame.data[index] as i32;
                    let right = if x + 1 < width {
                        (frame.data[index + 1] as i32 - value).unsigned_abs() as u64
                    } else {
                        0
                    };
                    let down = if y + 1 < height {
                        (frame.data[index + width] as i32 - value).unsigned_abs() as u64
                    } else {
                        0
                    };
                    energy += right + down;
                }
            }
            map[cell_y * map_width + cell_x] = energy;
        }
    }
    map
}

/// 积分图：integral 的第 (x, y) 项是从 (0,0) 到 (x-1,y-1) 的和
fn integral_image(source: &[u64], width: usize, height: usize) -> Vec<u64> {
    let stride = width + 1;
    let mut integral = vec![0u64; stride * (height + 1)];
    for y in 0..height {
        let mut row_sum = 0u64;
        for x in 0..width {
            row_sum = row_sum.saturating_add(source[y * width + x]);
            integral[(y + 1) * stride + x + 1] =
                integral[y * stride + x + 1].saturating_add(row_sum);
        }
    }
    integral
}

fn window_sum(integral: &[u64], width: usize, x: usize, y: usize, w: usize, h: usize) -> u64 {
    let stride = width + 1;
    let a = integral[y * stride + x];
    let b = integral[y * stride + x + w];
    let c = integral[(y + h) * stride + x];
    let d = integral[(y + h) * stride + x + w];
    d.saturating_add(a).saturating_sub(b).saturating_sub(c)
}

/// 背景虚化参数
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BlurConfig {
    /// 清晰区椭圆半径占半宽/半高的比例
    pub keep_radius: f32,
    /// 羽化宽度（相对半径）
    pub feather: f32,
    /// 模糊半径（像素）
    pub radius: u32,
}

impl Default for BlurConfig {
    fn default() -> Self {
        Self {
            keep_radius: 0.55,
            feather: 0.35,
            radius: 8,
        }
    }
}

/// 背景虚化：中间椭圆保持清晰，向外按羽化过渡到模糊。
///
/// # 为什么用椭圆而不是矩形
///
/// 矩形边缘会在画面上留下明显的四道直边，观感像「贴了个窗户」。
/// 椭圆配羽化在视觉上更接近浅景深的自然过渡。
pub fn blur_background(frame: &mut Frame, config: BlurConfig) {
    let width = frame.geometry.width() as usize;
    let height = frame.geometry.height() as usize;
    if width < 8 || height < 8 || config.radius == 0 {
        return;
    }

    let y_len = width * height;
    let original: Vec<u8> = frame.data.clone();
    let blurred_y = box_blur(&frame.data[..y_len], width, height, config.radius as usize);
    let blurred_uv = box_blur(&frame.data[y_len..], width, height / 2, 2);

    let center_x = width as f32 / 2.0;
    let center_y = height as f32 / 2.0;
    let radius_x = (center_x * config.keep_radius.clamp(0.05, 1.0)).max(1.0);
    let radius_y = (center_y * config.keep_radius.clamp(0.05, 1.0)).max(1.0);
    let feather = config.feather.clamp(0.01, 1.0);

    let alpha_at = |x: f32, y: f32| -> f32 {
        let dx = (x - center_x) / radius_x;
        let dy = (y - center_y) / radius_y;
        let distance = (dx * dx + dy * dy).sqrt();
        ((distance - 1.0) / feather).clamp(0.0, 1.0)
    };

    for y in 0..height {
        for x in 0..width {
            let alpha = alpha_at(x as f32, y as f32);
            if alpha <= 0.0 {
                continue;
            }
            let index = y * width + x;
            frame.data[index] = blend(original[index], blurred_y[index], alpha);
        }
    }

    // UV 平面：一对 UV 覆盖 2x2 像素，按块取中心的 alpha
    for uv_y in 0..(height / 2) {
        for uv_x in 0..(width / 2) {
            let alpha = alpha_at((uv_x * 2) as f32, (uv_y * 2) as f32);
            if alpha <= 0.0 {
                continue;
            }
            let index = uv_y * width + uv_x * 2;
            for channel in 0..2 {
                frame.data[y_len + index + channel] = blend(
                    original[y_len + index + channel],
                    blurred_uv[index + channel],
                    alpha,
                );
            }
        }
    }
}

fn blend(original: u8, blurred: u8, alpha: f32) -> u8 {
    (original as f32 * (1.0 - alpha) + blurred as f32 * alpha) as u8
}

/// 可分离盒式模糊（水平 + 垂直两趟）
fn box_blur(source: &[u8], width: usize, rows: usize, radius: usize) -> Vec<u8> {
    if radius == 0 || rows == 0 || width == 0 || source.len() < width * rows {
        return source.to_vec();
    }
    let window = radius * 2 + 1;
    let mut horizontal = vec![0u8; width * rows];
    for y in 0..rows {
        for x in 0..width {
            let mut sum = 0u32;
            for offset in 0..window {
                // 边缘用夹取而不是补零：补零会让边缘发黑
                let sample = (x + offset).saturating_sub(radius).min(width - 1);
                sum += source[y * width + sample] as u32;
            }
            horizontal[y * width + x] = (sum / window as u32) as u8;
        }
    }

    let mut vertical = vec![0u8; width * rows];
    for y in 0..rows {
        for x in 0..width {
            let mut sum = 0u32;
            for offset in 0..window {
                let sample = (y + offset).saturating_sub(radius).min(rows - 1);
                sum += horizontal[sample * width + x] as u32;
            }
            vertical[y * width + x] = (sum / window as u32) as u8;
        }
    }
    vertical
}

/// 人像平滑（双边滤波的盒式近似）
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SmoothConfig {
    /// 空间半径（像素）
    pub radius: u32,
    /// 亮度差阈值：差超过它的像素不参与平均（保边）
    pub threshold: u16,
}

impl Default for SmoothConfig {
    fn default() -> Self {
        Self {
            radius: 2,
            threshold: 12,
        }
    }
}

/// 保边降噪：只对邻域内亮度差小于阈值的像素做平均。
///
/// 真正的双边滤波按距离与亮度差双重加权；这里用「阈值内算 1、阈值外算 0」
/// 替代，少了乘加，但保留了「不糊边」这个关键性质。
pub fn smooth_skin(frame: &mut Frame, config: SmoothConfig) {
    let width = frame.geometry.width() as usize;
    let height = frame.geometry.height() as usize;
    if width < 4 || height < 4 || config.radius == 0 {
        return;
    }
    let y_len = width * height;
    let source: Vec<u8> = frame.data[..y_len].to_vec();
    let radius = config.radius as usize;
    let threshold = config.threshold;

    for y in 0..height {
        for x in 0..width {
            let center = source[y * width + x] as u16;
            let mut sum = 0u32;
            let mut count = 0u32;
            for sy in y.saturating_sub(radius)..(y + radius + 1).min(height) {
                for sx in x.saturating_sub(radius)..(x + radius + 1).min(width) {
                    let value = source[sy * width + sx] as u16;
                    if value.abs_diff(center) <= threshold {
                        sum += value as u32;
                        count += 1;
                    }
                }
            }
            // count 至少是 1（中心像素自己一定在阈值内），这里的除零检查
            // 只是为了让代码在极端参数下也不会 panic。
            if let Some(average) = sum.checked_div(count) {
                frame.data[y * width + x] = average as u8;
            }
        }
    }
}

/// 把自动取景算出的窗口转成裁切参数
pub fn focus_crop(window: FocusWindow) -> Option<sidelens_frame_transform::Crop> {
    sidelens_frame_transform::Crop::new(window.x, window.y, window.width, window.height)
}

/// 一帧的全部效果开关
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Effects {
    pub auto_frame: Option<AutoFrameConfig>,
    pub blur: Option<BlurConfig>,
    pub smooth: Option<SmoothConfig>,
}

impl Effects {
    pub fn is_identity(&self) -> bool {
        self.auto_frame.is_none() && self.blur.is_none() && self.smooth.is_none()
    }
}

/// 应用效果。自动取景返回算出的窗口，调用方决定怎么用（裁切或提示）。
pub fn apply(
    frame: &mut Frame,
    effects: Effects,
    previous: Option<FocusWindow>,
) -> Option<FocusWindow> {
    if effects.is_identity() {
        return None;
    }
    let window = effects
        .auto_frame
        .and_then(|config| find_focus_window(frame, config, previous));
    if let Some(config) = effects.smooth {
        smooth_skin(frame, config);
    }
    if let Some(config) = effects.blur {
        blur_background(frame, config);
    }
    window
}

/// 给定几何与效果开关，输出几何会变成什么（自动取景会改尺寸）
pub fn output_geometry(input: Geometry, effects: Effects, window: Option<FocusWindow>) -> Geometry {
    match (effects.auto_frame, window) {
        (Some(_), Some(window)) => Geometry::new(window.width, window.height).unwrap_or(input),
        _ => input,
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn g(width: u32, height: u32) -> Geometry {
        Geometry::new(width, height).expect("偶数尺寸")
    }

    /// 造一帧：背景暗且平，中间一块亮且带细节（模拟人脸）
    fn frame_with_subject(width: u32, height: u32) -> Frame {
        let geometry = g(width, height);
        let mut frame = Frame {
            geometry,
            data: vec![0u8; geometry.frame_len()],
        };
        let w = width as usize;
        let h = height as usize;
        for y in 0..h {
            for x in 0..w {
                // 背景恒定 40，无梯度
                frame.data[y * w + x] = 40;
            }
        }
        // 主体：中心 1/2 区域，带棋盘细节
        let mx = w / 4;
        let my = h / 4;
        for y in my..(my + h / 2) {
            for x in mx..(mx + w / 2) {
                frame.data[y * w + x] = if (x + y) % 2 == 0 { 200 } else { 120 };
            }
        }
        frame
    }

    fn edge_energy(frame: &Frame, x0: usize, y0: usize, w: usize, h: usize) -> u64 {
        let stride = frame.geometry.width() as usize;
        let mut energy = 0u64;
        for y in y0..(y0 + h) {
            for x in x0..(x0 + w) {
                let index = y * stride + x;
                if x + 1 < stride {
                    energy += frame.data[index].abs_diff(frame.data[index + 1]) as u64;
                }
            }
        }
        energy
    }

    fn variance(frame: &Frame, width: usize, height: usize) -> f64 {
        let count = width * height;
        let mean: f64 = frame.data[..count].iter().map(|v| *v as f64).sum::<f64>() / count as f64;
        frame.data[..count]
            .iter()
            .map(|v| {
                let delta = *v as f64 - mean;
                delta * delta
            })
            .sum::<f64>()
            / count as f64
    }

    #[test]
    fn 自动取景把窗口放在主体上() {
        let frame = frame_with_subject(160, 120);
        let window = find_focus_window(
            &frame,
            AutoFrameConfig {
                scale: 0.5,
                downsample: 4,
                smoothing: 0.0,
            },
            None,
        )
        .expect("应算出窗口");
        assert!(
            window.x >= 20 && window.x <= 60,
            "x={} 应靠近中心",
            window.x
        );
        assert!(
            window.y >= 10 && window.y <= 45,
            "y={} 应靠近中心",
            window.y
        );
    }

    #[test]
    fn 自动取景窗口是偶数对齐的() {
        let frame = frame_with_subject(160, 120);
        for scale in [0.4f32, 0.5, 0.6, 0.75, 0.8] {
            let window = find_focus_window(
                &frame,
                AutoFrameConfig {
                    scale,
                    downsample: 2,
                    smoothing: 0.0,
                },
                None,
            )
            .expect("应算出窗口");
            assert_eq!(window.x % 2, 0, "x 必须是偶数（scale={scale}）");
            assert_eq!(window.y % 2, 0, "y 必须是偶数（scale={scale}）");
            assert_eq!(window.width % 2, 0);
            assert_eq!(window.height % 2, 0);
        }
    }

    #[test]
    fn 自动取景窗口不越界() {
        let frame = frame_with_subject(160, 120);
        for scale in [0.2f32, 0.5, 0.95, 1.0] {
            let window = find_focus_window(
                &frame,
                AutoFrameConfig {
                    scale,
                    downsample: 4,
                    smoothing: 0.0,
                },
                None,
            )
            .expect("应算出窗口");
            assert!(
                window.x + window.width <= 160,
                "横向越界：{} + {} > 160",
                window.x,
                window.width
            );
            assert!(
                window.y + window.height <= 120,
                "纵向越界：{} + {} > 120",
                window.y,
                window.height
            );
        }
    }

    #[test]
    fn 平滑让窗口不逐帧跳动() {
        let frame = frame_with_subject(160, 120);
        let config = AutoFrameConfig {
            scale: 0.6,
            downsample: 4,
            smoothing: 0.9,
        };
        let first = find_focus_window(&frame, config, None).unwrap();
        let previous = FocusWindow {
            x: 0,
            y: 0,
            width: first.width,
            height: first.height,
        };
        let smoothed = find_focus_window(&frame, config, Some(previous)).unwrap();
        assert!(
            smoothed.x >= previous.x && smoothed.x <= first.x,
            "平滑结果 {} 不在 [{}, {}] 之间",
            smoothed.x,
            previous.x,
            first.x
        );
    }

    #[test]
    fn 尺寸变化时不做平滑而是直接取新窗口() {
        let frame = frame_with_subject(160, 120);
        let config = AutoFrameConfig {
            scale: 0.5,
            downsample: 4,
            smoothing: 0.9,
        };
        let stale = FocusWindow {
            x: 0,
            y: 0,
            width: 40,
            height: 30,
        };
        let window = find_focus_window(&frame, config, Some(stale)).unwrap();
        assert_ne!(
            (window.width, window.height),
            (40, 30),
            "尺寸不同时不该沿用旧窗口"
        );
    }

    #[test]
    fn 均匀画面的自动取景不崩且窗口合法() {
        let geometry = g(64, 48);
        let frame = Frame {
            geometry,
            data: vec![128u8; geometry.frame_len()],
        };
        let window = find_focus_window(&frame, AutoFrameConfig::default(), None)
            .expect("均匀画面也该给出一个窗口");
        assert!(window.width > 0 && window.height > 0);
    }

    #[test]
    fn 最小尺寸的画面也能算窗口() {
        let geometry = g(16, 16);
        let frame = Frame {
            geometry,
            data: vec![100u8; geometry.frame_len()],
        };
        let window = find_focus_window(&frame, AutoFrameConfig::default(), None)
            .expect("16x16 应能算出窗口");
        assert!(window.x + window.width <= 16);
        assert!(window.y + window.height <= 16);
    }

    #[test]
    fn 太小的画面直接返回无窗口() {
        for (w, h) in [(8u32, 8u32), (14, 14)] {
            let geometry = g(w, h);
            let frame = Frame {
                geometry,
                data: vec![100u8; geometry.frame_len()],
            };
            assert!(find_focus_window(&frame, AutoFrameConfig::default(), None).is_none());
        }
    }

    #[test]
    fn 背景虚化不改动清晰区中心() {
        let mut frame = frame_with_subject(64, 48);
        let before_center = frame.luma(32, 24).unwrap();
        blur_background(
            &mut frame,
            BlurConfig {
                keep_radius: 0.4,
                feather: 0.2,
                radius: 4,
            },
        );
        let after_center = frame.luma(32, 24).unwrap();
        assert!(
            (before_center as i32 - after_center as i32).abs() <= 2,
            "中心被误改：{before_center} -> {after_center}"
        );
    }

    #[test]
    fn 背景虚化让边缘的梯度变小() {
        let mut frame = frame_with_subject(64, 48);
        let w = 64usize;
        for y in 0..8 {
            for x in 0..8 {
                frame.data[y * w + x] = 250;
            }
        }
        let before = edge_energy(&frame, 0, 0, 8, 8);
        blur_background(
            &mut frame,
            BlurConfig {
                keep_radius: 0.35,
                feather: 0.2,
                radius: 6,
            },
        );
        let after = edge_energy(&frame, 0, 0, 8, 8);
        assert!(after < before, "模糊后边缘能量应下降：{before} -> {after}");
    }

    #[test]
    fn 背景虚化不改动帧长度与几何() {
        let geometry = g(32, 32);
        let mut frame = frame_with_subject(32, 32);
        let original_len = frame.data.len();
        blur_background(&mut frame, BlurConfig::default());
        assert_eq!(frame.data.len(), original_len, "帧长度不能变");
        assert_eq!(frame.geometry, geometry, "几何不能变");
    }

    #[test]
    fn 人像平滑降低噪点() {
        let geometry = g(32, 32);
        let mut frame = Frame {
            geometry,
            data: vec![0u8; geometry.frame_len()],
        };
        let w = 32usize;
        for y in 0..32 {
            for x in 0..32 {
                let noise = ((x * 7 + y * 13) % 7) as i32 - 3;
                frame.data[y * w + x] = (128 + noise).clamp(0, 255) as u8;
            }
        }
        let before = variance(&frame, 32, 32);
        smooth_skin(
            &mut frame,
            SmoothConfig {
                radius: 2,
                threshold: 12,
            },
        );
        let after = variance(&frame, 32, 32);
        assert!(after < before, "平滑后方差应下降：{before} -> {after}");
    }

    #[test]
    fn 人像平滑保住强边缘() {
        let geometry = g(32, 32);
        let mut frame = Frame {
            geometry,
            data: vec![0u8; geometry.frame_len()],
        };
        let w = 32usize;
        for y in 0..32 {
            for x in 0..32 {
                frame.data[y * w + x] = if x < 16 { 50 } else { 200 };
            }
        }
        smooth_skin(
            &mut frame,
            SmoothConfig {
                radius: 2,
                threshold: 12,
            },
        );
        assert_eq!(frame.data[16 * w + 10], 50, "左侧被误改");
        assert_eq!(frame.data[16 * w + 20], 200, "右侧被误改");
    }

    #[test]
    fn 小尺寸画面直接跳过而不是越界() {
        for (w, h) in [(2u32, 2u32), (4, 4), (8, 8)] {
            let geometry = g(w, h);
            let mut frame = Frame {
                geometry,
                data: vec![100u8; geometry.frame_len()],
            };
            blur_background(&mut frame, BlurConfig::default());
            smooth_skin(&mut frame, SmoothConfig::default());
        }
    }

    #[test]
    fn 半径为零时什么都不做() {
        let mut frame = frame_with_subject(32, 32);
        let before = frame.data.clone();
        blur_background(
            &mut frame,
            BlurConfig {
                radius: 0,
                ..Default::default()
            },
        );
        assert_eq!(frame.data, before);
        smooth_skin(
            &mut frame,
            SmoothConfig {
                radius: 0,
                ..Default::default()
            },
        );
        assert_eq!(frame.data, before);
    }

    #[test]
    fn 效果全关时走快速路径() {
        let effects = Effects::default();
        assert!(effects.is_identity());
        let mut frame = frame_with_subject(32, 32);
        let before = frame.data.clone();
        let window = apply(&mut frame, effects, None);
        assert!(window.is_none());
        assert_eq!(frame.data, before, "恒等路径不该改数据");
    }

    #[test]
    fn 自动取景窗口能转成合法裁切() {
        let frame = frame_with_subject(160, 120);
        let window = find_focus_window(&frame, AutoFrameConfig::default(), None).unwrap();
        let crop = focus_crop(window).expect("窗口应能转成裁切");
        assert_eq!(crop.width, window.width);
        assert_eq!(crop.x, window.x);
        assert_eq!(crop.x % 2, 0);
        assert_eq!(crop.y % 2, 0);
    }

    #[test]
    fn 输出几何在自动取景时变成窗口尺寸() {
        let input = g(160, 120);
        let effects = Effects {
            auto_frame: Some(AutoFrameConfig::default()),
            ..Default::default()
        };
        let window = FocusWindow {
            x: 10,
            y: 20,
            width: 80,
            height: 60,
        };
        let output = output_geometry(input, effects, Some(window));
        assert_eq!(output.width(), 80);
        assert_eq!(output.height(), 60);
        assert_eq!(output_geometry(input, effects, None), input);
    }

    #[test]
    fn 组合效果按顺序作用且不破坏帧() {
        let mut frame = frame_with_subject(64, 48);
        let geometry = frame.geometry;
        let effects = Effects {
            auto_frame: Some(AutoFrameConfig {
                scale: 0.5,
                downsample: 4,
                smoothing: 0.0,
            }),
            blur: Some(BlurConfig {
                keep_radius: 0.5,
                feather: 0.3,
                radius: 3,
            }),
            smooth: Some(SmoothConfig {
                radius: 1,
                threshold: 10,
            }),
        };
        let window = apply(&mut frame, effects, None);
        assert!(window.is_some());
        assert_eq!(frame.geometry, geometry, "效果不该改几何");
        assert_eq!(frame.data.len(), geometry.frame_len());
    }
}
