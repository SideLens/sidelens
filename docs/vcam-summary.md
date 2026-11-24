# 虚拟摄像头 B3：十四轮排查的收敛

> 这一篇是**总览**，把散在 `vcam-pitfalls.md` / `vcam-registry.md` /
> `vcam-sdk-gaps.md` / `vcam-regentry.md` 里的结论收敛成一页。

## 当前状态

`IMFVirtualCamera::Start` 返回 `E_INVALIDARG` (0x80070057)。
FrameServer 走完 13 步（含 `CreatePresentationDescriptor` 成功、流描述符 OK、
`IKsProperty` 成功）后直接 `Shutdown`，**从未调用 `IMFMediaSource::Start`**。

## 已经排除的（都有实证，不是推测）

| # | 猜测 | 排除方式 | 结论 |
| --- | --- | --- | --- |
| 1 | 系统版本不够 | `RtlGetVersion` → build 26300 | ✅ 满足（需 22000+） |
| 2 | COM 没初始化 | 全程 MTA，`MFStartup` 成功 | ✅ |
| 3 | sourceId 格式错 | `CO_E_CLASSSTRING` → 改 CLSID 字符串 | ✅ 已修 |
| 4 | 类工厂拿不到 | `ActivateObject` 成功 | ✅ |
| 5 | `IMFActivate` 属性方法不全 | 30 个方法全实现，`vt=31` 已支持 | ✅ |
| 6 | 缺 `IMFMediaSourceEx` | 已实现 + 8 属性 NV12 类型 | ✅ |
| 7 | 流描述符没媒体类型 | `SetCurrentMediaType` 成功 | ✅ |
| 8 | 缺 `IMFGetService` / `IKsControl` | 已实现，实测被调用 | ✅ |
| 9 | 流 ID 不一致 | 四处统一为 0 | ✅ |
| 10 | HKCU 对 session 0 不可见 | 改 HKLM | ✅ |
| 11 | DLL 目录权限 | ACL 授权 LOCAL SERVICE 读 | ✅ |
| 12 | 媒体源实现不合格 | `--vcam-selfcheck` 绕过 FrameServer 全通过 | ✅ |
| 13 | 缺 `CustomCaptureSourceClsid` | `FrameServer.dll` 里**没有这个字符串** | ❌ **证伪** |
| 14 | 缺 `VCamCreate_*` 属性 | `Get-PnpDeviceProperty` 读出四个全空 | ⚠️ **待定论** |
| 15 | 设备类别错（SWD 通用类） | 实测确实是通用类，不是摄像头类 | ⚠️ **结构性差异** |
| 15b | 应该登记到 `KSCATEGORY_FSM_VIRTUAL_CAMERA` | **两轮顺序反转 A/B，`vc` 组 DLL 加载 / `fsm` 与 `both` 组不加载** | ❌ **证伪（且方向相反）** |
| 16 | 缺 `AddRegistryEntry` 写的注册表项 | **严格 A/B：baseline 与 regentry 完全一致** | ❌ **证伪** |
| 17 | `2032C7EF` 是可选扩展接口 | 它紧跟 `vcam_session-{...}` 格式串，是会话对象 | ⚠️ **很可能是必需** |

## 三个仍然成立的怀疑（按证据强度）

### 1. ~~`2032C7EF` 是必需的事件接收器接口~~（已证伪）

> ## ⚠️ 本条已被实验证伪
>
> 把 `GetService(2032C7EF)` 的返回值从 `MF_E_UNSUPPORTED_SERVICE` 改成
> `E_NOINTERFACE`，FrameServer 的**行为完全不变**：
>
> | 版本 | Start 错误码 | DLL 日志 |
> | --- | --- | --- |
> | `MF_E_UNSUPPORTED_SERVICE`（基线） | `0x80070057` | 13 步完整 |
> | `E_NOINTERFACE`（实验） | `0x80070057` | **完全一样** |
>
> 这两个 HRESULT 语义不同（「知道服务但不提供」vs「不认识这个接口」），
> 而 FrameServer **不区分**它们 —— 说明它只是「拿不到就往下走」，
> 不做分支。
>
> 所以 `2032C7EF` **不是根因**。原假设（「它是必需的事件接收器」）不成立。
>
> 保留下面的证据链，是因为它仍然正确描述了 `2032C7EF` 是什么 ——
> 只是它不是 Start 失败的原因。

#### 原假设（已证伪，保留证据链）

