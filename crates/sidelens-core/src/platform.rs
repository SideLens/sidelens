//! 平台能力清单与移植要点
//!
//! # 这个模块要回答什么问题
//!
//! 项目目前**只有 Windows 能跑**：虚拟摄像头走 `MFCreateVirtualCamera`、
//! 定时器分辨率靠 `timeBeginPeriod`、进程间传输是命名管道 + 共享内存，
//! 这些差异散落在 `sidelens-vcam`、`sidelens-ipc`、`sidelens-codec`、
//! 本 crate 的 [`crate::timer`] 里，没有任何一处能看到全貌。
//!
//! 要往 macOS / Linux 移植，第一步不是写代码，而是先回答
//! **「哪些东西是平台相关的」**。本模块就是这份清单：把散落的 `cfg` 分支
//! 收敛成一张可查询、可测试、可评审的能力矩阵。
//!
//! # 三层结构
//!
//! | 层 | 类型 | 用途 |
//! | --- | --- | --- |
//! | 身份 | [`Platform`] | 当前跑在哪个系统上 |
//! | 能力 | [`Capability`] + [`support_level`] | 这个系统支不支持某件事，支持到什么程度 |
//! | 工作 | [`PlatformNotes`] | 移植到该系统要做什么、会踩什么坑 |
//!
//! # 关于「部分支持」的简化
//!
//! 调用方最常问的是一个布尔问题：「这个平台能不能用 X」。
//! 所以对外的主入口是 [`supports`]，返回 `bool`。
//!
//! 但真实世界有第三态 —— Android 的 UDP 打洞在 `WiFi` 下很顺、
//! 在移动网络下成功率明显下降；HarmonyOS 的 H.265 取决于芯片。
//! 把这类情况压成 `true` 会误导（以为和 Windows 一样稳），
//! 压成 `false` 又会漏掉可用的能力。
//!
//! 所以底层实现是三态的 [`SupportLevel`]：
//!
//! - [`SupportLevel::Full`]：平台有官方 API，行为可预期
//! - [`SupportLevel::Partial`]：能做，但有前提条件、需要降级策略，或需要自行验证
//! - [`SupportLevel::None`]：平台不提供该能力
//!
//! [`supports`] 的定义就是 `support_level(...) != SupportLevel::None`。
//! 需要做降级决策的调用方（比如「打洞失败就转中继」）应该直接读
//! [`support_level`]，别用 [`supports`] 再把三态信息丢掉一次。
//!
//! # 本模块的边界
//!
//! 这里**只有数据与判定，没有任何系统调用**。整个模块是纯函数 + 常量表，
//! 因此可以 `#![forbid(unsafe_code)]`，也能在任何平台上跑通全部测试
//! （测试不依赖宿主机，只依赖矩阵本身）。
//!
//! 真正的实现（谁去调 `timeBeginPeriod`、谁去开 v4l2loopback）留在各 crate 里，
//! 由它们查这张表决定走哪条分支。
//!
//! # 事实来源与可信度
//!
//! 矩阵里每条结论都标了可信度，见 [`evidence_for`]（按平台细化）
//! 与 [`Capability::evidence`]（按能力归类）：
//!
//! - **已查证**：本项目实测过，或与官方文档 / 平台 API 名称一一对应
//! - **待验证**：生态较新或资料不足，需要在真机上确认后才能当依据
//!
//! 后者主要集中在 `HarmonyOS` —— 它的公开文档覆盖度还不如前五个平台，
//! 与其猜，不如显式标出来，让移植时第一件事就是去验证。
//!
//! # 与现有代码的对应关系
//!
//! | 现有位置 | 对应能力 |
//! | --- | --- |
//! | [`crate::timer::HighResTimer`] | [`Capability::HighResTimer`] |
//! | `sidelens-vcam::detect`（`MIN_BUILD = 22000`） | [`Capability::VirtualCameraApi`] |
//! | `sidelens-vcam-dll`（`DllGetClassObject` / CLSID 注册） | [`Capability::OutOfProcComServer`] |
//! | `sidelens-ipc::pipe` | [`Capability::NamedPipe`] |
//! | `sidelens-ipc::shm` | [`Capability::SharedMemory`] |
//! | `sidelens-codec` | [`Capability::HardwareH264`] / [`Capability::HardwareH265`] |
//!
//! 也就是说：这张表里每一行 `None`，都是一处移植时必须新写的模块。

#![forbid(unsafe_code)]

/// 目标平台
///
/// 只列本项目真要移植过去的平台，外加一个 [`Platform::Unknown`] 兜底。
/// 刻意不做「所有 `target_os` 值」的枚举 —— 表里没有的平台无法给出
/// 有意义的能力结论，假装支持比明确不支持更危险。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Platform {
    /// Windows 桌面
    Windows,
    /// macOS 桌面
    MacOs,
    /// Linux 桌面（含各类发行版）
    Linux,
    /// Android
    Android,
    /// iOS / iPadOS
    Ios,
    /// `HarmonyOS`（含 `OpenHarmony`）
    HarmonyOs,
    /// 未知或未支持
    Unknown,
}

