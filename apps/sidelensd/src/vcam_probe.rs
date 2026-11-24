//! 虚拟摄像头验证工具
//!
//! 完整链路：初始化 COM → 注册类工厂 → MFCreateVirtualCamera → Start
//!
//! 每一步都可能失败，且错误码各不相同。详见 docs/vcam-pitfalls.md。

#![allow(unsafe_code)]

use std::time::Duration;

use windows::core::GUID;

use sidelens_vcam::detect::probe;
use sidelens_vcam::media_source::RegisteredClass;
use sidelens_vcam::register::{
    fsm_virtual_camera_category, video_camera_category, CameraSpec, VirtualCamera,
    SIDELENS_SOURCE_CLSID,
};
use sidelens_vcam::source::SourceConfig;
use tracing::{error, info, warn};

pub async fn run(hold_seconds: u64, remove_on_exit: bool, skip_inproc: bool) -> anyhow::Result<()> {
    let report = probe();
    info!(support = %report.support, build = ?report.build, "能力检测");
    if !report.can_run() {
        error!("本机不支持：{}", report.support);
        return Ok(());
    }

    // 门槛 2：COM(MTA) + MF
    let _mf = match sidelens_vcam::MediaFoundation::init() {
        Ok(g) => {
            info!("COM(MTA) + Media Foundation 已初始化");
            g
        }
        Err(e) => {
            error!("初始化失败：{e}");
            return Ok(());
        }
    };

    let spec = CameraSpec::default();
    let clsid = match spec.source_guid() {
        Some(g) => g,
        None => {
            error!("CLSID 解析失败：{}", spec.source_id);
            return Ok(());
        }
    };
    info!(clsid = %spec.source_id, "CLSID 就绪");

    // 门槛 4：注册类工厂。
    //
    // 注意：CoRegisterClassObject 只对**本进程**的 CoCreateInstance 生效。
    // 如果 DLL 已登记到注册表，FrameServer 会自己加载 DLL，
    // 此时进程内注册反而会"抢先"生效，掩盖真实路径。
    // 用 --no-inproc-class 可强制走注册表路径来验证。
    let _class = if skip_inproc {
        info!("跳过进程内类工厂注册（强制走注册表 DLL 路径）");
        None
    } else {
        match RegisteredClass::register(clsid, SourceConfig::default()) {
            Ok(c) => {
                info!(cookie = c.cookie(), "类工厂已注册（进程内）");
                Some(c)
            }
            Err(e) => {
                error!("类工厂注册失败：{e}");
                return Ok(());
            }
        }
    };

    // 门槛 3 + MFCreateVirtualCamera
    info!(
        friendly = %spec.friendly_name,
        "创建虚拟摄像头"
    );
    // `--regentry`：走完 `VirtualCamera::start()` 的完整流程后，
    // 再补设注册表项（AddRegistryEntry），然后重试 Start。
    //
    // # 为什么要单独一个开关
    //
    // 直接改 `start()` 会让所有路径都受影响，看不出「加了注册表项」
    // 到底有没有用。用开关做 A/B 才能定位。
    let use_regentry = std::env::args().any(|a| a == "--regentry");
    // `--category`：把设备登记到哪个 KS 类别下。可重复，用来做 A/B：
    //
    // | 参数 | 类别 |
    // | --- | --- |
    // | （默认） | `KSCATEGORY_VIDEO_CAMERA` `{E5323777-...}` |
    // | `--category fsm` | `KSCATEGORY_FSM_VIRTUAL_CAMERA` `{588C8D20-...}` |
    // | `--category both` | 两个都登记 |
    //
    // `both` 是关键：如果 FrameServer 只在 FSM 类别里找，
    // 但应用（相机 / Zoom）只在 VIDEO_CAMERA 里找，两个都登记才能同时满足。
    let cats: Vec<GUID> = {
        let mut out = Vec::new();
        let mut want_fsm = false;
        let mut want_vc = false;
        for (i, a) in std::env::args().enumerate() {
            if a == "--category" {
                if let Some(v) = std::env::args().nth(i + 1) {
                    match v.as_str() {
                        "fsm" => want_fsm = true,
                        "vc" => want_vc = true,
                        "both" => {
                            want_fsm = true;
                            want_vc = true;
                        }
                        other => warn!("未知类别 {other}，可选 fsm / vc / both"),
                    }
                }
            }
        }
        if !want_fsm && !want_vc {
            want_vc = true;
        }
        if want_vc {
            out.push(video_camera_category());
        }
        if want_fsm {
            out.push(fsm_virtual_camera_category());
        }
        out
    };
    info!(
        categories = ?cats.iter().map(|g| format!("{g:?}")).collect::<Vec<_>>(),
        "设备类别"
    );
    let mut cam = match VirtualCamera::start_with_categories(spec, use_regentry, &cats) {
        Ok(c) => c,
        Err(e) => {
            error!("创建失败：{e}");
            // 权限是最常见的失败原因，给出可操作的提示而不是让用户猜
            if format!("{e}").contains("0x80070005") || format!("{e}").contains("拒绝访问") {
                error!("");
                error!("════════════════════════════════════════════════════════");
                error!("原因：注册虚拟摄像头需要管理员权限");
                error!("");
                error!("AddProperty 要写系统设备接口属性，普通用户没有权限。");
                error!("这与 MFVirtualCameraAccess_CurrentUser 无关 —— 即使只给");
                error!("当前用户创建，写设备树本身仍是管理员级操作。");
                error!("");
                error!("请用管理员权限的终端重试：");
                error!("  Windows 终端(管理员) → 重新运行本命令");
                error!("════════════════════════════════════════════════════════");
            }
            return Ok(());
        }
    };
    if remove_on_exit {
        cam.arm_removal();
    }

    info!("✅ Start 成功 —— 设备已出现在系统的摄像头列表里");
    info!("请在 {hold_seconds} 秒内打开「相机」应用检查 SideLens Camera");
    info!("（当前实现只协商格式，不投递帧，画面会是黑的 —— 这是预期的中间状态）");

    tokio::time::sleep(Duration::from_secs(hold_seconds)).await;

    info!("撤销");
    drop(cam);
    tokio::time::sleep(Duration::from_millis(500)).await;
    info!("完成");
    let _ = SIDELENS_SOURCE_CLSID;
    let _ = &_class;
    warn!("注意：若未加 --remove，注册记录会保留以便下次复用");
    Ok(())
}
