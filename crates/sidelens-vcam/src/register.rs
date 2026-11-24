//! 虚拟摄像头注册与生命周期
//!
//! # 关键事实（必须先理解）
//!
//! `MFCreateVirtualCamera` 创建的不是"一个摄像头设备"，而是**一个 COM 对象**。
//! 它做三件事：
//!
//! 1. 在系统里注册一条"这台机器上有个虚拟摄像头"的记录（`Start`）
//! 2. 提供 `GetMediaSource` 让消费端拿到媒体源（由本进程实现）
//! 3. `Stop`/`Remove` 时撤销记录
//!
//! 也就是说：**本进程必须一直活着**，摄像头才存在。
//! 这不是 bug，是设计 —— 虚拟摄像头的帧数据在进程内存里。
//!
//! # 生命周期
//!
//! ```text
//! MFCreateVirtualCamera
//!        ↓
//!    Start(callback)  ← 设备出现在系统里
//!        ↓
//!   [消费端连接，调用 GetMediaSource]
//!        ↓
//!    Stop()           ← 设备从系统里消失
//!        ↓
//!    Remove()         ← 清理注册表痕迹
//! ```
//!
//! # 当前进度
//!
//! 实现了 `Start` / `Stop` / `Remove` 的调用链，
//! **但 `GetMediaSource` 返回的是空实现** —— 消费端能发现设备，
//! 但连上去会失败。这是有意的中间状态：先验证"设备能被系统识别"，
//! 再实现媒体源（那是几百行 COM 代码）。

#![allow(unsafe_code)]

use std::fmt;

use sidelens_proto::now_us;
use windows::core::{GUID, HSTRING, PCWSTR};
use windows::Win32::Media::MediaFoundation::{
    IMFVirtualCamera, MFCreateVirtualCamera, MFVirtualCameraAccess_AllUsers,
    MFVirtualCameraLifetime_System, MFVirtualCameraType_SoftwareCameraSource,
};

/// 注册错误
#[derive(Debug)]
pub enum RegisterError {
    /// 系统不支持（Build < 22000 或组件缺失）
    Unsupported(String),
    /// 已存在同名虚拟摄像头
    AlreadyExists(String),
    /// source_id 格式不对（不是 CLSID 字符串）
    BadSourceId(String),
    /// Win32 调用失败
    Win32(String),
}

impl fmt::Display for RegisterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RegisterError::Unsupported(s) => write!(f, "系统不支持：{s}"),
            RegisterError::AlreadyExists(s) => write!(f, "已存在：{s}"),
            RegisterError::BadSourceId(s) => write!(f, "source_id 非法：{s}"),
            RegisterError::Win32(s) => write!(f, "Win32 调用失败：{s}"),
        }
    }
}

impl std::error::Error for RegisterError {}

/// SideLens 媒体源的 CLSID
///
/// 不是随机生成的 —— 它必须**稳定**。MFCreateVirtualCamera 的 sourceId 参数
/// 期望 CLSID 的字符串形式（带花括号），并用它去 CoCreateInstance 找媒体源实现。
/// 每次启动换 GUID 会让系统设备列表堆满重复项。
pub const SIDELENS_SOURCE_CLSID: &str = "{7A3E1C42-5D88-4F6B-9E20-1B4D8C3A9F57}";

/// 设备描述
#[derive(Debug, Clone)]
pub struct CameraSpec {
    /// 显示给用户的名字（出现在 Zoom/Teams 的设备列表里）
    pub friendly_name: String,
    /// 媒体源的 CLSID 字符串
    ///
    /// **必须是 {GUID} 格式**，否则 MFCreateVirtualCamera 直接报
    /// CO_E_CLASSSTRING（0x800401F3）—— 本项目实测踩过：
    /// 一开始传的是 "SideLens.Camera.Virtual.1"，就是这个错误。
    pub source_id: String,
}

impl Default for CameraSpec {
    fn default() -> Self {
        Self {
            friendly_name: "SideLens Camera".into(),
            source_id: SIDELENS_SOURCE_CLSID.into(),
        }
    }
}

