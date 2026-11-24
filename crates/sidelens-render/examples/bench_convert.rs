//! NV12 → RGBA 转换性能实测
//!
//! 跑法：
//!
//! ```text
//! cargo run -p sidelens-render --example bench_convert --release
//! ```
//!
//! 性能目标是 **1280x720 单帧 < 2ms**（纯 Rust，release）。
//!
//! # 为什么要在 release 下测
//!
//! debug 构建里每像素的边界检查、`clamp8` 的内联全部没有优化，
//! 720p 能跑出 30ms+ —— 那个数字和代码质量无关，只会误导人。
//! 所以这个 example 在 debug 下会**打印一条警告**但仍然跑，方便快速验证逻辑。
//!
//! # 输出什么
//!
//! | 列 | 含义 |
//! | --- | --- |
//! | `min/p50/p95/max` | 逐帧耗时分布（µs） |
//! | `MP/s` | 每秒处理的百万像素数，跨分辨率可比 |
//! | `GB/s` | 写入带宽（输出 RGBA 的字节数 / 耗时） |
//! | `vs 2ms` | 相对 720p 目标的余量 |

use std::time::Instant;

use sidelens_render::{Matrix, Nv12View};

/// 一次测量结果
#[derive(Debug, Clone, Copy)]
struct Stats {
    min_us: f64,
    p50_us: f64,
    p95_us: f64,
    max_us: f64,
    mean_us: f64,
}

impl Stats {
    fn from_samples(mut v: Vec<f64>) -> Self {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let n = v.len();
        let pick = |q: f64| -> f64 {
            if n == 0 {
                return 0.0;
            }
            let idx = ((n as f64 - 1.0) * q).round() as usize;
            v[idx.min(n - 1)]
        };
        Stats {
            min_us: pick(0.0),
            p50_us: pick(0.5),
            p95_us: pick(0.95),
            max_us: pick(1.0),
            mean_us: if n == 0 {
                0.0
            } else {
                v.iter().sum::<f64>() / n as f64
            },
        }
    }
}

/// 造一帧「像真实画面」的 NV12
///
/// 全零帧会让分支预测和缓存表现得过于理想，测出来的数字偏乐观。
/// 这里用渐变 + 移动竖条，和 `sidelenssend` 的合成方式一致。
fn synth_nv12(width: u32, height: u32, seq: u32) -> Vec<u8> {
    let w = width as usize;
    let h = height as usize;
    let mut buf = vec![128u8; w * h * 3 / 2];
    let bar_x = (seq as usize * 7) % w.max(1);
    let bar_w = (w / 12).max(8);
    for y in 0..h {
        let row = y * w;
        for x in 0..w {
            buf[row + x] = if x >= bar_x && x < bar_x + bar_w {
                235
            } else {
                (((x * 200) / w.max(1)) as u8).wrapping_add((y % 7) as u8)
            };
        }
    }
    // UV 平面已经全是 128（中性），不用再填
    buf
}

/// 跑一组测量
fn bench(label: &str, w: u32, h: u32, coded_h: u32, frames: usize) -> Stats {
    let buf = synth_nv12(w, coded_h, 0);
    let view = Nv12View::new(&buf, (w, coded_h), (w, h)).expect("构造视图");
    let mut rgba = Vec::new();

    // 预热：第一次调用要建查表 + 首次触碰内存页，不能算进分布
    for _ in 0..3 {
        view.to_rgba(&mut rgba).unwrap();
    }

    let mut samples = Vec::with_capacity(frames);
    for i in 0..frames {
        let buf_i = if i == 0 {
            None
        } else {
            // 换一帧内容，避免 CPU 缓存把整帧都留在 L2 里
            Some(synth_nv12(w, coded_h, i as u32))
        };
        let local;
        let v = match &buf_i {
            Some(b) => {
                local = Nv12View::new(b, (w, coded_h), (w, h)).unwrap();
                &local
            }
            None => &view,
        };
        let t = Instant::now();
        v.to_rgba(&mut rgba).unwrap();
        samples.push(t.elapsed().as_secs_f64() * 1e6);
    }

    let s = Stats::from_samples(samples);
    let mp = (w as f64 * h as f64) / 1e6;
    let gb = (w as f64 * h as f64 * 4.0) / 1e9;
    println!(
        "{label:<28} {w}x{h:<5} min {:>7.1}  p50 {:>7.1}  p95 {:>7.1}  max {:>7.1}  mean {:>7.1} µs   {:>6.1} MP/s   {:>5.2} GB/s",
        s.min_us,
        s.p50_us,
        s.p95_us,
        s.max_us,
        s.mean_us,
        mp / (s.p50_us / 1e6),
        gb / (s.p50_us / 1e6),
    );
    s
}

fn main() {
    println!("=== sidelens-render NV12 → RGBA 转换性能 ===");
    let profile = if cfg!(debug_assertions) {
        "debug（未优化，数字仅供参考）"
    } else {
        "release（目标：720p 单帧 < 2ms）"
    };
    println!("构建：{profile}");
    println!("矩阵：{}", Matrix::default().as_str());
    println!();

    println!("{:<28} {:<11} 耗时分布 / 吞吐", "场景", "尺寸");
    println!("{}", "-".repeat(118));

    let mut worst_720 = 0.0f64;

    let s = bench("720p coded==display", 1280, 720, 720, 200);
    worst_720 = worst_720.max(s.p50_us);

    let s2 = bench("720p coded 728 → 720", 1280, 720, 728, 200);
    worst_720 = worst_720.max(s2.p50_us);

    bench("1080p coded 1088 → 1080", 1920, 1080, 1088, 100);
    bench("360p coded 368 → 360", 640, 360, 368, 400);
    bench("4K coded 2176 → 2160", 3840, 2160, 2176, 30);

    // BT.601 走另一套查表，确认两条路径性能一致
    let buf = synth_nv12(1280, 720, 0);
    let view = Nv12View::new(&buf, (1280, 720), (1280, 720)).unwrap();
    let mut out = Vec::new();
    for _ in 0..3 {
        view.to_rgba_with(&mut out, Matrix::Bt601).unwrap();
    }
    let mut samples = Vec::new();
    for _ in 0..200 {
        let t = Instant::now();
        view.to_rgba_with(&mut out, Matrix::Bt601).unwrap();
        samples.push(t.elapsed().as_secs_f64() * 1e6);
    }
    let b601 = Stats::from_samples(samples);
    println!(
        "{:<28} {:<11} min {:>7.1}  p50 {:>7.1}  p95 {:>7.1}  max {:>7.1}  mean {:>7.1} µs",
        "720p BT.601（对照）",
        "1280x720",
        b601.min_us,
        b601.p50_us,
        b601.p95_us,
        b601.max_us,
        b601.mean_us
    );

    println!();
    println!("=== 结论 ===");
    let target = 2000.0;
    println!("720p p50 最差：{worst_720:.1} µs（目标 < {target:.0} µs）");
    if worst_720 < target {
        println!(
            "  [PASS] 余量 {:.1}x —— 还有很大空间，暂时不需要 SIMD",
            target / worst_720
        );
    } else {
        println!(
            "  [FAIL] 超目标 {:.1}x —— 需要上查表优化 / SIMD",
            worst_720 / target
        );
    }

    if cfg!(debug_assertions) {
        println!();
        println!("提示：这是 debug 构建。要看真实数字请加 --release。");
    }
}