impl Platform {
    /// 当前编译目标的平台
    ///
    /// 用 `cfg!` 而不是 `#[cfg]`：`cfg!` 是编译期常量，会被优化成常量 `true`/`false`，
    /// 但**所有分支都必须通过类型检查** —— 好处是六个平台的映射关系
    /// 在任何宿主机上都能编译验证，不会出现「只有在 mac 上才发现写错了」。
    ///
    /// 判定顺序不能随便调：`target_os = "android"` 同时也属于 Linux 家族，
    /// `target_os = "ios"` 也属于类 Unix 家族。必须先判 Android / iOS，
    /// 再判 Linux，否则移动端会被误判成桌面 Linux。
    pub const fn current() -> Self {
        if cfg!(target_os = "windows") {
            Platform::Windows
        } else if cfg!(target_os = "macos") {
            Platform::MacOs
        } else if cfg!(target_os = "android") {
            Platform::Android
        } else if cfg!(target_os = "ios") {
            Platform::Ios
        } else if cfg!(target_os = "linux") {
            Platform::Linux
        } else {
            Platform::Unknown
        }
    }

    /// 平台名（稳定标识，用于日志、配置、测试断言）
    ///
    /// 不复用 `Debug` 输出：`Debug` 是实现细节，改名会悄悄破坏日志解析。
    pub const fn name(&self) -> &'static str {
        match self {
            Platform::Windows => "windows",
            Platform::MacOs => "macos",
            Platform::Linux => "linux",
            Platform::Android => "android",
            Platform::Ios => "ios",
            Platform::HarmonyOs => "harmonyos",
            Platform::Unknown => "unknown",
        }
    }

    /// 全部平台，按「桌面 → 移动 → 未知」排列
    ///
    /// 遍历能力矩阵、生成移植清单都从这里取，避免各处手抄一份列表后不同步。
    pub const ALL: [Platform; 7] = [
        Platform::Windows,
        Platform::MacOs,
        Platform::Linux,
        Platform::Android,
        Platform::Ios,
        Platform::HarmonyOs,
        Platform::Unknown,
    ];

    /// 是否桌面平台（Windows / macOS / Linux）
    ///
    /// 桌面与移动的差异是**结构性的**，不只是「能力多少」：
    /// 桌面有常驻进程和文件系统，移动端有前台/后台生命周期和电量约束。
    /// 很多判定会先分这一刀。
    pub const fn is_desktop(&self) -> bool {
        matches!(self, Platform::Windows | Platform::MacOs | Platform::Linux)
    }

    /// 是否移动平台（Android / iOS / `HarmonyOS`）
    pub const fn is_mobile(&self) -> bool {
        matches!(
            self,
            Platform::Android | Platform::Ios | Platform::HarmonyOs
        )
    }
}

impl std::fmt::Display for Platform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// 项目实际依赖的平台能力
///
/// 每一项都对应代码里真实存在（或移植时必须新增）的一处平台分支，
/// 不是泛泛的「操作系统特性」。加新能力前先问：代码里有对应的 `cfg` 吗？
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Capability {
    /// 能把系统定时器分辨率提到 1ms
    ///
    /// Windows 的 `timeBeginPeriod`。这是本项目踩过的最隐蔽的坑：
    /// 默认 15.625ms 分辨率会让 `interval(1ms)` 实际 15.6ms 才触发，
    /// 详见 [`crate::timer`]。
    HighResTimer,

    /// 有系统级虚拟摄像头 API
    ///
    /// Windows 的 `MFCreateVirtualCamera`（Win11 Build 22000+）。
    VirtualCameraApi,

    /// 能注册进程外 COM 服务器
    ///
    /// Windows 的 CLSID + `InprocServer32` 注册表登记。
    /// 虚拟摄像头的媒体源必须编成 DLL 被 `FrameServer` 加载，
    /// 这条能力是「单进程实现在 Windows 上走不通」的直接原因。
    OutOfProcComServer,

    /// 支持命名管道（Windows）或 Unix domain socket
    NamedPipe,

    /// 支持共享内存映射
    ///
    /// Windows `CreateFileMapping` / POSIX `shm_open`。
    /// 3.1MB 的 1080p NV12 帧只能走共享内存，走管道会被拷贝开销拖死。
    SharedMemory,

    /// 支持 UDP 打洞（需要 NAT 类型检测）
    UdpHolePunching,

    /// 有硬件 H.264 编码器
    HardwareH264,

    /// 有硬件 H.265 编码器
    HardwareH265,

    /// 支持屏幕 / 窗口捕获 API
    ScreenCapture,
}

impl Capability {
    /// 全部能力，顺序与文档里的矩阵一致
    pub const ALL: [Capability; 9] = [
        Capability::HighResTimer,
        Capability::VirtualCameraApi,
        Capability::OutOfProcComServer,
        Capability::NamedPipe,
        Capability::SharedMemory,
        Capability::UdpHolePunching,
        Capability::HardwareH264,
        Capability::HardwareH265,
        Capability::ScreenCapture,
    ];

