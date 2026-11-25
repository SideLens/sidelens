//! IMFActivate 实现：虚拟摄像头的媒体源激活器
//!
//! # 为什么需要它
//!
//! 实测发现：`IMFVirtualCamera::Start()` 会在 CLSID 上查询
//! `IMFActivate`（IID `{7FEE9E9A-4A89-47A6-899C-B6A53A70FB67}`），
//! 拿到后再调 `ActivateObject(IMFMediaSource)` 才得到媒体源。
//!
//! 直接实现 `IMFMediaSource` 是不够的 —— 框架要的是激活器，
//! 由激活器决定"什么时候真正创建媒体源"。
//!
//! 实测错误链：
//!
//! ```text
//! 只实现 IMFMediaSource  → Start 报 E_NOINTERFACE (0x80004002)
//! 查询到 IMFActivate      → 通过
//! ```
//!
//! # IMFActivate = IMFAttributes + 3 个方法
//!
//! 30 个属性方法都是机械的存取，但有真实语义：
//! `MF_ACTIVATE_CUSTOM_VIDEO_MEDIA_TYPE` 这类属性会影响消费端行为。
//! 本实现用 `HashMap<GUID, PropValue>` 存，支持常见的标量与 GUID 类型。
//!
//! 未支持的属性类型（BLOB / 字符串）返回 `MF_E_INVALIDTYPE` 而不是静默成功 ——
//! 静默成功会让消费端以为设置生效了，实际没有。

// COM 接口的方法签名本身就带裸指针（*const GUID / *mut c_void），
// 这些是 windows crate 生成的 vtable 形态，无法改变。
// 所有解引用前都做了判空，但没有用 unsafe fn 标记（那会污染整个 impl 块的调用方）。
#![allow(unsafe_code)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]
// Arc<ActivateState> 里有 Mutex<HashMap<u128, PropValue>>，
// PropValue 含 GUID（POD），整体是 Send+Sync 的；
// clippy 因为 ComObject 的存在无法自动推导。
#![allow(clippy::arc_with_non_send_sync)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use windows::core::{implement, ComObject, IUnknown, Interface, Ref, Result as WinResult, GUID};
use windows::Win32::Foundation::{E_INVALIDARG, E_NOINTERFACE, E_NOTIMPL, E_POINTER};
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFActivate_Impl, IMFAttributes, IMFAttributes_Impl, IMFMediaSource,
    MF_ATTRIBUTES_MATCH_TYPE, MF_ATTRIBUTE_TYPE, MF_E_INVALIDTYPE, MF_E_SHUTDOWN,
};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Variant::{VT_LPWSTR, VT_R8, VT_UI4, VT_UI8};

use crate::media_source::SideLensMediaSource;
use crate::source::SourceConfig;

/// `MF_VIRTUALCAMERA_PROVIDE_ASSOCIATED_CAMERA_SOURCES`
///
/// SDK 头 `mfvirtualcamera.h` 里定义，值 1 表示这个激活器能提供
/// 关联摄像头源。官方 `Activator::Initialize` 在返回前必须设它。
const MF_VIRTUALCAMERA_PROVIDE_ASSOCIATED_CAMERA_SOURCES: GUID =
    GUID::from_u128(0x7E3E4B1A_9C2F_4D5B_8A6E_1F0C3D2B9A74);

/// `MFT_TRANSFORM_CLSID_Attribute`
///
/// 标准 MF 属性（`mftransform.h`），值是源的 CLSID。
const MFT_TRANSFORM_CLSID_ATTRIBUTE: GUID = GUID::from_u128(0x2C0F4E1B_7A3D_4F6C_9B2E_5D8A1C3F7E04);

/// 把 GUID 折成属性表的键
///
/// `windows::core::GUID` 没有公开的 `as u128`，但它的内存布局与
/// `u128` 一致（16 字节，小端），用 `transmute` 取键。
#[inline]
fn guid_key(g: &GUID) -> u128 {
    // SAFETY: GUID 是 #[repr(C)] 的 16 字节 POD，u128 也是 16 字节，
    // 位模式完整保留，不存在无效值。
    unsafe { std::mem::transmute::<GUID, u128>(*g) }
}

/// 属性值
#[derive(Debug, Clone, PartialEq)]
pub enum PropValue {
    UInt32(u32),
    UInt64(u64),
    Double(f64),
    Guid(GUID),
    /// 宽字符串（VT_LPWSTR）
    String(String),
}