impl CameraSpec {
    /// 把 `source_id` 解析成 GUID
    ///
    /// 返回 None 表示格式非法（应当先用 [`CameraSpec::validate`] 拦住）。
    pub fn source_guid(&self) -> Option<GUID> {
        parse_clsid(&self.source_id)
    }

    /// 校验 source_id 是否是合法的 CLSID 字符串
    pub fn validate(&self) -> Result<(), RegisterError> {
        let s = self.source_id.trim();
        if s.len() != 38 || !s.starts_with('{') || !s.ends_with('}') {
            return Err(RegisterError::BadSourceId(format!(
                "source_id 必须是 {{XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX}} 格式，收到 {s}"
            )));
        }
        let inner = &s[1..s.len() - 1];
        if !inner.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
            return Err(RegisterError::BadSourceId(format!(
                "source_id 含非法字符：{s}"
            )));
        }
        Ok(())
    }
}

/// 已注册的虚拟摄像头
///
/// Drop 时会 `Stop`；是否 `Remove` 由 [`VirtualCamera::disarm`] 控制。
/// 默认**不 Remove** —— 因为下次启动要复用同一条注册记录，
/// 反复 Remove/Start 会让系统设备列表出现"幽灵设备"。
pub struct VirtualCamera {
    inner: IMFVirtualCamera,
    spec: CameraSpec,
    started: bool,
    /// 是否在 Drop 时真正移除
    remove_on_drop: bool,
    registered_at_us: u64,
}

impl fmt::Debug for VirtualCamera {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VirtualCamera")
            .field("friendly_name", &self.spec.friendly_name)
            .field("source_id", &self.spec.source_id)
            .field("started", &self.started)
            .finish_non_exhaustive()
    }
}

impl VirtualCamera {
    /// 创建一个虚拟摄像头并 Start 它
    ///
    /// `callback` 传 `None`：当前用同步方式，Start 后立刻检查。
    /// 真实实现需要传 `IMFAsyncCallback` 来接收"消费端已连接"的异步通知。
    pub fn start(spec: CameraSpec) -> Result<Self, RegisterError> {
        Self::start_with_options(spec, false)
    }

    /// 用**自定义 KS 类别**创建虚拟摄像头
    ///
    /// # 为什么需要这个入口
    ///
    /// `MFCreateVirtualCamera` 的 `categories` 参数决定设备被登记到哪些
    /// KS 类别下。官方示例传 `KSCATEGORY_VIDEO_CAMERA`，我们照抄了 ——
    /// 但 `mfsensorgroup.dll` 里还有一个 **专门给虚拟摄像头用的类别**
    /// `KSCATEGORY_FSM_VIRTUAL_CAMERA`（`{588C8D20-...}`），
    /// 它在 SDK 头文件里查不到，只能从二进制里反推。
    ///
    /// 本机 `Control\\DeviceClasses\\{588c8d20-...}` 键**存在但为空** ——
    /// 说明系统准备好了这个类别，却没有任何设备登记进去。
    ///
    /// 如果 FrameServer 只在前者里找虚拟摄像头，那我们必须登记到那里。
    pub fn start_with_categories(
        spec: CameraSpec,
        with_registry_entries: bool,
        categories: &[GUID],
    ) -> Result<Self, RegisterError> {
        Self::start_full(spec, with_registry_entries, categories)
    }

    /// 带选项的 `start`
    ///
    /// `with_registry_entries`：在 4 个 `VCamCreate_*` 属性之后，
    /// 再补设一批注册表项（`AddRegistryEntry`）。用于 A/B 定位。
    ///
    /// # 为什么用参数而不是直接改 `start`
    ///
    /// 加了注册表项之后 `Start` 的错误码会变，但**必须能关掉它再跑一遍**
    /// 才能确认「变化是它引起的」。直接改 `start` 就失去了对照。
    pub fn start_with_options(
        spec: CameraSpec,
        with_registry_entries: bool,
    ) -> Result<Self, RegisterError> {
        // 必须显式给出设备类别。
        //
        // 传 None 时 API 声称会用默认类别，但实测下来
        // `IMFVirtualCamera::Start` 仍报 E_INVALIDARG —— 因为「默认」
        // 只在打包应用（有 AppX 身份）里成立，普通 exe 拿不到默认值。
        // 显式传 KSCATEGORY_VIDEO_CAMERA 让设备真正归到「摄像头」类别下。
        let categories = [video_camera_category()];
        Self::start_full(spec, with_registry_entries, &categories)
    }

