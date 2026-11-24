//! 虚拟摄像头媒体源自检（不经 FrameServer）
//!
//! # 为什么需要这个
//!
//! `IMFVirtualCamera::Start` 一直报 `E_INVALIDARG`，而 FrameServer 的
//! 调用轨迹看起来已经走完了所有检查。要判断问题在「我们的媒体源实现」
//! 还是「虚拟摄像头设备注册」这两侧，最干净的办法是**绕过 FrameServer**：
//!
//!     1. 按注册表 CLSID 直接 CoCreateInstance → IMFActivate
//!     2. ActivateObject → IMFMediaSource
//!     3. CreatePresentationDescriptor → 选流
//!     4. 手动调 IMFMediaSource::Start
//!     5. 调 IMFMediaStream::RequestSample，看能不能拿到 MEMediaSample
//!
//! 这一串全过，说明媒体源本身是合格的，问题出在设备注册/FrameServer 侧。

#![allow(unsafe_code)]

use anyhow::Result;
use sidelens_vcam::register::SIDELENS_SOURCE_CLSID;
use tracing::{error, info, warn};
use windows::core::{Interface, GUID};
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFMediaSource, IMFMediaStream, IMFPresentationDescriptor, MEMediaSample,
    MFShutdown, MFStartup, MFSTARTUP_FULL, MF_VERSION,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};

/// 把 CLSID 字符串解析成 GUID
fn parse(s: &str) -> Option<GUID> {
    let t = s.trim().trim_start_matches('{').trim_end_matches('}');
    let parts: Vec<&str> = t.split('-').collect();
    if parts.len() != 5 {
        return None;
    }
    let d1 = u32::from_str_radix(parts[0], 16).ok()?;
    let d2 = u16::from_str_radix(parts[1], 16).ok()?;
    let d3 = u16::from_str_radix(parts[2], 16).ok()?;
    let mut bytes = [0u8; 8];
    let hex = format!("{}{}", parts[3], parts[4]);
    if hex.len() != 16 {
        return None;
    }
    for (i, b) in bytes.iter_mut().enumerate() {
        *b = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(GUID {
        data1: d1,
        data2: d2,
        data3: d3,
        data4: bytes,
    })
}

pub async fn run() -> Result<()> {
    let clsid = match parse(SIDELENS_SOURCE_CLSID) {
        Some(g) => g,
        None => {
            error!("CLSID 解析失败");
            return Ok(());
        }
    };
    info!(clsid = %SIDELENS_SOURCE_CLSID, "目标 CLSID");

    // SAFETY: COM/MF 初始化，成对清理
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
        MFStartup(MF_VERSION, MFSTARTUP_FULL)?;
    }

    let result = inner(clsid).await;

    // SAFETY: 与上面的初始化成对
    unsafe {
        let _ = MFShutdown();
        CoUninitialize();
    }
    result
}

async fn inner(clsid: GUID) -> Result<()> {
    // ---- 1. 按 CLSID 创建（走注册表，验证 HKLM 登记是否真的生效）----
    // SAFETY: clsid 有效；CLSCTX_INPROC_SERVER 表示进程内 DLL
    let activate: IMFActivate =
        match unsafe { CoCreateInstance(&clsid, None, CLSCTX_INPROC_SERVER) } {
            Ok(a) => {
                info!("✅ CoCreateInstance → IMFActivate");
                a
            }
            Err(e) => {
                error!("❌ CoCreateInstance 失败：{e}（注册表登记有问题）");
                return Ok(());
            }
        };

    // ---- 2. ActivateObject ----
    // SAFETY: activate 有效
    let source: IMFMediaSource = match unsafe { activate.ActivateObject() } {
        Ok(s) => {
            info!("✅ ActivateObject → IMFMediaSource");
            s
        }
        Err(e) => {
            error!("❌ ActivateObject 失败：{e}");
            return Ok(());
        }
    };

    // ---- 3. 呈现描述符 ----
    // SAFETY: source 有效
    let pd: IMFPresentationDescriptor = match unsafe { source.CreatePresentationDescriptor() } {
        Ok(p) => {
            info!("✅ CreatePresentationDescriptor");
            p
        }
        Err(e) => {
            error!("❌ CreatePresentationDescriptor 失败：{e}");
            return Ok(());
        }
    };

    // SAFETY: pd 有效
    let count = unsafe { pd.GetStreamDescriptorCount() }.unwrap_or(0);
    info!(streams = count, "流数量");

    if count > 0 {
        let mut selected = windows::core::BOOL(0);
        let mut sd = None;
        // SAFETY: pd 有效；索引 0 存在
        if unsafe { pd.GetStreamDescriptorByIndex(0, &mut selected, &mut sd) }.is_ok() {
            info!(selected = selected.as_bool(), "流 0 选中状态");
        }
    }

    // ---- 4. 手动 Start ----
    // SAFETY: pd 有效；时间格式传 null 表示 MF 默认
    match unsafe { source.Start(&pd, std::ptr::null(), std::ptr::null()) } {
        Ok(()) => info!("✅ IMFMediaSource::Start"),
        Err(e) => {
            error!("❌ IMFMediaSource::Start 失败：{e}");
            return Ok(());
        }
    }

    // ---- 5. 取流并请求样本 ----
    // SAFETY: source 有效
    let stream: IMFMediaStream = match source.cast() {
        Ok(s) => s,
        Err(e) => {
            warn!("source 不能直接转成 IMFMediaStream（流是独立对象，正常）：{e}");
            return Ok(());
        }
    };

    match unsafe { stream.RequestSample(None) } {
        Ok(()) => info!("✅ IMFMediaStream::RequestSample 被接受"),
        Err(e) => {
            error!("❌ RequestSample 失败：{e}");
            return Ok(());
        }
    }

    // 尝试取事件（有没有 MEMediaSample）
    // SAFETY: stream 有效
    match unsafe {
        stream.GetEvent(
            windows::Win32::Media::MediaFoundation::MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS(0),
        )
    } {
        Ok(e) => {
            // SAFETY: e 有效
            let ty = unsafe { e.GetType() }.unwrap_or(0);
            if ty as i32 == MEMediaSample.0 {
                info!("✅ 收到 MEMediaSample —— 帧投递链路通了");
            } else {
                info!(event_type = ty, "收到事件（非 MEMediaSample）");
            }
        }
        Err(e) => warn!("GetEvent 未拿到事件：{e}"),
    }

    // SAFETY: 成对 Shutdown
    unsafe {
        let _ = source.Shutdown();
    }
    info!("自检完成");
    Ok(())
}
