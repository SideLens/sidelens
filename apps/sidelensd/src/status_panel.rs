//! daemon 的状态面板（原生 Win32 UI）
//!
//! # 为什么单独一个模块
//!
//! `udp_server.rs` 已经 2000+ 行，主循环里塞 UI 代码会让「网络逻辑」和
//! 「界面逻辑」纠缠在一起 —— 而这两者的调试方式完全不同。
//!
//! 这个模块只做三件事：
//!
//! 1. **建窗口**：标题栏 + 状态标签 + 视频区 + 开关
//! 2. **喂数据**：主循环每 N 个统计窗口调一次 [`StatusPanel::update`]，
//!    把 `frames` / `playable_ms` / `loss` 这些数字写进标签
//! 3. **出事件**：主循环每 tick 调一次 [`StatusPanel::pump`]，返回用户操作
//!
//! # 为什么默认关闭（`SIDELENS_UI` 未设置时不建）
//!
//! 1. 第三十九到五十二轮的所有延迟数字都是在「无 UI」下测的，
//!    默认开 UI 会让那些数字失去可比性（多一个窗口合成器参与）
//! 2. CI / 无桌面环境里建窗口会失败，默认关闭省掉一条降级分支
//!
//! # 与渲染层的关系
//!
//! 本模块**不画视频**。它通过 [`StatusPanel::video_rect`] 暴露一块客户区
//! 矩形（物理像素），由 `sidelens-render-gpu` 在那块区域建 swap chain。
//! 这样 UI 层不需要知道 D3D11 的存在，渲染层也不需要知道按钮在哪。

#![cfg(windows)]

use sidelens_ui::{ControlId, MainWindow, UiEvent};
use tracing::{info, warn};

/// 面板的初始客户区尺寸（逻辑像素，会被 DPI 缩放）
///
/// 900x640 是「1080p 屏幕 100% 缩放」下的舒适尺寸。200% 缩放下物理像素
/// 会变成 1800x1280 —— 那台机器的屏幕通常是 4K，放得下。
const INITIAL_W: u32 = 900;
const INITIAL_H: u32 = 640;

/// 状态面板
pub struct StatusPanel {
    win: MainWindow,
    /// 「连接状态」标签
    lbl_state: ControlId,
    /// 「延迟/丢包」标签
    lbl_stats: ControlId,
    /// 「分辨率/帧数」标签
    lbl_video: ControlId,
    /// 「启用音频」复选框
    chk_audio: ControlId,
    /// 音频开关的当前值（UI 状态，业务侧读这个）
    audio_enabled: bool,
    /// 用户点了关闭
    closed: bool,
}

impl StatusPanel {
    /// 建窗口。失败返回 `None` —— 调用方应继续跑无 UI 模式，不要中断接收。
    ///
    /// # 为什么先 `new_hidden` 再 `show`
    ///
    /// `new` 建完立刻显示。如果后面还要建十几个控件，用户会先看到空窗口
    /// 再看到控件一个个冒出来（实测 4-5ms 的闪烁）。正确顺序是
    /// hidden -> 建控件 -> show。
    pub fn new(title: &str) -> Option<Self> {
        let mut win = match MainWindow::new_hidden(title, INITIAL_W, INITIAL_H) {
            Ok(w) => w,
            Err(e) => {
                warn!(error = %e, "状态面板创建失败（无桌面环境？），继续无 UI 模式");
                return None;
            }
        };

        // 布局用逻辑像素；`MainWindow` 内部按 DPI 缩放到物理像素。
        // 这个文件里所有坐标都是逻辑像素 —— 不要混用，否则 200% 下会错位。
        let lbl_state = win.add_label("● 等待连接…", 16, 12, 860, 24);
        let lbl_stats = win.add_label("延迟 -   丢包 -", 16, 40, 860, 22);
        let lbl_video = win.add_label("视频 -", 16, 66, 860, 22);

        // 视频区：从 y=96 到 y=560，左右各留 16
        win.set_video_rect(16, 96, 868, 464);

        let chk_audio = win.add_checkbox("启用音频", true, 16, 574, 140, 26);

        win.show();

        info!(
            dpi = win.dpi(),
            client = ?win.client_size(),
            "状态面板已创建（原生 Win32 控件）"
        );

        Some(Self {
            win,
            lbl_state,
            lbl_stats,
            lbl_video,
            chk_audio,
            audio_enabled: true,
            closed: false,
        })
    }