    /// 能力名（稳定标识）
    pub const fn name(&self) -> &'static str {
        match self {
            Capability::HighResTimer => "high_res_timer",
            Capability::VirtualCameraApi => "virtual_camera_api",
            Capability::OutOfProcComServer => "out_of_proc_com_server",
            Capability::NamedPipe => "named_pipe",
            Capability::SharedMemory => "shared_memory",
            Capability::UdpHolePunching => "udp_hole_punching",
            Capability::HardwareH264 => "hardware_h264",
            Capability::HardwareH265 => "hardware_h265",
            Capability::ScreenCapture => "screen_capture",
        }
    }

    /// 该能力在各平台上的实现载体（用于文档与排错）
    ///
    /// 这是「支撑矩阵的原始事实」：查不到 API 名字的能力，结论就可疑。
    /// 返回值按 [`Platform::ALL`] 顺序对应；[`Platform::Unknown`] 恒为「无」。
    pub const fn providers(&self) -> [&'static str; 7] {
        match self {
            Capability::HighResTimer => [
                "timeBeginPeriod(1)（winmm）",
                "无（系统时钟本就是微秒级，不需要提精度）",
                "无（同 macOS；精度受 CONFIG_HZ 影响但 1ms 够用）",
                "无",
                "无",
                "无",
                "无",
            ],
            Capability::VirtualCameraApi => [
                "MFCreateVirtualCamera（Win11 Build 22000+）",
                "无系统 API（需 DriverKit 扩展 + Apple entitlement）",
                "无系统 API（需 v4l2loopback 内核模块）",
                "无",
                "无（沙箱限制，不允许系统级虚拟摄像头）",
                "无",
                "无",
            ],
            Capability::OutOfProcComServer => [
                "CLSID + InprocServer32 注册表登记，FrameServer 加载 DLL",
                "无（无 COM）",
                "无（无 COM）",
                "无",
                "无",
                "无",
                "无",
            ],
            Capability::NamedPipe => [
                "命名管道（Win32 命名管道对象）",
                "Unix domain socket（AF_UNIX）",
                "Unix domain socket（AF_UNIX）",
                "仅本机抽象 socket（LocalSocket，非 POSIX AF_UNIX 完整语义）",
                "仅本机抽象 socket（同 Android）",
                "仅本机抽象 socket（同 Android）",
                "无",
            ],
            Capability::SharedMemory => [
                "CreateFileMapping / MapViewOfFile",
                "POSIX shm_open + mmap",
                "POSIX shm_open + mmap（也可 memfd_create）",
                "ashmem / ASharedMemory",
                "POSIX shm + mmap（受沙箱约束）",
                "ashmem 类接口（待验证）",
                "无",
            ],
            Capability::UdpHolePunching => [
                "标准 UDP socket + NAT 类型检测",
                "标准 UDP socket + NAT 类型检测",
                "标准 UDP socket + NAT 类型检测",
                "UDP socket 可用，但移动网络 NAT 复杂，成功率低于 WiFi",
                "同 Android（运营商级 NAT 更常见）",
                "同 Android（待验证）",
                "无",
            ],
            Capability::HardwareH264 => [
                "Media Foundation（MFT）/ NVENC / QSV / AMF",
                "VideoToolbox",
                "VA-API / NVENC",
                "MediaCodec",
                "VideoToolbox",
                "AVCodec Kit（待验证）",
                "无",
            ],
            Capability::HardwareH265 => [
                "Media Foundation（HEVC MFT，需系统装 HEVC 视频扩展）/ NVENC",
                "VideoToolbox（HEVC）",
                "VA-API（需驱动与硬件支持）/ NVENC",
                "MediaCodec（需硬件与 Profile 支持）",
                "VideoToolbox（A10 及以后机型）",
                "视芯片（待验证）",
                "无",
            ],
            Capability::ScreenCapture => [
                "Windows.Graphics.Capture（Win10 1803+）/ DXGI Desktop Duplication",
                "ScreenCaptureKit（macOS 12.3+）/ CGDisplayStream（已弃用）",
                "PipeWire（Wayland）/ X11 XShm",
                "MediaProjection（必须配前台 Service + 用户授权）",
                "ReplayKit",
                "待验证",
                "无",
            ],
        }
    }

    /// 该能力的证据等级（矩阵可信度）
    ///
    /// 移植排期时应该先做 [`Evidence::Verified`] 的项，把
    /// [`Evidence::Unverified`] 的项单独列成「调研任务」——
    /// 后者可能一查就发现做不了，早发现比晚发现便宜。
    pub const fn evidence(&self) -> Evidence {
        match self {
            Capability::HighResTimer
            | Capability::VirtualCameraApi
            | Capability::OutOfProcComServer
            | Capability::NamedPipe
            | Capability::SharedMemory
            | Capability::UdpHolePunching
            | Capability::ScreenCapture => Evidence::Verified,
            // 硬件编码的存在性依赖具体机型 / 驱动 / 系统版本，
            // 与「平台」绑定得没那么死，统一降一档：
            // 结论方向可信，但「一定有」必须按机型确认。
            Capability::HardwareH264 | Capability::HardwareH265 => Evidence::DeviceDependent,
        }
    }
}