**证据链**：

```text
mfsensorgroup.dll 0x80848  GUID 表
  ← 前一条（0x807C0）: UTF-16 "vcam_session-{%08lX-%04X-..."
  → 它是「虚拟摄像头会话」的接口

SDK 头 mfvirtualcamera.h 第 101-106 行定义了六个事件 GUID：
  MF_FRAMESERVER_VCAMEVENT_EXTENDED_SOURCE_INITIALIZE / START / STOP /
  UNINITIALIZE / PIPELINE_SHUTDOWN / CUSTOM_EVENT

mfsensorgroup.dll 0x86620-0x86688 有六个对应的字符串：
  VCamEvent_Initialize / Start / Stop / Uninitialize / Shutdown / CustomEvent
```

**六个事件一一对应。** FrameServer 通过 `IMFGetService(2032C7EF)` 拿
「事件接收器」，我们返回 `MF_E_UNSUPPORTED_SERVICE` 之后它就不知道
该往哪发 `SOURCE_INITIALIZE` 了。

**这解释了 13 步的形态**：QI 失败后它继续走完后续检查（想找替代路径），
最后没有替代路径就 `Shutdown`。

**验证方法**：实现 `GetService(2032C7EF)` 返回一个实现了
`IMFAsyncCallback`（或 `IMFMediaEventGenerator`）的对象。
IID 已经从二进制里拿到，方法表未知 —— 但可以先用
「返回一个 `IUnknown`，所有方法返回 `E_NOTIMPL`」试探，
看 FrameServer 的下一步行为是否变化。

### 1.5 `KSCATEGORY_FSM_VIRTUAL_CAMERA`：一个从未注册的类别（本轮新发现）

`mfsensorgroup.dll` 0x85e40 有字符串 `KSCATEGORY_FSM_VIRTUAL_CAMERA`，
夹在一串 KS 类别名中间：

```text
0x85de8  KSCATEGORY_VIDEO_CAMERA
0x85e00  KSCATEGORY_SENSOR_CAMERA
0x85e18  KSCATEGORY_FRAMEPROVIDER
0x85e40  KSCATEGORY_FSM_VIRTUAL_CAMERA    ← 这个
0x85e60  KSCATEGORY_VIDEO
0x85e78  KSCATEGORY_CAPTURE
```

**实测我们的设备只注册到了 `KSCATEGORY_VIDEO_CAMERA`**：

```text
Control\DeviceClasses\{e5323777-f976-4f5b-9b55-b94699c46e44}\   ← VIDEO_CAMERA
  3 个 VCAMDEVAPI 接口节点（都在）
```

而 `KSCATEGORY_FSM_VIRTUAL_CAMERA` 对应的 `DeviceClasses` 节点**不存在**。

### 这意味着什么

`MFCreateVirtualCamera` 的 `categories` 参数我们传的是
`KSCATEGORY_VIDEO_CAMERA`（官方示例也这么传）。

但 `mfsensorgroup` 里还有一个专门的 `KSCATEGORY_FSM_VIRTUAL_CAMERA` ——
**如果 FrameServer 只在后者里找虚拟摄像头，那我们的设备永远不在它的视野里**。

这个 GUID 在 SDK 的 `ks.h` / `ksmedia.h` 里**查不到**（实测），
只能从 `mfsensorgroup.dll` 的 GUID 表里反推。

**下一轮最值得做的事**：从 `mfsensorgroup.dll` 0x85e40 附近的 GUID 表里
找出 `KSCATEGORY_FSM_VIRTUAL_CAMERA` 的 GUID，然后用
`AddProperty` / `AddRegistryEntry` 把设备也注册到那个类别下。

#### ✅ GUID 找到了 —— 但假设已被实验证伪

**GUID = `{588C8D20-C0E3-4FD3-B511-8F2F692156F8}`**

##### 怎么找到的（方法本身值得记下来）

前两轮的失败路径：

| 尝试 | 结果 |
| --- | --- |
| 按「字符串表顺序 = GUID 表顺序」猜 | ❌ 错位一个，且槽位 1/2/4/5/7 是垃圾（`{80085DE8-0001-...}` = 指针/重定位数据） |
| 按 RVA 搜字符串引用 | ❌ 0 命中 —— x64 用 `lea rip-relative`，数据段里根本没有绝对指针 |

成功的路径：**`mfsensorgroup.dll` 的 `.rdata` 里有一张 24 字节步长的混合表**，
每项是 `[GUID 16B][名字指针 8B]`：

