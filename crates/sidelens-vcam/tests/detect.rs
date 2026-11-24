//! 能力检测测试
//!
//! 在 Windows 上会真实读注册表，所以只断言"能跑通且结论自洽"，
//! 不断言具体 Build 号（不同机器不同）。

use sidelens_vcam::{probe, CapabilityReport, VirtualCameraSupport, MIN_BUILD};

#[test]
fn min_build_is_win11_21h2() {
    // MFCreateVirtualCamera 从 Win11 21H2（Build 22000）开始提供
    assert_eq!(MIN_BUILD, 22000);
}

#[test]
fn detect_returns_self_consistent_report() {
    let report: CapabilityReport = probe();

    // build 与 support 必须自洽
    match (report.build, report.support) {
        (Some(b), VirtualCameraSupport::Available) => {
            assert!(b >= MIN_BUILD, "标记可用但 Build {b} < {MIN_BUILD}");
        }
        (Some(b), VirtualCameraSupport::UnsupportedVersion) => {
            assert!(b < MIN_BUILD, "标记不支持但 Build {b} >= {MIN_BUILD}");
        }
        (None, VirtualCameraSupport::UnsupportedVersion) => {}
        (None, VirtualCameraSupport::NotWindows) => {}
        other => panic!("检测结果不自洽：{other:?}"),
    }
}

#[test]
fn can_run_agrees_with_support() {
    let report = probe();
    assert_eq!(report.can_run(), report.support.is_available());
}

#[cfg(windows)]
#[test]
fn windows_build_is_detected() {
    let report = probe();
    let build = report.build.expect("Windows 上应能拿到 Build 号");
    // 合理下限：Windows 10 是 10240
    assert!(build >= 10240, "Build 号异常小：{build}");
}

#[cfg(windows)]
#[test]
fn this_machine_reports_available() {
    // 本次开发机是 Build 26300（26H2），应当判定可用
    let report = probe();
    assert!(
        report.can_run(),
        "本机应支持虚拟摄像头，实际：{} / build={:?} / notes={:?}",
        report.support,
        report.build,
        report.notes
    );
}

#[test]
fn support_ordering_is_sensible() {
    use VirtualCameraSupport::*;
    assert!(NotWindows < Available);
    assert!(UnsupportedVersion < Available);
    assert!(MissingComponents < Available);
    assert!(Available.is_available());
    assert!(!NotWindows.is_available());
}

#[test]
fn support_display_is_human_readable() {
    for s in [
        VirtualCameraSupport::NotWindows,
        VirtualCameraSupport::UnsupportedVersion,
        VirtualCameraSupport::MissingComponents,
        VirtualCameraSupport::Available,
    ] {
        let text = s.to_string();
        assert!(!text.is_empty(), "{s:?} 的显示文本不应为空");
    }
}
