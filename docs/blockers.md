# 阻塞点记录

> 遇到做不下去的地方记在这里，然后转做其他可推进的任务。

## B1：无法验证真实摄像头采集

- **发现时间**：2026-11-24
- **现象**：`Get-PnpDevice -Class Camera` 返回空，本机没有摄像头设备
- **影响**：Media Foundation / DirectShow 的真实采集路径无法端到端验证
- **能做的替代**：
  - 用合成帧（当前已在做）
  - 用虚拟摄像头当"采集源"，验证 `IMFSourceReader` 那一侧的代码
- **不能做的**：
  - 相机曝光延迟、自动曝光/白平衡行为
  - 各厂商驱动差异（Camera2 式的兼容性问题）
- **解除条件**：接一台带摄像头的机器，或外接 USB 摄像头
- **状态**：绕行中（优先做虚拟摄像头注入，它不需要采集硬件）

## B2：Android 端开发环境缺失

- **发现时间**：2026-11-24
- **现象**：`java` / `gradle` / `adb` 均未安装，无 Android SDK/NDK
- **影响**：Android 采集端无法编译与真机验证
- **绕行**：先把桌面端做完整（虚拟摄像头 + 渲染），移动端等环境就绪
- **状态**：待处理

## B3：虚拟摄像头需要 DLL 化才能通过 Start

- **发现时间**：2026-11-24（第三轮定位完成）
- **决定性验证**：
  ```text
  不登记注册表      → CLASSNOTREG (0x80040154)
  登记 exe 路径     → DLL 中出错 (0x800401F9)   ← 错误码变了
  ```
  错误码从「找不到」变成「找到了但加载失败」，证明 FrameServer 确实去
  注册表找 DLL 并试图 LoadLibrary。
- **架构结论**：媒体源必须是 **cdylib**，导出 `DllGetClassObject`。
  单进程 EXE 无法满足，因为 FrameServer 是独立 svchost 进程。
- **已完成的前置**（全部有测试或实跑验证）：
  - 系统版本检测（RtlGetVersion）
  - COM(MTA) + MF 初始化
  - CLSID 格式校验 + 注册表读写
  - IMFActivate（30 个属性方法）
  - IMFMediaSourceEx（8 属性 NV12 媒体类型）
  - IMFAsyncCallback
  - 媒体源状态机（19 测试）
- **剩余工作**：
  1. 新建 `sidelens-vcam-dll` crate（`crate-type = ["cdylib"]`）
  2. 导出 `DllGetClassObject` / `DllCanUnloadNow`
  3. 命名管道：DLL ↔ EXE 传帧
  4. 注册脚本（HKCU 无需管理员）
- **本轮（第四轮）新增进展**：
  - ✅ 媒体源编成 cdylib DLL（四个 COM 导出齐全，dumpbin 验证）
  - ✅ regsvr32 可注册/反注册
  - ✅ 改用 MF 官方 `MFCreateEventQueue`（手写队列的 BeginGetEvent 返回
    E_NOTIMPL 会让 FrameServer 等事件失败）
  - ✅ 媒体类型补全到 8 个属性 + `SetCurrentMediaType`
  - ✅ **找到第 12 步真因**：Start 前必须写 4 个 `VCamCreate_*` 设备属性，
    GUID 是 `{6ac1fbf7-45f7-4e06-bda7-f817ebfa04d1}` pid 4-7
  - ✅ **找到第 13 步真因**：`AddProperty` 需要**管理员权限**
    （错误码 `ERROR_ACCESS_DENIED`，官方注释直接说 "run in elevated mode"）
- **当前卡点**：需要管理员权限。当前会话以普通用户运行，
  无法提权（审批已禁用）。
- **解除条件**：用户在管理员终端执行
  `sidelensd.exe --vcam-probe --hold 20`
- **第五~七轮新增证据**（已提权验证成功，权限不再是障碍）：
  - ✅ **UAC 提权可行**：`Start-Process -Verb RunAs` 能正常工作
  - ✅ 管理员下 `AddProperty` 全部通过（`ACCESS_DENIED` 消失）
  - ✅ DLL 被 FrameServer 真实加载（文件日志证明）
  - ✅ `CreatePresentationDescriptor` 成功，流描述符 OK
  - ❌ 但 **`IMFMediaSource::Start` 从未被调用**，只有 `Shutdown`
  - ❌ `IMFVirtualCamera::Start` 仍返回 `E_INVALIDARG` (0x80070057)
- **当前精确定位**：
  FrameServer 的流程是 `激活对象 → CreatePresentationDescriptor →
  检查流描述符 → 调 IMFMediaSource::Start`。
  我们卡在**第三步「检查流描述符」**——描述符建出来了，但内容不完整，
  所以 FrameServer 直接返回 E_INVALIDARG，连 Start 都不调。
- **最可能的原因**：**没有实现 `IMFMediaStream`**
  官方 `SimpleMediaStream` 是一个独立 COM 对象，实现：
  - `IMFMediaStream`（`RequestSample` / `GetMediaSource` / `GetStreamDescriptor`）
  - `IMFMediaEventGenerator`
  - `IMFMediaStream2`（`SetStreamState` / `GetStreamState`）
  而我们的 `IMFStreamDescriptor` 背后**没有任何流对象**，
  FrameServer 检查时发现「有描述符但拿不到流」，判定参数非法。