impl std::fmt::Display for Capability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// 矩阵结论的可信度
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Evidence {
    /// 已查证：本项目实测过，或与官方 API 一一对应
    Verified,
    /// 依赖具体设备 / 驱动 / 系统版本，平台级结论只能当方向
    DeviceDependent,
    /// 待验证：资料不足，需真机确认后才能作为依据
    Unverified,
}

impl Evidence {
    /// 证据等级名（稳定标识）
    pub const fn name(&self) -> &'static str {
        match self {
            Evidence::Verified => "verified",
            Evidence::DeviceDependent => "device_dependent",
            Evidence::Unverified => "unverified",
        }
    }
}

impl std::fmt::Display for Evidence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// 查证：某个 (平台, 能力) 的结论有多可信
///
/// [`Capability::evidence`] 给的是「这类能力整体上有多可信」，
/// 这里进一步按平台细化 —— 同一项能力在不同平台上的查证程度并不一样：
///
/// - **Windows / macOS / Linux / Android / iOS**：结论来自官方 API 名称或本项目实测，
///   记 [`Evidence::Verified`]（硬件编码再降一档，见下）
/// - **`HarmonyOS`**：尚未在真机上验证过任何一项，一律 [`Evidence::Unverified`]
/// - **Unknown**：什么都不知道，也是 [`Evidence::Unverified`]
/// - **硬件编码**：即使平台确定了，具体机器有没有编码器仍要按机型确认，
///   记 [`Evidence::DeviceDependent`]
pub const fn evidence_for(platform: Platform, cap: Capability) -> Evidence {
    match platform {
        // 这两个平台的任何结论都还没验证过
        Platform::HarmonyOs | Platform::Unknown => Evidence::Unverified,
        // 硬件编码取决于机型 / 驱动，平台级结论只能当方向
        Platform::Windows
        | Platform::MacOs
        | Platform::Linux
        | Platform::Android
        | Platform::Ios => cap.evidence(),
    }
}

/// 三态支持程度
///
/// 为什么不直接用 `bool`：见模块文档「关于『部分支持』的简化」。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SupportLevel {
    /// 平台不提供该能力
    None,
    /// 能做，但有前提条件、需要降级策略，或需要自行验证
    Partial,
    /// 平台有官方 API，行为可预期
    Full,
}

impl SupportLevel {
    /// 支持程度名（稳定标识）
    pub const fn name(&self) -> &'static str {
        match self {
            SupportLevel::None => "none",
            SupportLevel::Partial => "partial",
            SupportLevel::Full => "full",
        }
    }

    /// 是否至少部分可用
    pub const fn is_usable(&self) -> bool {
        !matches!(self, SupportLevel::None)
    }
}

