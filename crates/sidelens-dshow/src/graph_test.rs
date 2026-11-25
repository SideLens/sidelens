//! 建图测试：复刻 OBS 的 RenderStream 调用链
//!
//! # 为什么需要这个
//!
//! OBS 的 `dshow_input` 建图流程是：
//!
//! ```text
//! CoCreateInstance(CLSID_CaptureGraphBuilder2)
//!   → SetFiltergraph
//!   → RenderStream(&PIN_CATEGORY_CAPTURE, &MEDIATYPE_Video, pSrcFilter, NULL, NULL)
//! ```
//!
//! `RenderStream` 内部会：
//! 1. 枚举源滤镜的输出引脚
//! 2. 找一个能连的媒体类型
//! 3. 调 `IPin::Connect`（**不是 ReceiveConnection**）
//! 4. 插入解码器 / 渲染器
//!
//! 关键洞察：**`RenderStream` 走的是 `Connect`，不是 `ReceiveConnection`**。
//! 我的 `Connect` 直接返回 `VFW_E_NOT_CONNECTED`，所以 OBS 永远连不上。
//!
//! # 正确实现
//!
//! 源滤镜的 `Connect` 应该：
//! 1. 检查对端是输入引脚
//! 2. 尝试各种媒体类型（`EnumMediaTypes` 里的）
//! 3. 对每个类型调 `peer.ReceiveConnection(self, mt)`
//! 4. 成功就返回 S_OK

#![allow(unsafe_code)]

use windows::core::{Interface, Result as WinResult};
use windows::Win32::Media::DirectShow::IBaseFilter;
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};

use crate::filter::CLSID_SIDELENS_DSHOW;

/// 手动建图：直接调 IPin::Connect 到 Null Renderer
///
/// # 为什么绕过 RenderStream
///
/// `RenderStream` 在找「匹配 PIN_CATEGORY_CAPTURE 的引脚」时用了
/// 额外的判据（`IKsPin` / 引脚类别），我们的引脚没声明类别，
/// 所以它在 Connect 之前就放弃了。
///
/// 这个测试直接用 `IPin::Connect` —— 那是最终真正的连接动作。
/// 只要这一步能通，就说明引脚实现是对的，剩下的只是「怎么让
/// RenderStream 找到我们」的问题。
pub fn test_direct_connect() -> WinResult<()> {
    use windows::Win32::Media::DirectShow::{IBaseFilter, IGraphBuilder, IPin};
    use windows::Win32::Media::MediaFoundation::CLSID_FilterGraph;

    println!("=== 手动建图：直接 Connect ===");

    unsafe {
        let graph: IGraphBuilder =
            CoCreateInstance(&CLSID_FilterGraph, None, CLSCTX_INPROC_SERVER)?;
        println!("✅ FilterGraph 已创建");

        let src: IBaseFilter = CoCreateInstance(&CLSID_SIDELENS_DSHOW, None, CLSCTX_INPROC_SERVER)?;
        graph.AddFilter(&src, windows::core::w!("SideLens Source"))?;
        println!("✅ 源滤镜已加入图");

        // 拿源滤镜的输出引脚
        let pins = src.EnumPins()?;
        let mut arr: [Option<IPin>; 1] = [None];
        let mut n = 0u32;
        let _ = pins.Next(&mut arr, Some(&mut n));
        let Some(out_pin) = arr[0].take() else {
            println!("❌ 没有输出引脚");
            return Ok(());
        };
        println!("✅ 拿到输出引脚");

        // 关键验证：QueryInterface 能否拿到 IKsPropertySet
        use windows::Win32::Media::KernelStreaming::IKsPropertySet;
        match out_pin.cast::<IKsPropertySet>() {
            Ok(ks) => {
                println!("✅ QI(IKsPropertySet) 成功");
                // 查引脚类别
                let mut guid = windows::core::GUID::zeroed();
                let mut returned = 0u32;
                let propset =
                    windows::core::GUID::from_u128(0xf72a76a0_eb0a_11d0_ace4_0000c0cc16ba);
                match ks.Get(
                    &propset,
                    0,
                    std::ptr::null(),
                    0,
                    &mut guid as *mut _ as *mut core::ffi::c_void,
                    std::mem::size_of::<windows::core::GUID>() as u32,
                    &mut returned,
                ) {
                    Ok(()) => {
                        println!("   引脚类别 = {guid:?}");
                        let capture = windows::Win32::Media::MediaFoundation::PIN_CATEGORY_CAPTURE;
                        if guid == capture {
                            println!("   ✅ 是 PIN_CATEGORY_CAPTURE");
                        } else {
                            println!("   ⚠️  不是 PIN_CATEGORY_CAPTURE（期望 {capture:?}）");
                        }
                    }
                    Err(e) => println!("   ❌ IKsPropertySet::Get 失败: {e}"),
                }
            }
            Err(e) => println!("❌ QI(IKsPropertySet) 失败: {e} —— RenderStream 就是卡在这"),
        }

        // 建一个渲染器当消费端
        //
        // 用 VideoRenderer（CLSID_NullRenderer 在 windows crate 里没导出）。
        // 它接受 NV12 并自己协商分配器 —— 正是我们要验证的路径。
        let null_renderer: IBaseFilter = CoCreateInstance(
            &windows::Win32::Media::MediaFoundation::CLSID_VideoRenderer,
            None,
            CLSCTX_INPROC_SERVER,
        )?;
        graph.AddFilter(&null_renderer, windows::core::w!("Video Renderer"))?;
        println!("✅ Video Renderer 已加入图");

        // 拿它的输入引脚
        let rpins = null_renderer.EnumPins()?;
        let mut rarr: [Option<IPin>; 1] = [None];
        let mut rn = 0u32;
        let _ = rpins.Next(&mut rarr, Some(&mut rn));
        let Some(in_pin) = rarr[0].take() else {
            println!("❌ Null Renderer 没有输入引脚");
            return Ok(());
        };
        println!("✅ 拿到输入引脚");

        // 关键：直接 Connect
        println!("   调用 out_pin.Connect(in_pin, None)…");
        match out_pin.Connect(&in_pin, None) {
            Ok(()) => {
                println!("✅ Connect 成功！");
                // 查状态
                match out_pin.ConnectedTo() {
                    Ok(_peer) => {
                        println!("✅ ConnectedTo 返回对端");
                        let mut mt =
                            windows::Win32::Media::MediaFoundation::AM_MEDIA_TYPE::default();
                        if out_pin.ConnectionMediaType(&mut mt).is_ok() {
                            println!("   协商格式: {:?}", mt.subtype);
                        }
                    }
                    Err(e) => println!("❌ ConnectedTo 失败: {e}"),
                }
            }
            Err(e) => println!("❌ Connect 失败: {e} (0x{:08X})", e.code().0),
        }
    }
    Ok(())
}