    fn start_full(
        spec: CameraSpec,
        with_registry_entries: bool,
        categories: &[GUID],
    ) -> Result<Self, RegisterError> {
        // 先本地校验，避免把明显的错误送给 Win32 换来一个含糊的 HRESULT
        spec.validate()?;

        let friendly = HSTRING::from(spec.friendly_name.as_str());
        let source = HSTRING::from(spec.source_id.as_str());

        // SAFETY: 两个 HSTRING 在调用期间存活；categories 由调用方给出。
        // 用 System 生命周期 + AllUsers 访问范围。
        //
        // 实测：Session + CurrentUser 时，FrameServer 走完激活/属性/描述符
        // 检查后直接 Shutdown，`Start` 报 E_INVALIDARG。原因是
        // 「Session 生命周期」的设备只属于创建它的登录会话，
        // 而 FrameServer 在 session 0 —— 它看不到一个「只属于 session 1」
        // 的摄像头设备，于是把参数判为非法。
        //
        // System 生命周期是机器级的，session 0 可见。代价是需要管理员权限
        // （我们本来就需要，因为 AddProperty 要写设备树）。
        let lifetime = MFVirtualCameraLifetime_System;
        let access = MFVirtualCameraAccess_AllUsers;
        let inner = unsafe {
            MFCreateVirtualCamera(
                MFVirtualCameraType_SoftwareCameraSource,
                lifetime,
                access,
                PCWSTR(friendly.as_ptr()),
                PCWSTR(source.as_ptr()),
                Some(categories),
            )
        }
        .map_err(|e| {
            let code = e.code().0;
            // MF_E_ATTRIBUTENOTFOUND / HRESULT_FROM_WIN32(ERROR_ALREADY_EXISTS)
            if code == 0x8004_01F3u32 as i32 {
                // CO_E_CLASSSTRING：source_id 不是合法 CLSID
                RegisterError::BadSourceId(format!("Win32 认为不是合法 CLSID：{}", spec.source_id))
            } else if code == 0x8007_00B7u32 as i32 {
                RegisterError::AlreadyExists(spec.source_id.clone())
            } else {
                RegisterError::Win32(format!("MFCreateVirtualCamera: {e} (0x{code:08X})"))
            }
        })?;

        // 关键：callback 不能传 None。实测传 None 直接 E_INVALIDARG (0x80070057)，
        // 而且这个失败发生在 IMFMediaSource::Start 之前，从媒体源侧的日志里
        // 完全看不到线索 —— 这个坑卡了很久。
        // 关键：Start 之前必须写 4 个设备属性。
        // 缺任何一个，FrameServer 创建设备接口时拿不到信息，
        // 报 ERROR_PATH_NOT_FOUND (0x80070003) —— 与真实原因无关的错误码。
        // 对照官方 VCamUtils::RegisterVirtualCamera。
        add_device_properties(
            &inner,
            &spec,
            MFVirtualCameraAccess_AllUsers.0,
            MFVirtualCameraLifetime_System.0,
        )?;

        // 设官方 SDK 定义的设备接口属性（`IsVirtualCamera` 等）。
        //
        // # 为什么这一步是新增的（见 docs/vcam-sdk-gaps.md）
        //
        // 前十四轮只设了 `VCamCreate_*`（`6ac1fbf7`），而那组 GUID
        // 在 SDK 头文件、`devpkey.h`、本机 10 个 MF DLL 里**全部查不到** ——
        // 它是官方示例自造的约定值。
        //
        // 而 `mfvirtualcamera.h` 明确定义了 `IsVirtualCamera` 等三个属性，
        // 系统自己会读。实测我们的设备上该属性是 Empty（不存在）。
        //
        // 单个属性失败不中断流程 —— 不同 build 的支持程度不同。
        for (name, result) in add_interface_properties(&inner) {
            match result {
                Ok(()) => eprintln!("[SideLens] 设备接口属性 {name} 已设置"),
                Err(e) => eprintln!("[SideLens] 设备接口属性 {name} 设置失败：{e}"),
            }
        }

        // 可选的注册表项（`AddRegistryEntry`）
        if with_registry_entries {
            for (name, result) in add_registry_entries(&inner, &spec) {
                match result {
                    Ok(()) => eprintln!("[SideLens] 注册表项 {name} 已写"),
                    Err(e) => eprintln!("[SideLens] 注册表项 {name} 写入失败：{e}"),
                }
            }
        }

        let callback = crate::callback::VcamCallback::create();

        // 诊断：Start 之前先看 GetMediaSource 是否可用。
        // 如果这一步就失败，说明 CLSID → Activate → MediaSource 链路有问题；
        // 如果成功，说明 Start 的失败与媒体源无关，是 IMFVirtualCamera 自身的参数问题。
        // SAFETY: inner 有效
        match unsafe { inner.GetMediaSource() } {
            Ok(src) => {
                // SAFETY: src 是有效接口
                let hr = unsafe { src.GetCharacteristics() };
                eprintln!("[SideLens] GetMediaSource 成功，GetCharacteristics={hr:?}");
            }
            Err(e) => eprintln!("[SideLens] GetMediaSource 失败：{e}"),
        }

        // SAFETY: inner 与 callback 都是有效 COM 对象，调用期间存活。
        unsafe { inner.Start(&callback) }
            .map_err(|e| RegisterError::Win32(format!("Start: {e}")))?;

        Ok(Self {
            inner,
            spec,
            started: true,
            remove_on_drop: false,
            registered_at_us: now_us(),
        })
    }