impl std::fmt::Display for SupportLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// 能力矩阵：`(平台, 能力) -> 支持程度`
///
/// 实现方式是一张**完整的二维表**，而不是一串 `if` / `match` 分支。
/// 原因：这张表是给人看、给人改的规格说明，不是控制流。
/// 写成表之后，`Unknown` 列、缺失格子、`⚠️` 转 `Partial` 都能一眼核对，
/// 而且测试可以按行列遍历验证「没有漏格」。
///
/// 行内顺序固定为 [`Platform::ALL`]；**新增平台时编译器会报错**
/// （数组长度不匹配），不会静默漏填。
///
/// # 读法
///
/// ```text
/// 每行： 能力 => [Windows, MacOs, Linux, Android, Ios, HarmonyOs, Unknown]
/// ```
// 矩阵刻意保留列对齐：这是给人核对的规格表，不是普通代码。
#[rustfmt::skip]
pub const MATRIX: [(Capability, [SupportLevel; 7]); 9] = [
    (
        Capability::HighResTimer,
        [
            SupportLevel::Full,    // Windows：timeBeginPeriod
            SupportLevel::None,    // macOS：本身就是微秒级，不需要
            SupportLevel::None,    // Linux：同 macOS
            SupportLevel::None,    // Android
            SupportLevel::None,    // iOS
            SupportLevel::None,    // HarmonyOS
            SupportLevel::None,    // Unknown
        ],
    ),
    (
        Capability::VirtualCameraApi,
        [
            SupportLevel::Full,    // Windows：MFCreateVirtualCamera（Win11 22000+）
            SupportLevel::None,    // macOS：无系统 API，需 DriverKit
            SupportLevel::None,    // Linux：需 v4l2loopback 内核模块
            SupportLevel::None,    // Android
            SupportLevel::None,    // iOS
            SupportLevel::None,    // HarmonyOS
            SupportLevel::None,    // Unknown
        ],
    ),
    (
        Capability::OutOfProcComServer,
        [
            SupportLevel::Full,    // Windows
            SupportLevel::None,    // macOS
            SupportLevel::None,    // Linux
            SupportLevel::None,    // Android
            SupportLevel::None,    // iOS
            SupportLevel::None,    // HarmonyOS
            SupportLevel::None,    // Unknown
        ],
    ),
    (
        Capability::NamedPipe,
        [
            SupportLevel::Full,    // Windows：命名管道
            SupportLevel::Full,    // macOS：Unix socket 算命名管道
            SupportLevel::Full,    // Linux：Unix socket
            SupportLevel::Partial, // Android：仅本机抽象 socket
            SupportLevel::Partial, // iOS：同 Android
            SupportLevel::Partial, // HarmonyOS：同 Android
            SupportLevel::None,    // Unknown
        ],
    ),
    (
        Capability::SharedMemory,
        [
            SupportLevel::Full,    // Windows：CreateFileMapping
            SupportLevel::Full,    // macOS：POSIX shm
            SupportLevel::Full,    // Linux：POSIX shm
            SupportLevel::Full,    // Android：ashmem
            SupportLevel::Full,    // iOS：POSIX shm（受沙箱约束，但能力存在）
            SupportLevel::Full,    // HarmonyOS：ashmem 类接口（待验证）
            SupportLevel::None,    // Unknown
        ],
    ),
    (
        Capability::UdpHolePunching,
        [
            SupportLevel::Full,    // Windows
            SupportLevel::Full,    // macOS
            SupportLevel::Full,    // Linux
            SupportLevel::Partial, // Android：需处理移动网络 NAT
            SupportLevel::Partial, // iOS：同 Android
            SupportLevel::Partial, // HarmonyOS：同 Android
            SupportLevel::None,    // Unknown
        ],
    ),
    (
        Capability::HardwareH264,
        [
            SupportLevel::Full,    // Media Foundation
            SupportLevel::Full,    // VideoToolbox
            SupportLevel::Full,    // VA-API / NVENC
            SupportLevel::Full,    // MediaCodec
            SupportLevel::Full,    // VideoToolbox
            SupportLevel::Partial, // HarmonyOS：AVCodec Kit 存在，具体 API 待验证
            SupportLevel::None,    // Unknown
        ],
    ),
    (
        Capability::HardwareH265,
        [
            SupportLevel::Full,    // Windows
            SupportLevel::Full,    // macOS
            SupportLevel::Full,    // Linux
            SupportLevel::Full,    // Android
            SupportLevel::Full,    // iOS
            SupportLevel::Partial, // HarmonyOS：视芯片
            SupportLevel::None,    // Unknown
        ],
    ),
    (
        Capability::ScreenCapture,
        [
            SupportLevel::Full,    // Windows.Graphics.Capture
            SupportLevel::Full,    // ScreenCaptureKit
            SupportLevel::Full,    // PipeWire / X11
            SupportLevel::Full,    // MediaProjection
            SupportLevel::Full,    // ReplayKit
            SupportLevel::Partial, // HarmonyOS：待验证
            SupportLevel::None,    // Unknown
        ],
    ),
];

/// 查表：`(平台, 能力) -> 支持程度`
///
/// 纯函数，无 IO、无全局状态。
/// 找不到格子（理论上不可能，表是全覆盖的）时返回
/// [`SupportLevel::None`] —— 保守失败：宁可认为「不支持」，
/// 也不要误以为有 API 而在运行时炸掉。
pub fn support_level(platform: Platform, cap: Capability) -> SupportLevel {
    let idx = platform_index(platform);
    for (row_cap, row) in &MATRIX {
        if *row_cap == cap {
            return row[idx];
        }
    }
    SupportLevel::None
}

/// `Platform` 在矩阵行内的下标
///
/// 与 [`Platform::ALL`] 的顺序**必须一致**。这是整张表唯一的隐式耦合，
/// 所以有专门的测试（`matrix_platform_order_matches_all`）守着。
const fn platform_index(platform: Platform) -> usize {
    match platform {
        Platform::Windows => 0,
        Platform::MacOs => 1,
        Platform::Linux => 2,
        Platform::Android => 3,
        Platform::Ios => 4,
        Platform::HarmonyOs => 5,
        Platform::Unknown => 6,
    }
}

/// 查表：这个平台支不支持某能力
///
/// 语义是 `support_level(...) != SupportLevel::None`，即
/// **`Partial` 也算支持**。判断依据是「能不能做出功能」，
/// 而不是「能不能和 Windows 一样省事」。
///
/// 需要区分「原生支持」和「有前提条件」时请直接用 [`support_level`]。
pub fn supports(platform: Platform, cap: Capability) -> bool {
    support_level(platform, cap).is_usable()
}

/// 某平台支持的全部能力（含 `Partial`）
///
/// 用于生成平台报告、决定编译期要启用哪些模块。
pub fn capabilities_of(platform: Platform) -> Vec<Capability> {
    Capability::ALL
        .iter()
        .copied()
        .filter(|cap| supports(platform, *cap))
        .collect()
}

/// 某平台**缺失**的能力
///
/// 这就是移植清单的机器可读版本：每一项都需要一个新的平台实现模块。
pub fn missing_capabilities(platform: Platform) -> Vec<Capability> {
    Capability::ALL
        .iter()
        .copied()
        .filter(|cap| !supports(platform, *cap))
        .collect()
}