```text
0x90cd8  {24E552D7-6523-47F7-A647-D3465BF1F5CA}  ->  KSCATEGORY_SENSOR_CAMERA
0x90cf0  {5C7164F5-7E6A-42B6-81F3-68C0B1FAAA0C}  ->  KSCATEGORY_FRAMEPROVIDER
0x90d08  {588C8D20-C0E3-4FD3-B511-8F2F692156F8}  ->  KSCATEGORY_FSM_VIRTUAL_CAMERA
0x90d20  {6994AD05-93EF-11D0-A3CC-00A0C9223196}  ->  KSCATEGORY_VIDEO
0x90d38  {65E8773D-8F56-11D0-A3B9-00A0C9223196}  ->  KSCATEGORY_CAPTURE
0x90d50  {00000000-...}                          ->  image
0x90d68  {00000000-...}                          ->  perception
0x90d80  {00000000-...}                          ->  stream
```

**关键洞察**：后半段（`image`/`perception`/`stream`/`video`/`RGB32`）GUID 全零 ——
那是「媒体类型描述」不是 KS 类别。所以这张表是**混合表**，
**必须靠名字指针确认身份，不能靠位置推**。前两轮就是败在「靠位置推」。

交叉验证：全盘扫 `C:\Windows\System32` 的 4532 个 `.dll`/`.exe`，
这 16 字节只出现在 11 个 FrameServer 相关模块里
（`FrameServer*.dll` ×5、`FsNVSDeviceSource`、`MFCaptureEngine`、`mfcore`、
`MFDshowReverseBridge`、`mfsensorgroup`、`Windows.Media`）。
一个随机猜的 GUID 不会这样。

本机 `Control\DeviceClasses\{588c8d20-...}` 键**存在、ACL 正常、SubKeyCount=0** ——
系统备好了这个类别，没有任何设备登记进去。

##### A/B 实验结果：**没有区别**

给 `MFCreateVirtualCamera` 的 `categories` 参数做了三组对照
（`--category vc|fsm|both`，走完整 `start_full` 同一条代码路径）：

```text
组     Start 错误码    DLL 是否被加载    DeviceClasses(FSM) 节点
vc     0x80070057      是，15 行          0
fsm    0x80070057      否                 0
both   0x80070057      否                 0
```

第一版直接连跑 `vc → fsm → both`，后两组「DLL 未被加载」看起来像因果 ——
但那是**顺序污染**：`Start` 失败后设备记录不自动清除
（`register.rs` 默认 `remove_on_drop=false`），第一组留下的
`SWD\VCAMDEVAPI\<hash>` 幽灵节点类别是写死的 `VIDEO_CAMERA`，后两组复用了它。

修正做法：**每组前用 `pnputil /remove-device` 清幽灵设备，再跑两轮、第二轮顺序反转**。

```text
第一轮 vc → fsm → both        第二轮 both → fsm → vc
  vc   : DLL 加载 15 行          vc   : DLL 加载 15 行
  fsm  : DLL 未加载              fsm  : DLL 未加载
  both : DLL 未加载              both : DLL 未加载
```

两轮完全一致 → **不是顺序效应，是真实差异**。
但差异的方向是「登记到 FSM 类别后 FrameServer 连 DLL 都不加载了」，
即 **FSM 类别让情况变坏，不是变好**。所以：

> **假设 1.5 证伪。`KSCATEGORY_FSM_VIRTUAL_CAMERA` 不是「FrameServer 找虚拟摄像头的地方」。**
>
> 它是**输出侧**的类别（FrameServer 把自己生成的虚拟摄像头**登记到**那里给消费端看），
> 不是**输入侧**的查找条件。我们作为「提供者」登记进去，反而让设备变得不可识别。

保留这个 GUID 常量的价值：`register.rs::fsm_virtual_camera_category()` 有完整推导注释，
将来如果要做「FrameServer 输出 → 我们消费」方向会用到。

### 2. 设备节点落在 SWD 通用设备类（结构性）

```text
Enum\SWD\VCAMDEVAPI\<3 实例>
  ClassGUID        = {62f9c741-b25a-46ce-b54c-9bccce08b6f2}   ← 不是摄像头类
  MatchingDeviceId = SWD\GenericRaw
  DEVPKEY_Device_Stack = \Driver\SoftwareDevice
Control\Class\{e5323777-...}（摄像头类）→ 不存在
```

