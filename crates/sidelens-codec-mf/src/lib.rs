//! SideLens H.264 编解码器（Windows Media Foundation）
//!
//! # 为什么用 MFT 而不是自己编 H.264
//!
//! 本机已注册的编码器 MFT：
//!
//! | MFT | 模块 | 类型 |
//! | --- | --- | --- |
//! | H.264 Encoder MFT | mfh264enc.dll | 软件、同步 |
//! | Intel® Quick Sync Video H.264 Encoder MFT | mfxplugin64_h264e.dll | 硬件、**异步** |
//! | NVIDIA H.264 Encoder MFT | nvEncMFTCodec64.dll | 硬件、**异步** |
//!
//! 本 crate 只走**同步** MFT：ProcessInput / ProcessOutput 轮询。
//! 硬件 MFT 是异步的，必须解锁 MF_TRANSFORM_ASYNC_UNLOCK 并跑
//! IMFMediaEventGenerator 事件循环 —— 那是另一套复杂度，先不碰。
//! 用 [@@H264Encoder::list_encoders@@] / [@@H264Decoder::list_decoders@@]
//! 可以列出本机所有候选及其异步标记。
//!
//! # 两端
//!
//! | 方向 | 类型 | 输入 | 输出 |
//! | --- | --- | --- | --- |
//! | 编码 | [@@H264Encoder@@] | NV12 | H.264 Annex-B |
//! | 解码 | [@@H264Decoder@@] | H.264 Annex-B | NV12 / YV12 / IYUV |
//!
//! 两者都用微软自带的**软件** MFT（mfh264enc.dll / msmpeg2vdec.dll 系），
//! 行为确定、可复现，适合实时投屏这种「延迟比画质重要」的场景。
//!
//! # 输出格式：不猜，实测
//!
//! 协议层与 sidelens-codec::nalu 都按 **Annex-B**（00 00 00 01 起始码）设计。
//! MF 的 H.264 **编码器**到底吐 Annex-B 还是 AVCC **没有文档保证**，所以本 crate
//! 在拿到第一帧输出时按字节特征**实测**判断，再统一归一化成 Annex-B
//! （见 [@@encoder::OutputFormat@@] 与 [@@H264Encoder::output_format@@]）。
//!
//! 解码器这一侧反过来：实测它**只吃 Annex-B**，不需要任何转换
//! （见 [@@H264Decoder@@] 的文档）。
//!
//! 猜错了整条链路的 is_keyframe 都会失效，而且表现为「偶尔花屏」
//! 这种最难查的症状。
//!
//! # 两个同名但语义不同的 OutputFormat
//!
//! | 名字 | 说的是什么 | 取值 |
//! | --- | --- | --- |
//! | [@@encoder::OutputFormat@@] | **码流字节布局** | Annex-B / AVCC |
//! | [@@decoder::OutputFormat@@] | **像素布局** | NV12 / YV12 / IYUV |
//!
//! crate 根把解码器那个重导出成 [@@DecodedFormat@@]，避免调用方
//! `use sidelens_codec_mf::OutputFormat` 时拿错类型。
//!
//! # 跨平台
//!
//! 整个实现都在 cfg(windows) 之下；非 Windows 上这个 crate 只剩
//! [@@EncoderError@@] 类型，可以正常参与 workspace 构建。

#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_debug_implementations)]

pub mod error;

#[cfg(windows)]
mod com;
#[cfg(windows)]
mod decoder;
#[cfg(windows)]
mod encoder;
#[cfg(windows)]
mod variant;

pub use error::{EncoderError, Result};

#[cfg(windows)]
pub use com::{InitError, MediaFoundation};
#[cfg(windows)]
pub use decoder::{
    DecoderConfig, DecoderInfo, H264Decoder, OutputFormat as DecodedFormat,
    CLSID_CMSH264DECODERMFT, DEFAULT_LEVEL_IDC, DEFAULT_PROFILE_IDC,
};
#[cfg(windows)]
pub use encoder::{
    detect_format, max_bitrate_for, EncoderConfig, EncoderInfo, H264Encoder, OutputFormat,
    CLSID_CMSH264ENCODERMFT, MAX_BITRATE_BPS,
};
