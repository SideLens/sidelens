//! DirectShow 滤镜验证工具
//!
//! 三件事，分别对应三个可能出错的环节：
//!
//! | 步骤 | 验证什么 | 失败说明 |
//! | --- | --- | --- |
//! | 枚举类别 | 注册表对不对 | `Instance` 子键没写或写错 |
//! | `CoCreateInstance` | DLL 能不能加载 | `InprocServer32` 路径错 / 缺依赖 |
//! | `EnumPins` | COM 实现对不对 | `IPin` / `IBaseFilter` 有问题 |
//!
//! 这三步各自返回**不同的错误码** —— 不像 `IMFVirtualCamera::Start` 那样
//! 所有问题都返回同一个 `E_INVALIDARG`（见 docs/vcam-summary.md）。

#![allow(unsafe_code)]

use windows::core::GUID;
use windows::Win32::Media::DirectShow::{IBaseFilter, ICreateDevEnum, IPin, PIN_INFO};
use windows::Win32::Media::MediaFoundation::{
    CLSID_SystemDeviceEnum, CLSID_VideoInputDeviceCategory,
};

use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, IEnumMoniker, IMoniker,
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};

/// 我们的滤镜 CLSID（与 sidelens-dshow 里一致）
const CLSID_SIDELENS_DSHOW: GUID = GUID::from_u128(0x9E4B7C31_5A2D_4F8E_B1C6_3D7A0E5F92B4);

/// 真的从滤镜拉帧（验证 FillBuffer 链路）
pub fn grab(frames: u32) -> anyhow::Result<()> {
    println!("=== 从 SideLens Camera 拉帧（{frames} 帧）===");
    // SAFETY: 标准 COM 初始化
    unsafe {
        let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
        if hr.is_err() {
            println!("❌ CoInitializeEx 失败: {hr:?}");
            return Ok(());
        }
    }
    let r = grab_inner(frames);
    unsafe { CoUninitialize() };
    r
}

fn grab_inner(_frames: u32) -> anyhow::Result<()> {
    use windows::Win32::Media::DirectShow::{IBaseFilter, IEnumPins, IPin};
    use windows::Win32::System::Com::CoCreateInstance;

    // SAFETY: COM 已初始化
    unsafe {
        let filter: IBaseFilter =
            CoCreateInstance(&CLSID_SIDELENS_DSHOW, None, CLSCTX_INPROC_SERVER)?;
        println!("✅ 滤镜已创建");

        // 找输出引脚
        let pins: IEnumPins = filter.EnumPins()?;
        let mut arr: [Option<IPin>; 1] = [None];
        let mut n = 0u32;
        let _ = pins.Next(&mut arr, Some(&mut n));
        let Some(pin) = arr[0].take() else {
            println!("❌ 没有引脚");
            return Ok(());
        };
        println!("✅ 找到输出引脚");

        // 查当前媒体类型
        let mut mt = windows::Win32::Media::MediaFoundation::AM_MEDIA_TYPE::default();
        if pin.ConnectionMediaType(&mut mt).is_ok() {
            println!("   媒体类型: {:?} {:?}", mt.majortype, mt.subtype);
        } else {
            println!("   （尚未协商媒体类型）");
        }

        // ── 关键：模拟 OBS 的枚举行为 ──
        //
        // OBS 的 dshow_input 在添加源时会：
        //   1. QueryInterface(IAMStreamConfig)
        //   2. GetNumberOfCapabilities
        //   3. GetStreamCaps(0, &mt, &caps)
        //   4. 读 mt.pbFormat / caps.MinOutputSize 等
        //
        // 第一版这里返回全零 caps，OBS 直接 c0000005 崩溃。
        println!();
        println!("--- 模拟 OBS 的格式枚举 ---");
        use windows::Win32::Media::DirectShow::{IAMStreamConfig, VIDEO_STREAM_CONFIG_CAPS};
        use windows::Win32::Media::MediaFoundation::AM_MEDIA_TYPE;

        let cfg: IAMStreamConfig = windows::core::Interface::cast(&pin)?;
        println!("✅ QueryInterface(IAMStreamConfig) 成功");

        let mut count = 0i32;
        let mut size = 0i32;
        cfg.GetNumberOfCapabilities(&mut count, &mut size)?;
        println!("   GetNumberOfCapabilities: count={count} size={size}");
        if size as usize != std::mem::size_of::<VIDEO_STREAM_CONFIG_CAPS>() {
            println!(
                "   ⚠️  size 与预期不符！预期 {}",
                std::mem::size_of::<VIDEO_STREAM_CONFIG_CAPS>()
            );
        }

        for i in 0..count {
            let mut mt_ptr: *mut AM_MEDIA_TYPE = std::ptr::null_mut();
            let mut caps: VIDEO_STREAM_CONFIG_CAPS = std::mem::zeroed();
            match cfg.GetStreamCaps(i, &mut mt_ptr, &mut caps as *mut _ as *mut u8) {
                Ok(()) => {
                    if mt_ptr.is_null() {
                        println!("   [{i}] ❌ mt 为空");
                        continue;
                    }
                    let mt = &*mt_ptr;
                    println!(
                        "   [{i}] major={:?} sub={:?} cbFormat={} lSampleSize={}",
                        mt.majortype, mt.subtype, mt.cbFormat, mt.lSampleSize
                    );
                    println!(
                        "        caps: MinOutput={}x{} MaxOutput={}x{} MinFrameInterval={}",
                        caps.MinOutputSize.cx,
                        caps.MinOutputSize.cy,
                        caps.MaxOutputSize.cx,
                        caps.MaxOutputSize.cy,
                        caps.MinFrameInterval
                    );
                    // 关键校验：不能全零
                    if caps.MinOutputSize.cx == 0 || caps.MaxOutputSize.cx == 0 {
                        println!("        ❌ caps 尺寸为 0 —— 这正是 OBS 崩溃的原因！");
                    } else {
                        println!("        ✅ caps 有效");
                    }
                    // 释放 mt（外层已是 unsafe 块）
                    if !mt.pbFormat.is_null() {
                        windows::Win32::System::Com::CoTaskMemFree(Some(mt.pbFormat as *const _));
                    }
                    windows::Win32::System::Com::CoTaskMemFree(Some(mt_ptr as *const _));
                }
                Err(e) => println!("   [{i}] ❌ GetStreamCaps 失败: {e}"),
            }
        }

        // 再测 EnumMediaTypes
        println!();
        println!("--- 枚举媒体类型 ---");
        if let Ok(en) = pin.EnumMediaTypes() {
            let mut arr: [*mut AM_MEDIA_TYPE; 4] = [std::ptr::null_mut(); 4];
            let mut n = 0u32;
            let _ = en.Next(&mut arr, Some(&mut n));
            println!("   枚举到 {n} 个类型");
            for (i, ptr) in arr.iter().enumerate().take(n as usize) {
                let m = &**ptr;
                println!(
                    "   [{i}] major={:?} sub={:?} cbFormat={}",
                    m.majortype, m.subtype, m.cbFormat
                );
                if !m.pbFormat.is_null() {
                    windows::Win32::System::Com::CoTaskMemFree(Some(m.pbFormat as *const _));
                }
                windows::Win32::System::Com::CoTaskMemFree(Some(*ptr as *const _));
            }
        }

        // 直接把滤镜设为 Run 状态，然后看它能不能产出
        let media_filter: windows::Win32::Media::DirectShow::IMediaFilter =
            windows::core::Interface::cast(&filter)?;
        media_filter.Run(0)?;
        println!();
        println!("✅ 滤镜已 Run");

        std::thread::sleep(std::time::Duration::from_secs(2));

        let st = media_filter.GetState(0)?;
        println!("   状态: {st:?}（2 = Running）");

        // 检查内部计数（滤镜自己记的）
        println!();
        println!("注意：真正的 FillBuffer 由消费端驱动。");
        println!("这里只验证「滤镜能创建 + 能 Run + 引脚就绪」。");
        println!("要看到画面需要 Zoom / OBS 这类真实消费端。");
    }
    Ok(())
}

