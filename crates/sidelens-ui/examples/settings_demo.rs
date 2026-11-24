//! SideLens 设置界面演示：开一个真窗口，摆出实际的设置页，跑 5 秒自动退出
//!
//! # 为什么用「定时器 + sleep 循环」而不是阻塞消息循环
//!
//! 这个 demo 要同时演示两件事：界面长什么样、**pump() 是给 1ms tick 主循环用的**。
//! 如果写成 `GetMessageW` 的阻塞循环，就完全演示不出「主循环在两次消息之间
//! 还能干别的事」这个核心特性 —— 而 SideLens 的真实主循环正是这样。
//!
//! # 跨平台
//!
//! 窗口代码全部放进 `imp` 模块并加 `#[cfg(windows)]`，非 Windows 上只有
//! 一个打印提示的 `main`。**不要**在文件顶部写 `#![cfg(windows)]` ——
//! 那会把整个 `main` 删掉，其它平台编译直接 E0601（找不到 main）。
//!
//! # 输出里那几个数字是怎么来的
//!
//! - **建窗（隐藏）**：`new_hidden` 的耗时，不含显示。通常 2~5 ms。
//! - **pump 空转**：连调 10000 次 `pump()` 的平均值。这是判断「主循环能不能
//!   跑 1ms tick」的唯一硬指标 —— 若它是几百微秒，1ms 里就有一半时间在抽消息。
//! - **show()**：把已建好的窗口一次性显示出来的耗时，**含 DWM 首次合成**，
//!   是这台机器上桌面合成器的一次性开销，不是本 crate 的成本
//!   （裸 Win32 窗口首次 ShowWindow 在同样环境下也要 60~110 ms）。
//!
//! # 脚本动作
//!
//! 没有真人点鼠标时，demo 用 `click()` 按时间表「点」几个控件。它走的是
//! `BM_CLICK → WM_COMMAND → 事件队列` 这条**和真人完全一样**的路，
//! 所以输出里的 `[事件 N]` 就是用户真点出来的效果。

#[cfg(windows)]
mod imp {
    use std::time::{Duration, Instant};

    use sidelens_ui::{Anchor, ControlId, MainWindow, UiEvent};

    /// 演示窗口跑多久后自动退出
    const RUN_FOR: Duration = Duration::from_secs(5);

    /// 主循环 tick：1ms，和 SideLens 真实主循环一致
    const TICK: Duration = Duration::from_millis(1);

    /// 脚本动作：收到「控件三件套」后决定点哪个
    ///
    /// 抽成类型别名而不是写内联闭包类型：`Vec<(Duration, &str, fn(..))>`
    /// 直接内联时 rustc 会报「type is very complex」，而且读起来也难受。
    type Action = fn(&mut MainWindow, ControlId, ControlId, ControlId);