`MFCreateVirtualCamera` 建的设备被 PnP 匹配到通用软件设备驱动。
官方示例走的是 **DriverPackage + 桩驱动**路线（有 INF 声明
`KSCATEGORY_VIDEO_CAMERA`），我们是 API 路线 —— 这是**结构性差异**。

### 3. `VCamCreate_*` 属性不在设备节点上

`Get-PnpDeviceProperty` 读出 `{6ac1fbf7-...}` 的 pid 4/5/6/7 全部为空。

而 `6ac1fbf7` 这个 GUID 在 **SDK 头文件、`devpkey.h`、本机 10 个 MF DLL、
注册表**里全部 0 命中。

两种解释：
1. `AddProperty` 只写 `IMFVirtualCamera` 对象的内存状态，`Start` 时才落盘 ——
   `Start` 失败所以没落
2. 这组 GUID 是官方示例自造的约定值，系统根本不读

## 一个方法论教训

本轮我犯了两次「把无关变量当成因果」的错：

| 错误 | 真相 |
| --- | --- |
| 以为 `AddRegistryEntry` 让错误码从 `0x80070057` 变成 `0xC00D36BD` | 那个 experiment 手写流程**漏了 4 个 `VCamCreate_*` 属性**，两边不是同一条代码路径 |
| 以为「`AddProperty(IsVirtualCamera)` 失败、`AddProperty(IsWindowsCameraEffectAvailable)` 成功」说明前者被保护 | 这个观察是对的，但**它不影响 `Start`**（严格 A/B 证明） |

**教训**：A/B 的两边必须是同一条代码路径，只有被测变量不同。
手写的简化流程不能当对照组。

## 下一轮优先级

| 优先级 | 动作 | 依据 |
| --- | --- | --- |
| **1** | **提权 + ProcMon 抓 `Start` 期间的注册表/文件访问** | 现在是唯一能**直接看到**它读了什么的手段。前面 18 个假设全是「猜 + 试」，边际收益已经耗尽 |
| **2** | 反查 `2032C7EF` 的 vtable（`mfsensorgroup.pdb` 在线符号） | 拿到方法表才知道「事件接收器」要实现什么。但注意：`GetService` 返回不同 HRESULT 时 FrameServer 行为不变，所以它**不是根因**，只可能是根因的**下游** |
| **3** | 若 1-2 都不成，接受 DriverPackage 路线 | 结构性差异（`SWD\GenericRaw` + `\Driver\SoftwareDevice`）可能无法用 API 绕过 |
| ~~4~~ | ~~实现 `IMFGetService(2032C7EF)` 返回事件接收器~~ | 已证伪（返回 `E_NOINTERFACE` 与 `MF_E_UNSUPPORTED_SERVICE` 行为完全一致） |
| ~~5~~ | ~~写 `CustomCaptureSourceClsid`~~ | 已证伪（`FrameServer.dll` 里没这个字符串） |
| ~~6~~ | ~~`AddRegistryEntry` 写注册表项~~ | 已证伪（严格 A/B） |
| ~~7~~ | ~~登记到 `KSCATEGORY_FSM_VIRTUAL_CAMERA`~~ | 已证伪（本轮，两轮顺序反转 A/B） |

## 一个更大的判断

18 个假设、14 轮排查、每次都是「猜 → 试 → 错误码不变」。
这个模式本身在说一件事：**`E_INVALIDARG` 不是「某个参数不对」，而是「这条路径根本不通」**。

FrameServer 完整走完 13 步（含 `CreatePresentationDescriptor` 成功、
流描述符 OK、`IKsProperty` 成功），**然后主动 `Shutdown`** ——
它不是「检查失败」，是「检查通过了，但我决定不用你」。

这种「全过然后主动退出」的形态，最可能是 FrameServer 内部有一份
「允许的虚拟摄像头来源」白名单，而 API 路线创建的对象不在名单里。
白名单的判据很可能就是设备节点落在 `\Driver\SoftwareDevice` 而非真正的
摄像头驱动栈下 —— 也就是 **DriverPackage 路线**（假设 2 的「结构性差异」）。

**决策建议**：B3 暂停在「已充分排查、结论是需要换路线」，
把剩余时间投入能确定完成的功能（H.264 动态码率、Opus 调优、FEC 跨帧分组、
Android 客户端）。vcam 的 API 路线作为「Win11 已知限制」记档。