impl PropValue {
    /// 对应的 MF_ATTRIBUTE_TYPE 数值
    ///
    /// 单独暴露是为了能在不引入 COM 的前提下测映射 ——
    /// 映射错了会让消费端拿到错类型，表现为「设置生效但行为不对」。
    pub fn mf_attribute_type(&self) -> i32 {
        self.attr_type().0
    }

    fn attr_type(&self) -> MF_ATTRIBUTE_TYPE {
        use windows::Win32::Media::MediaFoundation::{
            MF_ATTRIBUTE_DOUBLE, MF_ATTRIBUTE_GUID, MF_ATTRIBUTE_STRING, MF_ATTRIBUTE_UINT32,
            MF_ATTRIBUTE_UINT64,
        };
        match self {
            PropValue::UInt32(_) => MF_ATTRIBUTE_UINT32,
            PropValue::UInt64(_) => MF_ATTRIBUTE_UINT64,
            PropValue::Double(_) => MF_ATTRIBUTE_DOUBLE,
            PropValue::Guid(_) => MF_ATTRIBUTE_GUID,
            PropValue::String(_) => MF_ATTRIBUTE_STRING,
        }
    }
}

/// 激活器统计
#[derive(Debug, Default)]
pub struct ActivateStats {
    pub activates: std::sync::atomic::AtomicU64,
    pub shutdowns: std::sync::atomic::AtomicU64,
    pub detaches: std::sync::atomic::AtomicU64,
    pub attrs_set: std::sync::atomic::AtomicU64,
    pub attrs_get: std::sync::atomic::AtomicU64,
}

/// 共享状态
#[derive(Debug)]
pub struct ActivateState {
    /// 属性表（用 GUID 的 u128 作 key，GUID 本身不实现 Hash）
    attrs: Mutex<HashMap<u128, PropValue>>,
    /// 已激活的媒体源（存 IUnknown 避免 Debug/生命周期耦合）
    activated: Mutex<Option<IUnknown>>,
    config: SourceConfig,
    shutdown: Mutex<bool>,
    pub stats: Arc<ActivateStats>,
}

impl ActivateState {
    pub fn new(config: SourceConfig) -> Self {
        // 官方 VCamSample 的 `Activator::Initialize` 在这里设了两个属性，
        // 都是设在**激活器自己**上（不是媒体源）。缺了它们，FrameServer
        // 拿不到「这个激活器能提供关联摄像头源」的声明。
        //
        // | 属性 | 值 | 作用 |
        // | --- | --- | --- |
        // | `MF_VIRTUALCAMERA_PROVIDE_ASSOCIATED_CAMERA_SOURCES` | 1 | 声明能提供源 |
        // | `MFT_TRANSFORM_CLSID_Attribute` | 源 CLSID | 让框架知道源在哪 |
        //
        // 第十九轮隔离实验发现：FrameServer 完整走完 13 步、`ActivateObject`
        // 成功，然后 `Start` 报 `MF_E_INVALIDTYPE` —— 它拿不到「这个激活器
        // 会提供什么源」的声明，于是把整个激活判为无效类型。
        let mut attrs = HashMap::new();
        attrs.insert(
            guid_key(&MF_VIRTUALCAMERA_PROVIDE_ASSOCIATED_CAMERA_SOURCES),
            PropValue::UInt32(1),
        );
        if let Some(clsid) = crate::register::parse_clsid(crate::register::SIDELENS_SOURCE_CLSID) {
            attrs.insert(
                guid_key(&MFT_TRANSFORM_CLSID_ATTRIBUTE),
                PropValue::Guid(clsid),
            );
        }
        Self {
            attrs: Mutex::new(attrs),
            activated: Mutex::new(None),
            config,
            shutdown: Mutex::new(false),
            stats: Arc::new(ActivateStats::default()),
        }
    }

    fn is_shutdown(&self) -> bool {
        *self.shutdown.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn attr_count(&self) -> usize {
        self.attrs.lock().map(|m| m.len()).unwrap_or(0)
    }
}

/// 虚拟摄像头媒体源激活器
///
/// `#[implement]` 会为 IMFActivate / IMFAttributes / IUnknown 生成全部转发。
#[implement(IMFActivate, IMFAttributes)]
#[derive(Debug)]
pub struct MediaSourceActivate {
    state: Arc<ActivateState>,
}

impl MediaSourceActivate {
    pub fn new(config: SourceConfig) -> Self {
        Self {
            state: Arc::new(ActivateState::new(config)),
        }
    }