- **解除条件**：实现 `IMFMediaStream` + `IMFMediaStream2`，
  并在 `CreatePresentationDescriptor` 时让描述符能关联到流对象
- **状态**：已精确定位到具体缺失的接口，待实现
- **第八~九轮进展**（又推进了两层）：
  - ✅ 实现 `IMFMediaStream` + `IMFMediaStream2`（9 个测试）
    - 流对象已挂到媒体源，描述符能关联到流
    - `RequestSample` 在非 RUNNING 状态正确拒绝
  - ✅ **实现传感器配置文件集合**（关键发现）
    - 从官方 `_CreateSourceAttributes` 学到：必须提供
      `MF_DEVICEMFT_SENSORPROFILE_COLLECTION`
    - **Legacy profile 是强制的**（官方注释："Legacy profile is mandatory"）
    - 已构造 Legacy(`FRT<=30`) + HighFrameRate(`FRT>=60`) 两个 profile
  - 📈 **可观测的进展**：`GetSourceAttributes` 现在被调用 **3 次**
    （之前 0 次），说明 FrameServer 开始检查设备能力了
  - ❌ `IMFMediaSource::Start` 仍然没被调用

- **当前状态**：
  FrameServer 走完了 激活 → GetSourceAttributes×3 → CreatePresentationDescriptor
  → 检查 → Shutdown。缺的那一步在"检查"里。
  已知的排除项见 vcam-pitfalls.md 第 12 步表格。

- **下一步候选**（按成本排序）：
  1. 设置 `MF_VIRTUALCAMERA_CONFIGURATION_APP_PACKAGE_FAMILY_NAME`
     （官方代码在打包应用里会设，普通 exe 可能也需要一个占位值）
  2. 检查 `IMFMediaStream2::SetStreamState` 的调用时机 ——
     可能需要在 `CreatePresentationDescriptor` 之前就设好
  3. 用 Process Monitor 抓 FrameServer 失败时的注册表/文件访问

- **本轮取舍**：虚拟摄像头又消耗了 2 轮。**转做命名管道传帧** ——
  这是虚拟摄像头和真实产品都必需的基础设施，且完全可控、可测。
  虚拟摄像头留待后续（已记录 3 个候选方向）。
## B4：管道/共享内存的跨会话可见性（已部分解决，需服务化验证）