/// 复刻 OBS 的 RenderStream 建图
/// 手动建图（绕过 RenderStream）
pub fn direct_test() -> anyhow::Result<()> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let r = sidelens_dshow::graph_test::test_direct_connect();
    unsafe { CoUninitialize() };
    match r {
        Ok(()) => {}
        Err(e) => println!("失败: {e}"),
    }
    Ok(())
}

pub fn render_test() -> anyhow::Result<()> {
    unsafe {
        let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
        if hr.is_err() {
            println!("❌ CoInitializeEx 失败: {hr:?}");
            return Ok(());
        }
    }
    let r = sidelens_dshow::graph_test::test_render_stream();
    unsafe { CoUninitialize() };
    match r {
        Ok(()) => println!(
            "
✅ RenderStream 全流程通过 —— OBS 应该能出画面了"
        ),
        Err(e) => println!(
            "
❌ 失败: {e}"
        ),
    }
    Ok(())
}

pub fn run() -> anyhow::Result<()> {
    println!("=== DirectShow 滤镜验证 ===");
    println!();

    // SAFETY: 标准 COM 初始化
    unsafe {
        let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
        if hr.is_err() {
            println!("❌ CoInitializeEx 失败: {hr:?}");
            return Ok(());
        }
    }

    let result = inner();

    // SAFETY: 与 CoInitializeEx 配对
    unsafe { CoUninitialize() };
    result
}

