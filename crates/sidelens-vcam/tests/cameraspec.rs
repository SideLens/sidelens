#![cfg(windows)]
//
// 整个测试文件都是 Windows 专属的：要 CoInitializeEx、要 Media Foundation、
// 要真实注册表。非 Windows target 上直接跳过，否则 `cargo test` 会因为
// 「找不到 windows crate」而失败（见 docs/cross-platform.md P2）。

//! CameraSpec 校验测试
//!
//! 这些校验能在本地拦住「必然被 Win32 拒绝」的输入，
//! 避免开发者对着含糊的 HRESULT 猜。

use sidelens_vcam::register::RegisterError;
use sidelens_vcam::{CameraSpec, SIDELENS_SOURCE_CLSID};

#[test]
fn default_spec_is_valid() {
    let spec = CameraSpec::default();
    assert!(spec.validate().is_ok());
    assert_eq!(spec.source_id, SIDELENS_SOURCE_CLSID);
    assert!(!spec.friendly_name.is_empty());
}

#[test]
fn default_clsid_is_well_formed() {
    let s = SIDELENS_SOURCE_CLSID;
    assert_eq!(s.len(), 38, "CLSID 带花括号应为 38 字符");
    assert!(s.starts_with('{') && s.ends_with('}'));
    // 标准 GUID：8-4-4-4-12
    let inner = &s[1..s.len() - 1];
    let parts: Vec<&str> = inner.split('-').collect();
    assert_eq!(parts.len(), 5);
    assert_eq!(parts[0].len(), 8);
    assert_eq!(parts[1].len(), 4);
    assert_eq!(parts[2].len(), 4);
    assert_eq!(parts[3].len(), 4);
    assert_eq!(parts[4].len(), 12);
}

#[test]
fn dolphin_name_is_rejected() {
    // 这是实测踩过的错误写法
    let spec = CameraSpec {
        source_id: "SideLens.Camera.Virtual.1".into(),
        ..Default::default()
    };
    let err = spec.validate().expect_err("非 CLSID 格式应被拒绝");
    assert!(matches!(err, RegisterError::BadSourceId(_)));
}

#[test]
fn missing_braces_is_rejected() {
    let spec = CameraSpec {
        source_id: "7A3E1C42-5D88-4F6B-9E20-1B4D8C3A9F57".into(),
        ..Default::default()
    };
    assert!(spec.validate().is_err());
}

#[test]
fn non_hex_chars_are_rejected() {
    let spec = CameraSpec {
        source_id: "{ZZZZZZZZ-5D88-4F6B-9E20-1B4D8C3A9F57}".into(),
        ..Default::default()
    };
    assert!(spec.validate().is_err());
}

#[test]
fn empty_source_id_is_rejected() {
    let spec = CameraSpec {
        source_id: String::new(),
        ..Default::default()
    };
    assert!(spec.validate().is_err());
}

#[test]
fn custom_valid_clsid_is_accepted() {
    let spec = CameraSpec {
        friendly_name: "My Cam".into(),
        source_id: "{12345678-1234-1234-1234-123456789ABC}".into(),
    };
    assert!(spec.validate().is_ok());
}

#[test]
fn start_rejects_bad_spec_before_touching_win32() {
    // 关键：本地校验必须发生在任何 Win32 调用之前。
    // 这里在非初始化 COM 的测试线程里调用，如果校验没拦住就会报 COM 错误。
    let spec = CameraSpec {
        source_id: "not-a-clsid".into(),
        ..Default::default()
    };
    let err = sidelens_vcam::VirtualCamera::start(spec).expect_err("应被本地校验拒绝");
    assert!(
        matches!(err, RegisterError::BadSourceId(_)),
        "实际：{err:?}"
    );
}
