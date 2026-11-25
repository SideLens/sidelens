# DirectShow 虚拟摄像头

> 第二十轮。`MFCreateVirtualCamera` 路线经 19 个假设 + 13 组实验证伪后，
> 改用 DirectShow 源滤镜。**首次注册即在系统摄像头列表里出现。**

## 为什么放弃 MFCreateVirtualCamera

第十九轮的六组 A/B（`EnableFrameServerMode` 不存在 / =1 / =0，顺序反转跑两轮）
**全部返回同一个错误码**：

```text
[SideLens] ActivateObject 成功: 3C9B2EB9-...
[SideLens]   查询到 IMFMediaSource，characteristics=0x1
ERROR Start: 无效类型。 (0xC00D36BD)     ← MF_E_INVALIDTYPE
```

七种调用组合（lifetime × access × categories × callback）也全部同一个码。
DLL 侧日志显示 FrameServer 走完 13 步，**从未调用 `GetMediaTypeByIndex`**，
然后直接 `Shutdown`。

结论：`E_INVALIDARG` / `MF_E_INVALIDTYPE` 不是「某个参数不对」，是「这条路径不通」。
FrameServer 内部有一份白名单，判据是设备节点是否落在真正的摄像头驱动栈下 ——
API 路线建的设备是 `SWD\GenericRaw` + `\Driver\SoftwareDevice`，不在名单里。

## DirectShow 路线

| 维度 | MFCreateVirtualCamera | DirectShow 源滤镜 |
| --- | --- | --- |
| 注册方式 | API 调用 | `regsvr32` |
| 需要驱动 | 否 | 否 |
| 需要签名 | 否 | 否 |
| 最低系统 | Win11 22000+ | 全版本 |
| 权限 | 管理员（每次启动） | 管理员（仅注册时） |
| 生命周期 | 进程退出即消失 | 注册后常驻 |
| 兼容性 | 部分应用 | OBS 用了十几年 |

## 架构

```text
sidelensd (生产者)                本 DLL (消费者)
─────────────────                ─────────────────
采集 → 编码 → NV12 帧
  ↓ 命名管道（帧元数据）
  ↓ 共享内存环（像素）
                                   FrameSource
                                     ↓ poll
                                   OutputPin::fill_sample
                                     ↓
                                   消费端（Zoom/OBS/浏览器）
```

传帧层（`sidelens-ipc` 的 `PipeClient` + `ShmRing`）与虚拟摄像头那轮**完全共用** ——
那部分不依赖任何虚拟摄像头 API，seqlock 防撕裂与命名空间回退都已在第十轮验证过。

## 关键实现

### 1. 回调模式（不是推模式）

DirectShow 有三种驱动方式，摄像头类设备走**回调模式**：

| 模式 | 驱动方 | 说明 |
| --- | --- | --- |
| 推模式 | 源滤镜起线程调 `Receive` | 需要自己控节拍 |
| 拉模式 | 下游调 `SyncReadAligned` | 文件源用 |
| **回调模式** | 下游调 `FillBuffer` | **摄像头用这个** |

下游协商完格式后自己起线程按帧率调 `FillBuffer`。上游只需把当前帧填进去。

### 2. `FillBuffer` 必须永远填满

返回 `S_OK` 但不写缓冲区，消费端会拿到未初始化内存 —— 花屏或绿屏。
所以 `FrameSource::poll` 保证**永远返回一帧**：没有新帧就重复上一帧，
从没拿到过就返回全黑（Y=16, UV=128，limited range 的合法黑）。

这个「Y 必须是 16 不能是 0」的坑在第三十轮踩过（见 progress.md 已知坑第 30 条）。

### 3. 两层注册表登记

```text
HKLM\SOFTWARE\Classes\CLSID\{9E4B7C31-5A2D-4F8E-B1C6-3D7A0E5F92B4}
  └─ InprocServer32
       (Default)      = <DLL 完整路径>
       ThreadingModel = Both

HKLM\SOFTWARE\Classes\CLSID\{860BB310-5D01-11D0-BD3B-00A0C911CE86}
  └─ Instance\SideLens Camera
       CLSID        = {9E4B7C31-...}
       FriendlyName = SideLens Camera
```

第一层是 COM 类登记，第二层是**类别登记** ——
`{860BB310-5D01-11D0-BD3B-00A0C911CE86}` 就是 `CLSID_VideoInputDeviceCategory`。
少了第二层，`CoCreateInstance` 能成功但应用枚举不到。

