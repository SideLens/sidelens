# 虚拟摄像头：SDK 头文件里的三个被忽略的 API

> 发现方式：直接读 SDK 的 `mfvirtualcamera.h`（`C:\Program Files (x86)\Windows Kits\10\Include\10.0.26100.0\um\`）
> 而不是只看官方示例代码。

## 背景

前十四轮一直在猜「FrameServer 为什么拒绝这个媒体源」，
排查过 `IMFGetService` 的 IID、`CustomCaptureSourceClsid` 注册表值、
`VCamCreate_*` 属性、设备类别…… 全部无果。

本轮直接读 SDK 头文件，发现**有三个 API / 属性我们从来没用过**。

---

## 1. `IMFVirtualCamera::AddRegistryEntry`（从未调用）

```cpp
virtual HRESULT STDMETHODCALLTYPE AddRegistryEntry(
    _In_z_ LPCWSTR EntryName,
    _In_opt_z_ LPCWSTR SubkeyPath,
    _In_ DWORD dwRegType,
    _In_reads_bytes_(cbData) const BYTE *pbData,
    _In_ ULONG cbData) = 0;
```

**这是「往设备接口节点写注册表值」的官方 API。**

前几轮一直在纠结「怎么把 `CustomCaptureSourceClsid` 写进设备接口节点」——
注册表 ACL 拒绝直接写，需要提权，而且写进去也未必被读。

**官方给的正是这个入口**：`AddRegistryEntry` 由系统代写，
不需要自己处理 ACL。

`windows` crate 0.62 **已经导出这个 API**：

```rust
pub unsafe fn AddRegistryEntry<P0, P1>(
    &self,
    entryname: P0,
    subkeypath: P1,
    dwregtype: u32,
    pbdata: &[u8],
) -> windows_core::Result<()>
```

我们的 `crates/sidelens-vcam/src/register.rs` **一次都没调用过它**。

---

## 2. `DEVPKEY_DeviceInterface_IsVirtualCamera`（从未设置）

SDK 头文件第 81 行：

```cpp
// {6EDC630D-C2E3-43B7-B2D1-20525A1AF120}, 3
DEFINE_DEVPROPKEY(DEVPKEY_DeviceInterface_IsVirtualCamera,
    0x6EDC630D, 0xC2E3, 0x43B7, 0xB2, 0xD1, 0x20, 0x52, 0x5A, 0x1A, 0xF1, 0x20, 3);
    // DEVPROP_TYPE_BOOLEAN
```

**实测我们的设备上这个属性是空的**：

```powershell
Get-PnpDeviceProperty -InstanceId 'SWD\VCAMDEVAPI\2D33F5D0...' \
    -KeyName '{6EDC630D-C2E3-43B7-B2D1-20525A1AF120} 3'
