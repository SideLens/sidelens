# 虚拟摄像头踩坑链路

> 每一步都是实际运行后遇到的错误，不是查文档猜的。
> 这些门槛**互不包含**：过了前一个不代表后一个能过。

## 错误码演进（同一段代码，修一个冒一个）

| # | 错误码 | 含义 | 根因 | 修法 |
| --- | --- | --- | --- | --- |
| 1 | — | 检测误报「不支持」 | `GetVersionExW` 返回 9200 | 改 `RtlGetVersion` |
| 2 | `0x800401F0` | CO_E_NOTINITIALIZED | 没 `CoInitializeEx` | COM(MTA) + `MFStartup` |
| 3 | `0x800401F3` | CO_E_CLASSSTRING | sourceId 不是 CLSID | `{GUID}` 格式 |
| 4 | `0x80040154` | REGDB_E_CLASSNOTREG | CLSID 未注册 | `CoRegisterClassObject` |
| 5 | `0x80004002` | E_NOINTERFACE (IMFActivate) | 只实现 IMFMediaSource | 实现 `IMFActivate` |
| 6 | `0x80004001` | E_NOTIMPL (SetItem) | 属性存储没实现 | PROPVARIANT 读写 |
| 7 | `0xC00D36BD` | MF_E_INVALIDTYPE (vt=31) | 不支持 VT_LPWSTR | 加字符串变体 |
| 8 | `0x80004002` | E_NOINTERFACE (IMFMediaSourceEx) | 框架要 Ex 版本 | 追加 `IMFMediaSourceEx` |
| 9 | `0x80070057` | E_INVALIDARG (Start) | 流描述符没有媒体类型 | 补 NV12 类型表 + SetCurrentMediaType |
| 10 | `0x80070003` | ERROR_PATH_NOT_FOUND (Start) | CLSID 未登记到注册表 | 写 `HKCU\Software\Classes\CLSID` |
| 11 | `0x800401F9` | DLL 中出错 | 登记的是 exe，没有 DllGetClassObject | 编成 cdylib DLL |
| 12 | `0x80070003` | ERROR_PATH_NOT_FOUND (Start) | **缺 4 个 AddProperty** | 补 VCamCreate_* 属性 |
| 13 | `0x80070005` | ERROR_ACCESS_DENIED (AddProperty) | 需要管理员权限 | **需提权**（见下） |

**第 10→11 是一次决定性验证**：

```text
不登记注册表      → CLASSNOTREG (0x80040154)
登记 exe 路径     → DLL 中出错 (0x800401F9)   ← 错误码变了，说明确实去加载了
```

错误码从「找不到」变成「找到了但加载失败」，证明三件事：

1. `CoRegisterClassObject` **不够** —— 它只对本进程的 `CoCreateInstance` 有效
2. `IMFVirtualCamera::Start` 由 **FrameServer（独立进程）** 发起，走注册表查 DLL
3. 所以媒体源**必须编成 DLL** 并导出 `DllGetClassObject`

## 架构结论：单进程实现在这条路上走不通

微软官方示例的工程结构本身就说明了这点：

```text
VirtualCameraMediaSource/
├── VirtualCameraMediaSource.def   ← 导出 DllGetClassObject / DllCanUnloadNow
├── dllmain.cpp
└── VirtualCamera_MSI/             ← 用 MSI 注册 CLSID
```

**三方组件必须分离**：

| 组件 | 形态 | 职责 |
| --- | --- | --- |
| 媒体源 | **DLL** | 提供 IMFActivate / IMFMediaSource，被 FrameServer 加载 |
| 常驻程序 | EXE | 建虚拟摄像头、喂帧、托盘 UI |
| 通信 | 命名管道 / 共享内存 | DLL（FrameServer 内）↔ EXE（采集端） |

这也解释了官方示例里为什么有 `CreateNamedPipeForStart` —— 不是可选优化，是架构必需。

## 各门槛详解