/// 每个平台的移植要点
///
/// 这里是**给人看的知识**，不是给机器判定的规则 —— 判定用 [`support_level`]。
/// 两者分开维护的理由：能力矩阵要稳定（代码依赖它），
/// 移植要点会随调研进展不断更新（一条坑验证完就可以删掉）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlatformNotes {
    /// 该平台
    pub platform: Platform,
    /// 该平台要做的移植工作的简短清单
    pub todo: Vec<&'static str>,
    /// 该平台特有的坑
    pub pitfalls: Vec<&'static str>,
}

impl PlatformNotes {
    /// 取某平台的移植要点
    ///
    /// 每个平台都返回非空 `todo` —— 包括 [`Platform::Unknown`]，
    /// 它的 todo 就是「先确定平台」，这本身也是一件必须做的事。
    pub fn for_platform(p: Platform) -> Self {
        match p {
            Platform::Windows => Self::windows(),
            Platform::MacOs => Self::macos(),
            Platform::Linux => Self::linux(),
            Platform::Android => Self::android(),
            Platform::Ios => Self::ios(),
            Platform::HarmonyOs => Self::harmonyos(),
            Platform::Unknown => Self::unknown(),
        }
    }

    /// 该平台缺失的能力数（移植工作量的粗略指标）
    pub fn missing_count(&self) -> usize {
        missing_capabilities(self.platform).len()
    }

    /// Windows：现状平台，工作主要是「保持」
    fn windows() -> Self {
        Self {
            platform: Platform::Windows,
            todo: vec![
                "保持现有实现：虚拟摄像头（DLL + FrameServer）、命名管道、共享内存均已可用",
                "把散落的 cfg(windows) 分支改为查 platform::supports，为其他平台留出接缝",
                "补齐 HardwareH265 的可用性探测（HEVC MFT 需要系统装了 HEVC 视频扩展）",
            ],
            pitfalls: vec![
                "GetVersionExW 会说谎：没有应用 manifest 时一律返回 6.2 / Build 9200，必须用 RtlGetVersion",
                "虚拟摄像头需要 Build 22000+（MFCreateVirtualCamera 的下限），且媒体源必须编成 DLL 被 FrameServer 加载，单进程实现走不通",
                "注册 CLSID 到 HKCU 需要写注册表权限，某些策略下会被拦",
                "定时器默认分辨率 15.625ms，不调 timeBeginPeriod 会让 interval(1ms) 实际 15.6ms 才触发",
                "命名管道传 3MB 帧时开销爆炸（单帧约 4.2ms），像素必须走共享内存、管道只传 44 字节信号",
            ],
        }
    }

    /// macOS：坑最多的平台
    fn macos() -> Self {
        Self {
            platform: Platform::MacOs,
            todo: vec![
                "用 Unix domain socket 替换命名管道（语义几乎一致，改造量小）",
                "用 POSIX shm_open + mmap 替换 CreateFileMapping",
                "用 VideoToolbox 替换 Media Foundation 做 H.264 / H.265 编解码",
                "用 ScreenCaptureKit 做屏幕捕获（macOS 12.3+）",
                "定时器无需处理：系统时钟本就是微秒级，删掉 HighResTimer 的调用即可",
                "UDP 打洞逻辑可原样复用（BSD socket 语义与 Winsock 基本一致）",
                "虚拟摄像头需要新写 DriverKit 扩展工程（独立于主工程，且需要 Apple 审批）",
            ],
            pitfalls: vec![
                "虚拟摄像头没有系统 API，必须写 DriverKit 扩展；而 DriverKit 的 Camera 类 entitlement 需要向 Apple 单独申请审批，个人开发者账号基本拿不到 —— 这是 macOS 移植的最大不确定性，应先确认能否拿到再排期",
                "VideoToolbox 编码器要求 CVPixelBuffer 输入，从裸 NV12 转过去要一次拷贝（CVPixelBufferCreateWithBytes 也免不了），1080p60 下这笔拷贝必须计入延迟预算",
                "屏幕录制需要 TCC 权限（系统设置 → 隐私与安全性 → 屏幕录制），首次调用会弹窗且无法用代码绕过；用户拒绝后再次调用不会重复弹窗，只能在 UI 上引导手动开",
                "mach_absolute_time 是单调时钟，但单位取决于 CPU（Intel 与 Apple Silicon 的 timebase 不同），必须用 mach_timebase_info 换算，直接当纳秒用会差出几十倍",
                "Unix domain socket 的路径上限是 104 字节（sun_path），用临时目录拼路径时容易超，报错还很隐晦",
                "App 沙箱下 shm_open 的名字要带团队前缀，否则可能与其他 App 撞名",
            ],
        }
    }