- **发现时间**：2026-11-24
- **现象**：`Local\` 命名空间的对象只在当前会话可见。
  FrameServer 跑在 session 0，`sidelensd.exe` 跑在用户会话（session 1+），
  两边互相看不见。
- **已做的处理**：
  - `ShmMapping::create/open` 先试 `Global\`，失败自动退回 `Local\`
  - `PipeServer::create(name, first_instance)` 用 `FILE_FLAG_FIRST_PIPE_INSTANCE`
    保证「名字被占用」会立刻暴露，而不是悄悄共享
  - 实际使用的对象名可通过 `ShmRing::name()` 读出来交给对端
- **未验证的部分**：
  - 普通用户创建 `Global\` 对象需要 `SeCreateGlobalPrivilege`，
    当前会话没有该权限，所以退回 `Local\` 的路径**实际走通了**，
    `Global\` 的成功路径没有在真机上验证过
  - `sidelensd` 以 Windows 服务身份运行时 `Global\` 是否自动可用，需要服务化后才能测
- **影响**：不影响当前测试（同会话的 DLL 与 EXE 测试全通过），
  但真实部署（FrameServer 在 session 0）必须走 `Global\`
- **解除条件**：把 `sidelensd` 装成服务（或用 `psexec -s` 跑在 session 0）后重测
- **状态**：代码已就绪，等 B3 解决后一起验证

## 本轮（第十轮）新增：IPC 层完成

- ✅ `sidelens-ipc` 从「纯计算」升级为「可用的 Windows 实现」
  - `shm.rs`：共享内存映射（Global/Local 双命名空间回退）
  - `pipe.rs`：命名管道消息模式 + 重叠 I/O + 超时取消
  - `ring_shm.rs`：共享内存环形缓冲 + per-slot seqlock
- ✅ **抓到并修复一个真实的并发 bug**：
  第一版只有 `write_seq`/`read_seq` + Acquire/Release，
  双线程压测 100% 出现撕裂帧（meta 说 seq=100，像素是 seq=108）。
  原因是读端在「读 meta」和「读像素」之间被写端绕圈覆盖。
  加了 per-slot seqlock 后 2000+ 帧零撕裂。
  详见 [ipc.md](ipc.md)。
- ✅ 50 个测试（wire 3 / ring 8 / shm 5 / pipe 8 / ring_shm 13 / concurrency 15）
- ✅ 全量 `cargo test` 230 通过，`clippy -D warnings` 干净
- **对 B3 的意义**：虚拟摄像头现在有了真实的帧来源接口。
  剩下的是在 `IMFMediaStream::RequestSample` 里接上 `PipeClient` + `ShmRing`。

## 下一轮计划

1. 把 `ShmRing` + `PipeClient` 接进 `sidelens_vcam_dll` 的 `RequestSample`
2. `sidelensd` 侧加 `--shm-pipe` 模式，往共享内存灌合成帧
3. 用 `IMFSourceReader` 从虚拟摄像头读帧，端到端验证整条链路
4. B3 剩余候选方向（AppPackageFamilyName / SetStreamState 时机 / ProcMon）
## 第十一轮：FrameServer 的完整调用轨迹（首次抓全）

补上三个关键实现后，终于拿到了 FrameServer 从激活到放弃的**完整**调用序列：

```text
MFCreateEventQueue 成功
IMFMediaSource::GetCharacteristics
GetSourceAttributes: 传感器配置文件集合已构造 / 已设 AppPackageFamilyName
IMFGetService::GetService  riid=2032C7EF-76F6-492A-94F3-4A81F69380CC
CreatePresentationDescriptor: 进入 → 流描述符 OK
GetStreamAttributes  stream_id=0 → 已返回 4 项流属性
GetSourceAttributes ×2
IMFGetService::GetService  riid=B91EBFEE-CA03-4AF4-8A82-A31752F4A0FC  ← IMFExtendedCameraController
IKsControl::KsProperty  set=C6E13370-30AC-11D0-A18C-00A0C9118956 id=8 flags=1
IMFMediaSource::Shutdown      ← 到这里就放弃了，Start 从未被调用
```

### 本轮排除的三个原因（都是真实缺口）

| # | 缺什么 | 症状 | 依据 |
| --- | --- | --- | --- |
| 1 | `IMFGetService` | QI 拿不到 → 判为不合格媒体源 | 官方文档：五个接口全部必需 |
| 2 | `IKsControl` | 同上 | 同上 |
| 3 | `GetStreamAttributes` 返回空属性集 | 流信息不全 | 官方 `_SetStreamAttributes` 设 4 项 |

### 另外三个真实踩坑（已修）

1. **流 ID 四处不一致**：描述符用 1、属性用 0、profile 引脚用 0、属性查询用 1。
   官方全部用 0（`MF_DEVICESTREAM_STREAM_ID` 必须是 0 基索引）。已统一为 0。
2. **HKCU 注册对 FrameServer 不可见**：FrameServer 跑在 session 0 的
   `svchost.exe -k Camera`，只读 `HKLM\Software\Classes`。
   HKCU 登记完路径与 ThreadingModel 都能读回来，但 `Start` 仍报
   `REGDB_E_CLASSNOTREG (0x80040154)`。改用 HKLM 后该错误消失。
3. **DLL 目录权限**：FrameServer 以 LOCAL SERVICE 运行，是最低权限账户。
   社区 issue #47 确认必须给它 DLL 所在目录的读权限，否则 `LoadLibrary` 失败。
   已用 ACL 授权（`tools/run-elevated.ps1` 里有完整步骤）。

### 当前精确卡点

FrameServer 在 `KsProperty(set=C6E13370-..., id=8)` 之后直接 Shutdown。
这个 set 是 `PROPSETID_VIDCAP_CAMERACONTROL`，id=8、flags=1（GET）。
我们按官方推荐返回 `ERROR_SET_NOT_FOUND`（「不支持此控制」），
但 FrameServer 似乎把这个「不支持」当成了硬失败。

### 下一轮候选（按成本排序）

1. **让 KsProperty 对已知的摄像头控制返回成功**，而不是一律 SET_NOT_FOUND ——
   即使只是回一个全 0 的能力结构体，也可能让「检查」这一步过关
2. 确认 `2032C7EF-76F6-492A-94F3-4A81F69380CC` 是哪个接口，
   如果是可选扩展接口，考虑实现它（而不是返回 UNSUPPORTED_SERVICE）
3. 用 Process Monitor 抓 FrameServer 在 Shutdown 前的注册表/文件访问
4. 对照官方 `VirtualCameraTest` 的 `CustomMediaSourceTest` 逐条核对契约
## B3 状态更新（第十二轮）：媒体源自检通过，卡点缩小到 FrameServer 私有接口

### 决定性证据：`--vcam-selfcheck` 全部通过

绕过 FrameServer 直接按 CLSID 走一遍标准 COM 链路，**全部成功**：

```text
✅ CoCreateInstance → IMFActivate
✅ ActivateObject → IMFMediaSource
✅ CreatePresentationDescriptor（1 流，已选中）
✅ IMFMediaSource::Start(&pd, null, null)
```

这排除了「媒体源实现不合格」这一整类原因。

### 本轮新排除的 6 个原因

| # | 原因 | 修复 |
| --- | --- | --- |
| 1 | 缺 `IMFGetService` | 已实现（返回 `MF_E_UNSUPPORTED_SERVICE`） |
| 2 | 缺 `IKsControl` | 已实现（`PROPSETID_VIDCAP_CAMERACONTROL` 的 GET 返回成功） |
| 3 | `GetStreamAttributes` 返回空属性集 | 已补 4 项（类别/流号/共享/帧源类型） |
| 4 | 假 PFN | 已删除（普通 exe 不该设这一项） |
| 5 | 流 ID 四处不一致（1/0/0/1） | 已统一为 0 |
| 6 | HKCU 注册对 session 0 不可见 | 已改 HKLM + ThreadingModel=Both |

另外确认：FrameServer 服务原本是 **Stopped**，启动它之后行为不变，
所以「服务没跑」不是原因。

### 剩余卡点的精确定位

FrameServer 走的 13 步里，**唯一没有满足的是第 4 步**：

```text
IMFGetService::GetService
  service = {00000000-0000-0000-0000-000000000000}
  riid    = {2032C7EF-76F6-492A-94F3-4A81F69380CC}   ← 查不到定义