    pub fn spec(&self) -> &CameraSpec {
        &self.spec
    }

    pub fn is_started(&self) -> bool {
        self.started
    }

    pub fn registered_at_us(&self) -> u64 {
        self.registered_at_us
    }

    /// 底层 COM 对象（用于后续接 GetMediaSource）
    pub fn inner(&self) -> &IMFVirtualCamera {
        &self.inner
    }

    /// 查询设备类别 GUID（诊断用）
    ///
    /// 返回 `IMFVirtualCamera` 上登记的类别列表。
    /// 用于确认"这个虚拟摄像头会被哪些应用看到"。
    pub fn category_count(&self) -> Option<u32> {
        // SAFETY: self.inner 有效；GetUINT32 是 IMFAttributes 的方法。
        // 属性键 MF_VIRTUALCAMERA_CATEGORIES 需要自己构造，这里先返回 None。
        None
    }

    /// 让 Drop 时执行 Remove
    ///
    /// 默认不 Remove：反复 Remove/Start 会让系统设备列表出现幽灵设备。
    /// 只有在"卸载"或"改了 source_id"时才需要显式清理。
    pub fn arm_removal(&mut self) {
        self.remove_on_drop = true;
    }

    /// 显式停止并移除
    pub fn shutdown(mut self) -> Result<(), RegisterError> {
        self.stop_inner()?;
        self.remove_inner()
    }

    fn stop_inner(&mut self) -> Result<(), RegisterError> {
        if !self.started {
            return Ok(());
        }
        // SAFETY: self.inner 有效且处于 started 状态。
        unsafe { self.inner.Stop() }.map_err(|e| RegisterError::Win32(format!("Stop: {e}")))?;
        self.started = false;
        Ok(())
    }

    fn remove_inner(&mut self) -> Result<(), RegisterError> {
        // SAFETY: self.inner 有效。
        unsafe { self.inner.Remove() }.map_err(|e| RegisterError::Win32(format!("Remove: {e}")))?;
        Ok(())
    }
}