    /// Linux：安装摩擦最大的平台
    fn linux() -> Self {
        Self {
            platform: Platform::Linux,
            todo: vec![
                "用 Unix domain socket 替换命名管道",
                "用 POSIX shm_open + mmap（或 memfd_create）替换 CreateFileMapping",
                "接 VA-API 做硬件编解码，NVENC 作为 N 卡的可选加速路径",
                "屏幕捕获写两套：Wayland 走 PipeWire + xdg-desktop-portal，X11 走 XShm",
                "音频采集同时支持 PipeWire 与 PulseAudio",
                "虚拟摄像头依赖 v4l2loopback，需要写检测 + 引导用户安装的逻辑",
                "定时器无需处理：tokio 定时器精度足够，CONFIG_HZ 只影响内核调度粒度",
            ],
            pitfalls: vec![
                "虚拟摄像头只能靠 v4l2loopback 内核模块，用户要自己装包并 modprobe，安装摩擦远大于 Windows 的「装个 MSI」；模块未加载时连设备节点都不存在，必须做能力检测 + 明确提示，不能静默失败",
                "PipeWire 与 PulseAudio 两套音频栈并存（不同发行版默认不同，PipeWire 还兼容 PulseAudio 协议），采集代码要都支持，或用 PipeWire 的 pulse 兼容层兜底",
                "X11 与 Wayland 的屏幕捕获完全不同：Wayland 下普通进程拿不到其他窗口的像素，必须走 portal（xdg-desktop-portal + PipeWire），而且每次会话都要用户授权，无法自动化",
                "Wayland 下拿不到窗口的全局坐标（安全设计），任何依赖「屏幕绝对坐标」的逻辑都要重写",
                "定时器精度依赖内核配置（CONFIG_HZ，常见 250 / 1000），但通常 1ms 够用，不需要像 Windows 那样提精度",
                "VA-API 的可用性取决于驱动（Intel / AMD 好，N 卡要装 nvidia-vaapi-driver），不能只看有没有 libva",
                "打包分发要面对 glibc 版本与发行版差异，静态链接或 AppImage 各有代价",
            ],
        }
    }

    /// Android：生命周期与电量是主要矛盾
    fn android() -> Self {
        Self {
            platform: Platform::Android,
            todo: vec![
                "把采集与编码放进前台 Service，配 foregroundServiceType（camera / mediaProjection）",
                "用 MediaCodec 做硬编，Surface 输入输出要处理 buffer 生命周期",
                "用 MediaProjection 做屏幕捕获（需用户授权 + 前台 Service）",
                "共享内存改用 ashmem / ASharedMemory",
                "本机 IPC 用 LocalSocket（注意它不是 POSIX AF_UNIX 的完整语义）",
                "加动态码率 / 动态帧率控制，应对发热降频",
                "适配移动网络 NAT：打洞失败要能快速回落到中继",
            ],
            pitfalls: vec![
                "摄像头必须在前台 Service 里访问，App 退到后台会被系统掐断（Android 9+ 禁止后台访问摄像头），必须做前台 Service + 常驻通知",
                "MediaCodec 是异步的：configure / start 后要跑一个消息循环收 buffer；Surface 输入输出模式下 buffer 生命周期由 Surface 管，释放顺序错了会泄漏或花屏",
                "移动网络 NAT 类型复杂（运营商级 NAT、对称 NAT 很常见），UDP 打洞成功率明显低于 WiFi，必须有中继兜底，不能把打洞当唯一路径",
                "电量与发热：持续编码 + 网络会显著发热，几分钟后系统降频，表现为掉帧和码率不稳；需要动态降帧率 / 降分辨率，并监听 thermal 状态",
                "屏幕捕获（MediaProjection）每次都要用户点确认弹窗，且 Android 14 起要求前台 Service 类型匹配，否则启动即崩",
                "不同厂商 ROM 对后台限制的策略差异很大（国产 ROM 尤其激进），需要引导用户加白名单",
            ],
        }
    }

    /// iOS：沙箱与后台限制
    fn ios() -> Self {
        Self {
            platform: Platform::Ios,
            todo: vec![
                "用 VideoToolbox 做硬编（与 macOS 同源，代码可共享）",
                "用 ReplayKit 做屏幕捕获",
                "共享内存用 POSIX shm + mmap，注意沙箱路径",
                "推流逻辑适配前台 / 后台切换，后台模式要申请对应的 background mode",
                "虚拟摄像头在 iOS 上不存在，相关功能直接裁掉而不是做降级",
            ],
            pitfalls: vec![
                "后台运行受限：推流必须在 foreground，或依赖有限的 background mode（voip / audio / external-accessory 等），普通 App 退到后台几秒后就会被挂起",
                "ReplayKit 的广播上传扩展有严格内存上限（约 50MB），超了直接被系统杀掉；所以编码必须放在主 App 做，扩展只负责收 SampleBuffer 并转交（走 socket 或共享内存）",
                "无法做系统级虚拟摄像头：沙箱不允许注册系统设备，第三方 App 也拿不到其他 App 的相机流",
                "屏幕捕获（ReplayKit）的广播扩展与主 App 是两个进程，通信只能走 App Group 或 socket，且扩展随时可能被系统回收",
                "所有硬件能力都要考虑机型差异（老设备可能没有 HEVC 硬编，或只有 8bit）",
            ],
        }
    }