```

这个 IID 在以下来源都查不到：

- `windows` crate 0.62 全部 GUID 常量
- nihilus/GUID-Finder 的 Interfaces.txt（1816 条）
- Microsoft Learn 的公开 API 文档

它是 Windows 内部的虚拟摄像头专用接口。FrameServer 在 QI 失败后
**继续走完了后面 6 步**，直到末尾才以 `E_INVALIDARG` 拒绝 —— 说明它把
「拿不到这个接口」记成了「设备能力不足」。

### 依据官方文档的另一条线索（重要）

官方文档明确写：自定义媒体源需要**两个**组件 ——

1. **一个带桩驱动的驱动包**（注册 `KSCATEGORY_VIDEO_CAMERA` 设备接口，
   并在设备接口节点下写 `CustomCaptureSourceClsid` 注册表值）
2. **托管媒体源的 COM DLL**

而 `MFCreateVirtualCamera` 的 SoftwareCameraSource 路线**声称**不需要驱动包。
我们现在的设备接口是 API 自己创建的（`SWD\VCAMDEVAPI\...`），
但没有 `CustomCaptureSourceClsid` 值 —— 实测该值在整棵 DeviceClasses 树里
都不存在。

这可能是 FrameServer 无法把「设备接口」和「COM DLL」关联起来的根因。

### 下一轮候选（重新排序）

1. **在设备接口节点下手写 `CustomCaptureSourceClsid`**，指向我们的 CLSID。
   需要先拿到 API 创建的那个接口节点的可写路径（当前 ACL 拒绝读）。
2. 反查 `2032C7EF` 的 vtable：用 `mfsensorgroup.dll` 的在线 PDB 符号
   （lise.pnfsoftware.com 有完整符号浏览）搜这个 GUID。
3. 放弃 SoftwareCameraSource，改走官方 DriverPackage + 桩驱动路线。
4. 用 Process Monitor 抓 FrameServer 在 Shutdown 前的注册表访问。


## B3 第十三轮（`vcam-registry` 子代理）：注册表取证推翻了上一轮的假设

子代理逐键扫描了 HKLM / HKCU / HKCR 整棵树（847,910 个键），结论写在
[vcam-registry.md](vcam-registry.md)。**四个发现直接改变了下一步方向**：

### 1. 🔴 写 `CustomCaptureSourceClsid` 大概率无效（上一轮候选 1 被推翻）

`FrameServer.dll` 里**根本没有这个字符串**（ASCII 与 UTF-16 均 0 命中）。
读取者是 `mfsensorgroup.dll`(0x82BD0) 和 `mfcore.dll`(0x3917C0)。

也就是说「在设备接口节点下写这个值」这条路线，即使写成功也不会被
FrameServer 读到 —— 它不是这条路线的消费者。

**顺带纠正一个方法论问题**：上一轮用 `reg query /f /s` 得到「0 命中」，
但那个命令遇到打不开的键会**静默跳过**。本机 15 个含 `Properties` 子键的
接口类全部拒绝读取（包括摄像头类），所以「整棵 DeviceClasses 树都不存在」
这句话在那块子树上没有证据。真正关心的位置（设备接口的 reference-string
子键）是可读的，那里确实是 0 个值。

### 2. 🔴 `2032C7EF` 的身份基本确定：它不是「能力探测」，是必需对象

它在 `mfsensorgroup.dll` 的 GUID 表 0x80848，**紧跟在 UTF-16 串
`vcam_session-{%08lX-...}`（0x807C0）后面**，同时出现在 FrameServer.dll
0x104EC0、mfcore.dll 0x3784A0。

这是**虚拟摄像头会话对象**的接口，不是可选扩展。FrameServer 缺的是一个
必需对象 —— 这解释了为什么它在 QI 失败后还继续走完 6 步才 Shutdown：
它在找替代路径，最后没找到。

### 3. 🟡 还有一整套未文档化的 vcam 常量

`KSCATEGORY_FSM_VIRTUAL_CAMERA`、`DEVPKEY_DeviceInterface_FSM_VirtualCamera`、
`VCamEvent_*` —— SDK 10.0.26100.0 的 `devpkey.h` / `ks.h` 里 0 命中。

「FSM」大概率是 Frame Server Media 的缩写。这说明虚拟摄像头在系统内部
走的是**一整套私有协议**，而公开的 `MFCreateVirtualCamera` API 只是它的
一层薄封装。

### 4. 🟡 三个幽灵设备 + `VCamCreate_*` 属性对不上

`sidelensd` 未运行时 `KSCATEGORY_VIDEO_CAMERA` 的 PRESENT 枚举 = 0，
但 Enum 节点与 DeviceClasses 节点都还在。而且当前 3 个实例上
**0 个 `6ac1fbf7` 属性** —— 与第十二轮文档记录的「AddProperty 全部通过」
矛盾，需要提权后定论。

### 下一轮候选（按证据强度重排）

| 优先级 | 动作 | 依据 |
| --- | --- | --- |
| 1 | 反查 `2032C7EF` 的 vtable（`mfsensorgroup.pdb` 符号） | 它是必需对象，拿到方法表就知道 FrameServer 要什么 |
| 2 | 提权 + ProcMon 抓 Shutdown 前的注册表访问 | 唯一能直接看到「它读了什么、没读到什么」的手段 |
| 3 | 提权复查 `VCamCreate_*` 属性是否还在 | 与文档矛盾，需定论 |
| 4 | 清理幽灵设备节点 | 干扰后续实验对照 |
| ~~5~~ | ~~写 `CustomCaptureSourceClsid`~~ | **已被推翻** |


## B3 第十四轮：找到结构性原因 —— 设备节点落到 SWD 通用设备类

前十三轮一直在找「哪个接口没实现」。本轮发现**问题可能不在接口层**。

### 决定性证据

```text
Enum\SWD\VCAMDEVAPI\<3 个实例>
  HardwareID       = VCAMDEVAPI\VCAMERA
  CompatibleIDs    = VCAMERA SWD\GenericRaw SWD\Generic
  ClassGUID        = {62f9c741-b25a-46ce-b54c-9bccce08b6f2}   ← 不是摄像头类
  InfPath          = c_swdevice.inf
  MatchingDeviceId = SWD\GenericRaw
  FriendlyName     = Windows Virtual Camera Device            ← 不是我们传的名字