**必须写 HKLM 不能写 HKCU**：消费端是任意进程，而 UAC 下提权进程的 HKCU
与普通进程的 HKCU 不是同一个。这个坑在虚拟摄像头那轮踩过。

### 4. `GetModuleFileNameW` 必须传模块句柄

第一版写的是 `GetModuleFileNameW(None, ...)` —— 它返回**调用进程的主模块**。
`regsvr32` 加载 DLL 后调 `DllRegisterServer`，此时主模块是 `regsvr32.exe`，
于是 `InprocServer32` 被写成 `C:\Windows\system32\regsvr32.exe`。
注册表看起来「成功」了，但 `CoCreateInstance` 永远拿不到滤镜。

正确做法：`GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, <本函数地址>, ...)`
反查所属模块。回归测试 `module_path_points_at_our_dll_not_the_host_process` 守住这一点。

### 5. `AM_MEDIA_TYPE` 在 MediaFoundation 模块下

`windows crate` 把 `AM_MEDIA_TYPE` / `VIDEOINFOHEADER` / `PIN_INFO` 挂在
`Win32::Media::MediaFoundation`（不是 `DirectShow`）—— 因为 MF 复用了这些结构。
第一次编译会全部 `unresolved import`，照报错改即可。

### 6. `IBaseFilter` 的 trait 继承链

```text
IBaseFilter ← IMediaFilter ← IPersist ← IUnknown
```

`#[implement]` 必须写全四个，状态机方法（`Stop`/`Pause`/`Run`/`GetState`）
在 `IMediaFilter` 里，`GetClassID` 在 `IPersist` 里。
只写 `IBaseFilter` 会报 `method is not a member of trait`。

## 用法

```powershell
# 编译
cargo build --release -p sidelens-dshow

# 注册（管理员）
regsvr32 /s target\release\sidelens_dshow.dll

# 验证
target\release\sidelensd.exe --dshow-probe

# 反注册
regsvr32 /u /s target\release\sidelens_dshow.dll
```

## 实测结果

```text
=== DirectShow 滤镜验证 ===

[1/3] 枚举 CLSID_VideoInputDeviceCategory
       @device:sw:{860BB310-...}\Lens
       @device:sw:{860BB310-...}\Lens 2
    ✅ @device:sw:{860BB310-...}\SideLens Camera
       @device:sw:{860BB310-...}\{A3FCE0F5-...}
    共 4 个视频捕获设备
    → SideLens Camera 出现在类别里

[2/3] CoCreateInstance(CLSID_SIDELENS_DSHOW)
    ✅ 创建成功
    ClassID = 9E4B7C31-5A2D-4F8E-B1C6-3D7A0E5F92B4
    初始状态 = FILTER_STATE(0)（0=Stopped）

[3/3] IBaseFilter::EnumPins
    引脚 1: 名字="Output" 方向=1（输出）
    ✅ 共 1 个引脚

结论：滤镜已注册且可创建 —— 可以在应用里选「SideLens Camera」
```

## 测试

20 个单元测试，覆盖：

| 分类 | 数量 | 内容 |
| --- | --- | --- |
| 媒体类型 | 4 | NV12 几何、biHeight 含 UV、30fps、格式块分配 |
| 帧来源 | 4 | NV12 长度、limited range、非全零、兜底不返回空 |
| 滤镜 | 4 | CLSID 稳定、初始 Stopped、状态迁移、引脚枚举 |
| 注册 | 4 | CLSID 字符串、宽字符串、类别 GUID、模块路径 |

## 已知限制

| 限制 | 说明 |
| --- | --- |
| 分辨率固定 1280x720 | 尚未接 `IAMStreamConfig::SetFormat` 的动态协商 |
| 只给 NV12 | 官方示例给 RGB32 + NV12，部分老应用可能只认 RGB32 |
| UWP 应用看不到 | DirectShow 是桌面 API，UWP 走 MF 需要 DShow 桥接 |
| 未接真实帧源 | `FrameSource` 已就绪，但 `sidelensd` 侧还没开始往管道灌帧 |

## OBS 崩溃排查（第二十一轮补）

### 现象

在 OBS 里「添加 → 视频捕获设备」，**OBS 直接崩溃**。

崩溃日志：

```text
Unhandled exception: c0000005
Fault address: 7FF8DB910836 (d:\software\obs-studio\core\win-dshow\win-dshow.dll)
obs.dll!obs_source_properties+0x87
obs.exe!OBSPropertiesView::ReloadProperties+0x81
```