    /// 处理消息。返回 `false` 表示用户关了窗口，调用方应退出主循环。
    ///
    /// # 为什么必须每 tick 调
    ///
    /// `pump` 用 `PeekMessageW`（非阻塞）。不调的话窗口不响应任何输入，
    /// 表现为「界面卡死」—— 而实际上业务逻辑还在跑。
    /// # 返回值
    ///
    /// `Some((w, h))` = 窗口尺寸变了，调用方应把新尺寸交给渲染器。
    /// `None` = 没有尺寸变化（或有别的事件已处理）。
    ///
    /// 为什么要把 resize 传出去：`StatusPanel` 只负责布局（更新视频区矩形），
    /// 而 **swap chain 的重建必须由持有渲染器的一方做** —— 面板不知道
    /// D3D11 的存在。
    pub fn pump(&mut self) -> Option<(u32, u32)> {
        if self.closed {
            return None;
        }
        let mut resized = None;
        while let Some(ev) = self.win.pump() {
            match ev {
                UiEvent::Closed => {
                    self.closed = true;
                    info!("用户关闭了状态面板，daemon 继续在后台接收");
                    // 不 return false：daemon 是常驻服务，关窗口不该停服务。
                    // 想停服务请 Ctrl+C 或杀掉进程。
                }
                UiEvent::CheckboxToggled(id, on) if id == self.chk_audio => {
                    self.audio_enabled = on;
                    info!(enabled = on, "音频开关已切换");
                }
                UiEvent::Resized(w, h) => {
                    // 视频区跟着客户区一起长：左右各留 16，底部留 96
                    let vw = (w as i32 - 32).max(64);
                    let vh = (h as i32 - 96 - 80).max(64);
                    self.win.set_video_rect(16, 96, vw, vh);
                    self.win.repaint();
                    resized = Some((vw as u32, vh as u32));
                }
                UiEvent::DpiChanged(dpi) => {
                    info!(dpi, "DPI 变化，布局已重算");
                }
                _ => {}
            }
        }
        resized
    }

    /// 更新状态文字。每个统计窗口调一次（默认 2 秒）。
    ///
    /// # 为什么不在每帧更新
    ///
    /// 每帧（16ms）调 `SetWindowTextW` 会让窗口过程不断重绘，
    /// 实测吃掉 2-3% CPU —— 而人眼根本看不清 60Hz 刷新的文字。
    /// 2 秒一次既够实时，又不干扰视频。
    pub fn update(&mut self, connected: bool, peer: &str, stats: &PanelStats) {
        if self.closed {
            return;
        }
        let state = if connected {
            format!("● 已连接  {peer}")
        } else {
            "● 等待连接…".to_string()
        };
        self.win.set_text(self.lbl_state, &state);

        let stats_text = format!(
            "延迟 p50 {:.2} ms  p95 {:.2} ms   丢包 {:.2}%   抖动缓冲 {:.1} ms",
            stats.playable_p50, stats.playable_p95, stats.loss_pct, stats.target_ms
        );
        self.win.set_text(self.lbl_stats, &stats_text);

        let video_text = format!(
            "视频 {}x{}   {} 帧   {:.1} fps   {:.1} Mbps",
            stats.width, stats.height, stats.frames, stats.fps, stats.mbps
        );
        self.win.set_text(self.lbl_video, &video_text);
    }

    /// 视频区矩形（客户区物理像素）：`(x, y, w, h)`
    pub fn video_rect(&self) -> (i32, i32, i32, i32) {
        self.win.video_rect()
    }

    /// 原生窗口句柄（交给渲染层当父窗口）
    ///
    /// 第五十四轮新增：GPU 渲染器以**子窗口**形式嵌进面板的视频区，
    /// 需要一个父句柄。返回 `isize` 而不是 `HWND`，是为了让本模块不
    /// 依赖 `windows` crate 的类型（面板本身只关心布局与文字）。
    pub fn hwnd(&self) -> isize {
        self.win.hwnd()
    }

    /// 音频开关的当前值
    pub fn audio_enabled(&self) -> bool {
        self.audio_enabled
    }
}

/// 面板要显示的数字
///
/// 抽成结构体而不是 8 个参数：调用点会变成
/// `update(true, peer, p50, p95, loss, target, w, h, frames, fps, mbps)` ——
/// 11 个位置参数里 6 个是浮点，读代码的人必须回看签名才知道哪个是哪个。
#[derive(Debug, Clone, Copy, Default)]
pub struct PanelStats {
    /// 可播放延迟 p50（毫秒）
    pub playable_p50: f64,
    /// 可播放延迟 p95（毫秒）
    pub playable_p95: f64,
    /// 丢包率（百分比，0-100）
    pub loss_pct: f64,
    /// 抖动缓冲目标（毫秒）
    pub target_ms: f64,
    /// 视频宽
    pub width: u32,
    /// 视频高
    pub height: u32,
    /// 累计解码帧数
    pub frames: u64,
    /// 接收帧率
    pub fps: f64,
    /// 码率（Mbps）
    pub mbps: f64,
}
