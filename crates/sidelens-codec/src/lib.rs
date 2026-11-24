//! SideLens 编解码层
//!
//! # 为什么需要单独一层
//!
//! 编码器是**平台专属**的：
//!
//! | 平台 | 硬件编码 | 硬件解码 |
//! | --- | --- | --- |
//! | Windows | Media Foundation / NVENC / QSV / AMF | 同左 |
//! | macOS / iOS | VideoToolbox | VideoToolbox |
//! | Android | MediaCodec | MediaCodec |
//! | Linux | VA-API / NVENC | 同左 |
//! | HarmonyOS | AVCodec Kit | AVCodec Kit |
//!
//! 但**码流处理**是跨平台通用的：NALU 切分、SPS/PPS 提取、
//! 关键帧判断、Annex-B 与 AVCC 互转。这些纯字节操作放这里，
//! 各平台实现只负责"把帧喂给硬件、把码流拿出来"。
//!
//! # 本层的能力
//!
//! - [x] Annex-B 码流切分（`00 00 00 01` / `00 00 01`）
//! - [x] NALU 类型识别（IDR / SPS / PPS / 普通切片）
//! - [x] 关键帧判断（IDR）
//! - [x] SPS/PPS 提取与缓存
//! - [x] Annex-B ↔ AVCC 长度前缀互转
//! - [ ] 参数集更新检测（分辨率变化）
//! - [ ] 码率统计

#![forbid(unsafe_code)]
#![warn(missing_debug_implementations)]

pub mod audio;
pub mod bitrate;
pub mod nalu;
pub mod sps;

pub use bitrate::{BitrateConfig, BitrateTracker, IntervalStats};
pub use nalu::{
    annexb_to_avcc, avcc_to_annexb, is_keyframe, split_annexb, NaluType, NaluView, ParameterSets,
    AVCC_LENGTH_SIZE,
};
pub use sps::{ParameterSetTracker, ResolutionChange, SpsError, SpsInfo, VuiInfo};
