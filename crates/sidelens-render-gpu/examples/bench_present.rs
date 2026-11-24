//! GDI vs GPU 的 1080p 单帧呈现耗时对比（各 200 帧）
//!
//! # 这是本轮最重要的数字
//!
//! 用户要求「超低延迟 + 极致的原生 ui 性能」。这个示例把两条渲染路径放在
//! 同一台机器、同一个 1080p 帧上跑，报 p50 / p95 —— 平均值会被首帧的管线
//! 创建稀释，p95 才是用户能感知到的卡顿。
//!
//! # 两条路径的差别
//!
//! ~~~text
//! GDI:  NV12 ──CPU 查表转 RGBA──▶ RGBA ──CPU 交换 R/B──▶ BGRA ──StretchDIBits──▶ 窗口
//!       ^ 3.1 MB 全帧遍历        ^ 3.1 MB 全帧遍历      ^ 3.1 MB 再拷一遍
//!
//! GPU:  NV12 ──Map staging memcpy──▶ NV12 纹理 ──VideoProcessorBlt(GPU)──▶ flip 呈现
//!       ^ 3.1 MB 一次 memcpy（无逐像素运算）
//! ~~~
//!
//! # 怎么读结果
//!
//! - `convert` 列：GDI 路径的 CPU 转换耗时（纯 CPU，必 >0）
//! - `blit` 列：GDI 的 StretchDIBits 耗时
//! - `upload` 列：GPU 路径的 Map + memcpy 耗时
//! - `gpu` 列：GPU 上视频处理器的实际耗时（timestamp query）
//! - `present` 列：整帧端到端（含 Present 等待）
//!
//! # cfg 门控
//!
//! GDI 和 D3D11 都是 Windows 专属 API，所以**全部实现**放在
//! `#[cfg(windows)] mod imp` 里，非 Windows 的 `main` 只打印一句话。
//! 千万不要在文件顶部写 `#![cfg(windows)]` —— 那会把 `main` 一起删掉，
//! 链接期报 E0601「main function not found」。

#[cfg(windows)]
mod imp {
    use std::time::Instant;

    /// 基准帧数（每边）
    const FRAMES: usize = 200;

    /// 跑基准
    pub fn main() {
        let coded = (1920u32, 1088u32);
        let display = (1920u32, 1080u32);

        println!("=== SideLens 1080p 单帧呈现基准（各 {FRAMES} 帧）===");
        println!("帧：coded {coded:?} / display {display:?}");
        println!();

        bench_gdi(coded, display);
        println!();
        bench_gpu(coded, display);
        println!();
        println!("说明：present 列含 Present 的垂直同步/帧延迟等待；p50/p95 更能反映体感。");
    }

    /// GDI 路径（sidelens-render）
    fn bench_gdi(coded: (u32, u32), display: (u32, u32)) {
        println!("--- GDI (sidelens-render) ---");

        let nv12 = bench_frame(coded);
        let view = match sidelens_render::Nv12View::new(&nv12, coded, display) {
            Ok(v) => v,
            Err(e) => {
                println!("构造 Nv12View 失败：{e}");
                return;
            }
        };

        // 先量纯转换（无窗口），这条在任何环境都能跑
        let mut rgba: Vec<u8> = Vec::new();
        let mut convert_us: Vec<f64> = Vec::with_capacity(FRAMES);
        for _ in 0..FRAMES {
            let t = Instant::now();
            if let Err(e) = view.to_rgba(&mut rgba) {
                println!("转换失败：{e}");
                return;
            }
            convert_us.push(t.elapsed().as_secs_f64() * 1e6);
        }
        let (c50, c95) = pct(&mut convert_us);
        println!(
            "  纯 CPU 转换：p50={c50:.0}us p95={c95:.0}us（{}x{} = {} 像素）",
            display.0,
            display.1,
            display.0 as usize * display.1 as usize
        );

        // 再量带窗口的完整路径
        let mut renderer =
            match sidelens_render::Renderer::new("SideLens GDI bench", display.0, display.1) {
                Ok(r) => r,
                Err(e) => {
                    println!("  GDI 窗口不可用（{e}），跳过 blit 测量");
                    return;
                }
            };

        let mut present_us: Vec<f64> = Vec::with_capacity(FRAMES);
        for _ in 0..FRAMES {
            if !renderer.pump_events() {
                break;
            }
            let t = Instant::now();
            if let Err(e) = renderer.present(&view) {
                println!("  present 失败：{e}");
                return;
            }
            present_us.push(t.elapsed().as_secs_f64() * 1e6);
        }

        let frames = renderer.frames();
        if frames > 0 && !present_us.is_empty() {
            let (p50, p95) = pct(&mut present_us);
            let avg = present_us.iter().sum::<f64>() / present_us.len() as f64;
            // blit_ns / convert_ns 是累计值，除以帧数得到均值
            let blit_avg = renderer.blit_ns() as f64 / frames as f64 / 1000.0;
            let conv_avg = renderer.convert_ns() as f64 / frames as f64 / 1000.0;
            println!(
                "  端到端 {} 帧：avg={avg:.0}us p50={p50:.0}us p95={p95:.0}us",
                present_us.len()
            );
            println!("    其中 convert={conv_avg:.0}us blit={blit_avg:.0}us（累计均值）");
        }
    }