impl Drop for VirtualCamera {
    fn drop(&mut self) {
        // 先 Stop（幂等）
        let _ = self.stop_inner();
        // 只有明确要求时才 Remove
        if self.remove_on_drop {
            let _ = self.remove_inner();
        }
    }
}

/// 默认的设备类别（视频摄像头）
///
/// 对应 `KSCATEGORY_VIDEO_CAMERA`：`{E5323777-F976-4f5b-9B55-B94699C46E44}`
pub fn video_camera_category() -> GUID {
    GUID::from_u128(0xE5323777_F976_4F5B_9B55_B94699C46E44)
}

/// **FrameServer 专用的虚拟摄像头类别**
///
/// 对应 `KSCATEGORY_FSM_VIRTUAL_CAMERA`：`{588C8D20-C0E3-4FD3-B511-8F2F692156F8}`
///
/// # 这个 GUID 怎么来的
///
/// 它在 **SDK 的 `ks.h` / `ksmedia.h` 里查不到** —— 不是公开常量。
/// 只能从二进制里反推。方法是：`mfsensorgroup.dll` 的 `.rdata` 里
/// 有一张 `[GUID 16B][名字指针 8B]` 的 24 字节步长表：
///
/// ```text
/// 0x90cd8  {24E552D7-...}  ->  KSCATEGORY_SENSOR_CAMERA
/// 0x90cf0  {5C7164F5-...}  ->  KSCATEGORY_FRAMEPROVIDER
/// 0x90d08  {588C8D20-...}  ->  KSCATEGORY_FSM_VIRTUAL_CAMERA   ← 这个
/// 0x90d20  {6994AD05-...}  ->  KSCATEGORY_VIDEO
/// 0x90d38  {65E8773D-...}  ->  KSCATEGORY_CAPTURE
/// ```
///
/// 名字指针指向 `.rdata` 0x85e40 的 ASCII 串。旁边 0x90d50 起的条目
/// 名字是 `image` / `perception` / `stream` / `video` / `RGB32`，
/// 但 GUID 全零 —— 那是「媒体类型描述」，不是类别，说明这张表是
/// 混合表，必须靠名字指针确认，不能靠位置猜。
///
/// # 交叉验证
///
/// 全盘扫 `C:\Windows\System32` 4532 个 `.dll`/`.exe`，
/// 这个 16 字节 GUID **只出现在 11 个 FrameServer 相关模块里**：
/// `FrameServer.dll` / `FrameServerClient.dll` / `FrameServerCore.dll` /
/// `FrameServerMonitor.dll` / `FrameServerMonitorClient.dll` /
/// `FsNVSDeviceSource.dll` / `MFCaptureEngine.dll` / `mfcore.dll` /
/// `MFDshowReverseBridge.dll` / `mfsensorgroup.dll` / `Windows.Media.dll`。
/// 一个随机猜的 GUID 不会这样 —— 它是真的在用。
///
/// # 本机状态
///
/// `HKLM\SYSTEM\CurrentControlSet\Control\DeviceClasses\{588c8d20-...}`
/// 键**存在但 SubKeyCount=0**：系统准备好了这个类别，没有任何设备登记进去。
pub fn fsm_virtual_camera_category() -> GUID {
    GUID::from_u128(0x588C8D20_C0E3_4FD3_B511_8F2F692156F8)
}
/// 写入虚拟摄像头创建所需的 4 个设备属性
fn add_device_properties(
    cam: &IMFVirtualCamera,
    spec: &CameraSpec,
    access: i32,
    lifetime: i32,
) -> Result<(), RegisterError> {
    use crate::devprop::*;

    // 1. SourceId
    let src = string_prop_bytes(&spec.source_id);
    // SAFETY: cam 有效；字节切片在调用期间存活
    unsafe {
        cam.AddProperty(
            &DEVPROPKEY_VCAM_CREATE_SOURCE_ID,
            windows::Win32::Devices::Properties::DEVPROPTYPE(DEVPROP_TYPE_STRING),
            &src,
        )
        .map_err(|e| RegisterError::Win32(format!("AddProperty(SourceId): {e}")))?;
    }

    // 2. FriendlyName
    let name = string_prop_bytes(&spec.friendly_name);
    // SAFETY: 同上
    unsafe {
        cam.AddProperty(
            &DEVPROPKEY_VCAM_CREATE_FRIENDLY_NAME,
            windows::Win32::Devices::Properties::DEVPROPTYPE(DEVPROP_TYPE_STRING),
            &name,
        )
        .map_err(|e| RegisterError::Win32(format!("AddProperty(FriendlyName): {e}")))?;
    }

    // 3. Lifetime
    let lt = i32_prop_bytes(lifetime);
    // SAFETY: 同上
    unsafe {
        cam.AddProperty(
            &DEVPROPKEY_VCAM_CREATE_LIFETIME,
            windows::Win32::Devices::Properties::DEVPROPTYPE(DEVPROP_TYPE_INT32),
            &lt,
        )
        .map_err(|e| RegisterError::Win32(format!("AddProperty(Lifetime): {e}")))?;
    }

    // 4. Access
    let ac = i32_prop_bytes(access);
    // SAFETY: 同上
    unsafe {
        cam.AddProperty(
            &DEVPROPKEY_VCAM_CREATE_ACCESS,
            windows::Win32::Devices::Properties::DEVPROPTYPE(DEVPROP_TYPE_INT32),
            &ac,
        )
        .map_err(|e| RegisterError::Win32(format!("AddProperty(Access): {e}")))?;
    }

    Ok(())
}

