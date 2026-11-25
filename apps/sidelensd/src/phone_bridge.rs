//! 手机摄像头 → 虚拟摄像头（完整链路）
//!
//! ```text
//! 手机 Camera2 → MediaCodec → TCP → 本模块
//!                                      ↓ H.264 解码（MF）
//!                                    NV12
//!                                      ↓ ShmRing
//!                                   共享内存
//!                                      ↓ FrameSource
//!                                   DirectShow 滤镜
//!                                      ↓
//!                                   Zoom / OBS / 浏览器
//! ```
//!
//! # 与 `phone_recv` 的区别
//!
//! | 模式 | 输出 | 用途 |
//! | --- | --- | --- |
//! | `--phone-recv` | .h264 文件 | 验证链路、离线分析 |
//! | `--phone-to-vcam` | 共享内存 | **让应用真的用上** |
//!
//! # 为什么在这里解码而不是让消费端解
//!
//! DirectShow 滤镜输出的是**原始 NV12**（摄像头类设备的约定）。
//! 消费端（Zoom）拿到的是解码后的像素，它不知道 H.264 的存在。
//! 所以解码必须在滤镜之前 —— 也就是这一层。

#![allow(unsafe_code)]

use std::collections::VecDeque;
use std::io::{BufRead, Write};
use std::net::{TcpListener, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use sidelens_audio_out::{AudioSink, PcmFormat};
use sidelens_codec::audio::OpusConfig;
use sidelens_codec_mf::AacDecoder;
use sidelens_codec_opus::OpusDecoder;
use sidelens_ipc::wire::{FrameMeta, FrameModeRequest, FrameSignal, PixelFormat};
use sidelens_ipc::{PipeServer, ShmRing, DEFAULT_PIPE_NAME, DEFAULT_SHM_NAME};
use sidelens_phone_proto::{
    read_message, write_message, AudioCodec, AudioConfig, AudioFrame, MessageType, VideoCodec,
    VideoConfig, VideoFrame,
};

const DISCOVERY_PORT: u16 = 27301;
const DISCOVERY_REQUEST: &[u8] = b"SIDELENS_DISCOVER_V1";

enum PhoneAudioDecoder {
    Opus(OpusDecoder),
    Aac(AacDecoder),
}

impl PhoneAudioDecoder {
    fn decode(&mut self, data: &[u8], pts_us: u64) -> std::result::Result<Vec<i16>, String> {
        match self {
            Self::Opus(decoder) => {
                let config = decoder.config();
                let mut pcm =
                    vec![0i16; config.frame_samples() as usize * config.channels as usize];
                decoder
                    .decode(Some(data), &mut pcm)
                    .map(|decoded| {
                        pcm.truncate(decoded.samples * config.channels as usize);
                        pcm
                    })
                    .map_err(|e| e.to_string())
            }
            Self::Aac(decoder) => decoder.decode(data, pts_us).map_err(|e| e.to_string()),
        }
    }
}

pub fn run(port: u16, width: u32, height: u32, fps: u32) -> Result<()> {
    println!("=== 手机摄像头 → 虚拟摄像头 ===");
    println!("监听 0.0.0.0:{port}");
    println!("初始输出 {width}x{height}@{fps}；摄像头客户端连接后协商输出模式");
    println!();

    start_discovery(port);
    let control_rx = start_control_input();

    // 发布清单：决定「手机端要不要升级」。读不到就没有这个功能 ——
    // 从开发机直接跑 sidelensd 时安装目录里没有 manifest.json，
    // 这时候不该报错，也不该瞎提示。
    let upgrade_manifest = sidelensd::update::hint::load_manifest(&install_root());
    match upgrade_manifest.as_ref() {
        Some(manifest) => println!(
            "发布清单 {}：手机端最新 versionCode {}，最低 versionCode {}，最低协议 v{}",
            manifest.version,
            manifest.phone.latest_version_code,
            manifest.phone.min_supported_version_code,
            manifest.phone.min_supported_protocol
        ),
        None => println!("未找到发布清单，手机端升级提示未启用"),
    }

    // 1. 建共享内存环与管道（DirectShow 滤镜会连上来）
    // One ring serves every DirectShow mode up to 4K. The decoder scales each
    // phone stream to the active mode negotiated by the camera client.
    let frame_size = 3840usize * 2160 * 3 / 2;
    // 4 个 slot 能吸收一个完整 RTT 的抖动（与 shm_feed 一致）
    let layout = sidelens_ipc::RingLayout::new(4, frame_size);
    let ring = Arc::new(ShmRing::create(DEFAULT_SHM_NAME, layout).context("创建共享内存失败")?);
    println!("共享内存已建: {DEFAULT_SHM_NAME}");

    let mut pipe_server = PipeServer::create(DEFAULT_PIPE_NAME, true).context("创建管道失败")?;
    pipe_server.set_timeout(Duration::from_millis(1));
    let server = Arc::new(Mutex::new(pipe_server));
    println!("管道已建: {DEFAULT_PIPE_NAME}");
    println!("（打开 Zoom / OBS，选「SideLens Camera」即可看到画面）");
    println!();

    // 2. 监听手机。手机端可以随时停止并重新开始共享，因此服务器必须
    //    在每个 TCP 会话结束后继续 accept，而不是跟着一起退出。
    let listener =
        TcpListener::bind(("0.0.0.0", port)).with_context(|| format!("无法监听端口 {port}"))?;
    println!("等待手机连接…");

    let connected = Arc::new(AtomicBool::new(false));
    // 命名管道连接独立于解码循环等待，避免消费者连得稍晚时阻塞解码，
    // 也避免零超时 accept 与客户端连接时序发生竞争。
    {
        let accept_server = Arc::clone(&server);
        let accept_connected = Arc::clone(&connected);
        std::thread::Builder::new()
            .name("sidelens-pipe-accept".into())
            .spawn(move || {
                let mut errors = 0u64;
                loop {
                    if accept_connected.load(Ordering::Acquire) {
                        std::thread::sleep(Duration::from_millis(10));
                        continue;
                    }

                    let result = match accept_server.lock() {
                        Ok(server) => server.accept(Duration::from_millis(250)),
                        Err(_) => {
                            eprintln!("命名管道状态锁中毒，停止等待摄像头连接");
                            return;
                        }
                    };

                    match result {
                        Ok(true) => {
                            accept_connected.store(true, Ordering::Release);
                            println!("✅ 滤镜已连接，开始转发手机帧");
                            errors = 0;
                        }
                        Ok(false) => {}
                        Err(e) => {
                            errors += 1;
                            if errors == 1 || errors % 100 == 0 {
                                eprintln!("等待滤镜管道连接失败（第 {errors} 次）: {e}");
                            }
                            std::thread::sleep(Duration::from_millis(10));
                        }
                    }
                }
            })
            .context("启动命名管道连接线程失败")?;
    }

    let frames = Arc::new(AtomicU64::new(0));
    let decoded = Arc::new(AtomicU64::new(0));
    let started = Instant::now();
    let mut output_width = width;
    let mut output_height = height;
    let mut output_fps = fps;
    let mut video_codec_data: Option<Vec<u8>> = None;
    let mut selected_audio_device: Option<String> = None;
    let mut mirror_horizontal = false;
    let mut flip_vertical = false;
    let mut color_look = ColorLook::Natural;
    let mut color_adjustments = ColorAdjustments::default();
    let mut night_mode_enabled = false;

    loop {
        let (mut stream, peer) = listener.accept().context("accept 失败")?;
        stream.set_nodelay(true).ok();
        let mut control_stream = stream.try_clone().context("复制控制通道失败")?;
        println!("✅ 手机已连接: {peer}");

        // 每次共享都从 HELLO/CONFIG 重新协商并建立解码器。
        let mut decoder: Option<sidelens_codec_mf::H264Decoder> = None;
        let mut audio_decoder: Option<PhoneAudioDecoder> = None;
        let mut audio_sink: Option<AudioSink> = None;
        let mut active_audio_format: Option<PcmFormat> = None;
        let mut audio_queue: VecDeque<(Instant, Vec<i16>)> = VecDeque::new();
        let mut nv12 = Vec::new();
        let mut night_previous = Vec::new();
        let mut night_current = Vec::new();
        let mut night_mode_dimensions = None;
        let mut sharpen_scratch = Vec::new();
        // 几何变换（旋转/裁切）。每帧都读一次，因此放在会话作用域而非循环外，
        // 让控制消息可以在会话进行中改它。
        let mut geometry = sidelens_frame_transform::Transform::default();
        let mut geometry_size_warned = false;
        // 画面效果：默认全关，用户从桌面界面打开才走这些路径。
        let mut effects = sidelens_frame_effects::Effects::default();
        let mut focus_window: Option<sidelens_frame_effects::FocusWindow> = None;
        // 手机端升级提示：只在提示"变化"时下发一次（每次会话重新开始，
        // 因为新连上的手机可能是另一台，也可能刚升级过）。
        let mut upgrade_tracker = sidelensd::update::hint::HintTracker::new();

        loop {
            let message = match read_message(&mut stream) {
                Ok(Some(message)) => message,
                Ok(None) => {
                    println!("手机断开连接，等待重新共享…");
                    break;
                }
                Err(e) => {
                    eprintln!("读取手机协议消息失败: {e}，等待重新连接…");
                    break;
                }
            };

            let mut decoded_frame = None;

            // The DirectShow filter sends its negotiated output mode over the
            // duplex frame pipe. Reconfigure the decoder on the next frame so
            // the shared-memory dimensions always match the consumer.
            if connected.load(Ordering::Acquire) {
                let mode_message = server.lock().ok().and_then(|pipe| {
                    let mut bytes = [0u8; FrameModeRequest::SIZE];
                    pipe.read_message(&mut bytes).ok().map(|size| (bytes, size))
                });
                if let Some((bytes, size)) = mode_message {
                    if let Some(mode) = FrameModeRequest::decode(&bytes[..size]) {
                        if (mode.width, mode.height, mode.fps)
                            != (output_width, output_height, output_fps)
                        {
                            output_width = mode.width;
                            output_height = mode.height;
                            output_fps = mode.fps;
                            println!(
                                "DirectShow 输出模式切换为 {output_width}x{output_height}@{output_fps}"
                            );
                            decoder = video_codec_data.as_deref().and_then(|codec_data| {
                                let cfg = sidelens_codec_mf::DecoderConfig {
                                    width: output_width,
                                    height: output_height,
                                    fps: output_fps,
                                    ..Default::default()
                                };
                                match sidelens_codec_mf::H264Decoder::new(cfg) {
                                    Ok(mut next) => {
                                        if !codec_data.is_empty() {
                                            let mut ignored = Vec::new();
                                            if let Err(e) =
                                                next.decode_into(codec_data, 0, &mut ignored)
                                            {
                                                eprintln!("新输出模式加载 H.264 参数集失败: {e}");
                                            }
                                        }
                                        Some(next)
                                    }
                                    Err(e) => {
                                        eprintln!("新输出模式创建 H.264 解码器失败: {e:#}");
                                        None
                                    }
                                }
                            });
                        }
                    }
                }
            }

            match message.kind {
                MessageType::Hello => {
                    match serde_json::from_slice::<serde_json::Value>(&message.payload) {
                        Ok(hello) => println!(
                            "手机 HELLO：{}",
                            hello
                                .get("device")
                                .and_then(|v| v.as_str())
                                .unwrap_or("Android")
                        ),
                        Err(e) => eprintln!("手机 HELLO JSON 无效: {e}"),
                    }
                }
                MessageType::Config => match VideoConfig::parse(&message.payload) {
                    Ok((config, codec_data)) if config.codec == VideoCodec::H264 => {
                        let input_width = u32::from(config.width);
                        let input_height = u32::from(config.height);
                        video_codec_data = Some(codec_data.to_vec());
                        // 手机可按能力采集高分辨率；Media Foundation 按虚拟摄像头的
                        // 固定输出格式解码并缩放，避免源分辨率和 DirectShow 模式不一致。
                        let cfg = sidelens_codec_mf::DecoderConfig {
                            width: output_width,
                            height: output_height,
                            fps: output_fps,
                            ..Default::default()
                        };
                        match sidelens_codec_mf::H264Decoder::new(cfg) {
                            Ok(mut new_decoder) => {
                                if !codec_data.is_empty() {
                                    let mut ignored = Vec::new();
                                    if let Err(e) =
                                        new_decoder.decode_into(codec_data, 0, &mut ignored)
                                    {
                                        eprintln!("加载 H.264 参数集失败: {e}");
                                    }
                                }
                                println!(
                                    "视频配置 H.264 输入 {input_width}x{input_height} → 输出 {output_width}x{output_height}，解码器: {}",
                                    new_decoder.decoder_name()
                                );
                                decoder = Some(new_decoder);
                            }
                            Err(e) => {
                                eprintln!("创建 H.264 解码器失败: {e:#}");
                                decoder = None;
                            }
                        }
                    }
                    Ok((config, _)) => {
                        eprintln!("当前 Windows 解码管线不支持 {:?}", config.codec);
                        decoder = None;
                    }
                    Err(e) => eprintln!("手机 CONFIG 无效: {e}"),
                },
                MessageType::Frame => match VideoFrame::parse(&message.payload) {
                    Ok(video) => {
                        if let Some(decoder) = decoder.as_mut() {
                            frames.fetch_add(1, Ordering::Relaxed);
                            nv12.clear();
                            match decoder.decode_into(
                                video.data,
                                video.pts_us.min(i64::MAX as u64) as i64,
                                &mut nv12,
                            ) {
                                Ok(()) if !nv12.is_empty() => {
                                    decoded_frame = Some((video.pts_us, video.is_keyframe()));
                                }
                                Ok(()) => {}
                                Err(e) => eprintln!(
                                    "解码失败（帧 {}）: {e}",
                                    frames.load(Ordering::Relaxed)
                                ),
                            }
                        }
                    }
                    Err(e) => eprintln!("手机 FRAME 无效: {e}"),
                },
                MessageType::State => {
                    if let Ok(state) = serde_json::from_slice::<serde_json::Value>(&message.payload)
                    {
                        println!("手机状态: {state}");
                        // 按发布清单判定手机端该不该升级，只在结论变化时下发。
                        // 判据与去重都在 sidelensd::update::hint 里，见那里的测试。
                        let text = state.to_string();
                        if let Some(command) =
                            upgrade_tracker.observe(&text, upgrade_manifest.as_ref())
                        {
                            if let Err(e) = write_message(
                                &mut control_stream,
                                MessageType::Control,
                                command.as_bytes(),
                            )
                            .and_then(|_| control_stream.flush())
                            {
                                eprintln!("下发手机端升级提示失败: {e}");
                            } else {
                                println!("已向手机下发升级提示: {command}");
                            }
                        }
                    }
                }
                MessageType::AudioConfig => match AudioConfig::parse(&message.payload) {
                    Ok((config, _codec_data)) if config.codec == AudioCodec::Opus => {
                        audio_queue.clear();
                        audio_decoder = None;
                        let opus_config = OpusConfig {
                            sample_rate: config.sample_rate,
                            channels: config.channels,
                            frame_ms: 20.0,
                        };
                        match OpusDecoder::new(opus_config) {
                            Ok(new_decoder) => {
                                let pcm_format =
                                    PcmFormat::new(config.sample_rate, config.channels as u16);
                                active_audio_format = Some(pcm_format);
                                let sink = AudioSink::new_on_device(
                                    pcm_format,
                                    selected_audio_device.as_deref(),
                                )
                                .unwrap_or_else(|e| {
                                    eprintln!("WASAPI 音频输出不可用，使用静音 sink: {e}");
                                    AudioSink::headless(pcm_format)
                                });
                                println!(
                                    "手机音频 Opus {}Hz/{}ch → {}",
                                    config.sample_rate,
                                    config.channels,
                                    sink.mode().as_str()
                                );
                                audio_decoder = Some(PhoneAudioDecoder::Opus(new_decoder));
                                audio_sink = Some(sink);
                            }
                            Err(e) => eprintln!("创建 Opus 解码器失败: {e}"),
                        }
                    }
                    Ok((config, codec_data)) if config.codec == AudioCodec::AacLc => {
                        audio_queue.clear();
                        audio_decoder = None;
                        match AacDecoder::new(
                            config.sample_rate,
                            config.channels as u16,
                            codec_data,
                        ) {
                            Ok(new_decoder) => {
                                let pcm_format = PcmFormat::new(
                                    new_decoder.sample_rate(),
                                    new_decoder.channels(),
                                );
                                let sink = AudioSink::new_on_device(
                                    pcm_format,
                                    selected_audio_device.as_deref(),
                                )
                                .unwrap_or_else(|e| {
                                    eprintln!("WASAPI AAC 输出不可用，使用静音 sink: {e}");
                                    AudioSink::headless(pcm_format)
                                });
                                println!(
                                    "手机音频 AAC-LC {}Hz/{}ch → {}",
                                    new_decoder.sample_rate(),
                                    new_decoder.channels(),
                                    sink.mode().as_str()
                                );
                                active_audio_format = Some(pcm_format);
                                audio_decoder = Some(PhoneAudioDecoder::Aac(new_decoder));
                                audio_sink = Some(sink);
                            }
                            Err(e) => eprintln!("创建 AAC 解码器失败: {e}"),
                        }
                    }
                    Ok((config, _)) => {
                        audio_queue.clear();
                        audio_decoder = None;
                        eprintln!("Windows 音频解码管线不支持 {:?}", config.codec)
                    }
                    Err(e) => eprintln!("手机 AUDIO_CONFIG 无效: {e}"),
                },
                MessageType::Audio => match AudioFrame::parse(&message.payload) {
                    Ok(audio) => {
                        if let Some(decoder) = audio_decoder.as_mut() {
                            match decoder.decode(audio.data, audio.pts_us) {
                                Ok(pcm) if !pcm.is_empty() => {
                                    audio_queue.push_back((Instant::now(), pcm));
                                }
                                Ok(_) => {}
                                Err(e) => eprintln!("手机音频解码失败: {e}"),
                            }
                        }
                    }
                    Err(e) => eprintln!("手机 AUDIO 无效: {e}"),
                },
                MessageType::Control | MessageType::Ping => {}
            }

            // PC->手机使用同一版本化消息头；控制消息仍在帧/状态边界发送。
            for command in control_rx.try_iter() {
                let mut phone_command = command.clone();
                if let Ok(mut value) = serde_json::from_str::<serde_json::Value>(&command) {
                    if let Some(object) = value.as_object_mut() {
                        if let Some(device) = object.get("audio_device").and_then(|v| v.as_str()) {
                            selected_audio_device =
                                (!device.is_empty()).then(|| device.to_string());
                            if let Some(format) = active_audio_format {
                                let sink = AudioSink::new_on_device(
                                    format,
                                    selected_audio_device.as_deref(),
                                )
                                .unwrap_or_else(|e| {
                                    eprintln!("无法切换音频输出设备，使用静音 sink: {e}");
                                    AudioSink::headless(format)
                                });
                                println!("手机音频输出切换为 {}", sink.mode().as_str());
                                audio_sink = Some(sink);
                            }
                            object.remove("audio_device");
                        }
                        if let Some(enabled) = object.get("mirror").and_then(|v| v.as_bool()) {
                            mirror_horizontal = enabled;
                            object.remove("mirror");
                        }
                        if let Some(enabled) = object.get("flip").and_then(|v| v.as_bool()) {
                            flip_vertical = enabled;
                            object.remove("flip");
                        }
                        if let Some(name) = object.get("look").and_then(|v| v.as_str()) {
                            if let Some(look) = ColorLook::parse(name) {
                                color_look = look;
                            }
                            object.remove("look");
                        }
                        if let Some(enabled) = object.get("night").and_then(|v| v.as_bool()) {
                            night_mode_enabled = enabled;
                        }
                        object.remove("night");
                        if let Some(value) = object.get("brightness").and_then(|v| v.as_f64()) {
                            color_adjustments.brightness = (value as f32).clamp(-40.0, 40.0);
                        }
                        object.remove("brightness");
                        if let Some(value) = object.get("contrast").and_then(|v| v.as_f64()) {
                            color_adjustments.contrast = (value as f32).clamp(0.4, 1.6);
                        }
                        object.remove("contrast");
                        if let Some(value) = object.get("saturation").and_then(|v| v.as_f64()) {
                            color_adjustments.saturation = (value as f32).clamp(0.0, 2.0);
                        }
                        object.remove("saturation");
                        if let Some(value) = object.get("warmth").and_then(|v| v.as_f64()) {
                            color_adjustments.warmth = (value as f32).clamp(-30.0, 30.0);
                        }
                        object.remove("warmth");
                        if let Some(value) = object.get("sharpen").and_then(|v| v.as_f64()) {
                            color_adjustments.sharpen = (value as f32).clamp(0.0, 1.5);
                        }
                        object.remove("sharpen");
                        // 旋转：0/90/180/270，非法值保持原样并报告
                        if let Some(degrees) = object.get("rotate").and_then(|v| v.as_u64()) {
                            match sidelens_frame_transform::Rotation::parse(degrees as u32) {
                                Some(rotation) => geometry.rotation = rotation,
                                None => {
                                    eprintln!("忽略非法旋转角度 {degrees}（只接受 0/90/180/270）")
                                }
                            }
                            geometry_size_warned = false;
                        }
                        object.remove("rotate");
                        // 裁切："x,y,w,h"，四个值都必须是偶数
                        if let Some(text) = object.get("crop").and_then(|v| v.as_str()) {
                            let parts: Vec<u32> = text
                                .split(',')
                                .filter_map(|p| p.trim().parse().ok())
                                .collect();
                            match parts.as_slice() {
                                [x, y, w, h] => {
                                    match sidelens_frame_transform::Crop::new(*x, *y, *w, *h) {
                                        Some(crop) => geometry.crop = Some(crop),
                                        None => {
                                            eprintln!("忽略非法裁切 \"{text}\"：x/y/宽/高都必须是非零偶数")
                                        }
                                    }
                                }
                                [] if text.is_empty() || text == "off" => geometry.crop = None,
                                _ => eprintln!("忽略非法裁切 \"{text}\"：格式应为 x,y,w,h"),
                            }
                            geometry_size_warned = false;
                        }
                        object.remove("crop");
                        // 画面效果：单个字段打开/关闭，缺省保持原值。
                        // 全部默认关闭，只有显式字段才启用 —— 效果是 CPU 逐像素的，
                        // 不该因为"用户没说话"就悄悄跑起来吃掉 60fps 的预算。
                        if let Some(enabled) = object.get("autoFrame").and_then(|v| v.as_bool()) {
                            effects.auto_frame =
                                enabled.then(|| sidelens_frame_effects::AutoFrameConfig {
                                    scale: object
                                        .get("autoFrameScale")
                                        .and_then(|v| v.as_f64())
                                        .map(|v| v as f32)
                                        .unwrap_or(0.75),
                                    ..Default::default()
                                });
                            if !enabled {
                                focus_window = None;
                                geometry.crop = None;
                            }
                        }
                        object.remove("autoFrame");
                        object.remove("autoFrameScale");
                        if let Some(radius) = object.get("blur").and_then(|v| v.as_u64()) {
                            effects.blur =
                                (radius > 0).then(|| sidelens_frame_effects::BlurConfig {
                                    radius: radius.min(32) as u32,
                                    ..Default::default()
                                });
                        }
                        object.remove("blur");
                        if let Some(radius) = object.get("smooth").and_then(|v| v.as_u64()) {
                            effects.smooth =
                                (radius > 0).then(|| sidelens_frame_effects::SmoothConfig {
                                    radius: radius.min(8) as u32,
                                    ..Default::default()
                                });
                        }
                        object.remove("smooth");
                        if object.is_empty() {
                            continue;
                        }
                        phone_command = value.to_string();
                    }
                }
                if let Err(e) = write_message(
                    &mut control_stream,
                    MessageType::Control,
                    phone_command.as_bytes(),
                )
                .and_then(|_| control_stream.flush())
                {
                    eprintln!("向手机发送 CONTROL 失败: {e}");
                    break;
                }
            }
            while audio_queue
                .front()
                .is_some_and(|(arrived, _)| arrived.elapsed() >= Duration::from_millis(60))
            {
                if let Some((_, pcm)) = audio_queue.pop_front() {
                    if let Some(sink) = audio_sink.as_mut() {
                        if let Err(e) = sink.write(&pcm) {
                            eprintln!("WASAPI 音频写入失败: {e}");
                        }
                    }
                }
            }
            let Some((pts, is_key)) = decoded_frame else {
                continue;
            };

            if night_mode_enabled {
                motion_aware_denoise_nv12(
                    &mut nv12,
                    output_width,
                    output_height,
                    &mut night_previous,
                    &mut night_current,
                    &mut night_mode_dimensions,
                );
            } else {
                night_previous.clear();
                night_current.clear();
                night_mode_dimensions = None;
            }

            // 画面效果先做（在「按目标模式解码」之后、几何变换之前）：
            // 虚化与平滑都不改尺寸，把它们放在几何变换前面可以让自动取景
            // 看到的是完整的画面，而不是被旋转裁切过的局部。
            if !effects.is_identity() {
                let input_geometry =
                    sidelens_frame_transform::Geometry::new(output_width, output_height);
                match input_geometry {
                    Some(input_geometry) => {
                        let frame = sidelens_frame_transform::Frame::from_bytes(
                            input_geometry,
                            std::mem::take(&mut nv12),
                        );
                        match frame {
                            Some(mut frame) => {
                                let window = sidelens_frame_effects::apply(
                                    &mut frame,
                                    effects,
                                    focus_window,
                                );
                                focus_window = window;
                                // 自动取景只算出窗口，不改帧尺寸（尺寸由 DirectShow
                                // 协商决定）。用户想要「真的裁」时，把窗口写进
                                // geometry.crop，让几何变换去执行。
                                if let Some(window) = window {
                                    if let Some(crop) = sidelens_frame_effects::focus_crop(window) {
                                        if effects.auto_frame.is_some()
                                            && geometry.crop.is_none()
                                            && window.width == output_width
                                            && window.height == output_height
                                        {
                                            // 窗口正好等于输出尺寸时无需裁切
                                        } else if effects.auto_frame.is_some() {
                                            geometry.crop = Some(crop);
                                            geometry_size_warned = false;
                                        }
                                    }
                                }
                                nv12 = frame.data;
                            }
                            None => {
                                eprintln!("解码帧长度不足以做画面效果，已跳过");
                            }
                        }
                    }
                    None => eprintln!(
                        "输出模式 {}x{} 不是合法 NV12 尺寸，跳过画面效果",
                        output_width, output_height
                    ),
                }
            }

            // 几何变换（旋转/裁切）先做：它可能改变尺寸，必须在按目标模式
            // 重建解码器之外单独处理 —— 解码器已按 output_width×output_height
            // 输出，这里再改尺寸会让共享内存里的帧与 DirectShow 协商的模式不一致。
            // 因此几何变换只在「用户显式请求」时启用，并且要求结果正好等于
            // 当前输出模式；不满足就打日志跳过，而不是悄悄塞一个尺寸不符的帧。
            if !geometry.is_identity() {
                let input_geometry =
                    sidelens_frame_transform::Geometry::new(output_width, output_height);
                match input_geometry {
                    Some(input_geometry) => {
                        let frame = sidelens_frame_transform::Frame::from_bytes(
                            input_geometry,
                            std::mem::take(&mut nv12),
                        );
                        match frame {
                            Some(frame) => {
                                match sidelens_frame_transform::apply(&frame, geometry) {
                                    Some(transformed)
                                        if transformed.geometry.width() == output_width
                                            && transformed.geometry.height() == output_height =>
                                    {
                                        nv12 = transformed.data;
                                    }
                                    Some(transformed) => {
                                        if !geometry_size_warned {
                                            geometry_size_warned = true;
                                            eprintln!(
                                            "几何变换会把 {}x{} 变成 {}x{}，与虚拟摄像头协商的输出模式不符，已跳过；\n  请让 DirectShow 客户端选择旋转后的分辨率",
                                            output_width,
                                            output_height,
                                            transformed.geometry.width(),
                                            transformed.geometry.height()
                                        );
                                        }
                                        // 变换结果尺寸不符：放回原帧，保持尺寸一致
                                        nv12 = frame.data;
                                    }
                                    None => {
                                        eprintln!("几何变换失败（尺寸或参数不合法），已跳过");
                                        nv12 = frame.data;
                                    }
                                }
                            }
                            None => {
                                eprintln!("解码帧长度不足以做几何变换，已跳过");
                            }
                        }
                    }
                    None => eprintln!(
                        "输出模式 {}x{} 不是合法 NV12 尺寸",
                        output_width, output_height
                    ),
                }
            }

            // 灌进共享内存
            transform_nv12(
                &mut nv12,
                output_width,
                output_height,
                mirror_horizontal,
                flip_vertical,
                color_look,
                color_adjustments,
                &mut sharpen_scratch,
            );
            let mut meta = FrameMeta::new(output_width, output_height, PixelFormat::Nv12);
            meta.pts_us = pts;
            meta.keyframe = u32::from(is_key);

            let slot = match ring.write_frame(&nv12, &meta) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("写共享内存失败: {e}");
                    continue;
                }
            };

            // 通知滤镜（如果已连接）
            if connected.load(Ordering::Relaxed) {
                let sig = FrameSignal::new(slot, meta);
                let result = match server.lock() {
                    Ok(server) => {
                        let result = server.write_message(&sig.encode());
                        if result.is_err() {
                            server.disconnect();
                        }
                        result
                    }
                    Err(_) => Err(std::io::Error::other("命名管道状态锁中毒")),
                };
                if let Err(e) = result {
                    connected.store(false, Ordering::Relaxed);
                    println!("滤镜管道写入失败，等待重连: {e}");
                }
            }

            decoded.fetch_add(1, Ordering::Relaxed);

            let n = decoded.load(Ordering::Relaxed);
            if n % 30 == 0 {
                let secs = started.elapsed().as_secs_f64();
                println!(
                    "  解码 {n} 帧  {:.1} fps  {:.1} Mbps",
                    n as f64 / secs,
                    (frames.load(Ordering::Relaxed) as f64 * 20_000.0 * 8.0) / secs / 1_000_000.0
                );
            }
        }
    }
}

