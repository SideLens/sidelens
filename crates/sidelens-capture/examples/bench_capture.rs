//! 对比 GDI vs DXGI 的采集耗时（各 100 帧，报 p50 / p95）
//!
//! # 为什么用 p50 / p95 而不是平均值
//!
//! 「超低延迟」关心的是**尾延迟**。平均 5 ms 但每 20 帧出现一次 40 ms
//! 的尖刺，用户感受到的是卡顿而不是「平均很快」。p95 就是那个尖刺的代表。
//!
//! # 为什么先跑 10 帧预热
//!
//! 第一帧包含：D3D 设备创建后的首次 PSO 编译、staging 纹理首次触碰
//! （缺页 + 显存分配）、CPU 频率爬升。这些都不代表稳态。预热帧计入
//! `frames()` 但不进统计。
//!
//! # 用法
//!
//! `cargo run -p sidelens-capture --release --example bench_capture`
//!
//! 可选环境变量 `SIDELENS_BENCH_FRAMES` 改帧数（默认 100）。

#[cfg(windows)]
mod imp {
    //! Windows 实现

    #![allow(unsafe_code)]

    use std::time::Instant;

    use sidelens_capture::{CaptureSource, ScreenCapture};

    /// 预热帧数（不进统计）
    const WARMUP: usize = 10;

    /// 一次测量结果
    struct Measurement {
        name: String,
        size: (u32, u32),
        coded: (u32, u32),
        samples: Vec<f64>,
        note: String,
    }

    impl Measurement {
        fn p(&self, q: f64) -> f64 {
            if self.samples.is_empty() {
                return 0.0;
            }
            let mut v = self.samples.clone();
            v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            // nearest-rank：idx = ceil(q * n) - 1，钳到合法范围
            let n = v.len();
            let idx = ((q * n as f64).ceil() as usize)
                .saturating_sub(1)
                .min(n - 1);
            v[idx]
        }

        fn mean(&self) -> f64 {
            if self.samples.is_empty() {
                return 0.0;
            }
            self.samples.iter().sum::<f64>() / self.samples.len() as f64
        }
    }

    /// 跑一个后端
    fn measure(name: &str, cap: &mut ScreenCapture, frames: usize, note: String) -> Measurement {
        let size = cap.display_size().unwrap_or((0, 0));
        let mut samples = Vec::with_capacity(frames);
        let mut coded = (0, 0);
        let mut misses = 0usize;

        // 预热
        for _ in 0..WARMUP {
            match cap.capture() {
                Ok(Some(f)) => {
                    coded = (f.coded_width, f.coded_height);
                }
                Ok(None) => {}
                Err(e) => {
                    return Measurement {
                        name: name.to_string(),
                        size,
                        coded: (0, 0),
                        samples: Vec::new(),
                        note: format!("{note}；预热阶段失败：{e}"),
                    };
                }
            }
        }

        for _ in 0..frames {
            let t0 = Instant::now();
            let r = cap.capture();
            let dt = t0.elapsed().as_secs_f64() * 1e6;
            match r {
                Ok(Some(f)) => {
                    coded = (f.coded_width, f.coded_height);
                    samples.push(dt);
                }
                // DXGI 在桌面静止时返回 None，这是正常现象；不计入统计，
                // 但要知道有多少帧是这样（否则「100 帧」可能只有 30 个样本）
                Ok(None) => misses += 1,
                Err(e) => {
                    return Measurement {
                        name: name.to_string(),
                        size,
                        coded,
                        samples,
                        note: format!("{note}；采集中途失败：{e}"),
                    };
                }
            }
        }

        let note = if misses > 0 {
            format!("{note}；另有 {misses} 次空轮询（桌面无变化，不计入样本）")
        } else {
            note
        };
        Measurement {
            name: name.to_string(),
            size,
            coded,
            samples,
            note,
        }
    }