    /// GPU 路径（本 crate）
    fn bench_gpu(coded: (u32, u32), display: (u32, u32)) {
        println!("--- GPU (sidelens-render-gpu) ---");

        let nv12 = bench_frame(coded);

        let mut renderer =
            match sidelens_render_gpu::GpuRenderer::new("SideLens GPU bench", display.0, display.1)
            {
                Ok(r) => r,
                Err(e) => {
                    println!("  GPU 窗口不可用：{e}");
                    println!(
                        "  可用适配器：{:?}",
                        sidelens_render_gpu::win32::device::GpuDevice::list_adapters()
                    );
                    // 窗口建不出来时退到离屏，至少把转换耗时量出来
                    bench_gpu_offscreen(&nv12, coded, display);
                    return;
                }
            };

        println!(
            "  适配器：{}（software={}）格式={}",
            renderer.adapter_name(),
            renderer.is_software(),
            renderer.swapchain_format()
        );

        // 预热 10 帧：首帧要建管线（几十毫秒），不能计入统计
        for _ in 0..10 {
            if renderer.present(&nv12, coded, display).is_err() {
                break;
            }
        }

        let mut present_us: Vec<f64> = Vec::with_capacity(FRAMES);
        for _ in 0..FRAMES {
            if !renderer.pump_events() {
                break;
            }
            let t = Instant::now();
            if let Err(e) = renderer.present(&nv12, coded, display) {
                println!("  present 失败：{e}");
                return;
            }
            present_us.push(t.elapsed().as_secs_f64() * 1e6);
        }

        if present_us.is_empty() {
            println!("  0 帧：窗口在测量开始前就被关闭了");
            return;
        }
        let (p50, p95) = pct(&mut present_us);
        let (g50, g95, _gmax) = renderer.gpu_percentiles();
        let avg = present_us.iter().sum::<f64>() / present_us.len() as f64;
        println!(
            "  {} 帧：p50={p50:.0}us p95={p95:.0}us avg={avg:.0}us",
            present_us.len()
        );
        println!(
            "    其中 upload={:.0}us（CPU memcpy） gpu p50={g50:.0}us p95={g95:.0}us",
            renderer.last_upload_us()
        );
    }

    /// GPU 窗口建不出来时的兜底：量离屏转换
    fn bench_gpu_offscreen(nv12: &[u8], coded: (u32, u32), display: (u32, u32)) {
        let mut r = match sidelens_render_gpu::GpuRenderer::offscreen(display.0, display.1) {
            Ok(r) => r,
            Err(e) => {
                println!("  离屏也不可用：{e}");
                return;
            }
        };
        println!("  （降级到离屏）适配器：{}", r.adapter_name());

        for _ in 0..5 {
            let _ = r.present(nv12, coded, display);
        }
        let mut present_us: Vec<f64> = Vec::with_capacity(FRAMES);
        for _ in 0..FRAMES {
            let t = Instant::now();
            if r.present(nv12, coded, display).is_err() {
                break;
            }
            present_us.push(t.elapsed().as_secs_f64() * 1e6);
        }
        let (p50, p95) = pct(&mut present_us);
        let (g50, _, _) = r.gpu_percentiles();
        let avg = if present_us.is_empty() {
            0.0
        } else {
            present_us.iter().sum::<f64>() / present_us.len() as f64
        };
        println!(
            "  离屏 {} 帧：p50={p50:.0}us p95={p95:.0}us avg={avg:.0}us upload={:.0}us gpu_p50={g50:.0}us",
            present_us.len(),
            r.last_upload_us()
        );
    }

    /// 造一张 1080p 的渐变 NV12（有内容才不会被驱动优化掉）
    fn bench_frame(coded: (u32, u32)) -> Vec<u8> {
        let layout = sidelens_render_gpu::Nv12Layout::new(coded, coded).expect("1920x1088 合法");
        let mut buf = sidelens_render_gpu::solid_nv12(layout, 16, 128);
        let w = coded.0 as usize;
        let h = coded.1 as usize;
        // 横向渐变：让每帧的每个像素都不同，避免「内容没变」的优化
        for row in 0..h {
            let base = row * w;
            for col in 0..w {
                buf[base + col] = ((col * 219 / w) + 16) as u8;
            }
        }
        buf
    }

    /// 算 p50 / p95（升序排序后取最近秩）
    fn pct(v: &mut [f64]) -> (f64, f64) {
        if v.is_empty() {
            return (0.0, 0.0);
        }
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let n = v.len();
        let p = |q: f64| -> f64 {
            let rank = ((q / 100.0) * n as f64).ceil() as usize;
            v[rank.saturating_sub(1).min(n - 1)]
        };
        (p(50.0), p(95.0))
    }
}

#[cfg(windows)]
fn main() {
    imp::main();
}

#[cfg(not(windows))]
fn main() {
    println!("bench_present 需要 Windows（GDI 与 D3D11 都是 Windows 专属 API）。");
    println!("本 crate 的跨平台部分（尺寸/裁剪/统计）用 cargo test 验证。");
}