### 1. 系统版本：GetVersionExW 会说谎

没有应用 manifest 的进程，`GetVersionExW` 一律返回 6.2/9200（Windows 8）。
本机实际 Build 是 **26300**，API 报 **9200** —— 直接导致能力检测误判。

**修法**：`RtlGetVersion`（ntdll），不受 manifest 影响。

### 2. COM 初始化：检测通过 ≠ 能创建

能力检测（纯读注册表）不需要 COM，所以出现「检测说可用，一创建就失败」。

选 MTA 而非 STA：虚拟摄像头会被多个消费端同时连接，
STA 需要消息泵，而 tokio 运行时不泵 Windows 消息。

### 3. sourceId 必须是 CLSID

`"SideLens.Camera.Virtual.1"` → `CO_E_CLASSSTRING`。
必须 `{7A3E1C42-5D88-4F6B-9E20-1B4D8C3A9F57}`。

**GUID 不能随机生成**：每次启动换 GUID 会让设备列表堆满重复项。

### 4-5. 类工厂 + IMFActivate

**反直觉**：框架要的不是 `IMFMediaSource`，而是 `IMFActivate`，
再通过 `ActivateObject(IMFMediaSource)` 才拿到媒体源。

```text
错误：CLSID → IMFMediaSource          → E_NOINTERFACE
正确：CLSID → IMFActivate → ActivateObject → IMFMediaSource
```

`IMFActivate` 继承 `IMFAttributes`，要写 30 个属性方法。

### 6-7. PROPVARIANT 的变体

在未实现分支里打印 `vt` 值比猜快得多 —— 直接看到 `31`（`VT_LPWSTR`）。

字段路径很绕，中间有 `ManuallyDrop`：

```rust
pv.Anonymous.Anonymous.vt               // 类型标签
pv.Anonymous.Anonymous.Anonymous.ulVal // uint32 值

// 写字段必须显式解引用
let inner = &mut *pv.Anonymous.Anonymous;
inner.Anonymous.ulVal = value;
inner.vt = VT_UI4;
```

### 8. IMFMediaSourceEx

`ActivateObject` 请求 `IMFMediaSourceEx`，比 `IMFMediaSource` 多 3 个方法。

`SetD3DManager` 要返回 `E_NOTIMPL` 而不是 `S_OK` ——
返回 S_OK 会让消费端以为能走 GPU 零拷贝，后续拿到 CPU 帧会失败。

### 9. 流描述符必须有媒体类型

对照官方 `SimpleMediaStream::Initialize` 才发现，一个 NV12 类型要设 **8 个属性**：

| 属性 | 值 |
| --- | --- |
| `MF_MT_MAJOR_TYPE` | `MFMediaType_Video` |
| `MF_MT_SUBTYPE` | `MFVideoFormat_NV12` |
| `MF_MT_INTERLACE_MODE` | `Progressive` |
| `MF_MT_ALL_SAMPLES_INDEPENDENT` | 1 |
| `MF_MT_FRAME_SIZE` | `(w << 32) \| h` |
| `MF_MT_FRAME_RATE` | `(num << 32) \| den` |
| `MF_MT_PIXEL_ASPECT_RATIO` | `(1 << 32) \| 1` |
| `MF_MT_AVG_BITRATE` | `w * 1.5 * h * 8 * fps` |

而且描述符建好后**必须 `SetCurrentMediaType`** —— 只有格式表还不够，
消费端要读「当前选中」的那个。

## 第 12 步的排查记录（当前卡点）

**已排除的可能性**（都有实证）：

| 怀疑点 | 验证方式 | 结论 |
| --- | --- | --- |
| DLL 没被加载 | 文件日志显示 `MFCreateEventQueue 成功` | ❌ 已加载 |
| 类工厂没找到 | `ActivateObject 成功` 打印 | ❌ 找到了 |
| 媒体源接口不全 | 能 QueryInterface 到 IMFMediaSource + Ex | ❌ 齐全 |
| 描述符构建失败 | 文件日志 `流描述符 OK` | ❌ 成功 |
| 事件队列返回 E_NOTIMPL | 改用 MF 官方 `MFCreateEventQueue` | ❌ 已修复 |
| 缺管理员权限 | 用的是 `CurrentUser`，文档说不需管理员 | ❌ 不需要 |