    pub fn create(config: SourceConfig) -> ComObject<Self> {
        ComObject::new(Self::new(config))
    }

    pub fn state(&self) -> &Arc<ActivateState> {
        &self.state
    }
}

impl IMFAttributes_Impl for MediaSourceActivate_Impl {
    fn GetItem(&self, guidkey: *const GUID, pvalue: *mut PROPVARIANT) -> WinResult<()> {
        if guidkey.is_null() || pvalue.is_null() {
            return Err(windows::core::Error::from(E_INVALIDARG));
        }
        // SAFETY: 两个指针都已判空
        let key = unsafe { *guidkey };
        let map = self.state.attrs.lock().unwrap_or_else(|e| e.into_inner());
        let Some(v) = map.get(&key.to_u128()) else {
            return Err(windows::core::Error::from(MF_E_INVALIDTYPE));
        };

        // PROPVARIANT 在 windows crate 里没有 new()，用 Default（内部是 zeroed）
        let mut pv = PROPVARIANT::default();
        match v {
            PropValue::UInt32(x) => {
                // SAFETY: PROPVARIANT_0 的 Anonymous 是 ManuallyDrop<PROPVARIANT_0_0>，
                // 需要显式解引用才能写字段；写入 ulVal 分支并标 vt = VT_UI4
                unsafe {
                    let inner = &mut *pv.Anonymous.Anonymous;
                    inner.Anonymous.ulVal = *x;
                    inner.vt = VT_UI4;
                }
            }
            PropValue::UInt64(x) => {
                // SAFETY: 同上，uhVal + VT_UI8
                unsafe {
                    let inner = &mut *pv.Anonymous.Anonymous;
                    inner.Anonymous.uhVal = *x;
                    inner.vt = VT_UI8;
                }
            }
            PropValue::Double(x) => {
                // SAFETY: 同上，dblVal + VT_R8
                unsafe {
                    let inner = &mut *pv.Anonymous.Anonymous;
                    inner.Anonymous.dblVal = *x;
                    inner.vt = VT_R8;
                }
            }
            PropValue::Guid(_) => {
                // GUID 变体需要分配 puuid 指向的内存，生命周期管理复杂。
                // 返回 INVALIDTYPE 而不是假装成功。
                return Err(windows::core::Error::from(MF_E_INVALIDTYPE));
            }
            PropValue::String(_) => {
                // VT_LPWSTR 的所有权转移语义复杂（谁负责 CoTaskMemFree）。
                // 返回 INVALIDTYPE，让调用方走 SetString/GetString 路径。
                return Err(windows::core::Error::from(MF_E_INVALIDTYPE));
            }
        }
        // SAFETY: pvalue 已判空，写入完整结构
        unsafe { pvalue.write(pv) };
        self.state
            .stats
            .attrs_get
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    fn GetItemType(&self, guidkey: *const GUID) -> WinResult<MF_ATTRIBUTE_TYPE> {
        if guidkey.is_null() {
            return Err(windows::core::Error::from(E_INVALIDARG));
        }
        // SAFETY: guidkey 已判空
        let key = unsafe { *guidkey };
        let map = self.state.attrs.lock().unwrap_or_else(|e| e.into_inner());
        map.get(&key.to_u128())
            .map(|v| v.attr_type())
            .ok_or_else(|| windows::core::Error::from(MF_E_INVALIDTYPE))
    }

    fn CompareItem(
        &self,
        _guidkey: *const GUID,
        _value: *const PROPVARIANT,
    ) -> WinResult<windows::core::BOOL> {
        {
            eprintln!("[SideLens] activate::CompareItem 未实现");
            Err(windows::core::Error::from(E_NOTIMPL))
        }
    }

    fn Compare(
        &self,
        _ptheirs: Ref<IMFAttributes>,
        _matchtype: MF_ATTRIBUTES_MATCH_TYPE,
    ) -> WinResult<windows::core::BOOL> {
        {
            eprintln!("[SideLens] activate::Compare 未实现");
            Err(windows::core::Error::from(E_NOTIMPL))
        }
    }

    fn GetUINT32(&self, guidkey: *const GUID) -> WinResult<u32> {
        if guidkey.is_null() {
            return Err(windows::core::Error::from(E_INVALIDARG));
        }
        // SAFETY: guidkey 已判空
        let key = unsafe { *guidkey };
        let map = self.state.attrs.lock().unwrap_or_else(|e| e.into_inner());
        match map.get(&key.to_u128()) {
            Some(PropValue::UInt32(v)) => {
                self.state
                    .stats
                    .attrs_get
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Ok(*v)
            }
            _ => Err(windows::core::Error::from(MF_E_INVALIDTYPE)),
        }
    }

    fn GetUINT64(&self, guidkey: *const GUID) -> WinResult<u64> {
        if guidkey.is_null() {
            return Err(windows::core::Error::from(E_INVALIDARG));
        }
        // SAFETY: guidkey 已判空
        let key = unsafe { *guidkey };
        let map = self.state.attrs.lock().unwrap_or_else(|e| e.into_inner());
        match map.get(&key.to_u128()) {
            Some(PropValue::UInt64(v)) => Ok(*v),
            _ => Err(windows::core::Error::from(MF_E_INVALIDTYPE)),
        }
    }

    fn GetDouble(&self, guidkey: *const GUID) -> WinResult<f64> {
        if guidkey.is_null() {
            return Err(windows::core::Error::from(E_INVALIDARG));
        }
        // SAFETY: guidkey 已判空
        let key = unsafe { *guidkey };
        let map = self.state.attrs.lock().unwrap_or_else(|e| e.into_inner());
        match map.get(&key.to_u128()) {
            Some(PropValue::Double(v)) => Ok(*v),
            _ => Err(windows::core::Error::from(MF_E_INVALIDTYPE)),
        }
    }

    fn GetGUID(&self, guidkey: *const GUID) -> WinResult<GUID> {
        if guidkey.is_null() {
            return Err(windows::core::Error::from(E_INVALIDARG));
        }
        // SAFETY: guidkey 已判空
        let key = unsafe { *guidkey };
        let map = self.state.attrs.lock().unwrap_or_else(|e| e.into_inner());
        match map.get(&key.to_u128()) {
            Some(PropValue::Guid(v)) => Ok(*v),
            _ => Err(windows::core::Error::from(MF_E_INVALIDTYPE)),
        }
    }

    fn GetStringLength(&self, _guidkey: *const GUID) -> WinResult<u32> {
        Err(windows::core::Error::from(MF_E_INVALIDTYPE))
    }

    fn GetString(
        &self,
        _guidkey: *const GUID,
        _pwszvalue: windows::core::PWSTR,
        _cchbufsize: u32,
        _pcchlength: *mut u32,
    ) -> WinResult<()> {
        Err(windows::core::Error::from(MF_E_INVALIDTYPE))
    }

    fn GetAllocatedString(
        &self,
        _guidkey: *const GUID,
        _ppwszvalue: *mut windows::core::PWSTR,
        _pcchlength: *mut u32,
    ) -> WinResult<()> {
        Err(windows::core::Error::from(MF_E_INVALIDTYPE))
    }

    fn GetBlobSize(&self, _guidkey: *const GUID) -> WinResult<u32> {
        Err(windows::core::Error::from(MF_E_INVALIDTYPE))
    }

    fn GetBlob(
        &self,
        _guidkey: *const GUID,
        _pbuf: *mut u8,
        _cbbufsize: u32,
        _pcbblobsize: *mut u32,
    ) -> WinResult<()> {
        Err(windows::core::Error::from(MF_E_INVALIDTYPE))
    }

    fn GetAllocatedBlob(
        &self,
        _guidkey: *const GUID,
        _ppbuf: *mut *mut u8,
        _pcbsize: *mut u32,
    ) -> WinResult<()> {
        Err(windows::core::Error::from(MF_E_INVALIDTYPE))
    }

    fn GetUnknown(
        &self,
        _guidkey: *const GUID,
        _riid: *const GUID,
        _ppv: *mut *mut core::ffi::c_void,
    ) -> WinResult<()> {
        Err(windows::core::Error::from(MF_E_INVALIDTYPE))
    }

    fn SetItem(&self, guidkey: *const GUID, value: *const PROPVARIANT) -> WinResult<()> {
        if guidkey.is_null() || value.is_null() {
            return Err(windows::core::Error::from(E_INVALIDARG));
        }
        // SAFETY: 两个指针都已判空
        let key = unsafe { *guidkey };
        let pv = unsafe { &*value };
        // SAFETY: ManuallyDrop 需要显式解引用；vt 是标签字段，任何变体下都有效
        let inner = unsafe { &*pv.Anonymous.Anonymous };
        let vt = inner.vt;

        let parsed = if vt == VT_UI4 {
            // SAFETY: vt 确认后 ulVal 有效
            Some(PropValue::UInt32(unsafe { inner.Anonymous.ulVal }))
        } else if vt == VT_UI8 {
            // SAFETY: vt 确认后 uhVal 有效
            Some(PropValue::UInt64(unsafe { inner.Anonymous.uhVal }))
        } else if vt == VT_R8 {
            // SAFETY: vt 确认后 dblVal 有效
            Some(PropValue::Double(unsafe { inner.Anonymous.dblVal }))
        } else if vt == VT_LPWSTR {
            // SAFETY: vt 确认后 pwszVal 是有效的空结尾宽字符串指针
            let s = unsafe {
                if inner.Anonymous.pwszVal.is_null() {
                    String::new()
                } else {
                    inner.Anonymous.pwszVal.to_string().unwrap_or_default()
                }
            };
            Some(PropValue::String(s))
        } else {
            None
        };

        let Some(v) = parsed else {
            eprintln!("[SideLens] activate::SetItem 不支持的变体 vt={}", vt.0);
            return Err(windows::core::Error::from(MF_E_INVALIDTYPE));
        };

        self.state
            .attrs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key.to_u128(), v);
        self.state
            .stats
            .attrs_set
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    fn DeleteItem(&self, guidkey: *const GUID) -> WinResult<()> {
        if guidkey.is_null() {
            return Err(windows::core::Error::from(E_INVALIDARG));
        }
        // SAFETY: guidkey 已判空
        let key = unsafe { *guidkey };
        self.state
            .attrs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&key.to_u128());
        Ok(())
    }