    /// HarmonyOS：资料最少，先调研再动手
    fn harmonyos() -> Self {
        Self {
            platform: Platform::HarmonyOs,
            todo: vec![
                "先做调研：确认 AVCodec Kit 的 H.264 / H.265 硬编接口与能力查询方式（待验证）",
                "确认屏幕捕获能力是否对外开放、需要什么权限、API 名称（待验证）",
                "确认共享内存接口（ashmem 类）与跨进程语义（待验证）",
                "确认 UDP socket 在移动网络 NAT 下的行为（待验证）",
                "评估 ArkTS / Native API 混编的工程结构，确定核心逻辑放在哪一层",
            ],
            pitfalls: vec![
                "生态较新，很多能力需要查官方文档确认 —— 本矩阵里 HarmonyOS 列凡标 Partial 且注明「待验证」的项，都是尚未在真机验证的推断，不能作为排期依据",
                "与 Android 的 API 差异较大，不能直接复用 Android 的 MediaCodec / MediaProjection 代码，即使概念相似",
                "HarmonyOS NEXT 起不再兼容 Android 应用（无 AOSP 兼容层），历史 Android 代码无法平移",
                "设备碎片化与系统版本差异可能比 Android 更影响硬件编码可用性",
            ],
        }
    }

    /// 未知平台：唯一要做的是先搞清楚它是什么
    fn unknown() -> Self {
        Self {
            platform: Platform::Unknown,
            todo: vec![
                "先确定目标平台：Platform::current() 返回 Unknown 说明该 target 未纳入矩阵",
                "在 Platform::ALL 与 MATRIX 中补齐该平台的一列（数组长度会让编译器强制补齐）",
                "补齐该平台的 PlatformNotes（todo 与 pitfalls 都不能空）",
            ],
            pitfalls: vec![
                "所有能力都返回 None：这是保守的失败方式，不会误报「有 API」而在运行时崩",
                "不要用 Unknown 兜底实现「通用逻辑」—— 它代表「不知道」，不是「都支持」",
            ],
        }
    }
}

/// 当前平台的移植要点
pub fn current_notes() -> PlatformNotes {
    PlatformNotes::for_platform(Platform::current())
}

/// 移植代价的粗略评分：越小越省事
///
/// 计分规则：
///
/// - [`SupportLevel::None`] 记 **2 分** —— 要新写一整个平台实现模块
/// - [`SupportLevel::Partial`] 记 **1 分** —— 要写降级分支，但主路径能复用
/// - [`SupportLevel::Full`] 记 **0 分**
///
/// 为什么 `Partial` 也要计分：只数 `None` 的话，六个非 Windows 平台
/// 都是「缺 3 项」，排序会退化回枚举声明顺序，等于没排。
/// 而实际差异恰恰在 `Partial` 上 —— 移动端多出来的每一处降级分支
/// 都是实打实的工作量。
///
/// 注意这只是**能力维度**的代价，不含人力、审批、生态成熟度。
pub fn migration_cost(platform: Platform) -> usize {
    Capability::ALL
        .iter()
        .map(|cap| match support_level(platform, *cap) {
            SupportLevel::None => 2,
            SupportLevel::Partial => 1,
            SupportLevel::Full => 0,
        })
        .sum()
}

/// 建议的移植顺序（从代价最小到最大）
///
/// 按 [`migration_cost`] 升序；同分时按 [`Platform::ALL`] 的顺序，
/// 保证结果稳定可复现（排期表不能每次跑都不一样）。
///
/// 这是个**粗排**，不含人力与商业因素：比如 macOS 虽然代价不高，
/// 但 `DriverKit` entitlement 拿不到的话整条虚拟摄像头路线都不成立，
/// 实际排期要先把这件事问清楚（见 [`PlatformNotes::for_platform`] 的 pitfalls）。
///
/// [`Platform::Unknown`] 不参与排序：它不是目标平台。
pub fn migration_order() -> Vec<Platform> {
    let mut order: Vec<Platform> = Platform::ALL
        .iter()
        .copied()
        .filter(|p| *p != Platform::Unknown)
        .collect();
    // sort_by_key 是稳定排序，同分时保持 Platform::ALL 的原始顺序
    order.sort_by_key(|p| migration_cost(*p));
    order
}

/// 生成一份人可读的能力矩阵（Markdown 表格）
///
/// 给文档和排期用。放在代码里而不是手写文档，是为了让矩阵永远与
/// [`MATRIX`] 一致 —— 手抄的表格一定会过期。
///
/// 符号约定：`✅` Full、`⚠️` Partial、`❌` None。
pub fn matrix_markdown() -> String {
    let mut out = String::new();

    out.push_str("| 能力 |");
    for p in &Platform::ALL {
        out.push(' ');
        out.push_str(p.name());
        out.push_str(" |");
    }
    out.push('\n');

    out.push_str("| --- |");
    for _ in &Platform::ALL {
        out.push_str(" --- |");
    }
    out.push('\n');

    for cap in &Capability::ALL {
        out.push_str("| ");
        out.push_str(cap.name());
        out.push_str(" |");
        for p in &Platform::ALL {
            let symbol = match support_level(*p, *cap) {
                SupportLevel::Full => "✅",
                SupportLevel::Partial => "⚠️",
                SupportLevel::None => "❌",
            };
            out.push(' ');
            out.push_str(symbol);
            out.push_str(" |");
        }
        out.push('\n');
    }

    out
}