**文件日志是唯一可靠的排查手段**：DLL 在 FrameServer 进程里跑，
stderr 完全看不到。加 `log_to_file` 之后才知道描述符那一步是成功的。

**第 12 步的真相（从官方 VCamUtils 源码找到）**：

`Start` 之前必须写 **4 个设备属性**，缺一个就是 `PATH_NOT_FOUND`：

```cpp
spVirtualCamera->AddProperty(
    &DEVPKEY_DeviceInterface_VCamCreate_SourceId, ...);
spVirtualCamera->AddProperty(
    &DEVPKEY_DeviceInterface_VCamCreate_FriendlyName, ...);
spVirtualCamera->AddProperty(
    &DEVPKEY_DeviceInterface_VCamCreate_Lifetime, ...);
spVirtualCamera->AddProperty(
    &DEVPKEY_DeviceInterface_VCamCreate_Access, ...);
```

**这些 GUID 在 SDK 的 `devpkey.h` 里本机找不到**（可能 SDK 版本差异），
从官方示例源码里直接抄：

```cpp
const winrt::hstring _DEVPKEY_DeviceInterface_VCamCreate_SourceId(
    L"{6ac1fbf7-45f7-4e06-bda7-f817ebfa04d1}, 4");
```

pid 从 4 连续到 7，共用同一个 GUID。

**第 13 步：AddProperty 需要管理员**

补上属性后立刻变成 `ERROR_ACCESS_DENIED`。官方代码对此有明确注释：

```cpp
if (hr == HRESULT_FROM_WIN32(ERROR_ACCESS_DENIED))
{
    LOG_WARNING(L"Please run application in elevated mode");
    return hr;
}
```

这说明：**注册虚拟摄像头是管理员级操作**，与 `CurrentUser` 访问范围无关。
写设备接口属性要动系统设备树，普通用户没有权限。

## 已确认可用的能力

- ✅ 系统版本检测（`RtlGetVersion`）
- ✅ COM(MTA) + MF 初始化
- ✅ `MFCreateVirtualCamera` 创建对象
- ✅ CLSID 格式校验 + 注册表读写（`clsid` 模块）
- ✅ 类工厂注册（`CoRegisterClassObject`）
- ✅ `IMFActivate` 实现（30 个属性方法 + PROPVARIANT 变体）
- ✅ `IMFMediaSourceEx` 实现（含 8 属性 NV12 媒体类型）
- ✅ `IMFAsyncCallback` 实现（队列 ID 用 STANDARD=1）
- ✅ 媒体源状态机（19 测试）
- ✅ **媒体源 DLL**（cdylib，四个 COM 导出齐全）
- ✅ regsvr32 可注册/反注册
- ✅ `AddProperty` 实现（4 个 VCamCreate 属性）
- ❌ `Start()` 通过 —— **需要管理员权限**（第 13 步）
- ❌ 帧投递（`MEMediaSample`）

## 下一步（DLL 化）

```text
1. 新建 crate type = ["cdylib"]
2. 导出 DllGetClassObject / DllCanUnloadNow
3. 媒体源状态通过命名管道与 EXE 共享
4. MSI 或 PowerShell 脚本注册 CLSID → DLL
5. 用 regsvr32 或自写注册逻辑安装
```
## 第十二轮：媒体源自检通过，问题锁定在 FrameServer 侧的 `2032C7EF` 接口

### 新增的自检工具（关键手段）

```bash
sidelensd.exe --vcam-selfcheck
```

它**绕过 FrameServer**，直接按注册表 CLSID 走一遍：