# → 值 = （空）  类型 = Empty
```

类型是 `Empty` 说明**这个属性根本不存在**，而不是「值为 false」。

一个虚拟摄像头却没有 `IsVirtualCamera = true` 标记 ——
这很可能就是 FrameServer 判定「这不是合格的虚拟摄像头」的直接依据。

**同一组 GUID 还有两个属性**（SDK 第 83、85 行）：

| pid | 名称 | 类型 |
| --- | --- | --- |
| 3 | `DEVPKEY_DeviceInterface_IsVirtualCamera` | BOOLEAN |
| 4 | `DEVPKEY_DeviceInterface_IsWindowsCameraEffectAvailable` | BOOLEAN |
| 5 | `DEVPKEY_DeviceInterface_VirtualCameraAssociatedCameras` | STRING_LIST |

---

## 3. `MF_FRAMESERVER_VCAMEVENT_EXTENDED_*`（六个事件 GUID，从未使用）

SDK 第 101-106 行定义了六个 FrameServer 虚拟摄像头事件：

```cpp
EXTERN_GUID(MF_FRAMESERVER_VCAMEVENT_EXTENDED_SOURCE_INITIALIZE,       0xE52C4DFF, ...);
EXTERN_GUID(MF_FRAMESERVER_VCAMEVENT_EXTENDED_SOURCE_START,            0xB1EEB989, ...);
EXTERN_GUID(MF_FRAMESERVER_VCAMEVENT_EXTENDED_SOURCE_STOP,             0xB7FE7A61, ...);
EXTERN_GUID(MF_FRAMESERVER_VCAMEVENT_EXTENDED_SOURCE_UNINITIALIZE,     0xA0EBABA7, ...);
EXTERN_GUID(MF_FRAMESERVER_VCAMEVENT_EXTENDED_PIPELINE_SHUTDOWN,       0x45A81B31, ...);
EXTERN_GUID(MF_FRAMESERVER_VCAMEVENT_EXTENDED_CUSTOM_EVENT,            0x6E59489C, ...);
```

这与 `mfsensorgroup.dll` 里的 `VCamEvent_Initialize` / `VCamEvent_Start` /
`VCamEvent_Stop` / `VCamEvent_Uninitialize` / `VCamEvent_Shutdown` /
`VCamEvent_CustomEvent` **一一对应**。

**这解释了 `2032C7EF` 是什么**：它很可能是这套事件接口的 IID。
FrameServer 通过 `IMFGetService(2032C7EF)` 拿「虚拟摄像头事件接收器」，
我们返回 `MF_E_UNSUPPORTED_SERVICE` 之后它就不知道该往哪发
`SOURCE_INITIALIZE` 事件了 —— 于是流程走完就 Shutdown。

---

## 4. 顺带纠正：`6ac1fbf7` 这个 GUID 在 SDK 里不存在

我们 `devprop.rs` 用的 `VCAM_CREATE_GUID = 6ac1fbf7-45f7-4e06-bda7-f817ebfa04d1`
是从**官方示例源码的 C++ 字符串**里抄的（`"{6ac1fbf7-45f7-4e06-bda7-f817ebfa04d1}, 4"`）。

实测：

| 检查 | 结果 |
| --- | --- |
| `devpkey.h`（10.0.26100.0） | 0 命中 |
| `mfvirtualcamera.h` | 0 命中 |
| `mfsensorgroup.dll` | 0 命中 |
| `mfcore.dll` / `FrameServer.dll` / `mfplat.dll` 等 10 个 MF DLL | 全部 0 命中 |
| 注册表（Enum 树） | 0 命中 |

**这个 GUID 在本机所有相关二进制里都不存在。**

两种可能：

1. 它是示例代码自造的「约定值」，靠 `AddRegistryEntry` 写进注册表，
   然后由**示例自己的**代码读（而不是系统读）
2. 它属于某个更新版本的 Windows，本机 build 26300 还没有

第 1 种更可能 —— 因为官方示例的 `VirtualCamera_Installer` 会写注册表，
而 `AddRegistryEntry` 正是那个入口。

---

## 下一轮优先级（完全重排）

| 优先级 | 动作 | 依据 |
| --- | --- | --- |
| **1** | 调 `AddRegistryEntry` 设 `DEVPKEY_DeviceInterface_IsVirtualCamera = true` | SDK 明确定义，我们的设备上该属性为空 |
| **2** | 用 `AddRegistryEntry` 写 `VCamClsid`（不是 `CustomCaptureSourceClsid`） | 官方入口，不需要处理 ACL |
| 3 | 实现 `IMFGetService(2032C7EF)` 返回一个事件接收器 | 六个 `VCAMEVENT_EXTENDED_*` GUID 与 `VCamEvent_*` 一一对应 |
| 4 | 反查 `2032C7EF` 的 vtable 确认方法表 | 若与事件接口吻合则第 3 条成立 |
| ~~5~~ | ~~写 `CustomCaptureSourceClsid`~~ | **已证伪**（FrameServer.dll 里没这个字符串） |

---

## 方法论

前十四轮全部在「看官方示例代码 → 猜 → 试 → 失败」这个循环里。
本轮换了方法：**直接读 SDK 头文件，把它当规范而不是当参考**。

结果立刻看到三个从未使用的 API —— 而官方示例代码里也没用它们
（示例走的是「安装程序写注册表」路线，不是 `AddRegistryEntry` 路线）。

**教训**：当示例代码与 SDK 头文件不一致时，头文件更权威 ——
它是系统契约，示例只是其中一种实现。