OBS 自己的日志：

```text
视频采集设备: DecodeDeviceId failed
视频采集设备: Video configuration failed
```

### 四个真 bug

#### 1. `VIDEO_STREAM_CONFIG_CAPS` 全零（致命）

`IAMStreamConfig::GetStreamCaps` 的 `pscc` 输出参数我传了全零。

消费端拿到 `MinOutputSize = MaxOutputSize = 0`，做边界检查时得到
「宽度必须在 0 到 0 之间」—— 随后崩溃。

**修法**：填真实值，`Min == Max == 实际尺寸`，粒度 1，帧间隔上下限都给。

#### 2. `QueryPinInfo` 返回空指针（致命）

DirectShow 规范要求 `PIN_INFO.pFilter` 指向**拥有该引脚的滤镜**，
且调用方负责 Release（所以实现里要 AddRef）。

第一版填了 `null`。OBS 读这个字段一解引用就是 `c0000005`。

**修法**：引脚持有 `Option<IBaseFilter>`，`QueryPinInfo` 返回它。
因为引脚是在滤镜构造过程中创建的（此时 `ComObject<Self>` 还不存在），
用「先占位创建 → 滤镜建好 → 回填」两步走。

#### 3. `set_string` 的悬垂指针

```rust
// 错的
let name_w = if name.is_empty() { PCWSTR::null() } else { PCWSTR(wide(name).as_ptr()) };
```

看起来对，但 `wide(name)` 返回的 `Vec<u16>` 是**临时值**，
`as_ptr()` 拿到的指针在语句结束时就悬垂了。

表现：注册表里 `FriendlyName` 消失，设备名变成了键名
（`SideLens Camera REG_SZ SideLens Camera`）。

**修法**：把 `Vec` 绑到局部变量，让它在调用期间存活。

#### 4. `IEnumMediaTypes` 读已释放内存

枚举器持有裸 `AM_MEDIA_TYPE`，其 `pbFormat` 是 `CoTaskMemAlloc` 分配的。
但那个 `AM_MEDIA_TYPE` 本身没人管，`Next` 里再 `from_raw_parts` 读它
是读已释放内存。

表现：`EnumMediaTypes` 枚举到 **0 个类型**。

**修法**：枚举器存 `OwnedMediaType`（内部 `Vec<u8>`），
`Next` 时按需 `to_raw()` 分配新的给调用方。

### 5. 缺 `FilterData` 注册表项

对照本机正常设备（Lens）和 OBS Virtual Camera，它们的 `Instance` 子键里
除了 `CLSID` / `FriendlyName`，还有二进制 `FilterData` ——
这是滤镜序列化的配置，声明支持的格式。

缺失时 OBS 报 `DecodeDeviceId failed`。

**修法**：写入 `FilterData`，内容是 `AM_MEDIA_TYPE` 序列化骨架，
尾部 16 字节是 `MEDIASUBTYPE_NV12` 的 GUID。

### 验证方法

新增 `--dshow-grab` 模式，**完整模拟 OBS 的枚举行为**：

```text
=== 从 SideLens Camera 拉帧（10 帧）===
✅ 滤镜已创建
✅ 找到输出引脚

--- 模拟 OBS 的格式枚举 ---
✅ QueryInterface(IAMStreamConfig) 成功
   GetNumberOfCapabilities: count=1 size=128
   [0] major=73646976-... sub=3231564E-... cbFormat=88 lSampleSize=1382400
        caps: MinOutput=1280x720 MaxOutput=1280x720 MinFrameInterval=333333
        ✅ caps 有效

--- 枚举媒体类型 ---
   枚举到 1 个类型
   [0] major=73646976-... sub=3231564E-... cbFormat=88

✅ 滤镜已 Run
   状态: FILTER_STATE(2)（2 = Running）
```

这个模式复刻了 OBS 会调的每一个方法，比「打开 OBS 试一下」更快也更准 ——
OBS 崩溃时拿不到任何堆栈之外的线索，而这个模式能直接指出是哪一步返回了非法值。

### 教训

**「能创建、能 Run」不等于「消费端能用」。** 第一版验证只做到了
`CoCreateInstance` + `EnumPins` + `Run`，全部通过 —— 但 OBS 一碰就崩。
真正的验证必须覆盖**消费端实际会调的每一个方法**，尤其是那些
「有输出参数、返回值被下游拿去算」的方法（`GetStreamCaps` / `QueryPinInfo`）。
