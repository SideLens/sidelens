//! 手机摄像头接收器的 Windows 原生启动界面。
//!
//! 界面进程负责生命周期和 USB 配对，真正的视频接收仍由同一 exe 的
//! `--phone-to-vcam` worker 模式运行，避免 UI 消息泵被网络读取阻塞。

#![cfg(windows)]

use std::io::{BufRead, BufReader, Write};
use std::os::windows::process::CommandExt;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use crate::autostart;
use anyhow::{Context, Result};
use sidelens_audio_out::enumerate_render_devices;
use sidelens_ipc::{ShmRing, DEFAULT_SHM_NAME};
use sidelens_render_gpu::GpuRenderer;
use sidelens_ui::{MainWindow, UiEvent};

const PORT: u16 = 27300;
const HOTKEY_START_STOP: i32 = 27301;
const HOTKEY_PAUSE: i32 = 27302;
const HOTKEY_MIRROR: i32 = 27303;

pub fn run(background: bool) -> Result<()> {
    let mut window = MainWindow::new_hidden("SideLens · 手机摄像头", 1100, 820)
        .context("创建 SideLens 窗口失败")?;
    let title = window.add_label("SideLens Camera", 24, 16, 900, 36);
    let status = window.add_label("接收器已停止", 24, 54, 900, 28);
    let help = window.add_label(
        "USB 连接：连接手机并启用 USB 调试，然后点击“配置 USB”。",
        24,
        88,
        900,
        24,
    );
    let help2 = window.add_label(
        "Wi-Fi 连接：手机与电脑在同一网络，在 SideLens 手机端点击“搜索同一 Wi-Fi 下的电脑”。",
        24,
        116,
        900,
        28,
    );
    let output = window.add_label("虚拟摄像头模式：480p–4K · 30/60 fps", 24, 148, 1050, 24);
    let ctl_zoom_out = window.add_button("变焦 -", 24, 176, 92, 32);
    let ctl_zoom_in = window.add_button("变焦 +", 124, 176, 92, 32);
    let ctl_focus = window.add_button("自动对焦", 224, 176, 92, 32);
    let ctl_ev_down = window.add_button("曝光 -", 324, 176, 92, 32);
    let ctl_ev_up = window.add_button("曝光 +", 424, 176, 92, 32);
    let ctl_ae = window.add_button("曝光自动", 524, 176, 92, 32);
    let ctl_awb = window.add_button("白平衡自动", 624, 176, 100, 32);
    let ctl_torch = window.add_button("补光灯关", 732, 176, 92, 32);
    let ctl_stab = window.add_button("防抖关", 832, 176, 92, 32);
    let audio_device_label = window.add_label("音频输出", 24, 212, 82, 26);
    let output_devices = enumerate_render_devices();
    let mut device_labels = vec!["系统默认".to_string()];
    device_labels.extend(output_devices.iter().map(|(name, _)| name.clone()));
    let device_label_refs: Vec<&str> = device_labels.iter().map(String::as_str).collect();
    let audio_device_combo = window.add_combo(&device_label_refs, 0, 108, 210, 360, 28);
    let mirror_button = window.add_button("镜像关", 490, 210, 88, 28);
    let flip_button = window.add_button("翻转关", 586, 210, 88, 28);
    let night_checkbox = window.add_checkbox("夜景降噪", false, 690, 210, 110, 28);
    let face_exposure_checkbox = window.add_checkbox("脸部曝光", false, 808, 210, 120, 28);
    let look_label = window.add_label("画面外观", 24, 244, 82, 26);
    let look_options = [
        "自然", "暖色", "冷色", "胶片", "鲜艳", "柔和", "黑白", "夜景",
    ];
    let look_combo = window.add_combo(&look_options, 0, 108, 242, 300, 28);
    let brightness_label = window.add_label("亮度 0", 24, 273, 62, 24);
    let brightness_slider = window.add_slider(50, 88, 269, 138, 30);
    let contrast_label = window.add_label("对比 1.0", 246, 273, 68, 24);
    let contrast_slider = window.add_slider(50, 315, 269, 138, 30);
    let saturation_label = window.add_label("饱和 1.0", 470, 273, 68, 24);
    let saturation_slider = window.add_slider(50, 539, 269, 138, 30);
    let warmth_label = window.add_label("色温 0", 694, 273, 62, 24);
    let warmth_slider = window.add_slider(50, 758, 269, 138, 30);
    let sharpen_label = window.add_label("锐化 0.0", 910, 273, 76, 24);
    let sharpen_slider = window.add_slider(0, 910, 269, 138, 30);
    let log = window.add_label("日志：等待启动", 24, 734, 900, 24);
    window.set_video_rect(24, 310, 1052, 421);
    let usb_button = window.add_button("配置 USB", 24, 770, 130, 38);
    let start_button = window.add_button("启动接收器", 170, 770, 150, 38);
    let stop_button = window.add_button("停止", 336, 770, 110, 38);
    let ctl_audio = window.add_button("麦克风关", 466, 770, 130, 38);
    let ctl_pause = window.add_button("暂离", 610, 770, 120, 38);
    let _ = (title, help, help2, output, audio_device_label, look_label);
    let mut active_hotkeys = Vec::new();
    for (id, key, label) in [
        (HOTKEY_START_STOP, b'S' as u32, "共享"),
        (HOTKEY_PAUSE, b'P' as u32, "暂停"),
        (HOTKEY_MIRROR, b'M' as u32, "镜像"),
    ] {
        match window.register_hotkey(id, 0x0001 | 0x0002, key) {
            Ok(()) => active_hotkeys.push(format!("Ctrl+Alt+{} {label}", key as u8 as char)),
            Err(error) => eprintln!("注册{label}快捷键失败：{error}"),
        }
    }
    if !active_hotkeys.is_empty() {
        window.set_text(
            output,
            &format!(
                "{}    虚拟摄像头：480p–4K · 30/60 fps",
                active_hotkeys.join(" · ")
            ),
        );
    }
    window.enable_tray();
    let exe = std::env::current_exe().context("获取程序路径失败")?;
    match autostart::is_enabled(&exe) {
        Ok(enabled) => window.set_autostart_enabled(enabled),
        Err(error) => eprintln!("读取自启动状态失败：{error}"),
    }
    if !background {
        window.show();
    }

    let (tx, rx) = mpsc::channel::<String>();
    let mut child: Option<Worker> = None;
    let mut shared_frames: Option<ShmRing> = None;
    let mut preview: Option<GpuRenderer> = None;
    let mut frame = vec![0u8; 3840 * 2160 * 3 / 2];
    let mut last_log = "日志：等待启动".to_string();
    let mut closed = false;
    let mut zoom = 1.0f32;
    let mut manual_focus = false;
    let mut exposure_ev = 0i32;
    let mut ae_locked = false;
    let mut awb_locked = false;
    let mut torch = false;
    let mut stabilization = false;
    let mut audio_enabled = false;
    let mut video_paused = false;
    let mut selected_audio_device: Option<String> = None;
    let mut mirror = false;
    let mut flip_vertical = false;
    let mut look = "natural";
    let mut night_mode = false;
    let mut face_exposure = false;
    let mut brightness = 0.0f32;
    let mut contrast = 1.0f32;
    let mut saturation = 1.0f32;
    let mut warmth = 0.0f32;
    let mut sharpen = 0.0f32;
    if background {
        // 托盘启动且没有人坐在屏幕前时，DirectShow 协商可能很快触发重绘。
        // 先收起视频区，等首帧真正到达再显示，避免托盘图标旁闪一下空白框。
        window.set_video_rect(0, 0, 0, 0);
        window.click(start_button);
    }

    while !closed {
        while let Some(event) = window.pump() {
            match event {
                UiEvent::Closed => closed = true,
                UiEvent::Hotkey(id) if id == HOTKEY_START_STOP => window.click(start_button),
                UiEvent::Hotkey(id) if id == HOTKEY_PAUSE => window.click(ctl_pause),
                UiEvent::Hotkey(id) if id == HOTKEY_MIRROR => window.click(mirror_button),
                UiEvent::TrayAutostartToggled(enabled) => {
                    match autostart::set_enabled(enabled, &exe) {
                        Ok(()) => {
                            last_log = if enabled {
                                "已启用：登录 Windows 后在托盘启动接收器".into()
                            } else {
                                "已停用：登录 Windows 时不再自动启动".into()
                            };
                            window.set_text(status, &last_log);
                        }
                        Err(error) => {
                            // 托盘菜单已经翻转了本地状态；写注册表失败时回滚，
                            // 避免菜单显示“已启用”但实际没有启动项。
                            window.set_autostart_enabled(!enabled);
                            last_log = format!("自启动设置失败：{error}");
                            window.set_text(status, &last_log);
                        }
                    }
                }
                UiEvent::ButtonClicked(id) if id == start_button => {
                    if child.is_none() {
                        match start_worker(&tx) {
                            Ok(worker) => {
                                child = Some(worker);
                                let endpoint = selected_audio_device.as_deref().unwrap_or("");
                                let command =
                                    serde_json::json!({"audio_device": endpoint}).to_string();
                                send_control(&mut child, &command, &mut last_log);
                                window.set_text(status, "接收器启动中…");
                                last_log = "日志：正在启动手机摄像头接收器".into();
                            }
                            Err(error) => {
                                last_log = format!("启动失败：{error:#}");
                                window.set_text(status, "接收器启动失败");
                            }
                        }
                    }
                }
                UiEvent::ButtonClicked(id) if id == stop_button => {
                    stop_worker(&mut child);
                    preview = None;
                    shared_frames = None;
                    video_paused = false;
                    window.set_text(ctl_pause, "暂离");
                    window.set_text(status, "接收器已停止");
                    last_log = "日志：接收器已停止".into();
                }
                UiEvent::ButtonClicked(id) if id == ctl_audio => {
                    audio_enabled = !audio_enabled;
                    send_control(
                        &mut child,
                        &format!("{{\"audio\":{audio_enabled},\"mic\":\"voice\"}}"),
                        &mut last_log,
                    );
                    window.set_text(
                        ctl_audio,
                        if audio_enabled {
                            "麦克风开"
                        } else {
                            "麦克风关"
                        },
                    );
                }
                UiEvent::ButtonClicked(id) if id == ctl_pause => {
                    video_paused = !video_paused;
                    send_control(
                        &mut child,
                        &format!("{{\"pause\":{video_paused}}}"),
                        &mut last_log,
                    );
                    window.set_text(
                        ctl_pause,
                        if video_paused {
                            "恢复视频"
                        } else {
                            "暂离"
                        },
                    );
                    if child.is_some() {
                        window.set_text(
                            status,
                            if video_paused {
                                "视频暂停，虚拟摄像头保持最后画面"
                            } else {
                                "正在恢复视频…"
                            },
                        );
                    }
                }
                UiEvent::ButtonClicked(id) if id == ctl_zoom_out => {
                    zoom = (zoom - 0.25).max(1.0);
                    send_control(
                        &mut child,
                        &format!("{{\"zoom\":{zoom:.2}}}"),
                        &mut last_log,
                    );
                }
                UiEvent::ButtonClicked(id) if id == ctl_zoom_in => {
                    zoom = (zoom + 0.25).min(8.0);
                    send_control(
                        &mut child,
                        &format!("{{\"zoom\":{zoom:.2}}}"),
                        &mut last_log,
                    );
                }
                UiEvent::ButtonClicked(id) if id == ctl_focus => {
                    manual_focus = !manual_focus;
                    let af = if manual_focus { "manual" } else { "auto" };
                    send_control(
                        &mut child,
                        &format!("{{\"af\":\"{af}\",\"focus\":0.5}}"),
                        &mut last_log,
                    );
                    window.set_text(
                        ctl_focus,
                        if manual_focus {
                            "手动对焦"
                        } else {
                            "自动对焦"
                        },
                    );
                }
                UiEvent::ButtonClicked(id) if id == ctl_ev_down => {
                    exposure_ev = exposure_ev.saturating_sub(1);
                    send_control(
                        &mut child,
                        &format!("{{\"ev\":{exposure_ev}}}"),
                        &mut last_log,
                    );
                }
                UiEvent::ButtonClicked(id) if id == ctl_ev_up => {
                    exposure_ev = exposure_ev.saturating_add(1);
                    send_control(
                        &mut child,
                        &format!("{{\"ev\":{exposure_ev}}}"),
                        &mut last_log,
                    );
                }
                UiEvent::ButtonClicked(id) if id == ctl_ae => {
                    ae_locked = !ae_locked;
                    if ae_locked && face_exposure {
                        face_exposure = false;
                        window.set_checked(face_exposure_checkbox, false);
                    }
                    let ae = if ae_locked { "lock" } else { "auto" };
                    send_control(
                        &mut child,
                        &format!("{{\"ae\":\"{ae}\",\"faceExposure\":{face_exposure}}}"),
                        &mut last_log,
                    );
                    window.set_text(
                        ctl_ae,
                        if ae_locked {
                            "曝光锁定"
                        } else {
                            "曝光自动"
                        },
                    );
                }
                UiEvent::ButtonClicked(id) if id == ctl_awb => {
                    awb_locked = !awb_locked;
                    let awb = if awb_locked { "lock" } else { "auto" };
                    send_control(&mut child, &format!("{{\"awb\":\"{awb}\"}}"), &mut last_log);
                    window.set_text(
                        ctl_awb,
                        if awb_locked {
                            "白平衡锁定"
                        } else {
                            "白平衡自动"
                        },
                    );
                }
                UiEvent::ButtonClicked(id) if id == ctl_torch => {
                    torch = !torch;
                    send_control(&mut child, &format!("{{\"torch\":{torch}}}"), &mut last_log);
                    window.set_text(
                        ctl_torch,
                        if torch {
                            "补光灯开"
                        } else {
                            "补光灯关"
                        },
                    );
                }
                UiEvent::ButtonClicked(id) if id == ctl_stab => {
                    stabilization = !stabilization;
                    send_control(
                        &mut child,
                        &format!("{{\"stab\":{stabilization}}}"),
                        &mut last_log,
                    );
                    window.set_text(
                        ctl_stab,
                        if stabilization {
                            "防抖开"
                        } else {
                            "防抖关"
                        },
                    );
                }
                UiEvent::ComboChanged(id, index) if id == audio_device_combo => {
                    selected_audio_device = index
                        .checked_sub(1)
                        .and_then(|device_index| output_devices.get(device_index))
                        .map(|(_, endpoint_id)| endpoint_id.clone());
                    let endpoint = selected_audio_device.as_deref().unwrap_or("");
                    let command = serde_json::json!({"audio_device": endpoint}).to_string();
                    if child.is_some() {
                        send_control(&mut child, &command, &mut last_log);
                    }
                    window.set_text(
                        status,
                        if child.is_none() {
                            "已选择音频输出设备"
                        } else if endpoint.is_empty() {
                            "音频将输出到系统默认设备"
                        } else {
                            "正在切换音频输出设备…"
                        },
                    );
                }
                UiEvent::ButtonClicked(id) if id == mirror_button => {
                    mirror = !mirror;
                    send_control(
                        &mut child,
                        &format!("{{\"mirror\":{mirror}}}"),
                        &mut last_log,
                    );
                    window.set_text(mirror_button, if mirror { "镜像开" } else { "镜像关" });
                }
                UiEvent::ButtonClicked(id) if id == flip_button => {
                    flip_vertical = !flip_vertical;
                    send_control(
                        &mut child,
                        &format!("{{\"flip\":{flip_vertical}}}"),
                        &mut last_log,
                    );
                    window.set_text(
                        flip_button,
                        if flip_vertical {
                            "翻转开"
                        } else {
                            "翻转关"
                        },
                    );
                }
                UiEvent::CheckboxToggled(id, enabled) if id == night_checkbox => {
                    night_mode = enabled;
                    send_control(
                        &mut child,
                        &format!("{{\"night\":{night_mode}}}"),
                        &mut last_log,
                    );
                }
                UiEvent::CheckboxToggled(id, enabled) if id == face_exposure_checkbox => {
                    face_exposure = enabled;
                    if enabled && ae_locked {
                        ae_locked = false;
                        window.set_text(ctl_ae, "曝光自动");
                    }
                    let ae = if ae_locked { "lock" } else { "auto" };
                    send_control(
                        &mut child,
                        &format!("{{\"ae\":\"{ae}\",\"faceExposure\":{face_exposure}}}"),
                        &mut last_log,
                    );
                }
                UiEvent::ComboChanged(id, index) if id == look_combo => {
                    look = match index {
                        1 => "warm",
                        2 => "cool",
                        3 => "film",
                        4 => "vivid",
                        5 => "faded",
                        6 => "mono",
                        7 => "night",
                        _ => "natural",
                    };
                    send_control(
                        &mut child,
                        &format!("{{\"look\":\"{look}\"}}"),
                        &mut last_log,
                    );
                }
                UiEvent::SliderChanged(id, position) => {
                    if id == brightness_slider {
                        brightness = (position - 50) as f32 * 0.8;
                        window.set_text(brightness_label, &format!("亮度 {brightness:.0}"));
                    } else if id == contrast_slider {
                        contrast = 0.4 + position as f32 * 0.012;
                        window.set_text(contrast_label, &format!("对比 {contrast:.1}"));
                    } else if id == saturation_slider {
                        saturation = position as f32 * 0.02;
                        window.set_text(saturation_label, &format!("饱和 {saturation:.1}"));
                    } else if id == warmth_slider {
                        warmth = (position - 50) as f32 * 0.6;
                        window.set_text(warmth_label, &format!("色温 {warmth:.0}"));
                    } else if id == sharpen_slider {
                        sharpen = position as f32 * 0.015;
                        window.set_text(sharpen_label, &format!("锐化 {sharpen:.1}"));
                    } else {
                        continue;
                    }
                    let command = format!(
                        "{{\"brightness\":{brightness:.1},\"contrast\":{contrast:.2},\"saturation\":{saturation:.2},\"warmth\":{warmth:.1},\"sharpen\":{sharpen:.2}}}"
                    );
                    send_control(&mut child, &command, &mut last_log);
                }
                UiEvent::ButtonClicked(id) if id == usb_button => {
                    let sender = tx.clone();
                    std::thread::spawn(move || {
                        let result = Command::new("adb")
                            .args(["reverse", &format!("tcp:{PORT}"), &format!("tcp:{PORT}")])
                            .creation_flags(0x0800_0000)
                            .output();
                        let message = match result {
                            Ok(output) if output.status.success() => {
                                "USB 已配置：手机 127.0.0.1:27300 → 电脑 27300".to_string()
                            }
                            Ok(output) => format!(
                                "adb reverse 失败：{}",
                                String::from_utf8_lossy(&output.stderr).trim()
                            ),
                            Err(error) => format!(
                                "未找到 adb：{error}。请安装 Android Platform Tools 并加入 PATH。"
                            ),
                        };
                        let _ = sender.send(message);
                    });
                }
                _ => {}
            }
        }

        while let Ok(line) = rx.try_recv() {
            last_log = line.clone();
            if line.contains("手机已连接") || line.contains("✅ 手机已连接") {
                window.set_text(
                    status,
                    if video_paused {
                        "手机已连接，视频暂停并保持最后画面"
                    } else {
                        "手机已连接，正在接收视频"
                    },
                );
                send_control(
                    &mut child,
                    &format!(
                        "{{\"zoom\":{zoom:.2},\"af\":\"{}\",\"focus\":0.5,\"ev\":{exposure_ev},\"ae\":\"{}\",\"awb\":\"{}\",\"torch\":{torch},\"stab\":{stabilization},\"audio\":{audio_enabled},\"mic\":\"voice\",\"pause\":{video_paused},\"mirror\":{mirror},\"flip\":{flip_vertical},\"look\":\"{look}\",\"brightness\":{brightness:.1},\"contrast\":{contrast:.2},\"saturation\":{saturation:.2},\"warmth\":{warmth:.1},\"sharpen\":{sharpen:.2},\"night\":{night_mode},\"faceExposure\":{face_exposure}}}",
                        if manual_focus { "manual" } else { "auto" },
                        if ae_locked { "lock" } else { "auto" },
                        if awb_locked { "lock" } else { "auto" }
                    ),
                    &mut last_log,
                );
            } else if line.contains("手机断开连接") || line.contains("等待重新连接") {
                window.set_text(status, "连接中断，等待手机重新共享…");
            } else if line.contains("等待手机连接") {
                window.set_text(status, "等待手机连接…");
            } else if let Some(payload) = line.strip_prefix("手机状态: ") {
                if let Ok(state) = serde_json::from_str::<serde_json::Value>(payload) {
                    if let Some(enabled) =
                        state.get("faceExposure").and_then(|value| value.as_bool())
                    {
                        face_exposure = enabled;
                        window.set_checked(face_exposure_checkbox, enabled);
                    }
                }
            } else if line.contains("USB 已配置") {
                window.set_text(status, &line);
            }
        }
        window.set_text(log, &last_log);

        let worker_exit = child
            .as_mut()
            .map(|worker| worker.process.try_wait())
            .transpose()
            .context("读取接收器进程状态失败")?
            .flatten();
        if let Some(exit) = worker_exit {
            child = None;
            preview = None;
            shared_frames = None;
            window.set_text(status, &format!("接收器已退出（{exit}）"));
        }

        if child.is_some() && shared_frames.is_none() {
            if let Ok(ring) = ShmRing::open(DEFAULT_SHM_NAME) {
                shared_frames = Some(ring);
                let (x, y, width, height) = window.video_rect();
                match GpuRenderer::new_child(
                    window.hwnd(),
                    "SideLens 实时预览",
                    x,
                    y,
                    width as u32,
                    height as u32,
                ) {
                    Ok(renderer) => preview = Some(renderer),
                    Err(error) => last_log = format!("GPU 预览初始化失败：{error}"),
                }
            }
        }

        if let Some(renderer) = preview.as_mut() {
            let _ = renderer.pump_events();
        }
        if let (Some(ring), Some(renderer)) = (shared_frames.as_ref(), preview.as_mut()) {
            if let Some(meta) = ring.read_latest(&mut frame) {
                if background {
                    window.set_video_rect(24, 310, 1052, 421);
                }
                let frame_len = (meta.payload_len as usize).min(frame.len());
                if let Err(error) = renderer.present(
                    &frame[..frame_len],
                    (meta.width, meta.height),
                    (meta.width, meta.height),
                ) {
                    last_log = format!("GPU 预览帧失败：{error}");
                }
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    stop_worker(&mut child);
    Ok(())
}

struct Worker {
    process: Child,
    input: ChildStdin,
}

fn start_worker(tx: &mpsc::Sender<String>) -> Result<Worker> {
    let exe = std::env::current_exe().context("获取程序路径失败")?;
    let mut command = Command::new(exe);
    command
        .args(["--phone-to-vcam", &PORT.to_string()])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::piped())
        .creation_flags(0x0800_0000);
    let mut child = command.spawn().context("启动视频接收器失败")?;

    let input = child.stdin.take().context("打开接收器控制输入失败")?;
    if let Some(stdout) = child.stdout.take() {
        let sender = tx.clone();
        std::thread::spawn(move || {
            // map_while 而不是 flatten：读错误时 lines() 会无限产出 Err，
            // flatten 会因此变成不终止的循环（clippy 的 flatten_option 检查项）。
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let _ = sender.send(line);
            }
        });
    }
    if let Some(stderr) = child.stderr.take() {
        let sender = tx.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                let _ = sender.send(line);
            }
        });
    }
    Ok(Worker {
        process: child,
        input,
    })
}

fn send_control(worker: &mut Option<Worker>, json: &str, log: &mut String) {
    let Some(worker) = worker.as_mut() else {
        *log = "控制未发送：请先启动接收器".into();
        return;
    };
    if let Err(error) = writeln!(worker.input, "{json}").and_then(|_| worker.input.flush()) {
        *log = format!("发送相机控制失败：{error}");
    }
}

fn stop_worker(worker: &mut Option<Worker>) {
    if let Some(mut worker) = worker.take() {
        let _ = worker.process.kill();
        let _ = worker.process.wait();
    }
}