```text
CoCreateInstance(CLSID) → IMFActivate
  → ActivateObject → IMFMediaSource
  → CreatePresentationDescriptor（1 个流，已选中）
  → IMFMediaSource::Start(&pd, null, null)      ← FrameServer 从来不走这里
  → IMFMediaStream::RequestSample
```

结果：**全部通过**。

```text
✅ CoCreateInstance → IMFActivate
✅ ActivateObject → IMFMediaSource
✅ CreatePresentationDescriptor
   流数量 streams=1
   流 0 选中状态 selected=true
✅ IMFMediaSource::Start
```

**结论：我们的媒体源实现是合格的。** 问题出在 `IMFVirtualCamera::Start`
（设备注册侧）对媒体源的一次额外检查上。

### 本轮排除的四个原因（都是真实缺口，都已修）

| # | 缺什么 | 怎么发现的 |
| --- | --- | --- |
| 1 | `IMFGetService` | 官方文档列出五个必需接口，我们只有三个 |
| 2 | `IKsControl` | 同上 |
| 3 | `GetStreamAttributes` 返回空属性集 | 官方 `_SetStreamAttributes` 设了 4 项 |
| 4 | `MF_VIRTUALCAMERA_CONFIGURATION_APP_PACKAGE_FAMILY_NAME` 塞了假 PFN | 官方是 try/catch，不在 App 包内就**不设** |

### 另外三个已修的坑

1. **流 ID 四处不一致**：描述符 1 / 属性 0 / profile 引脚 0 / 属性查询 1。
   官方全部 0（`MF_DEVICESTREAM_STREAM_ID` 必须 0 基索引）。已统一。
2. **HKCU 注册对 FrameServer 不可见**：FrameServer 在 session 0，只读
   `HKLM\Software\Classes`。HKCU 登记完路径能读回来，但 `Start` 报
   `REGDB_E_CLASSNOTREG (0x80040154)`。改 HKLM 后消失。
3. **DLL 目录权限**：FrameServer 以 LOCAL SERVICE 运行，必须给它
   DLL 所在目录的读权限（社区 issue #47 确认）。

### 当前唯一卡点

FrameServer 的完整轨迹（已抓到全部 13 步）：

```text
1  MFCreateEventQueue
2  IMFMediaSource::GetCharacteristics
3  GetSourceAttributes
4  IMFGetService riid=2032C7EF-76F6-492A-94F3-4A81F69380CC   ← 未知接口，返回 UNSUPPORTED_SERVICE
5  CreatePresentationDescriptor
6  GetStreamAttributes(0)
7  GetSourceAttributes
8  GetSourceAttributes
9  IMFGetService riid=B91EBFEE-...  (IMFExtendedCameraController)
10 IKsControl::KsProperty(PROPSETID_VIDCAP_CAMERACONTROL, id=8, GET)
11 IMFMediaSource::Shutdown                                   ← 放弃，Start 从未调用
```

第 4 步的 `2032C7EF-76F6-492A-94F3-4A81F69380CC` 在 windows crate、
GUID-Finder 接口表、公开文档里都查不到 —— 它是 Windows 内部的
虚拟摄像头专用接口。FrameServer 在这个 QI 上拿到 UNSUPPORTED_SERVICE 后
继续走完了 5-10 步，最后仍然拒绝，说明它把「拿不到这个接口」记成了
「设备能力不足」，直到流程末尾才以 `E_INVALIDARG` 表现出来。

### 下一轮候选（已按证据重排）

1. **在设备接口节点写 `CustomCaptureSourceClsid`** ——
   官方文档说这是「声明 COM DLL 的 CoCreate-able CLSID」的**唯一**方式，
   而我们的设备接口树里整棵都没有这个值。
2. 反查 `2032C7EF` 的 vtable（`mfsensorgroup.dll` 的在线 PDB 符号）。
3. 放弃 SoftwareCameraSource，改走 DriverPackage + 桩驱动。
4. 用 Process Monitor 抓 Shutdown 前的注册表访问。