#![cfg(windows)]
//
// 整个测试文件都是 Windows 专属的：要 CoInitializeEx、要 Media Foundation、
// 要真实注册表。非 Windows target 上直接跳过，否则 `cargo test` 会因为
// 「找不到 windows crate」而失败（见 docs/cross-platform.md P2）。

//! 激活器属性语义测试
//!
//! 这些不涉及 COM，只验证 PropValue 与 MF_ATTRIBUTE_TYPE 的映射是否自洽。
//! 映射错了会让消费端拿到错的类型，表现为「设置生效但行为不对」—— 极难排查。

use sidelens_vcam::activate::PropValue;

/// 把属性类型映射单独暴露出来测（与 IMFAttributes::GetItemType 用的是同一逻辑）
fn attr_type_of(v: &PropValue) -> i32 {
    v.mf_attribute_type()
}

#[test]
fn uint32_maps_to_vt_ui4_type() {
    // MF_ATTRIBUTE_UINT32 = 19
    assert_eq!(attr_type_of(&PropValue::UInt32(0)), 19);
}

#[test]
fn uint64_maps_to_vt_ui8_type() {
    // MF_ATTRIBUTE_UINT64 = 21
    assert_eq!(attr_type_of(&PropValue::UInt64(0)), 21);
}

#[test]
fn double_maps_to_vt_r8_type() {
    // MF_ATTRIBUTE_DOUBLE = 5
    assert_eq!(attr_type_of(&PropValue::Double(0.0)), 5);
}

#[test]
fn string_maps_to_vt_lpwstr_type() {
    // MF_ATTRIBUTE_STRING = 31
    assert_eq!(attr_type_of(&PropValue::String(String::new())), 31);
}

#[test]
fn guid_maps_to_vt_clsid_type() {
    // MF_ATTRIBUTE_GUID = 72
    assert_eq!(attr_type_of(&PropValue::Guid(Default::default())), 72);
}

#[test]
fn prop_value_is_cloneable_and_comparable() {
    let a = PropValue::UInt32(42);
    let b = a.clone();
    assert_eq!(a, b);

    let s1 = PropValue::String("SideLens".into());
    let s2 = PropValue::String("SideLens".into());
    assert_eq!(s1, s2);
    assert_ne!(s1, PropValue::String("other".into()));
}
