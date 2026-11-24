//! 真窗口演示：一个移动的彩色方块，跑 10 秒
//!
//! # 这个示例在演示什么
//!
//! 1. **完整链路**：CPU 造 NV12 → 上传 → 视频处理器 YUV→RGB → flip 呈现；
//! 2. **coded vs display**：帧缓冲按 16 对齐（`W x H_aligned`），
//!    但只显示 `W x H` —— 底部填充行是故意填成「刺眼的洋红」的，
//!    如果裁剪失效，窗口底部会立刻出现一条紫红条，一眼就能看见；
//! 3. **低延迟呈现**：flip-model + 帧延迟 1，画面跟随方块位置没有拖影。
//!
//! # 怎么读输出
//!
//! 每秒打一行：帧号、实际 fps、p50/p95 呈现耗时、GPU 侧转换耗时。
//! 关闭窗口即退出（也可以等 10 秒自动结束）。
//!
//! # 为什么方块每帧都换颜色
//!
//! 纯静态画面会被驱动或 DWM 做「内容没变」的优化，测出来的耗时是假的。
//! 让颜色每帧都变，才能量到真实的转换 + 呈现成本。

#[cfg(windows)]
mod imp {
    use std::time::{Duration, Instant};

    /// 演示时长
    const DURATION: Duration = Duration::from_secs(10);

    /// 方块边长
    const SQUARE: u32 = 160;

    /// 跑演示
    pub fn main() {
        let w = 960u32;
        let h = 540u32;
        // coded 高度按 16 对齐：540 -> 544
        let coded_h = h.div_ceil(16) * 16;

        println!("=== SideLens GPU window demo ===");
        println!("窗口 {w}x{h}，coded {w}x{coded_h}，方块 {SQUARE}px，时长 {DURATION:?}");

        let mut r = match sidelens_render_gpu::GpuRenderer::new("SideLens GPU demo", w, h) {
            Ok(r) => r,
            Err(e) => {
                println!("GPU 渲染器创建失败：{e}");
                println!("这台机器上可用的适配器：");
                for a in sidelens_render_gpu::win32::device::GpuDevice::list_adapters() {
                    println!("  - {a}");
                }
                println!("（本示例需要 D3D11；无显卡环境请用 cargo test 验证跨平台逻辑）");
                return;
            }
        };

        println!(
            "适配器：{}（software={}）格式={}",
            r.adapter_name(),
            r.is_software(),
            r.swapchain_format()
        );

        let layout = sidelens_render_gpu::Nv12Layout::new((w, coded_h), (w, h)).expect("尺寸合法");
        let mut frame = sidelens_render_gpu::solid_nv12(layout, 16, 128);

        // 底部填充行刷成洋红：裁剪失效时会立刻暴露
        {
            let y_len = layout.y_len();
            let stride = w as usize;
            let pad_start = h as usize * stride;
            for b in &mut frame[pad_start..y_len] {
                *b = 90;
            }
            // 色度也染成洋红（U 高 V 高 ≈ 紫红）
            let uv_base = y_len;
            for pair in frame[uv_base..].chunks_exact_mut(2) {
                pair[0] = 200;
                pair[1] = 140;
            }
        }

        let start = Instant::now();
        let mut last_report = Instant::now();
        let mut last_frames = 0u64;
        let mut x = 0i32;
        let mut y = 0i32;
        let mut dx = 7i32;
        let mut dy = 4i32;
        let mut hue = 0u8;

        while r.pump_events() {
            if start.elapsed() >= DURATION {
                break;
            }

            // 用背景色填满整帧，再画方块
            let y_len = layout.y_len();
            frame[..y_len].fill(16);
            let bg_uv = &mut frame[y_len..];
            for pair in bg_uv.chunks_exact_mut(2) {
                pair[0] = 128;
                pair[1] = 128;
            }

            // 方块：颜色每帧微调，避免静态优化
            hue = hue.wrapping_add(3);
            let sq_y = 40u8.wrapping_add(hue);
            sidelens_render_gpu::fill_rect_nv12(
                &mut frame,
                layout,
                sidelens_render_gpu::Rect::new(x, y, x + SQUARE as i32, y + SQUARE as i32),
                sq_y,
                128,
            );

            // 反弹
            x += dx;
            y += dy;
            if x < 0 || x + SQUARE as i32 > w as i32 {
                dx = -dx;
                x = x.clamp(0, w as i32 - SQUARE as i32);
            }
            if y < 0 || y + SQUARE as i32 > h as i32 {
                dy = -dy;
                y = y.clamp(0, h as i32 - SQUARE as i32);
            }

            if let Err(e) = r.present(&frame, (w, coded_h), (w, h)) {
                println!("present 失败：{e}");
                break;
            }

            // 每秒报一次
            if last_report.elapsed() >= Duration::from_secs(1) {
                let frames = r.frames();
                let delta = frames - last_frames;
                let secs = last_report.elapsed().as_secs_f64();
                let (p50, p95, _max) = r.present_percentiles();
                let (g50, _g95, _gmax) = r.gpu_percentiles();
                println!(
                    "帧 {frames:5}  {:.1} fps  present p50={p50:.0}us p95={p95:.0}us  gpu p50={g50:.0}us  upload={:.0}us",
                    delta as f64 / secs,
                    r.last_upload_us()
                );
                last_report = Instant::now();
                last_frames = frames;
            }
        }

        let elapsed = start.elapsed().as_secs_f64();
        let (p50, p95, max) = r.present_percentiles();
        println!();
        println!(
            "结束：{} 帧 / {elapsed:.2}s = {:.1} fps",
            r.frames(),
            r.frames() as f64 / elapsed
        );
        println!("present p50={p50:.0}us p95={p95:.0}us max={max:.0}us");
        println!("gpu avg={:.0}us", r.avg_gpu_us());
    }
}

#[cfg(windows)]
fn main() {
    imp::main();
}

#[cfg(not(windows))]
fn main() {
    println!("window_demo 需要 Windows（D3D11 + Win32 窗口都是 Windows 专属 API）。");
}
