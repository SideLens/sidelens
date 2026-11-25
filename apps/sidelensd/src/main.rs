//! SideLens 电脑端常驻服务
//!
//! 支持两种传输：
//!
//! - `--ws`：WebSocket over TCP（默认，兼容 JS 版）
//! - `--udp`：UDP + 分片 + NACK（真实丢包环境，低延迟正解）
//!
//! # 用法
//!
//! ```bash
//! sidelensd 27100              # WebSocket
//! sidelensd --udp 27200        # UDP
//! ```

// 平台无关的模块
mod phone_recv;
mod udp_server;

// 应用内更新与它的命令行外壳住在 lib 目标里（src/lib.rs），
// 这样 tests/update_http.rs 能直接对 HTTP 客户端做集成测试。
use sidelensd::update_cli;

// 手机摄像头 → 虚拟摄像头（需要 MF 解码器，仅 Windows）
#[cfg(windows)]
mod autostart;
#[cfg(windows)]
mod gui;
#[cfg(windows)]
mod phone_bridge;

// 仅 Windows：这三个模式都要么直接用 `windows` crate，要么用
// `sidelens-vcam` / `sidelens-ipc` 的 Windows 实现。
//
// # 为什么必须门控（cross-platform.md P1 的第二层）
//
// 第一版只改了 Cargo.toml 把平台无关依赖移出 `cfg(windows)` 块，
// 但 `mod` 声明还是无条件的 —— 于是在 Linux 上会立刻炸出 17 个
// E0433「找不到 sidelens_ipc / sidelens_vcam / windows」。
//
// 这个雷之所以难发现：它被 vcam 的 142 个错误**完全掩盖**了。
// 修好 vcam 之后它才浮出来，看起来像「移植带出来的新问题」，
// 实际是同一份代码里两处独立的疏漏。
// 视频解码器包装（第三十八轮，decode-wire 子代理交付）。
//
// 只在 Windows 上编译 —— 它依赖 sidelens-codec-mf（Media Foundation）。
// 非 Windows 上视频帧只统计延迟、不解码（与之前行为一致）。
#[cfg(windows)]
mod shm_feed;
#[cfg(windows)]
mod shm_probe;
#[cfg(windows)]
mod vcam_probe;
#[cfg(windows)]
mod vcam_selfcheck;
// 隔离实验：逐项对照官方 VCamSample 的调用序列（第十九轮）
#[cfg(windows)]
mod vcam_isolate;
// DirectShow 滤镜验证（第二十轮）
#[cfg(windows)]
mod dshow_probe;
#[cfg(windows)]
mod video_decode;
// 状态面板（第五十三轮）。原生 Win32 控件，`SIDELENS_UI=1` 时启用。
#[cfg(windows)]
mod status_panel;

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use anyhow::Result;
use sidelens_core::{jitter::QueuedFrame, HighResTimer, JitterBuffer, JitterConfig};
use sidelens_proto::{now_us, FrameType};
use sidelens_transport::WsServer;
use tracing::{debug, info, warn};

/// 滑动窗口里的延迟样本上限
const SAMPLE_CAP: usize = 4000;

/// 分位数统计
#[derive(Debug)]
struct LatencyStats {
    samples: VecDeque<f64>,
}

impl Default for LatencyStats {
    fn default() -> Self {
        Self {
            samples: VecDeque::with_capacity(SAMPLE_CAP),
        }
    }
}

impl LatencyStats {
    fn push(&mut self, ms: f64) {
        if !ms.is_finite() || ms < 0.0 {
            return;
        }
        if self.samples.len() == SAMPLE_CAP {
            self.samples.pop_front();
        }
        self.samples.push_back(ms);
    }

    fn percentile(&self, p: f64) -> f64 {
        if self.samples.is_empty() {
            return 0.0;
        }
        let mut v: Vec<f64> = self.samples.iter().copied().collect();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let idx = ((v.len() as f64 - 1.0) * p).round() as usize;
        v[idx.min(v.len() - 1)]
    }