    pub fn main() -> Result<(), sidelens_ui::UiError> {
        // 设计尺寸：逻辑像素（96 DPI 下的像素），实际窗口会按 DPI 放大。
        //
        // 用 new_hidden + show 而不是 new：先把窗口和控件全部建好、布局算完，
        // 再一次性显示。直接 new 的话用户会先看到一个空窗口，几十毫秒后控件
        // 才一个个冒出来 —— 原生程序里这叫「白屏闪烁」，观感比慢一点还差。
        let t0 = Instant::now();
        let mut win = MainWindow::new_hidden("SideLens", 900, 640)?;
        let create_us = t0.elapsed().as_micros();

        println!(
            "窗口已创建（隐藏）：hwnd=0x{:x}，建窗 {create_us} µs",
            win.hwnd()
        );
        println!(
            "DPI = {}（{}%），客户区 = {}x{} 物理像素",
            win.dpi(),
            win.dpi() as f64 / 96.0 * 100.0,
            win.client_size().0,
            win.client_size().1
        );
        // ---- 顶部状态区 ----
        let status = win.add_label("连接状态：已连接 127.0.0.1:27200", 16, 12, 420, 22);
        let stats = win.add_label("延迟：12.4 ms    丢包：0.3%", 16, 38, 420, 22);
        let fps = win.add_label("帧率：60.0 fps   码率：8.2 Mbps", 16, 64, 420, 22);

        // ---- 视频区：由渲染层负责，这里只预留 ----
        //
        // 高 372 = 640（设计客户区高）− 96（顶部状态区）− 92（开关行 + 下拉框）
        // − 80（底部按钮行）。写成常量而不是 `640 - 268`，是为了让「哪一段占了
        // 多少」一眼可见；改布局时改这里一个数就够。
        const VIDEO_H: i32 = 372;
        win.set_video_rect(16, 96, 868, VIDEO_H);

        let (vx, vy, vw, vh) = win.video_rect();
        println!(
            "视频区 = ({vx}, {vy}, {vw}, {vh}) 物理像素（设计值 16,96,868,{VIDEO_H} 逻辑像素）"
        );

        // ---- 开关行（紧贴视频区下沿 + 8）----
        const ROW_Y: i32 = 96 + VIDEO_H + 8;
        let audio = win.add_checkbox("启用音频", true, 16, ROW_Y, 120, 24);
        let camera = win.add_checkbox("启用摄像头", false, 152, ROW_Y, 140, 24);
        let mic = win.add_checkbox("麦克风降噪", true, 308, ROW_Y, 140, 24);

        // ---- 分辨率下拉框 ----
        let _res_label = win.add_label("分辨率：", 16, ROW_Y + 38, 70, 24);
        let res = win.add_combo(
            &["1920x1080", "2560x1440", "3840x2160", "1280x720"],
            0,
            92,
            ROW_Y + 34,
            200,
            26,
        );

        // ---- 底部按钮行（贴右下，窗口拉大时跟着走）----
        let start = win.add_button("开始", 560, 596, 100, 32);
        let settings = win.add_button("设置", 672, 596, 100, 32);
        let quit = win.add_button("退出", 784, 596, 100, 32);
        for id in [start, settings, quit] {
            win.set_anchor(id, Anchor::BottomRight);
        }
        win.set_anchor(res, Anchor::BottomLeft);
        win.set_anchor(_res_label, Anchor::BottomLeft);
        win.set_anchor(audio, Anchor::BottomLeft);
        win.set_anchor(camera, Anchor::BottomLeft);
        win.set_anchor(mic, Anchor::BottomLeft);

        // 焦点给「开始」，回车就能触发（IsDialogMessageW 把回车翻译成 BN_CLICKED）
        win.focus(start);

        // 实测 pump 的单次开销：这是「主循环能不能做到 1ms tick」的关键指标。
        // 用黑盒计时（Instant）而不是内部计数器，测的是调用方真实感受到的延迟。
        // 必须在进入事件循环**之前**测：循环里点了「退出」会提前 return。
        let t1 = Instant::now();
        const PUMP_ROUNDS: u32 = 10_000;
        for _ in 0..PUMP_ROUNDS {
            let _ = win.pump();
        }
        let pump_ns = t1.elapsed().as_nanos() / u128::from(PUMP_ROUNDS);
        println!("实测 pump 空转：{pump_ns} ns/次（{PUMP_ROUNDS} 次平均）");

        // 控件都建好了，现在一次性显示
        let t2 = Instant::now();
        win.show();
        println!(
            "窗口显示完成，show() 耗时 {} µs（含 DWM 首次合成）",
            t2.elapsed().as_micros()
        );

        println!("控件就绪：3 个标签 / 3 个复选框 / 1 个下拉框 / 3 个按钮");
        println!("开始 5 秒演示（每 500ms 刷新一次延迟数字，模拟真实数据流）...");

        // ---- 主循环：pump 非阻塞，两次消息之间照样能干活 ----
        let begin = Instant::now();
        let mut ticks: u64 = 0;
        let mut events: u64 = 0;
        let mut last_refresh = Instant::now();
        let mut frame: u32 = 0;

        // 脚本化的「用户操作」：没有真人点鼠标时用它演示事件链路。
        // 走的是 click() → BM_CLICK → WM_COMMAND → 事件队列，和真人点完全同一条路。
        let mut script: Vec<(Duration, &str, Action)> = vec![
            (
                Duration::from_millis(1200),
                "点「启用摄像头」",
                |w, camera, _, _| w.click(camera),
            ),
            (
                Duration::from_millis(2400),
                "点「开始」",
                |w, _, start, _| w.click(start),
            ),
            (
                Duration::from_millis(4800),
                "点「退出」",
                |w, _, _, quit| w.click(quit),
            ),
        ];
        script.reverse(); // 从队尾取，省一次 remove(0)

        let mut resized = false;
        while begin.elapsed() < RUN_FOR {
            // 3.6 秒时把客户区拉大：走的是和用户拖边框完全相同的 WM_SIZE 路径，
            // 用来验证「控件重排 + 视频区跟着长」。
            if !resized && begin.elapsed() >= Duration::from_millis(3600) {
                resized = true;
                println!("-- 把客户区拉到 1200x820 逻辑像素，验证重排");
                win.set_client_size(1200, 820);
            }

            // 到点就「点」一下，模拟用户操作
            while let Some((at, label, action)) = script.last() {
                if begin.elapsed() < *at {
                    break;
                }
                println!("-- 脚本动作 @{:?}：{label}", at);
                action(&mut win, camera, start, quit);
                script.pop();
            }

            // 抽干事件队列；pump 立刻返回 None 表示当前没有事件
            while let Some(ev) = win.pump() {
                events += 1;
                println!("[事件 {:>3}] {ev}", events);
                match ev {
                    UiEvent::ButtonClicked(id) if id == quit => {
                        println!("点到了「退出」，提前结束");
                        return Ok(());
                    }
                    UiEvent::ButtonClicked(id) if id == start => {
                        win.set_text(status, "连接状态：正在连接…");
                        win.set_enabled(start, false);
                    }
                    UiEvent::CheckboxToggled(id, on) => {
                        if id == camera {
                            // 摄像头关掉时，分辨率下拉框没有意义
                            win.set_enabled(res, on);
                        }
                    }
                    UiEvent::Closed => {
                        println!("窗口已关闭");
                        return Ok(());
                    }
                    UiEvent::Resized(w, h) => {
                        let (vx, vy, vw, vh) = win.video_rect();
                        println!("        重排后视频区 = ({vx}, {vy}, {vw}, {vh})，客户区 {w}x{h}");
                    }
                    _ => {}
                }
            }

            // 模拟「1ms tick 里做别的事」：每 500ms 刷新一次统计数字
            if last_refresh.elapsed() >= Duration::from_millis(500) {
                last_refresh = Instant::now();
                frame += 1;
                let latency = 11.0 + (frame % 7) as f64 * 0.4;
                win.set_text(
                    stats,
                    &format!(
                        "延迟：{latency:.1} ms    丢包：{}%",
                        0.2 + (frame % 3) as f64 * 0.1
                    ),
                );
                win.set_text(
                    fps,
                    &format!(
                        "帧率：{}.0 fps   码率：{} Mbps",
                        59 + frame % 3,
                        82 + frame % 5
                    ),
                );
            }

            ticks += 1;
            std::thread::sleep(TICK);
        }

        println!(
            "演示结束。tick = {ticks} 次，事件 = {events} 个，最终客户区 = {:?}，视频区 = {:?}",
            win.client_size(),
            win.video_rect()
        );
        println!("实测：建窗 {create_us} µs（含 comctl32 首次加载），pump 空转 {pump_ns} ns/次");
        Ok(())
    }
}

#[cfg(windows)]
pub fn main() {
    if let Err(e) = imp::main() {
        eprintln!("settings_demo 失败：{e}");
        std::process::exit(1);
    }
}

#[cfg(not(windows))]
fn main() {
    println!("settings_demo 只在 Windows 上有原生窗口；");
    println!("非 Windows 平台请用 sidelens-ui 的 dpi / layout 单元测试验证逻辑层。");
}