/// 安装根目录。与 sidelens-setup / --update 用的是同一份推导规则。
fn install_root() -> std::path::PathBuf {
    sidelens_update::Layout::new(match std::env::var_os("SIDELENS_INSTALL_ROOT") {
        Some(root) => std::path::PathBuf::from(root),
        None => {
            let local = std::env::var_os("LOCALAPPDATA")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(std::env::temp_dir);
            sidelens_update::Layout::default_root(&local)
        }
    })
    .root()
    .to_path_buf()
}

fn start_control_input() -> Receiver<String> {
    let (tx, rx) = mpsc::channel();
    let _ = std::thread::Builder::new()
        .name("sidelens-ui-controls".into())
        .spawn(move || {
            for line in std::io::stdin().lock().lines() {
                let Ok(line) = line else { break };
                if line.len() > 4096 {
                    eprintln!("忽略超长手机控制消息");
                    continue;
                }
                if serde_json::from_str::<serde_json::Value>(&line).is_err() {
                    eprintln!("忽略格式错误的手机控制消息");
                    continue;
                }
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
    rx
}

/// 回应 Android 手机发出的局域网广播。视频仍走既有 TCP 帧流。
fn start_discovery(tcp_port: u16) {
    let _ = std::thread::Builder::new()
        .name("sidelens-phone-discovery".into())
        .spawn(move || {
            let socket = match UdpSocket::bind(("0.0.0.0", DISCOVERY_PORT)) {
                Ok(socket) => socket,
                Err(e) => {
                    eprintln!("Wi-Fi 自动发现未启动（UDP {DISCOVERY_PORT} 无法监听）: {e}");
                    return;
                }
            };
            let mut packet = [0u8; 256];
            println!("Wi-Fi 自动发现已启用（UDP {DISCOVERY_PORT}）");
            loop {
                match socket.recv_from(&mut packet) {
                    Ok((size, peer)) if &packet[..size] == DISCOVERY_REQUEST => {
                        let reply = format!("SIDELENS_HERE_V1|{tcp_port}");
                        if let Err(e) = socket.send_to(reply.as_bytes(), peer) {
                            eprintln!("回复手机发现请求失败: {e}");
                        }
                    }
                    Ok(_) => {}
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            || e.kind() == std::io::ErrorKind::TimedOut => {}
                    Err(e) => {
                        eprintln!("Wi-Fi 发现接收失败: {e}");
                        std::thread::sleep(Duration::from_millis(200));
                    }
                }
            }
        });
}

/// Applies desktop image orientation controls to the decoded NV12 frame before
/// it enters shared memory, so DirectShow consumers and the preview match.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ColorLook {
    Natural,
    Warm,
    Cool,
    Film,
    Vivid,
    Faded,
    Monochrome,
    Night,
}

/// A low-cost two-frame temporal filter. Static 2x2 blocks blend with the
/// previous frame; blocks with visible luma motion keep the current samples.
fn motion_aware_denoise_nv12(
    frame: &mut [u8],
    width: u32,
    height: u32,
    previous: &mut Vec<u8>,
    current: &mut Vec<u8>,
    previous_dimensions: &mut Option<(u32, u32)>,
) {
    let width_usize = width as usize;
    let height_usize = height as usize;
    if width_usize == 0 || height_usize == 0 || width_usize % 2 != 0 || height_usize % 2 != 0 {
        previous.clear();
        current.clear();
        *previous_dimensions = None;
        return;
    }
    let y_len = width_usize.saturating_mul(height_usize);
    let frame_len = y_len.saturating_add(y_len / 2);
    if frame.len() < frame_len {
        previous.clear();
        current.clear();
        *previous_dimensions = None;
        return;
    }
    let current_frame = &frame[..frame_len];
    if *previous_dimensions != Some((width, height)) || previous.len() != frame_len {
        previous.clear();
        previous.extend_from_slice(current_frame);
        current.clear();
        current.extend_from_slice(current_frame);
        *previous_dimensions = Some((width, height));
        return;
    }

    current.resize(frame_len, 0);
    current.copy_from_slice(current_frame);
    let threshold = 12u8;
    for index in 0..y_len {
        let now = current[index];
        let before = previous[index];
        if now.abs_diff(before) <= threshold {
            frame[index] = ((u16::from(now) * 3 + u16::from(before)) / 4) as u8;
        }
    }

    let previous_uv = &previous[y_len..frame_len];
    let current_uv = &current[y_len..frame_len];
    let output_uv = &mut frame[y_len..frame_len];
    for uv_row in 0..(height_usize / 2) {
        let y_row = uv_row * 2;
        let uv_row_start = uv_row * width_usize;
        for pair in 0..(width_usize / 2) {
            let x = pair * 2;
            let y0 = y_row * width_usize + x;
            let y1 = y0 + width_usize;
            let still = [y0, y0 + 1, y1, y1 + 1]
                .into_iter()
                .all(|index| current[index].abs_diff(previous[index]) <= threshold);
            if still {
                let uv = uv_row_start + x;
                for channel in 0..2 {
                    output_uv[uv + channel] = ((u16::from(current_uv[uv + channel]) * 3
                        + u16::from(previous_uv[uv + channel]))
                        / 4) as u8;
                }
            }
        }
    }
    previous.copy_from_slice(current);
    *previous_dimensions = Some((width, height));
}

impl ColorLook {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "natural" => Some(Self::Natural),
            "warm" => Some(Self::Warm),
            "cool" => Some(Self::Cool),
            "film" => Some(Self::Film),
            "vivid" => Some(Self::Vivid),
            "faded" => Some(Self::Faded),
            "mono" => Some(Self::Monochrome),
            "night" => Some(Self::Night),
            _ => None,
        }
    }

    /// Contrast, brightness offset, chroma saturation, warmth, and sharpening in NV12 space.
    fn adjustments(self) -> (f32, f32, f32, f32) {
        match self {
            Self::Natural => (1.0, 0.0, 1.0, 0.0),
            Self::Warm => (1.02, 0.0, 1.05, 8.0),
            Self::Cool => (1.0, 0.0, 1.05, -8.0),
            Self::Film => (0.92, 3.0, 0.78, 3.0),
            Self::Vivid => (1.08, 0.0, 1.25, 0.0),
            Self::Faded => (0.86, 10.0, 0.68, 0.0),
            Self::Monochrome => (1.0, 0.0, 0.0, 0.0),
            Self::Night => (0.90, 5.0, 0.82, 0.0),
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct ColorAdjustments {
    brightness: f32,
    contrast: f32,
    saturation: f32,
    warmth: f32,
    sharpen: f32,
}

impl Default for ColorAdjustments {
    fn default() -> Self {
        Self {
            brightness: 0.0,
            contrast: 1.0,
            saturation: 1.0,
            warmth: 0.0,
            sharpen: 0.0,
        }
    }
}

/// 对 NV12 帧做镜像/翻转/外观/调色/锐化的组合变换。
///
/// 八个参数都是逐帧变换的输入，且调用点只有一个（帧搬运的热路径）。
/// 把它们打包成结构体只会让每帧多一次构造 —— 这条路径在 60fps 下
/// 每秒走 60 次，而可读性收益为零。因此显式放宽 clippy 的参数上限。
#[allow(clippy::too_many_arguments)]
fn transform_nv12(
    frame: &mut [u8],
    width: u32,
    height: u32,
    mirror_horizontal: bool,
    flip_vertical: bool,
    look: ColorLook,
    adjustments: ColorAdjustments,
    sharpen_scratch: &mut Vec<u8>,
) {
    let has_adjustments = adjustments.brightness != 0.0
        || adjustments.contrast != 1.0
        || adjustments.saturation != 1.0
        || adjustments.warmth != 0.0
        || adjustments.sharpen > 0.0;
    if (!mirror_horizontal && !flip_vertical && look == ColorLook::Natural && !has_adjustments)
        || width == 0
        || height == 0
    {
        return;
    }
    let width = width as usize;
    let height = height as usize;
    if width % 2 != 0 || height % 2 != 0 {
        return;
    }
    let y_len = width.saturating_mul(height);
    let uv_len = y_len / 2;
    if frame.len() < y_len.saturating_add(uv_len) {
        return;
    }
    let (y_plane, rest) = frame.split_at_mut(y_len);
    let uv_plane = &mut rest[..uv_len];

    if look != ColorLook::Natural || has_adjustments {
        let (look_contrast, look_brightness, look_saturation, look_warmth) = look.adjustments();
        let contrast = look_contrast * adjustments.contrast;
        let brightness = look_brightness + adjustments.brightness;
        let saturation = look_saturation * adjustments.saturation;
        let warmth = look_warmth + adjustments.warmth;
        let mut luma_lut = [0u8; 256];
        let mut u_lut = [0u8; 256];
        let mut v_lut = [0u8; 256];
        for value in 0..256 {
            let centered = value as f32 - 128.0;
            luma_lut[value] = (centered * contrast + 128.0 + brightness)
                .round()
                .clamp(0.0, 255.0) as u8;
            u_lut[value] = ((value as f32 - 128.0) * saturation + 128.0 - warmth)
                .round()
                .clamp(16.0, 240.0) as u8;
            v_lut[value] = ((value as f32 - 128.0) * saturation + 128.0 + warmth)
                .round()
                .clamp(16.0, 240.0) as u8;
        }
        for value in y_plane.iter_mut() {
            *value = luma_lut[*value as usize];
        }
        for pair in uv_plane.chunks_exact_mut(2) {
            pair[0] = u_lut[pair[0] as usize];
            pair[1] = v_lut[pair[1] as usize];
        }
    }

    if adjustments.sharpen > 0.0 && width > 2 && height > 2 {
        sharpen_scratch.resize(y_plane.len(), 0);
        sharpen_scratch.copy_from_slice(y_plane);
        for y in 1..height - 1 {
            for x in 1..width - 1 {
                let index = y * width + x;
                let center = sharpen_scratch[index] as f32;
                let neighbors = sharpen_scratch[index - 1] as f32
                    + sharpen_scratch[index + 1] as f32
                    + sharpen_scratch[index - width] as f32
                    + sharpen_scratch[index + width] as f32;
                let detail = center - neighbors * 0.25;
                y_plane[index] = (center + detail * adjustments.sharpen)
                    .round()
                    .clamp(0.0, 255.0) as u8;
            }
        }
    }

    if mirror_horizontal {
        for row in y_plane.chunks_exact_mut(width) {
            row.reverse();
        }
        for row in uv_plane.chunks_exact_mut(width) {
            for pair in 0..(width / 4) {
                let left = pair * 2;
                let right = width - 2 - left;
                row.swap(left, right);
                row.swap(left + 1, right + 1);
            }
        }
    }

    if flip_vertical {
        for top in 0..(height / 2) {
            let bottom = height - 1 - top;
            let top_start = top * width;
            let bottom_start = bottom * width;
            let (before, after) = y_plane.split_at_mut(bottom_start);
            before[top_start..top_start + width].swap_with_slice(&mut after[..width]);
        }
        let uv_rows = height / 2;
        for top in 0..(uv_rows / 2) {
            let bottom = uv_rows - 1 - top;
            let top_start = top * width;
            let bottom_start = bottom * width;
            let (before, after) = uv_plane.split_at_mut(bottom_start);
            before[top_start..top_start + width].swap_with_slice(&mut after[..width]);
        }
    }
}