/// 设官方 SDK 定义的设备接口属性
///
/// # 为什么要单独一个函数
///
/// `add_device_properties` 设的是 `VCamCreate_*`（`6ac1fbf7`）——
/// 那组 GUID 在 SDK 头文件、`devpkey.h`、本机 10 个 MF DLL 里**全部查不到**，
/// 是官方示例自造的约定值。
///
/// 本函数设的是 **`mfvirtualcamera.h` 第 80-85 行明确定义的系统属性**：
///
/// | pid | 属性 | 类型 | 值 |
/// | --- | --- | --- | --- |
/// | 3 | `IsVirtualCamera` | BOOLEAN | true |
/// | 4 | `IsWindowsCameraEffectAvailable` | BOOLEAN | false |
///
/// # 实测依据
///
/// 我们的设备上 `IsVirtualCamera` 是 **Empty**（属性不存在）：
///
/// ```text
/// Get-PnpDeviceProperty -KeyName '{6EDC630D-...} 3'
///   → 值 = （空）  类型 = Empty
/// ```
///
/// 一个虚拟摄像头却没有这个标记，很可能是 FrameServer 拒绝它的直接原因。
///
/// # 返回值
///
/// 返回每个属性的设置结果。**单个失败不中断** —— 因为不同 Windows build
/// 对这些属性的支持程度不同，全失败也不该让整个注册流程崩掉。
fn add_interface_properties(cam: &IMFVirtualCamera) -> Vec<(&'static str, Result<(), String>)> {
    use crate::devprop::*;

    let mut out = Vec::new();

    // IsVirtualCamera = true
    //
    // BOOLEAN 的线上表示是 1 字节：0x01 = true，0x00 = false。
    // 不是 4 字节 BOOL —— 用错长度会让 SetupDi 把属性判为格式非法。
    let yes = [1u8];
    // SAFETY: cam 有效；1 字节切片在调用期间存活
    let r = unsafe {
        cam.AddProperty(
            &DEVPKEY_DEVICE_INTERFACE_IS_VIRTUAL_CAMERA,
            windows::Win32::Devices::Properties::DEVPROPTYPE(DEVPROP_TYPE_BOOLEAN),
            &yes,
        )
    };
    out.push(("IsVirtualCamera", r.map_err(|e| e.to_string())));

    // IsWindowsCameraEffectAvailable = false
    //
    // 我们没有实现相机效果（背景虚化之类），设 false 是诚实回答。
    // 设 true 会让系统尝试加载效果 MFT，然后失败。
    let no = [0u8];
    // SAFETY: 同上
    let r = unsafe {
        cam.AddProperty(
            &DEVPKEY_DEVICE_INTERFACE_IS_WIN_CAMERA_EFFECT_AVAILABLE,
            windows::Win32::Devices::Properties::DEVPROPTYPE(DEVPROP_TYPE_BOOLEAN),
            &no,
        )
    };
    out.push((
        "IsWindowsCameraEffectAvailable",
        r.map_err(|e| e.to_string()),
    ));

    out
}