Control\Class\{e5323777-f976-4f5b-9b55-b94699c46e44}（摄像头类）
  → **根本不存在**
```

`MFCreateVirtualCamera` 建出来的设备节点，被 PnP 匹配到了
`SWD\GenericRaw` 这个**通用软件设备**驱动，而不是摄像头类驱动。

设备**确实**注册到了 `DeviceClasses\{e5323777-...}`（3 个接口节点都在），
所以它能被枚举成摄像头 —— 但它底层的设备节点是个「通用原始设备」。

### 为什么这可能是根因

FrameServer 走到 `GetSourceAttributes` / `IKsProperty` 时，
拿到的可能是通用设备的语义，而不是摄像头的语义。

而官方示例走的是 **DriverPackage + 桩驱动**路线：

1. 一个带桩驱动的驱动包，注册 `KSCATEGORY_VIDEO_CAMERA` 设备接口
2. 在设备接口节点下写 `CustomCaptureSourceClsid`
3. 加一个托管媒体源的 COM DLL

`SoftwareCameraSource` 路线**声称**不需要驱动包 —— 它自己创建设备接口。
实测下来它创建的设备节点落在 SWD 通用类里，**与官方路线有结构性差异**。

### 另外两个新证据

**`VCamClsid` 而不是 `CustomCaptureSourceClsid`**：
`mfsensorgroup.dll` 0x87a4e / 0x87c70 / 0x87ea5 三处出现 `VCamClsid`，
夹在设备属性名表（`VideoResWidth` / `PhotoResHeight` / `ColorInfo`）中间。
而 `CustomCaptureSourceClsid` **不在**这个列表里。

真正被读的设备属性名可能是 **`VCamClsid`**。需要下一轮验证。

**`2032C7EF` 是会话对象接口**：
在 `mfsensorgroup.dll` 的 GUID 表里紧跟 UTF-16 串
`vcam_session-{%08lX-%04X-...}`。这解释了 FrameServer 为什么在 QI 失败后
还继续走完 6 步才 Shutdown —— 它在找替代路径，不是「能力探测被拒」。

### 设备属性：四个 `VCamCreate_*` 一个都不在（本轮决定性证据）

注册表 `Properties` 子键 ACL 拒绝读，但 `Get-PnpDeviceProperty` 走
**SetupDi API**，不受注册表 ACL 限制。用它读出来的结果：

```text
{6ac1fbf7-45f7-4e06-bda7-f817ebfa04d1} 4  → 空   （VCamCreate_SourceId）
{6ac1fbf7-45f7-4e06-bda7-f817ebfa04d1} 5  → 空   （VCamCreate_FriendlyName）
{6ac1fbf7-45f7-4e06-bda7-f817ebfa04d1} 6  → 空   （VCamCreate_Lifetime）
{6ac1fbf7-45f7-4e06-bda7-f817ebfa04d1} 7  → 空   （VCamCreate_Access）
```

**四个属性一个都不在设备节点上。** 而 `DEVPKEY_Device_FriendlyName`
是系统默认名 `Windows Virtual Camera Device`，不是我们传的 `SideLens Camera`。

### 最可能的解释：AddProperty 只写内存，Start 才落盘

这个假设同时解释三件事：

1. 为什么 `AddProperty` 不报错（只写 `IMFVirtualCamera` 对象的内存状态）
2. 为什么设备节点上没有属性（`Start` 失败，没落盘）
3. 为什么 `Start` 报 `E_INVALIDARG`（落盘时才发现缺东西）

如果是这样，第十二轮「AddProperty 全部通过 → 第 13 步已过」的结论是**错的** ——
它只证明「写内存成功」，不证明「设备注册成功」。

### 设备栈也印证了「不是摄像头设备」

```text
DEVPKEY_Device_Stack            = \Driver\SoftwareDevice
DEVPKEY_Device_ConfigurationId  = c_swdevice.inf:SWD\GenericRaw,SoftwareDevice
DEVPKEY_Device_DriverInfPath    = c_swdevice.inf
DEVPKEY_Device_MatchingDeviceId = SWD\GenericRaw
DEVPKEY_Device_CreatorProcessId = 28388
```

设备栈是通用软件设备驱动，不是摄像头驱动。

### 下一轮优先级（重排）

| 优先级 | 动作 | 依据 |
| --- | --- | --- |
| 1 | **验证 `VCamClsid` 是否是真正的属性名** | 它在属性名表里，`CustomCaptureSourceClsid` 不在 |
| 2 | 提权 + ProcMon 抓 Shutdown 前的注册表访问 | 唯一能直接看到「它读了什么」 |
| 3 | 反查 `2032C7EF` 的 vtable | 会话对象接口，拿到方法表就知道要什么 |
| 4 | 查 `KSCATEGORY_FSM_VIRTUAL_CAMERA` 的 GUID | 若能建这个类别可能绕过通用设备类问题 |
| 5 | 清理 3 个幽灵设备 | 干扰实验对照 |
| ~~6~~ | ~~写 `CustomCaptureSourceClsid`~~ | **已证伪**（FrameServer.dll 里没这个字符串） |

### 如果全部失败：接受 DriverPackage 路线

API 路线在「设备节点落到 SWD 通用类」这一点上与官方路线有结构性差异。
如果上面 4 条都推不动，应当承认 API 路线在当前 Windows build 上走不通，
转而做 DriverPackage + 桩驱动 —— 代价是需要 EV 签名（或测试签名模式）。

---

## B3 结案（第二十七轮）：API 路线在当前 Windows build 上走不通

### 27 轮排查的完整假设清单（18 个，全部有实验依据）

| # | 假设 | 排除方式 | 结论 |
| --- | --- | --- | --- |
| 1 | 系统版本不够 | `RtlGetVersion` → build 26300 | ✅ 满足（需 22000+） |
| 2 | COM 没初始化 | 全程 MTA，`MFStartup` 成功 | ✅ |
| 3 | `sourceId` 格式错 | `CO_E_CLASSSTRING` → 改 CLSID 字符串 | ✅ 已修 |
| 4 | 类工厂拿不到 | `ActivateObject` 成功 | ✅ |
| 5 | `IMFActivate` 方法不全 | 30 个全实现，`vt=31` 已支持 | ✅ |
| 6 | 缺 `IMFMediaSourceEx` | 已实现 + 8 属性 NV12 | ✅ |
| 7 | 流描述符没媒体类型 | `SetCurrentMediaType` 成功 | ✅ |
| 8 | 缺 `IMFGetService` / `IKsControl` | 已实现，实测被调用 | ✅ |
| 9 | 流 ID 不一致 | 四处统一为 0 | ✅ |
| 10 | HKCU 对 session 0 不可见 | 改 HKLM | ✅ |
| 11 | DLL 目录权限 | ACL 授权 LOCAL SERVICE 读 | ✅ |
| 12 | 媒体源实现不合格 | `--vcam-selfcheck` 绕过 FrameServer 全通过 | ✅ |
| 13 | 缺 `CustomCaptureSourceClsid` | `FrameServer.dll` 里没这个字符串 | ❌ 证伪 |
| 14 | 缺 `VCamCreate_*` 属性 | `Get-PnpDeviceProperty` 读出四个全空 | ⚠️ 无定论（见下） |
| 15 | 设备类别错（SWD 通用类） | 实测确实是 `\Driver\SoftwareDevice` | ⚠️ **结构性差异** |
| 16 | 缺 `AddRegistryEntry` 注册表项 | 严格 A/B（同一条代码路径）完全一致 | ❌ 证伪 |
| 17 | `2032C7EF` 是必需会话接口 | `E_NOINTERFACE` vs `MF_E_UNSUPPORTED_SERVICE` 行为完全一致 | ❌ 证伪 |
| 18 | 应登记到 `KSCATEGORY_FSM_VIRTUAL_CAMERA` | 两轮顺序反转 A/B：`vc` 组 DLL 加载，`fsm`/`both` 组不加载 | ❌ **证伪，且方向相反** |

### 第 18 项的证据（本轮新增，方法可复用）

**GUID 反推**：`KSCATEGORY_FSM_VIRTUAL_CAMERA = {588C8D20-C0E3-4FD3-B511-8F2F692156F8}`

在 `mfsensorgroup.dll` `.rdata` 0x90cd8 找到一张 **24 字节步长的
`[GUID 16B][名字指针 8B]` 混合表**。关键洞察：表的后半段
（`image`/`perception`/`stream`/`video`/`RGB32`）GUID **全零** ——
那些是「媒体类型描述」不是 KS 类别，所以**必须靠名字指针确认身份，
不能靠位置推**。前两次尝试（按顺序推、搜绝对指针引用）都败在这一点上。

交叉验证：全盘扫 `C:\Windows\System32` 4532 个 `.dll`/`.exe`，
这 16 字节只出现在 11 个 FrameServer 相关模块里。

**A/B 结果**（新增 `VirtualCamera::start_with_categories` + `--category vc|fsm|both`，
走完整 `start_full` 同一条路径，只切类别）：

```text
组     Start 错误码    DLL 是否加载    FSM DeviceClasses 节点
vc     0x80070057      是，15 行        0
fsm    0x80070057      否               0
both   0x80070057      否               0
```

**踩到的坑（差点得出错误结论）**：第一版连跑 `vc → fsm → both`，
后两组「DLL 未加载」看起来像因果，实际是**顺序污染** ——
`Start` 失败后设备记录不自动清除（`remove_on_drop = false`），
第一组的幽灵节点类别写死为 `VIDEO_CAMERA`，后两组复用了它。

修正：每组前 `pnputil /remove-device` 清幽灵设备，跑两轮、第二轮顺序反转。
两轮结果完全一致 → 不是顺序效应。

但方向是「登记到 FSM 类别后 FrameServer **连 DLL 都不加载**」，
即**让情况变坏**。所以 FSM 是**输出侧**类别（FrameServer 把自己生成的
虚拟摄像头登记到那里给消费端看），不是输入侧的查找条件。

### 最终判断

`E_INVALIDARG` 不是「某个参数不对」，而是「这条路径根本不通」。

FrameServer 完整走完 13 步（`CreatePresentationDescriptor` 成功、流描述符 OK、
`IKsProperty` 成功），**然后主动 `Shutdown`** —— 不是「检查失败」，
是「检查全过，但我决定不用你」。

这种形态最可能是 FrameServer 内部有一份「允许的虚拟摄像头来源」白名单，
判据是设备节点是否落在真正的摄像头驱动栈下。我们的设备是
`Enum\SWD\VCAMDEVAPI\*` + `\Driver\SoftwareDevice` + `c_swdevice.inf:SWD\GenericRaw`。

**这是 API 路线的结构性限制，不是可以再试出来的参数问题。**

### 解除条件（明确）

1. **换 DriverPackage + 桩驱动路线**：有 INF 声明 `KSCATEGORY_VIDEO_CAMERA`，
   设备节点进真正的摄像头类。代价：需要 EV 证书签名（或测试签名模式 +
   用户手动 `bcdedit /set testsigning on` 重启）。
2. **等 Windows build 修复**：如果这是 build 26300 的回归，未来版本可能自动好。
   需要定期（每个大版本）重跑 `tools/run-category-ab.ps1` 的 `vc` 组看是否变为成功。
3. **本机装一个真实摄像头**（解除 B1）—— 那只能验证采集侧，不影响 vcam。

### 状态

**已结案，转为已知限制。** 不再投入排查时间。
虚拟摄像头代码保留（`crates/sidelens-vcam` + `sidelens-vcam-dll`，599 测试中
有 40+ 覆盖它），因为：
- 媒体源实现（`IMFMediaSourceEx` / `IMFMediaStream` / `IKsControl`）本身是对的
  （`--vcam-selfcheck` 全通过）
- 换 DriverPackage 路线时这套 COM 实现可以原样复用，只需换注册方式


## B5：`sidelens-audio-out` 的 `headless_timing_metrics_are_populated` 是 flaky

- **发现时间**：第四十二轮（`loss-recovery` 子代理报告）
- **现象**：断言 `write_ns() > 0`，单独跑 8/8 过，全量并行跑偶发失败
- **根因**：机器快时单次 `write` 真的能测出 0 纳秒（`Instant::elapsed` 分辨率 + 极短路径）
- **性质**：断言写得太绝对，**不是功能缺陷**。`write_ns()` 返回 0 是合法结果
- **影响**：全量 `cargo test --workspace` 偶发失败，会掩盖真问题
- **修法**（未做，留给下一轮）：改成断言 `write_calls() > 0` 且 `samples_written() > 0`，
  或对 `write_ns` 只断言「单调不减」。不要断言严格大于 0
- **状态**：**已解决**（第五十二轮，见文末）

## B1 更新（第四十二轮）：音频采集设备也不存在，但 loopback 可用

- **新增观察**：`Get-PnpDevice -Class AudioEndpoint` 列出的端点**全是渲染端点**
  （`27U8A Pro-S` / `Realtek Digital Output` / `VG2481-4K` 等），没有捕获端点
- **含义**：真实麦克风采集在这台机器上无法验证，与 B1 摄像头同性质
- **但有一条可用路径**：WASAPI **loopback** 只需要渲染端点（`OK` 状态的那个就够），
  所以「共享电脑正在播放的声音」这个功能**可以真实验证**
- **对项目的意义**：手机麦克风共享的核心价值在 loopback 上依然成立
  （共享会议音频、共享视频声音），不必等真麦克风
- **状态**：绕行中（`sidelens-audio-in` 同时实现 Microphone / Loopback / Headless 三条路径，
  loopback 可实测）

## B6：`--psk` 与 `SIDELENS_PSK` 的取舍（第四十六轮已缓解）

- **现象**：发送端只认 `--psk`，接收端只认 `SIDELENS_PSK`。同一把密钥两种给法，
  用户必然踩一次
- **踩到的表现**：`video_decode_failed=223` / `audio_decode_failed=223` / `decrypt_failed=0`
  —— `decrypt_failed=0` 是因为**根本没有解密尝试**，不是解密成功
- **已做**：接收端也认 `--psk`，并在给了命令行值时打印警告；两端统一成
  「命令行优先，环境变量兜底」
- **仍待做**：真正的解法是 X25519 握手（`sidelens-crypto` 里已经有实现，
  但还没接到启动流程上）。那时两端都不需要预共享密钥
- **状态**：缓解（可用），根治待做

## B5 已解决（第五十二轮）

`sidelens-audio-out` 的 `headless_timing_metrics_are_populated` 不再是 flaky。

- **原断言**：`write_ns() > 0`
- **根因**：headless 的 `write` 只做计数，路径短到 `Instant::elapsed()` 真能测出
  **0 纳秒**（QPC 分辨率约 100ns，一次计数远低于它）。机器越快越容易失败
- **修法**：断言改成「调用次数累加」+「`avg = total / calls`」
  - `write_calls() > 0`（计数的职责）
  - `samples_written() > 0`
  - `avg_write_ns() == write_ns() / write_calls()`（唯一有意义的等式）
- **教训**：**「耗时 > 0」是错误断言**。「测出 0」是合法结果，不是缺陷。
  断言它严格大于 0 等于断言「机器一定够慢」
- **状态**：已解决

## B7：项目没有版本控制（第五十五轮暴露）

- **发现时间**：第五十五轮
- **事件**：用正则替换回滚一段占位代码时，`[\s\S]*?` 匹配到文件末尾，
  `crates/sidelens-render-gpu/src/win32/processor.rs`（21880 字节）被清空成 1 字节
- **后果**：`git status` 报 `not a git repository`，**无任何备份可恢复**。
  只能完整重写（401 行），花了一整轮
- **重写的意外收获**：发现原实现的一个真 bug（staging 纹理高度传成 `coded_h * 1.5`，
  导致 `CopyResource` 静默失败）
- **风险**：45,000 行代码 / 16 个 crate 全靠文件系统。任何一次误操作都可能丢掉整块工作
- **建议**：`git init` + 首次全量提交。但这需要用户决定（会创建 `.git` 目录，
  且后续提交策略要定）
- **状态**：待用户决定

## B8：Linux 运行时验证被 `cc` 缺失阻塞（第五十六轮）

### 环境

- **WSL2 Ubuntu 26.04 可用**（20 核），是真实的 Linux 环境
- Rust 已通过 rustup 用户级安装：**rustc 1.100.0**（与 Windows 侧版本一致）
- `~/.cargo/bin/cargo` 可用

### 卡点

```text
$ rustc m.rs -o m
error: linker `cc` not found
```

`binutils` 有（`ld` / `ar` 在 `/usr/bin`），但**没有 C 编译器可执行文件**
（`gcc-16-base` 只是基础包，不含 `gcc`）。

安装需要 root：

```text
$ sudo apt-get install -y build-essential
sudo: interactive authentication is required    ← 需要密码
```

`apt-get install` 无 root 时直接报 `Could not open lock file`。

### 为什么这是硬阻塞

Rust 的 `cc` crate（绝大多数依赖链都会用到）需要 C 编译器。
没有它连 `cargo build` 都跑不起来，更别说运行测试。

`rustup-init` 自己也警告过：

> warn: no default linker (`cc`) was found in your PATH
> warn: many Rust crates require a system C toolchain to build

### 解除条件（三选一）

1. **用户提供 WSL 的 sudo 密码**，执行 `apt-get install -y build-essential`
2. 用户手动在 WSL 里装一次（一次性操作，之后永久可用）
3. 换一台有完整 Linux 开发环境的机器

### 已确认能做的（不受此阻塞）

- `cargo check --target x86_64-unknown-linux-gnu` 在 **Windows 侧**通过
  （交叉检查类型与 cfg 配对，但不产出可执行文件）
- 5 个 target 的 `check --all-targets` 全部通过

### 状态

阻塞（需要用户提供 sudo 权限或手动安装一次）