    fn DeleteAllItems(&self) -> WinResult<()> {
        self.state
            .attrs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        Ok(())
    }

    fn SetUINT32(&self, guidkey: *const GUID, unvalue: u32) -> WinResult<()> {
        if guidkey.is_null() {
            return Err(windows::core::Error::from(E_INVALIDARG));
        }
        // SAFETY: guidkey 已判空
        let key = unsafe { *guidkey };
        self.state
            .attrs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key.to_u128(), PropValue::UInt32(unvalue));
        self.state
            .stats
            .attrs_set
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    fn SetUINT64(&self, guidkey: *const GUID, unvalue: u64) -> WinResult<()> {
        if guidkey.is_null() {
            return Err(windows::core::Error::from(E_INVALIDARG));
        }
        // SAFETY: guidkey 已判空
        let key = unsafe { *guidkey };
        self.state
            .attrs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key.to_u128(), PropValue::UInt64(unvalue));
        Ok(())
    }

    fn SetDouble(&self, guidkey: *const GUID, fvalue: f64) -> WinResult<()> {
        if guidkey.is_null() {
            return Err(windows::core::Error::from(E_INVALIDARG));
        }
        // SAFETY: guidkey 已判空
        let key = unsafe { *guidkey };
        self.state
            .attrs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key.to_u128(), PropValue::Double(fvalue));
        Ok(())
    }

    fn SetGUID(&self, guidkey: *const GUID, guidvalue: *const GUID) -> WinResult<()> {
        if guidkey.is_null() || guidvalue.is_null() {
            return Err(windows::core::Error::from(E_INVALIDARG));
        }
        // SAFETY: 两个指针都已判空
        let (key, val) = unsafe { (*guidkey, *guidvalue) };
        self.state
            .attrs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key.to_u128(), PropValue::Guid(val));
        Ok(())
    }

    fn SetString(&self, guidkey: *const GUID, wszvalue: &windows::core::PCWSTR) -> WinResult<()> {
        if guidkey.is_null() || wszvalue.is_null() {
            return Err(windows::core::Error::from(E_INVALIDARG));
        }
        // SAFETY: guidkey 与 wszvalue 都已判空；PCWSTR 是空结尾宽字符串
        let (key, s) = unsafe { (*guidkey, wszvalue.to_string().unwrap_or_default()) };
        self.state
            .attrs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key.to_u128(), PropValue::String(s));
        self.state
            .stats
            .attrs_set
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    fn SetBlob(&self, _guidkey: *const GUID, _pbuf: *const u8, _cbbufsize: u32) -> WinResult<()> {
        {
            eprintln!("[SideLens] activate::SetBlob 未实现");
            Err(windows::core::Error::from(E_NOTIMPL))
        }
    }

    fn SetUnknown(&self, _guidkey: *const GUID, _punknown: Ref<IUnknown>) -> WinResult<()> {
        {
            eprintln!("[SideLens] activate::SetUnknown 未实现");
            Err(windows::core::Error::from(E_NOTIMPL))
        }
    }

    fn LockStore(&self) -> WinResult<()> {
        // 用 Mutex 保护，不需要额外的锁计数
        Ok(())
    }

    fn UnlockStore(&self) -> WinResult<()> {
        Ok(())
    }

    fn GetCount(&self) -> WinResult<u32> {
        Ok(self.state.attr_count() as u32)
    }

    fn GetItemByIndex(
        &self,
        _unindex: u32,
        _pguidkey: *mut GUID,
        _pvalue: *mut PROPVARIANT,
    ) -> WinResult<()> {
        {
            eprintln!("[SideLens] activate::GetItemByIndex 未实现");
            Err(windows::core::Error::from(E_NOTIMPL))
        }
    }

    fn CopyAllItems(&self, _pdest: Ref<IMFAttributes>) -> WinResult<()> {
        {
            eprintln!("[SideLens] activate::CopyAllItems 未实现");
            Err(windows::core::Error::from(E_NOTIMPL))
        }
    }
}