/// 用 `AddRegistryEntry` 往设备接口节点写注册表项
///
/// # 这是官方 API，但官方示例从不用它
///
/// `IMFVirtualCamera::AddRegistryEntry` 在 SDK 的 `mfvirtualcamera.h` 里
/// 明确定义（第 262-272 行），是「往设备接口节点写注册表值」的**唯一官方入口**。
/// 它由系统代写，不需要自己处理 ACL。
///
/// 官方示例走的是「MSI 安装程序写注册表」路线，所以示例代码里看不到它。
///
/// # 写什么
///
/// 实测各写法都被接受（返回 S_OK）：
///
/// | 项 | 写法 | 结果 |
/// | --- | --- | --- |
/// | `IsVirtualCamera` | REG_DWORD，SubkeyPath = null | ✅ |
/// | `IsVirtualCamera` | REG_BINARY + DEVPROPTYPE 前缀，Properties 下 | ✅ |
/// | `VCamClsid` | REG_SZ，null subkey | ✅ |
/// | `CustomCaptureSourceClsid` | REG_SZ，null subkey | ✅（但已知无效） |
///
/// 「写成功」不代表「系统能读懂」—— 所以这些都需要 A/B 验证。
fn add_registry_entries(
    cam: &IMFVirtualCamera,
    spec: &CameraSpec,
) -> Vec<(&'static str, Result<(), String>)> {
    use windows::core::HSTRING;
    use windows::core::PCWSTR;

    /// REG_SZ
    const REG_SZ: u32 = 1;
    /// REG_DWORD
    const REG_DWORD: u32 = 4;

    let mut out = Vec::new();

    // 1) IsVirtualCamera = 1（REG_DWORD）
    let entry = HSTRING::from("{6EDC630D-C2E3-43B7-B2D1-20525A1AF120}, 3");
    // SAFETY: cam 有效；entry 与 4 字节数据在调用期间存活
    let r = unsafe {
        cam.AddRegistryEntry(
            PCWSTR(entry.as_ptr()),
            PCWSTR::null(),
            REG_DWORD,
            &1u32.to_le_bytes(),
        )
    };
    out.push(("IsVirtualCamera", r.map_err(|e| e.to_string())));

    // 2) VCamClsid = 我们的 CLSID（REG_SZ）
    //
    // `mfsensorgroup.dll` 0x87a4e / 0x87c70 / 0x87ea5 三处出现这个名字，
    // 夹在设备属性名表里 —— 它是 mfsensorgroup 真正读的属性名。
    let name = HSTRING::from("VCamClsid");
    let value: Vec<u8> = spec
        .source_id
        .encode_utf16()
        .chain(std::iter::once(0))
        .flat_map(|u| u.to_le_bytes())
        .collect();
    // SAFETY: 同上
    let r = unsafe { cam.AddRegistryEntry(PCWSTR(name.as_ptr()), PCWSTR::null(), REG_SZ, &value) };
    out.push(("VCamClsid", r.map_err(|e| e.to_string())));

    out
}

/// 把 `{XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX}` 解析成 GUID
pub fn parse_clsid(s: &str) -> Option<GUID> {
    let t = s.trim().trim_start_matches('{').trim_end_matches('}');
    let hex: String = t.chars().filter(|c| *c != '-').collect();
    if hex.len() != 32 {
        return None;
    }
    u128::from_str_radix(&hex, 16).ok().map(GUID::from_u128)
}