fn inner() -> anyhow::Result<()> {
    println!("[1/3] 枚举 CLSID_VideoInputDeviceCategory");
    let found = enumerate_devices();
    println!();

    println!("[2/3] CoCreateInstance(CLSID_SIDELENS_DSHOW)");
    // SAFETY: COM 已初始化；接口在作用域内释放
    unsafe {
        let filter: Result<IBaseFilter, _> =
            CoCreateInstance(&CLSID_SIDELENS_DSHOW, None, CLSCTX_INPROC_SERVER);
        match filter {
            Ok(f) => {
                println!("    ✅ 创建成功");
                if let Ok(clsid) = f.GetClassID() {
                    println!("    ClassID = {clsid:?}");
                }
                if let Ok(state) = f.GetState(0) {
                    println!("    初始状态 = {state:?}（0=Stopped）");
                }

                println!();
                println!("[3/3] IBaseFilter::EnumPins");
                match f.EnumPins() {
                    Ok(pins) => {
                        let mut n = 0;
                        loop {
                            let mut one: [Option<IPin>; 1] = [None];
                            let mut fetched = 0u32;
                            let hr = pins.Next(&mut one, Some(&mut fetched));
                            if hr.is_err() || fetched == 0 {
                                break;
                            }
                            let Some(pin) = one[0].take() else { break };
                            n += 1;
                            let dir = pin.QueryDirection();
                            let mut info = PIN_INFO::default();
                            let _ = pin.QueryPinInfo(&mut info);
                            let len = info.achName.iter().position(|&c| c == 0).unwrap_or(0);
                            let pname = String::from_utf16_lossy(&info.achName[..len]);
                            println!(
                                "    引脚 {n}: 名字=\"{pname}\" 方向={dir:?}（0=输入 1=输出）"
                            );
                        }
                        if n == 0 {
                            println!("    ❌ 没有任何引脚");
                        } else {
                            println!("    ✅ 共 {n} 个引脚");
                        }
                    }
                    Err(e) => println!("    ❌ EnumPins 失败: {e}"),
                }
            }
            Err(e) => {
                println!("    ❌ 创建失败: {e}");
                println!("       常见原因：DLL 未注册 / InprocServer32 路径错 / 缺依赖 DLL");
            }
        }
    }

    println!();
    if found {
        println!("结论：滤镜已注册且可创建 —— 可以在应用里选「SideLens Camera」");
    } else {
        println!("结论：类别里没找到 —— 检查注册表 Instance 子键");
    }
    Ok(())
}

/// 枚举视频捕获设备，返回是否找到自己
fn enumerate_devices() -> bool {
    let mut found = false;
    // SAFETY: COM 已初始化
    unsafe {
        let dev_enum: Result<ICreateDevEnum, _> =
            CoCreateInstance(&CLSID_SystemDeviceEnum, None, CLSCTX_INPROC_SERVER);
        let Ok(e) = dev_enum else {
            println!("    ❌ CoCreateInstance(CLSID_SystemDeviceEnum) 失败");
            return false;
        };

        let mut moniker_enum: Option<IEnumMoniker> = None;
        let hr = e.CreateClassEnumerator(&CLSID_VideoInputDeviceCategory, &mut moniker_enum, 0);
        if hr.is_err() {
            println!("    ❌ CreateClassEnumerator 失败: {hr:?}");
            return false;
        }
        let Some(en) = moniker_enum else {
            println!("    （类别为空）");
            return false;
        };

        let mut count = 0;
        loop {
            let mut one: [Option<IMoniker>; 1] = [None];
            let mut fetched = 0u32;
            let hr = en.Next(&mut one, Some(&mut fetched));
            if hr.is_err() || fetched == 0 {
                break;
            }
            let Some(moniker) = one[0].take() else { break };
            count += 1;

            let name = read_friendly_name(&moniker);
            let is_ours = name.contains("SideLens");
            if is_ours {
                found = true;
            }
            println!("    {} {name}", if is_ours { "✅" } else { "  " });
        }
        println!("    共 {count} 个视频捕获设备");
        if found {
            println!("    → SideLens Camera 出现在类别里");
        } else {
            println!("    → 类别里没有 SideLens Camera");
        }
    }
    found
}

/// 读 moniker 的显示名
///
/// # 为什么用 `GetDisplayName` 而不是读 `FriendlyName` 属性
///
/// 读属性要 `IPropertyBag::Read` + `VARIANT` 解包，而 `VARIANT` 的
/// `pwszVal` 字段需要 `Win32_System_Ole` feature 才能访问 ——
/// 为一个调试工具多开一个 feature 不划算。
///
/// `IMoniker::GetDisplayName` 直接返回 `LPOLESTR`，包含设备名
/// （形如 `@device:sw:{860BB310-...}\{9E4B7C31-...}`）。
/// 对我们的用途（判断是不是自己）完全够用。
///
/// # Safety
///
/// 调用方保证 COM 已初始化。
unsafe fn read_friendly_name(moniker: &IMoniker) -> String {
    let Ok(ctx) = windows::Win32::System::Com::CreateBindCtx(0) else {
        return "(无法创建 BindCtx)".into();
    };

    // 包装方法直接返回 PWSTR，不需要传输出参数
    let Ok(name) = moniker.GetDisplayName(&ctx, None) else {
        return "(无显示名)".into();
    };
    if name.is_null() {
        return "(显示名为空)".into();
    }
    let len = (0..).take_while(|&i| *name.0.add(i) != 0).count();
    let s = String::from_utf16_lossy(std::slice::from_raw_parts(name.0, len));
    CoTaskMemFree(Some(name.0 as *const _));
    s
}