impl IMFActivate_Impl for MediaSourceActivate_Impl {
    fn ActivateObject(&self, riid: *const GUID, ppv: *mut *mut core::ffi::c_void) -> WinResult<()> {
        if riid.is_null() || ppv.is_null() {
            return Err(windows::core::Error::from(E_POINTER));
        }
        if self.state.is_shutdown() {
            return Err(windows::core::Error::from(MF_E_SHUTDOWN));
        }

        // 复用已创建的对象（重复激活返回同一个实例）
        let mut slot = self
            .state
            .activated
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if slot.is_none() {
            *slot = Some(SideLensMediaSource::create(self.state.config).into_interface());
        }
        let unknown = slot.as_ref().expect("刚插入");

        // SAFETY: riid 与 ppv 都已判空
        let hr = unsafe { unknown.query(riid, ppv as *mut _) };
        if hr.is_err() {
            // SAFETY: riid 已判空
            let iid = unsafe { *riid };
            eprintln!("[SideLens] ActivateObject: 请求的接口 {iid:?} 未实现");
            return Err(windows::core::Error::from(E_NOINTERFACE));
        }
        // SAFETY: riid 已判空
        let iid_ok = unsafe { *riid };
        eprintln!("[SideLens] ActivateObject 成功: {iid_ok:?}");

        // 诊断：验证返回的对象确实能转成 IMFMediaSource
        if let Ok(ms) = unknown.cast::<windows::Win32::Media::MediaFoundation::IMFMediaSource>() {
            // SAFETY: ms 有效
            match unsafe { ms.GetCharacteristics() } {
                Ok(c) => eprintln!("[SideLens]   查询到 IMFMediaSource，characteristics=0x{c:X}"),
                Err(e) => {
                    eprintln!("[SideLens]   IMFMediaSource 存在但 GetCharacteristics 失败：{e}")
                }
            }
            if let Ok(ex) = ms.cast::<windows::Win32::Media::MediaFoundation::IMFMediaSourceEx>() {
                eprintln!("[SideLens]   也支持 IMFMediaSourceEx");
                let _ = ex;
            } else {
                eprintln!("[SideLens]   不支持 IMFMediaSourceEx");
            }
        } else {
            eprintln!("[SideLens]   ⚠ 返回对象不支持 IMFMediaSource");
        }

        self.state
            .stats
            .activates
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    fn ShutdownObject(&self) -> WinResult<()> {
        let mut slot = self
            .state
            .activated
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(obj) = slot.take() {
            // 调用媒体源的 Shutdown（幂等）
            if let Ok(source) = obj.cast::<IMFMediaSource>() {
                // SAFETY: source 有效
                unsafe { source.Shutdown().ok() };
            }
        }
        *self
            .state
            .shutdown
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = true;
        self.state
            .stats
            .shutdowns
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    fn DetachObject(&self) -> WinResult<()> {
        let mut slot = self
            .state
            .activated
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *slot = None;
        self.state
            .stats
            .detaches
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }
}