/// 用 CaptureGraphBuilder2 建图（复刻 OBS）
pub fn test_render_stream() -> WinResult<()> {
    use windows::Win32::Media::DirectShow::{ICaptureGraphBuilder2, IGraphBuilder};
    use windows::Win32::Media::MediaFoundation::{
        CLSID_CaptureGraphBuilder2, CLSID_FilterGraph, MEDIATYPE_Video, PIN_CATEGORY_CAPTURE,
    };

    println!("=== 复刻 OBS 的 RenderStream ===");

    // SAFETY: 调用方保证 COM 已初始化
    unsafe {
        // 1. 建图
        let graph: IGraphBuilder =
            CoCreateInstance(&CLSID_FilterGraph, None, CLSCTX_INPROC_SERVER)?;
        println!("✅ FilterGraph 已创建");

        // 2. 建 capture graph builder
        let builder: ICaptureGraphBuilder2 =
            CoCreateInstance(&CLSID_CaptureGraphBuilder2, None, CLSCTX_INPROC_SERVER)?;
        builder.SetFiltergraph(&graph)?;
        println!("✅ CaptureGraphBuilder2 已就绪");

        // 3. 加源滤镜
        let src: IBaseFilter = CoCreateInstance(&CLSID_SIDELENS_DSHOW, None, CLSCTX_INPROC_SERVER)?;
        graph.AddFilter(&src, windows::core::w!("SideLens Source"))?;
        println!("✅ 源滤镜已加入图");

        // 4. RenderStream —— 这就是 OBS 崩/失败的地方
        println!("   调用 RenderStream…");
        // 签名：RenderStream(pcategory: Option<*const GUID>, ptype: *const GUID,
        //                   psource, pfcompressor, pfrenderer)
        // 直接测 PIN_CATEGORY_CAPTURE（OBS 用的就是这个）
        println!("   调用 RenderStream(category=PIN_CATEGORY_CAPTURE)");
        match builder.RenderStream(
            Some(&PIN_CATEGORY_CAPTURE as *const _),
            &MEDIATYPE_Video as *const _,
            &src,
            None,
            None,
        ) {
            Ok(()) => println!("✅ RenderStream 成功"),
            Err(e) => {
                println!("❌ RenderStream 失败: {e} (0x{:08X})", e.code().0);
                return Err(e);
            }
        }

        // 5. 查引脚连接状态
        if let Ok(pins) = src.EnumPins() {
            let mut arr: [Option<windows::Win32::Media::DirectShow::IPin>; 1] = [None];
            let mut n = 0u32;
            let _ = pins.Next(&mut arr, Some(&mut n));
            if let Some(pin) = arr[0].take() {
                match pin.ConnectedTo() {
                    Ok(_peer) => {
                        let mut mt =
                            windows::Win32::Media::MediaFoundation::AM_MEDIA_TYPE::default();
                        let _ = pin.ConnectionMediaType(&mut mt);
                        println!("✅ 引脚已连接到对端，格式={:?}", mt.subtype);
                    }
                    Err(e) => println!("❌ 引脚未连接: {e}"),
                }
            }
        }
    }
    Ok(())
}