    fn summary(&self) -> (f64, f64, f64) {
        (
            self.percentile(0.50),
            self.percentile(0.95),
            self.percentile(0.99),
        )
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "sidelensd=info".into()),
        )
        .without_time()
        .init();

    // 提升定时器分辨率到 1ms；否则 interval(1ms) 实际 15.6ms，
    // 帧会在抖动缓冲之外白等平均 7.8ms。
    let _hrt = HighResTimer::new();
    if HighResTimer::supported() {
        info!("定时器分辨率已提升到 1ms");
    }

    let args: Vec<String> = std::env::args().skip(1).collect();

    // Windows 桌面界面。界面进程通过隐藏 worker 子进程托管视频接收服务。
    #[cfg(windows)]
    if args.iter().any(|a| a == "--gui") {
        let background = args.iter().any(|a| a == "--background");
        return gui::run(background);
    }

    // 应用内更新（跨平台）。放在 --gui 之后、手机链路之前：
    // 更新是纯 CLI 动作，不该启动任何采集或监听。
    if let Some(index) = args.iter().position(|a| a == "--check-update") {
        let Some(url) = args.get(index + 1).filter(|v| !v.starts_with("--")) else {
            eprintln!("[update] --check-update 后需要清单地址（URL 或本地路径）");
            std::process::exit(2);
        };
        let current = flag_value(&args, "--current");
        let quiet = args.iter().any(|a| a == "--quiet");
        std::process::exit(update_cli::run_check(url, current.as_deref(), quiet));
    }
    if let Some(index) = args.iter().position(|a| a == "--update") {
        let Some(url) = args.get(index + 1).filter(|v| !v.starts_with("--")) else {
            eprintln!("[update] --update 后需要清单地址（URL 或本地路径）");
            std::process::exit(2);
        };
        let current = flag_value(&args, "--current");
        let stage_only = args.iter().any(|a| a == "--stage-only");
        std::process::exit(update_cli::run_update(url, stage_only, current.as_deref()));
    }
    if let Some(index) = args.iter().position(|a| a == "--write-manifest") {
        let Some(dir) = args.get(index + 1).filter(|v| !v.starts_with("--")) else {
            eprintln!("[manifest] --write-manifest 后需要目录");
            std::process::exit(2);
        };
        let version = flag_value(&args, "--version").unwrap_or_else(|| {
            // 默认取 crate 版本：与 Cargo.toml 的 workspace.package.version 一致
            env!("CARGO_PKG_VERSION").to_string()
        });
        let channel = flag_value(&args, "--channel").unwrap_or_else(|| "stable".into());
        let notes = flag_value(&args, "--notes").unwrap_or_default();
        let out = flag_value(&args, "--out");
        let min_code = number_flag(&args, "--phone-min-code").unwrap_or(0);
        let latest_code = number_flag(&args, "--phone-latest-code").unwrap_or(0);
        let latest_name = flag_value(&args, "--phone-latest-name").unwrap_or_default();
        let min_protocol = number_flag(&args, "--phone-min-protocol").unwrap_or(1);
        std::process::exit(update_cli::run_write_manifest(
            dir,
            &version,
            &channel,
            min_code,
            latest_code,
            &latest_name,
            min_protocol,
            &notes,
            out.as_deref(),
        ));
    }

    // 手机帧接收端（跨平台）。验证「摄像头 → 编码 → 网络 → 电脑」这条链。
    //
    // 单独一个模式而不是复用 udp_server：手机端第一步用的是裸 TCP，
    // 不带分片/NACK/FEC —— 那些是为弱网设计的，不是为「先跑通」设计的。
    if let Some(i) = args.iter().position(|a| a == "--phone-recv") {
        let port: u16 = args
            .get(i + 1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(27300);
        let out = args
            .iter()
            .position(|a| a == "--out")
            .and_then(|j| args.get(j + 1))
            .map(std::path::PathBuf::from);
        return phone_recv::run(port, out);
    }

    // 手机摄像头 → 虚拟摄像头（完整链路）
    #[cfg(windows)]
    if let Some(i) = args.iter().position(|a| a == "--phone-to-vcam") {
        let port: u16 = args
            .get(i + 1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(27300);
        let mut w = 1280u32;
        let mut h = 720u32;
        let mut fps = 30u32;
        if let Some(j) = args.iter().position(|a| a == "--size") {
            if let Some(s) = args.get(j + 1) {
                if let Some((a, b)) = s.split_once('x') {
                    if let (Ok(a), Ok(b)) = (a.parse(), b.parse()) {
                        w = a;
                        h = b;
                    }
                }
            }
        }
        if let Some(j) = args.iter().position(|a| a == "--fps") {
            if let Some(f) = args.get(j + 1).and_then(|s| s.parse().ok()) {
                fps = f;
            }
        }
        return phone_bridge::run(port, w, h, fps);
    }

    // 四个 Windows 专属模式的入口。
    //
    // 整块用 cfg 门控而不是逐个 if 加 cfg：这些分支的实现全部依赖
    // `sidelens-vcam` / `sidelens-ipc` 的 Windows 实现，在非 Windows
    // 上连类型都不存在。逐个 if 加 cfg 会让 60 行参数解析也跟着重复一遍。
    #[cfg(windows)]
    {
        if let Some(code) = run_windows_mode(&args).await {
            return code;
        }
    }

    // 非 Windows 上明确告知，而不是落到 WebSocket 模式后让人困惑
    // 「为什么 --vcam-probe 没反应」。
    #[cfg(not(windows))]
    {
        for flag in [
            "--vcam-selfcheck",
            "--vcam-probe",
            "--shm-feed",
            "--shm-probe",
        ] {
            if args.iter().any(|a| a == flag) {
                anyhow::bail!(
                    "{flag} 仅支持 Windows —— 虚拟摄像头与共享内存 IPC 是 Windows 专属能力"
                );
            }
        }
    }

    // `--psk <64位hex>` 与 `SIDELENS_PSK` 环境变量等价。
    //
    // # 为什么两端都要支持两种来源（第四十五轮修的）
    //
    // 发送端一直只认 `--psk`，接收端只认 `SIDELENS_PSK`。同一把密钥
    // 要在两端用不同方式给，用户必然踩一次 —— 实测踩到的现象是
    // 「`video_decode_failed=223` / `audio_decode_failed=223` /
    // `decrypt_failed=0`」：发送端在加密，接收端完全不知道要解密，
    // 把密文当明文直接送去解码。`decrypt_failed=0` 是因为
    // **根本没有解密尝试**，不是解密成功 —— 一个误导性的「好消息」。
    //
    // 现在两端都是「命令行优先，环境变量兜底」，语义一致。
    // 命令行仍不推荐用于生产（`ps` / shell history 会留痕），
    // 所以给了命令行值时打印一条提示。
    let psk_cli = args
        .iter()
        .position(|a| a == "--psk")
        .and_then(|i| args.get(i + 1))
        .cloned();
    if let Some(v) = psk_cli.as_ref() {
        // SAFETY: 单线程启动阶段设置环境变量，此时还没有其他线程读环境。
        // 用 `set_var` 而不是把值一路传下去，是为了让 `udp_server` 里的
        // 读取点保持单一来源（它现在只读环境变量）。
        // 注：Rust 2024 起 `set_var` 是 unsafe，本项目 edition 2021 尚可。
        std::env::set_var("SIDELENS_PSK", v);
        tracing::warn!(
            "用 --psk 传密钥会留在 shell 历史与进程列表里；生产环境请改用 SIDELENS_PSK 环境变量"
        );
    }

    let use_udp = args.iter().any(|a| a == "--udp");
    let port: u16 = args
        .iter()
        .filter(|a| !a.starts_with("--"))
        .find_map(|s| s.parse().ok())
        .unwrap_or(if use_udp {
            sidelens_transport::DEFAULT_UDP_PORT
        } else {
            sidelens_transport::DEFAULT_PORT
        });

    if use_udp {
        return udp_server::run(port, Duration::from_secs(2)).await;
    }

    run_websocket(port).await
}

/// 取 `--flag value` 形式的参数值。
///
/// 单独抽出来是因为更新子命令有 6 个这种参数，每个都写一遍
/// `args.iter().position(...).and_then(|i| args.get(i+1))` 会让主函数难以扫读。
fn flag_value(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .filter(|v| !v.starts_with("--"))
        .cloned()
}

/// 取一个数值型参数
fn number_flag(args: &[String], flag: &str) -> Option<u32> {
    flag_value(args, flag).and_then(|v| v.parse().ok())
}

/// 四个 Windows 专属模式的分发
///
/// 返回 `Some(结果)` 表示命中了某个模式且已执行完；`None` 表示
/// 都没命中，调用方继续走默认的推流服务。
///
/// # 为什么参数解析也放进这里
///
/// 这些分支各自要解析 `--hold` / `--size` / `--fps` / `--frames`，
/// 加在一起约 60 行。如果只给 `return` 那一行加 cfg，60 行解析代码
/// 会在非 Windows 上留下「解析了一堆参数然后什么都不做」的死代码 ——
/// 而且 `cfg` 掉的类型会让这些变量变成未使用警告。
#[cfg(windows)]
async fn run_windows_mode(args: &[String]) -> Option<Result<()>> {
    // 媒体源自检（绕过 FrameServer，直接按 CLSID 激活）
    if args.iter().any(|a| a == "--vcam-selfcheck") {
        return Some(vcam_selfcheck::run().await);
    }

    // 隔离实验：逐项对照官方调用序列
    if args.iter().any(|a| a == "--vcam-isolate") {
        return Some(vcam_isolate::run().await);
    }

    // DirectShow 滤镜验证
    if args.iter().any(|a| a == "--dshow-probe") {
        return Some(dshow_probe::run());
    }

    // 手动建图（绕过 RenderStream）
    if args.iter().any(|a| a == "--dshow-direct") {
        return Some(dshow_probe::direct_test());
    }

    // 复刻 OBS 的 RenderStream 建图
    if args.iter().any(|a| a == "--dshow-render") {
        return Some(dshow_probe::render_test());
    }

    // 真的从滤镜拉帧
    if let Some(i) = args.iter().position(|a| a == "--dshow-grab") {
        let n: u32 = args.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or(10);
        return Some(dshow_probe::grab(n));
    }

    // 虚拟摄像头验证模式
    if args.iter().any(|a| a == "--vcam-probe") {
        let hold: u64 = args
            .iter()
            .position(|a| a == "--hold")
            .and_then(|i| args.get(i + 1))
            .and_then(|s| s.parse().ok())
            .unwrap_or(20);
        let remove = args.iter().any(|a| a == "--remove");
        let skip_inproc = args.iter().any(|a| a == "--no-inproc-class");
        return Some(vcam_probe::run(hold, remove, skip_inproc).await);
    }

    // 共享内存帧源模式：给虚拟摄像头灌合成帧
    if args.iter().any(|a| a == "--shm-feed") {
        let mut cfg = shm_feed::PatternConfig::default();
        if let Some(i) = args.iter().position(|a| a == "--size") {
            if let Some(s) = args.get(i + 1) {
                if let Some((w, h)) = s.split_once('x') {
                    if let (Ok(w), Ok(h)) = (w.parse(), h.parse()) {
                        cfg.width = w;
                        cfg.height = h;
                    }
                }
            }
        }
        if let Some(i) = args.iter().position(|a| a == "--fps") {
            if let Some(f) = args.get(i + 1).and_then(|s| s.parse().ok()) {
                cfg.fps = f;
            }
        }
        return Some(shm_feed::run(cfg).await);
    }

    // 共享内存帧源探针：从 --shm-feed 里读帧，验证 IPC 链路
    if args.iter().any(|a| a == "--shm-probe") {
        let mut w = 1280u32;
        let mut h = 720u32;
        if let Some(i) = args.iter().position(|a| a == "--size") {
            if let Some(s) = args.get(i + 1) {
                if let Some((a, b)) = s.split_once('x') {
                    if let (Ok(a), Ok(b)) = (a.parse(), b.parse()) {
                        w = a;
                        h = b;
                    }
                }
            }
        }
        let frames: u64 = args
            .iter()
            .position(|a| a == "--frames")
            .and_then(|i| args.get(i + 1))
            .and_then(|s| s.parse().ok())
            .unwrap_or(60);
        // --dump <文件>：把最后一帧的 NV12 原样写盘，用于确认画面不是黑的
        let dump = args
            .iter()
            .position(|a| a == "--dump")
            .and_then(|i| args.get(i + 1))
            .map(std::path::PathBuf::from);
        return Some(shm_probe::run_with_dump(w, h, frames, dump.as_deref()).await);
    }

    None
}

async fn run_websocket(arg_port: u16) -> Result<()> {
    let server = std::sync::Arc::new(WsServer::bind(arg_port).await?);
    let actual = server.port();
    if actual != arg_port {
        warn!("端口 {arg_port} 被占用，改用 {actual}");
    }
    info!("SideLens 已启动 ws://0.0.0.0:{actual}");

    let mut rx = server.subscribe();
    {
        let srv = server.clone();
        tokio::spawn(async move {
            if let Err(e) = srv.serve().await {
                warn!(error = %e, "accept 循环退出");
            }
        });
    }

    let mut jb = JitterBuffer::new(JitterConfig::default());
    let mut net_latency = LatencyStats::default();
    let mut playable = LatencyStats::default();
    let mut hold = LatencyStats::default();

    let mut fps_window: VecDeque<Instant> = VecDeque::new();
    let mut last_report = Instant::now();
    let mut total = 0u64;
    let mut dropped_late = 0u64;
    let mut keyframe_requests = 0u64;

    let tick_ms: u64 = std::env::var("SIDELENS_TICK_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);
    let mut ticker = tokio::time::interval(Duration::from_millis(tick_ms));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    info!(tick_ms, "出队节拍");

    loop {
        tokio::select! {
            biased;

            _ = ticker.tick() => {
                while let Some(frame) = jb.pop() {
                    let played_ms = now_us().saturating_sub(frame.send_ts_us) as f64 / 1000.0;
                    playable.push(played_ms);
                    let held_ms = now_us().saturating_sub(frame.arrived_at_us) as f64 / 1000.0;
                    hold.push(held_ms);
                }
            }

            recv = rx.recv() => {
                let Ok(frame) = recv else { continue };
                match frame.ty {
                    FrameType::Hello => {
                        info!(client = %frame.client_id, "握手");
                    }
                    FrameType::Video | FrameType::Audio => {
                        total += 1;

                        let lat_ms = now_us().saturating_sub(frame.send_ts_us) as f64 / 1000.0;
                        net_latency.push(lat_ms);

                        fps_window.push_back(Instant::now());
                        while fps_window
                            .front()
                            .is_some_and(|t| t.elapsed() > Duration::from_secs(1))
                        {
                            fps_window.pop_front();
                        }

                        let outcome = jb.push(QueuedFrame {
                            ty: frame.ty,
                            seq: frame.seq,
                            pts_us: frame.pts_us,
                            send_ts_us: frame.send_ts_us,
                            arrived_at_us: 0,
                            flags: frame.flags,
                            is_retransmit: false,
                            payload: frame.payload,
                        });

                        if outcome.late {
                            dropped_late += 1;
                        }

                        if outcome.needs_keyframe {
                            keyframe_requests += 1;
                            let req = sidelens_transport::control_frame(
                                FrameType::KeyframeReq,
                                &serde_json::json!({"reason": "jitter-overflow"}),
                            );
                            let cid = frame.client_id.clone();
                            let srv = server.clone();
                            tokio::spawn(async move {
                                let _ = srv.send_to(&cid, req).await;
                            });
                        }
                    }
                    _ => {}
                }
            }
        }

        if last_report.elapsed() >= Duration::from_secs(2) {
            // 抖动缓冲自适应（与 UDP 路径同一套逻辑）。
            //
            // # 为什么 WebSocket 路径也需要它
            //
            // 上一轮只在 `udp_server.rs` 里接了 `jb.adapt()`，
            // 这条 WebSocket 路径漏了 —— 于是 `--udp` 模式下 p50 降到 13.77ms，
            // 而默认的 WebSocket 模式仍停在 20.95ms（`target_ms` 恒为 20）。
            //
            // 两条路径的抖动缓冲是同一个 `JitterBuffer` 类型，
            // 没有理由只有一条路享受自适应。
            if let Some((old, new)) = jb.adapt(now_us()) {
                debug!(
                    from_ms = format!("{old:.2}"),
                    to_ms = format!("{new:.2}"),
                    jitter_ms = format!("{:.2}", jb.stats.avg_jitter_ms),
                    "抖动缓冲目标延迟已调整"
                );
            }

            let (p50, p95, p99) = net_latency.summary();
            let (q50, q95, _) = playable.summary();
            let (h50, h95, _) = hold.summary();
            let s = server.stats.snapshot();

            info!(
                fps = fps_window.len(),
                frames = total,
                bytes = s.bytes_in,
                net_ms = format!("{p50:.2}/{p95:.2}/{p99:.2}"),
                playable_ms = format!("{q50:.2}/{q95:.2}"),
                hold_ms = format!("{h50:.2}/{h95:.2}"),
                jitter = jb.len(),
                target_ms = format!("{:.1}", jb.target_ms()),
                jitter_adapt = jb.stats.adaptations,
                jitter_up = jb.stats.adapt_ups,
                jitter_down = jb.stats.adapt_downs,
                jitter_ewma_ms = format!("{:.2}", jb.stats.avg_jitter_ms),
                late = dropped_late,
                kf_req = keyframe_requests,
                "统计"
            );
            last_report = Instant::now();
        }
    }
}