    fn print(m: &Measurement) {
        if m.samples.is_empty() {
            println!("  {:<10} 无样本 —— {}", m.name, m.note);
            return;
        }
        println!(
            "  {:<10} {:>4}x{:<4} (coded {}x{})  p50 {:>7.2} ms  p95 {:>7.2} ms  mean {:>7.2} ms  min {:>7.2} ms  n={}",
            m.name,
            m.size.0,
            m.size.1,
            m.coded.0,
            m.coded.1,
            m.p(0.50) / 1000.0,
            m.p(0.95) / 1000.0,
            m.mean() / 1000.0,
            m.samples.iter().cloned().fold(f64::INFINITY, f64::min) / 1000.0,
            m.samples.len()
        );
        if !m.note.is_empty() {
            println!("             {}", m.note);
        }
    }

    pub fn main() {
        let frames: usize = std::env::var("SIDELENS_BENCH_FRAMES")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(100);

        println!("SideLens 采集后端压测（每后端 {frames} 帧，预热 {WARMUP} 帧）");
        println!("注意：必须用 --release 跑，debug 构建的像素循环慢一到两个数量级");
        println!();

        let mut results = Vec::new();

        // ---- GDI ----
        match ScreenCapture::new_gdi(CaptureSource::PrimaryMonitor) {
            Ok(mut cap) => {
                let m = measure("GDI", &mut cap, frames, String::new());
                print(&m);
                results.push(m);
            }
            Err(e) => println!("  GDI        不可用：{e}"),
        }

        // ---- DXGI ----
        match ScreenCapture::new_dxgi(CaptureSource::PrimaryMonitor) {
            Ok(mut cap) => {
                let m = measure("DXGI", &mut cap, frames, String::new());
                print(&m);
                results.push(m);
            }
            Err(e) => println!(
                "  DXGI       不可用（stage={} hresult={:?}）：{e}",
                e.stage(),
                e.hresult_hex()
            ),
        }

        // ---- 合成源（标尺：不含任何平台调用） ----
        {
            let size = ScreenCapture::new_gdi(CaptureSource::PrimaryMonitor)
                .ok()
                .and_then(|c| c.display_size())
                .unwrap_or((1920, 1080));
            let mut cap = ScreenCapture::synthetic(size.0, size.1);
            let m = measure(
                "Synthetic",
                &mut cap,
                frames,
                format!(
                    "同分辨率合成图案，用来剥离平台调用成本（{}x{}）",
                    size.0, size.1
                ),
            );
            print(&m);
            results.push(m);
        }

        println!();
        // 相对加速比
        if results.len() >= 2 {
            if let (Some(gdi), Some(dxgi)) = (
                results.iter().find(|m| m.name == "GDI"),
                results.iter().find(|m| m.name == "DXGI"),
            ) {
                if !gdi.samples.is_empty() && !dxgi.samples.is_empty() {
                    let ratio = gdi.p(0.50) / dxgi.p(0.50);
                    println!("DXGI 相对 GDI 的 p50 加速比：{ratio:.1}x");
                }
            }
            // 注意：**不要**拿 DXGI 减 Synthetic 来算「平台开销」——
            // Synthetic 走的是 render_pattern（直接生成 NV12），
            // 而 DXGI 走的是 CopyResource + BGRA→NV12，两条路径的
            // CPU 工作量完全不同，相减没有物理意义。
            // 真正的分解见下面「纯 BGRA→NV12」那一段。
        }

        // ---- 纯转换微基准（不碰平台 API，与屏幕分辨率无关） ----
        //
        // 上面的 Synthetic 是「按当前屏幕分辨率渲染 + 拷贝」，它把
        // 「合成图案生成」也算进去了。要看纯粹的 BGRA→NV12 成本，
        // 得单独跑这个 —— 而且它能给出 1080p 的数字，
        // 即使当前屏幕是 4K。
        println!();
        println!("纯 BGRA→NV12 转换成本（不含任何平台调用）：");
        for (w, h) in [(640u32, 360u32), (1920, 1080), (3840, 2160)] {
            let l = match sidelens_capture::FrameLayout::new(w, h) {
                Ok(l) => l,
                Err(e) => {
                    println!("  {w}x{h} 跳过：{e}");
                    continue;
                }
            };
            let stride = w as usize * 4;
            let mut src = vec![0u8; stride * h as usize];
            for (i, b) in src.iter_mut().enumerate() {
                *b = (i % 251) as u8;
            }
            let mut dst = vec![0u8; l.nv12_len()];
            let mut samples = Vec::with_capacity(frames);
            for _ in 0..frames {
                let t0 = Instant::now();
                if let Err(e) = sidelens_capture::bgra_to_nv12(&src, stride, l, &mut dst) {
                    println!("  {w}x{h} 转换失败：{e}");
                    break;
                }
                samples.push(t0.elapsed().as_secs_f64() * 1e6);
            }
            if samples.is_empty() {
                continue;
            }
            let m = Measurement {
                name: format!("{w}x{h}"),
                size: (w, h),
                coded: (l.coded_width(), l.coded_height()),
                samples,
                note: String::new(),
            };
            println!(
                "  {:>9}  p50 {:>6.2} ms  p95 {:>6.2} ms  （{} 字节 NV12）",
                m.name,
                m.p(0.50) / 1000.0,
                m.p(0.95) / 1000.0,
                l.nv12_len()
            );
        }

        // ---- 缩放成本（4K 源 → 各目标尺寸） ----
        //
        // 这是「超低延迟」最关键的一组数字：4K 屏 + 720p 编码器时，
        // 缩放是否比不缩放更慢？面积平均读的源数据一样多（甚至更多，
        // 因为要读 3x3 而不是 1x1），但写的目标只有 1/9。
        println!();
        println!("缩放 + 转换成本（源 3840x2160，不含平台调用）：");
        let src_w = 3840u32;
        let src_h = 2160u32;
        let stride = src_w as usize * 4;
        let mut big_src = vec![0u8; stride * src_h as usize];
        for (i, b) in big_src.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }

        for (dw, dh) in [
            (3840u32, 2160u32), // 不缩放（基准）
            (1920, 1080),       // 2:1 面积平均
            (1280, 720),        // 3:1 面积平均
            (1920, 1080 + 1),   // 非整数倍（最近邻），故意错开一行
        ] {
            let l = match sidelens_capture::FrameLayout::new(dw, dh) {
                Ok(l) => l,
                Err(e) => {
                    println!("  {dw}x{dh} 跳过：{e}");
                    continue;
                }
            };
            let mut dst = vec![0u8; l.nv12_len()];
            let mut samples = Vec::with_capacity(frames);
            for _ in 0..frames {
                let t0 = Instant::now();
                if let Err(e) = sidelens_capture::bgra_to_nv12_scaled(
                    &big_src,
                    stride,
                    (src_w, src_h),
                    l,
                    &mut dst,
                ) {
                    println!("  {dw}x{dh} 失败：{e}");
                    break;
                }
                samples.push(t0.elapsed().as_secs_f64() * 1e6);
            }
            if samples.is_empty() {
                continue;
            }
            let m = Measurement {
                name: format!("{dw}x{dh}"),
                size: (dw, dh),
                coded: (l.coded_width(), l.coded_height()),
                samples,
                note: String::new(),
            };
            let mode = sidelens_capture::ScalePlan::new(src_w, src_h, dw, dh)
                .map(|p| p.mode().as_str())
                .unwrap_or("?");
            println!(
                "  3840x2160 → {:>9}  模式 {:>8}  p50 {:>6.2} ms  p95 {:>6.2} ms  （输出 {} 字节）",
                m.name,
                mode,
                m.p(0.50) / 1000.0,
                m.p(0.95) / 1000.0,
                l.nv12_len()
            );
        }
    }
}

#[cfg(windows)]
fn main() {
    imp::main();
}

#[cfg(not(windows))]
fn main() {
    eprintln!(
        "此示例只测 Windows 后端；非 Windows 上请用 cargo test -p sidelens-capture 跑合成源测试"
    );
}
