# 虚拟摄像头注册表取证（第十三～十四轮）

> 调查者：`vcam-registry` 子代理 + 主线复核
> 方法：逐键扫描 HKLM / HKCU / HKCR（847,910 个键）+ 二进制字符串/GUID 表分析

## 结论摘要

| # | 结论 | 证据强度 |
| --- | --- | --- |
| 1 | 写 `CustomCaptureSourceClsid` **无效** —— FrameServer.dll 里没这个字符串 | 🔴 强 |
| 2 | `2032C7EF` 是**虚拟摄像头会话对象**接口，不是可选能力探测 | 🔴 强 |
| 3 | 设备类别被系统强制成 `{62f9c741-...}`（SWD 通用设备类），**不是**摄像头类 | 🔴 强 |
| 4 | `VCamCreate_*` 属性写进去了但**读不到**（ACL 拒绝），无法验证 | 🟡 中 |
| 5 | 还有一整套未文档化的 vcam 常量（`KSCATEGORY_FSM_VIRTUAL_CAMERA` 等） | 🟡 中 |

---

## 1. `CustomCaptureSourceClsid` 这条路被推翻

微软文档 [frame-server-custom-media-source](https://learn.microsoft.com/en-us/windows-hardware/drivers/stream/frame-server-custom-media-source) 说：

> Add a registry entry under the device interface node ... using the registry value name **CustomCaptureSourceClsid**, which declares the CoCreate-able CLSID of the Custom Media Source COM object.

上一轮据此把「在设备接口节点写这个值」列为**候选 1**。本轮证伪：

```text
FrameServer.dll   ：CustomCaptureSourceClsid  ASCII 0 命中，UTF-16 0 命中
mfsensorgroup.dll ：0x82BD0 命中
mfcore.dll        ：0x3917C0 命中
```

**读取者不是 FrameServer。** 这条路线即使写成功，也不会被发起
`IMFVirtualCamera::Start` 的那个进程读到。

### 顺带纠正一个方法论问题

上一轮用 `reg query /f /s` 得出「整棵 DeviceClasses 树都没有这个值」。
但那个命令**遇到打不开的键会静默跳过**，仍然报告 0 命中。

本机 15 个含 `Properties` 子键的接口类全部拒绝读取（包括摄像头类）。
所以上一轮那句话在那块子树上**没有证据**。

真正关心的位置（设备接口的 reference-string 子键）是可读的，
那里确实是 0 个值 —— 结论不变，但证据链现在完整了。

---

## 2. `2032C7EF` 的身份

### 定位

```text
mfsensorgroup.dll 0x80848   ← GUID 表
  前一条（0x807C0）：UTF-16 "vcam_session-{%08lX-%04X-%04X-%02X%02X-%02X%02X%02X%02X%02X%02X}"
  后一条（0x80858）：{814AF394-93F6-CC80-8838-99FA83435A41}
```

同一个 GUID 也在 `FrameServer.dll` 0x104EC0、`mfcore.dll` 0x3784A0 出现。

### 语义

它紧跟在 `vcam_session-{GUID}` 这个格式化串后面，**是虚拟摄像头会话对象的接口**。

这解释了一个之前想不通的现象：FrameServer 在 QI 失败后**没有立刻放弃**，
而是继续走完了 6 步（CreatePresentationDescriptor → GetStreamAttributes ×2 →
IMFGetService(IMFExtendedCameraController) → IKsProperty → Shutdown）才退出。

它在找替代路径 —— 没找到。

**不是**「能力探测被拒所以判设备不合格」，而是「缺一个必需对象」。

---

## 3. 设备类别被强制成 SWD 通用设备类（本轮新发现，可能是关键）

```text
Enum\SWD\VCAMDEVAPI\<3 个实例>
  HardwareID     = VCAMDEVAPI\VCAMERA
  CompatibleIDs  = VCAMERA SWD\GenericRaw SWD\Generic
  ClassGUID      = {62f9c741-b25a-46ce-b54c-9bccce08b6f2}   ← 不是摄像头类
  Driver         = {62f9c741-...}\0010..0012
  InfPath        = c_swdevice.inf
  MatchingDeviceId = SWD\GenericRaw
```

而 `HKLM\SYSTEM\CurrentControlSet\Control\Class\{e5323777-f976-4f5b-9b55-b94699c46e44}`
（摄像头类）**根本不存在**。

### 这意味着什么

`MFCreateVirtualCamera` 建出来的设备节点，被 PnP 匹配到了
`SWD\GenericRaw` 这个**通用软件设备**驱动，而不是摄像头类驱动。

设备**确实**注册到了 `DeviceClasses\{e5323777-...}`（3 个接口节点都在），
所以它能被枚举成摄像头 —— 但它底层的设备节点是个「通用原始设备」，
不是「摄像头设备」。

FrameServer 走到 `GetSourceAttributes` / `IKsProperty` 时，
拿到的可能是通用设备的语义，而不是摄像头的语义。

### 与官方示例的差异

官方示例是 **DriverPackage + 桩驱动**路线：

1. 一个带桩驱动的驱动包，注册 `KSCATEGORY_VIDEO_CAMERA` 设备接口
2. 并在设备接口节点下写 `CustomCaptureSourceClsid`
3. 再加一个托管媒体源的 COM DLL

而 `MFCreateVirtualCamera` 的 `SoftwareCameraSource` 路线**声称**不需要驱动包 ——
它自己创建设备接口。实测下来它创建的设备节点落在 SWD 通用类里。

**这可能是「为什么 FrameServer 不认这个媒体源」的真正结构性原因**，
而不是某个接口没实现。

---

## 4. `VCamCreate_*` 属性：**根本没落到设备节点上**（决定性证据）

### 绕过 ACL 的办法

注册表的 `Properties` 子键 ACL 拒绝读取，但 `Get-PnpDeviceProperty`
走的是 **SetupDi API**（`SetupDiGetDeviceProperty`），
不受注册表 ACL 限制。所以能读到真实值。

### 实测结果

```powershell
Get-PnpDeviceProperty -InstanceId 'SWD\VCAMDEVAPI\2D33F5D0...' \
    -KeyName '{6ac1fbf7-45f7-4e06-bda7-f817ebfa04d1} 4'
```

```text
pid=2 : （空）
pid=3 : （空）
pid=4 : （空）   ← VCamCreate_SourceId
pid=5 : （空）   ← VCamCreate_FriendlyName
pid=6 : （空）   ← VCamCreate_Lifetime
pid=7 : （空）   ← VCamCreate_Access
pid=8..12 : （空）
```

**四个 `VCamCreate_*` 属性一个都不在设备节点上。**

### 这与第十二轮的记录直接矛盾

第十二轮说「管理员下 `AddProperty` 全部通过（`ACCESS_DENIED` 消失）」，
并且「缺任何一个就报 `ERROR_PATH_NOT_FOUND`」。

但设备节点的 `DEVPKEY_Device_FriendlyName` 是
`Windows Virtual Camera Device`（系统默认名），**不是**我们传的
`SideLens Camera`。

**两种可能**：

1. `AddProperty` 成功但属性写到了**别的设备节点**上（不是这三个之一）
2. `AddProperty` 只写进了 `IMFVirtualCamera` 对象的内存状态，
   在 `Start` 时才尝试落盘，而 `Start` 失败了所以没落

第二种更可能 —— 它同时解释了：
- 为什么 `AddProperty` 不报错（只写内存）
- 为什么设备节点上没有属性（`Start` 失败，没落盘）
- 为什么 `Start` 报 `E_INVALIDARG`（落盘时才发现缺东西）

### 顺带发现：设备的 CreatorProcessId

```text
DEVPKEY_Device_CreatorProcessId = 28388
DEVPKEY_Device_ConfigurationId = c_swdevice.inf:SWD\GenericRaw,SoftwareDevice
DEVPKEY_Device_DriverInfPath   = c_swdevice.inf
DEVPKEY_Device_MatchingDeviceId = SWD\GenericRaw
DEVPKEY_Device_Stack           = \Driver\SoftwareDevice
```

设备栈是 `\Driver\SoftwareDevice` —— **通用软件设备驱动**，
不是摄像头驱动。这印证了第 3 节的结论。

### `{8800D4CA-C1BE-4FB9-B90A-7FFDBB364EA4} 3` 是什么

这是唯一一个**有内容**的自定义属性（200 字节）。解码后：

```text
偏移 20  : 01 00 00 00          （count = 1）
偏移 24  : 45 33 F5 D0 AB 40 C5 4B 8D CC 4A 1C 20 3E 79 72
偏移 40  : CF FD CF 07 FC 0A EC E8 0E EE 64 FF 91 E7 01 CC
偏移 56  : UTF-16 "2D33F5D0AB40C54B8DCC4A1C203E7972CFFDCF07FC0AECE80EEE64FF91E701CC"
```

那串 UTF-16 就是设备实例 ID 的后半段（哈希部分）。
所以这是一个「设备标识」类属性，与虚拟摄像头配置无关。

**不是 `VCamClsid`，也不是 `CustomCaptureSourceClsid`。**

---

## 5. 未文档化的 vcam 常量

`mfsensorgroup.dll` 里有一整套 SDK 里查不到的常量：

| 常量 | 位置 | 说明 |
| --- | --- | --- |
| `KSCATEGORY_FSM_VIRTUAL_CAMERA` | 0x85e40 | FSM = Frame Server Media |
| `DEVPKEY_DeviceInterface_FSM_VirtualCamera` | 0x865a8 | 设备接口属性键 |
| `VCamEvent_CustomEvent` | 0x86620 | 事件类型 |
| `VCamEvent_Initialize` | 0x86638 | |
| `VCamEvent_Start` | 0x86650 | |
| `VCamEvent_Stop` | 0x86660 | |
| `VCamEvent_Uninitialize` | 0x86670 | |
| `VCamEvent_Shutdown` | 0x86688 | |
| `VCamClsid` | 0x87a4e / 0x87c70 / 0x87ea5 | 设备属性名（**不是** `CustomCaptureSourceClsid`！） |

SDK 10.0.26100.0 的 `devpkey.h` / `ks.h` 里全部 0 命中。

### `VCamClsid` 值得注意

它出现在一个**很长的设备属性名列表**里，夹在 `VideoResWidth` / `PhotoResHeight`
和 `ColorInfo` / `ImageReplacement` 之间 —— 那是设备属性（DEVPKEY）的名字表。

而 `CustomCaptureSourceClsid` **不在**这个列表里。

所以真正被 `mfsensorgroup` 读的设备属性名可能是 **`VCamClsid`**，
不是文档里写的 `CustomCaptureSourceClsid`。这需要下一轮验证。

---

## 6. 三个幽灵设备

`sidelensd` 未运行时：

```text
Get-PnpDevice -Class Camera  →  0 个 PRESENT 设备
Enum\SWD\VCAMDEVAPI         →  3 个实例（0010 / 0011 / 0012）
DeviceClasses\{e5323777}    →  3 个接口节点
```

会干扰后续实验对照（分不清「新创建的成功了」还是「旧的还在」）。
需要清理，但清理要管理员权限。

---

## 下一轮候选（按证据强度重排）

| 优先级 | 动作 | 依据 |
| --- | --- | --- |
| 1 | **验证 `VCamClsid` 是否是真正的属性名** | 它在属性名表里，`CustomCaptureSourceClsid` 不在 |
| 2 | 提权 + ProcMon 抓 Shutdown 前的注册表访问 | 唯一能直接看到「它读了什么、没读到什么」 |
| 3 | 反查 `2032C7EF` 的 vtable（`mfsensorgroup.pdb` 符号） | 会话对象接口，拿到方法表就知道要什么 |
| 4 | 查 `KSCATEGORY_FSM_VIRTUAL_CAMERA` 的 GUID 值 | 若能在 DeviceClasses 下建这个类别，可能绕过通用设备类问题 |
| 5 | 清理 3 个幽灵设备 | 干扰实验对照 |
| ~~6~~ | ~~写 `CustomCaptureSourceClsid`~~ | **已证伪** |

### 如果全部失败：接受 DriverPackage 路线

官方示例走的是驱动包路线，我们走的是 API 路线。API 路线在
「设备节点落到 SWD 通用类」这一点上与官方路线有结构性差异。

如果上面 4 条都推不动，应当承认 API 路线在当前 Windows build 上走不通，
转而做 DriverPackage + 桩驱动 —— 代价是需要 EV 签名（或测试签名模式）。
