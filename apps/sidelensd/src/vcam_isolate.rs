//! 虚拟摄像头 E_INVALIDARG 隔离实验
//!
//! 官方 VCamSample 的调用序列是：
//!
//! ```cpp
//! MFCreateVirtualCamera(SoftwareCameraSource, Session, CurrentUser,
//!                      title, clsid, nullptr, 0, &vcam);
//! vcam->Start(nullptr);
//! ```
//!
//! 与我们不同的地方：categories 传 null、lifetime=Session、access=CurrentUser，
//! 而且不调任何 AddProperty。
//!
//! 本实验从「完全照抄官方」开始逐项叠加，看哪一项让 Start 变成 E_INVALIDARG。

#![allow(unsafe_code)]

use std::time::Duration;

use windows::core::HSTRING;
use windows::Win32::Media::MediaFoundation::{
    IMFAsyncCallback, MFCreateVirtualCamera, MFVirtualCameraAccess_AllUsers,
    MFVirtualCameraAccess_CurrentUser, MFVirtualCameraLifetime_Session,
    MFVirtualCameraLifetime_System, MFVirtualCameraType_SoftwareCameraSource,
};

use sidelens_vcam::register::{video_camera_category, SIDELENS_SOURCE_CLSID};
use sidelens_vcam::MediaFoundation;

struct Step {
    name: &'static str,
    lifetime: i32,
    access: i32,
    with_categories: bool,
    with_callback: bool,
}

fn run_step(step: &Step, idx: usize) -> (bool, String) {
    let friendly = HSTRING::from(format!("SideLens Iso {idx}"));
    let source = HSTRING::from(SIDELENS_SOURCE_CLSID);

    let cats = if step.with_categories {
        Some([video_camera_category()])
    } else {
        None
    };

    let cam = unsafe {
        MFCreateVirtualCamera(
            MFVirtualCameraType_SoftwareCameraSource,
            std::mem::transmute::<
                i32,
                windows::Win32::Media::MediaFoundation::MFVirtualCameraLifetime,
            >(step.lifetime),
            std::mem::transmute::<i32, windows::Win32::Media::MediaFoundation::MFVirtualCameraAccess>(
                step.access,
            ),
            &friendly,
            &source,
            cats.as_ref().map(|a| a.as_slice()),
        )
    };

    let cam = match cam {
        Ok(c) => c,
        Err(e) => return (false, format!("MFCreateVirtualCamera 失败: {e}")),
    };

    let hr = if step.with_callback {
        let cb = sidelens_vcam::callback::VcamCallback::create();
        unsafe { cam.Start(&cb) }
    } else {
        unsafe { cam.Start(None::<&IMFAsyncCallback>) }
    };

    match hr {
        Ok(()) => {
            unsafe {
                let _ = cam.Stop();
                let _ = cam.Remove();
            }
            (true, String::new())
        }
        Err(e) => {
            unsafe {
                let _ = cam.Remove();
            }
            (false, format!("Start: {e}"))
        }
    }
}

pub async fn run() -> anyhow::Result<()> {
    let _mf = match MediaFoundation::init() {
        Ok(g) => g,
        Err(e) => {
            eprintln!("COM/MF 初始化失败: {e}");
            return Ok(());
        }
    };

    let session = MFVirtualCameraLifetime_Session.0;
    let system = MFVirtualCameraLifetime_System.0;
    let current_user = MFVirtualCameraAccess_CurrentUser.0;
    let all_users = MFVirtualCameraAccess_AllUsers.0;

    let steps = [
        Step {
            name: "官方原样(Session+CurrentUser,无cats,无cb)",
            lifetime: session,
            access: current_user,
            with_categories: false,
            with_callback: false,
        },
        Step {
            name: "+callback",
            lifetime: session,
            access: current_user,
            with_categories: false,
            with_callback: true,
        },
        Step {
            name: "+categories",
            lifetime: session,
            access: current_user,
            with_categories: true,
            with_callback: false,
        },
        Step {
            name: "System+CurrentUser",
            lifetime: system,
            access: current_user,
            with_categories: false,
            with_callback: false,
        },
        Step {
            name: "Session+AllUsers",
            lifetime: session,
            access: all_users,
            with_categories: false,
            with_callback: false,
        },
        Step {
            name: "System+AllUsers(无cats无cb)",
            lifetime: system,
            access: all_users,
            with_categories: false,
            with_callback: false,
        },
        Step {
            name: "我们的组合(全开)",
            lifetime: system,
            access: all_users,
            with_categories: true,
            with_callback: true,
        },
    ];

    println!("=== 虚拟摄像头调用序列隔离实验 ===");
    println!(
        "枚举值: Session={session} System={system} CurrentUser={current_user} AllUsers={all_users}"
    );
    println!();

    let mut any_ok = false;
    let mut results: Vec<(&str, bool, String)> = Vec::new();
    for (i, step) in steps.iter().enumerate() {
        println!("[{}/{}] {}", i + 1, steps.len(), step.name);
        let (ok, err) = run_step(step, i);
        if ok {
            any_ok = true;
            println!("    ✅ Start 成功");
        } else {
            println!("    ❌ {err}");
        }
        results.push((step.name, ok, err));
        tokio::time::sleep(Duration::from_millis(300)).await;
    }

    println!();
    println!("=== 汇总 ===");
    for (name, ok, err) in &results {
        println!("  {:<40} {}", name, if *ok { "OK" } else { err.as_str() });
    }
    println!();
    if any_ok {
        println!("至少一种组合能让 Start 成功");
    } else {
        println!("全部失败 —— 问题不在调用参数");
    }
    Ok(())
}
