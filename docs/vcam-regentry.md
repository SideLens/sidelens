# 虚拟摄像头：`AddRegistryEntry` 实验（决定性突破）

> 实验代码：`crates/sidelens-vcam/examples/registry_entry.rs`
> 提权脚本：`tools/run-regentry.ps1`
> 日志：`target/vcam-regentry.log`

## 背景

前十四轮的错误码一直是 `E_INVALIDARG` (0x80070057) —— 一个**完全没有信息量**的错误。
它可能是参数错、状态错、缺东西、权限错，什么都有可能。

本轮实验让错误码变成了 **`0xC00D36BD` (MF_E_INVALIDTYPE)** ——
这是一个**具体得多**的错误：媒体类型不合法。

## 实验设计与结果

`IMFVirtualCamera::AddRegistryEntry` 是 SDK 定义但官方示例从未使用的 API。
它往**设备接口节点**写注册表值，由系统代写（不需要自己处理 ACL）。

| # | 写法 | 结果 |
| --- | --- | --- |
| 1 | `IsVirtualCamera = "1"`（REG_SZ，SubkeyPath = null） | ❌ `E_INVALIDARG` (0x80070057) |
| 2 | `pid 3` 写在 `Properties\{6EDC630D-...}` 下（REG_BINARY，DEVPROPTYPE 前缀） | ✅ **成功** |
| 3 | `VCamClsid = <我们的 CLSID>`（REG_SZ，null subkey） | ✅ **成功** |
| 4 | `CustomCaptureSourceClsid = <我们的 CLSID>`（对照） | ✅ 成功（但已知无效） |
| 5 | `IsVirtualCamera = 1`（REG_DWORD） | ✅ **成功** |

**5 个里成功 4 个。** 只有第 1 个失败 —— 因为 REG_SZ 的 `"1"` 与
`DEVPROP_TYPE_BOOLEAN` 的类型不匹配。

### 关键：`Start` 的错误码变了

```text
实验前：Start 失败: 参数错误。(0x80070057)        ← E_INVALIDARG，无信息量
实验后：Start 失败: 无效类型。(0xC00D36BD)        ← MF_E_INVALIDTYPE，指向具体问题
```

**这是十四轮以来第一次拿到有指向性的错误码。**

## 为什么这是突破

### 1. 证明 `AddRegistryEntry` 确实能写进去

第 2、3、5 条成功说明这个 API 工作正常，而且**不需要管理员权限做额外处理**
（提权是必需的，但 ACL 由系统代为处理）。

### 2. 证明「写注册表」这件事确实影响了 FrameServer 的行为

如果 `AddRegistryEntry` 只是「写了个没人读的值」，`Start` 的错误码不会变。
错误码从「泛型参数错」变成「类型错」，说明 FrameServer 走到了**更后面的一步**。

### 3. `MF_E_INVALIDTYPE` 指向什么

项目文档 `vcam-pitfalls.md` 第 16 行记录过同一个错误码：

> | 7 | `0xC00D36BD` | MF_E_INVALIDTYPE (vt=31) | 不支持 VT_LPWSTR | 加字符串变体 |

那次是 `PROPVARIANT` 的 `vt` 类型不被接受（`VT_LPWSTR` 要改成 `VT_UI4`）。

**这次大概率是同一类问题，但发生在不同位置**：某个我们设的属性类型不对。
最可疑的是第 2 条（`pid 3` 用 REG_BINARY + DEVPROP_TYPE 前缀）——
DEVPROP 的注册表表示有特定格式，可能少写了什么。

### 4. 第 5 条（REG_DWORD）与第 2 条（REG_BINARY）都「成功」但含义不同

`AddRegistryEntry` 的返回值只说明「注册表写成功」，
不说明「系统能读懂这个格式」。

两个都返回成功，但只有一个可能是对的 —— **需要看实际写进去的字节**。

## 正确的 A/B 结果：`AddRegistryEntry` 对 `Start` 毫无影响

走 `VirtualCamera::start()` 的完整流程（含 4 个 `VCamCreate_*` 属性 +
2 个官方接口属性），只切换 `--regentry` 开关：

| 模式 | Start 错误码 | FrameServer 的 13 步轨迹 |
| --- | --- | --- |
| `baseline`（不写注册表项） | `0x80070057` | 完整 13 步 |
| `--regentry`（写 IsVirtualCamera + VCamClsid） | `0x80070057` | **完全一样** |

**错误码、DLL 日志的 13 步、每一步的返回值全部一致。**

结论：**`AddRegistryEntry` 写了注册表项，但对 `Start` 没有任何影响。**

### 这意味着什么

`AddRegistryEntry` 确实能写（前面实验证明 4/5 成功），
但写进去的值**不是 FrameServer 在 `Start` 时读的东西**。

FrameServer 拒绝这个媒体源的原因**不在注册表里**。

---

## 一个被自己推翻的结论（重要）

### 我最初以为：错误码变化是 `AddRegistryEntry` 引起的

**错。**

第一次实验设了 5 个注册表项，错误码从 `0x80070057` 变成 `0xC00D36BD`，
我以为是那些注册表项起了作用。

随后做了严格的 A/B：`none` / `iscam_dword` / `iscam_binary` /
`vcamclsid` / `customclsid` 五种模式单独跑。

结果：

```text
模式 none            → Start 失败 0xC00D36BD
模式 iscam_dword     → Start 失败 0xC00D36BD
模式 iscam_binary    → Start 失败 0xC00D36BD
模式 vcamclsid       → Start 失败 0xC00D36BD
模式 customclsid     → Start 失败 0xC00D36BD
```

**连「什么都不设」都是 `0xC00D36BD`。**

### 真正的原因：这个 experiment 漏了 4 个必需的属性

`sidelens-vcam` 的 `VirtualCamera::start()` 会先调
`add_device_properties()` 设 4 个 `VCamCreate_*` 属性（SourceId / FriendlyName /
Lifetime / Access），**然后**才 `Start`。

而 `registry_entry.rs` 这个 experiment 是**手写的**，只调了
`MFCreateVirtualCamera` + `AddRegistryEntry` + `Start`，
**跳过了那 4 个属性**。

所以 `0xC00D36BD` 是「缺 `VCamCreate_*` 属性」的错误码，
不是「`AddRegistryEntry` 起了作用」。

### 这个错误很有教育意义

我第一次实验拿到的错误码变化，**把「实验代码与生产代码的差异」
误当成了「我改动的东西的效果」**。

正确的对照应该是「`VirtualCamera::start()` 的完整流程 ± 新增的注册表项」，
而不是「一个手写的简化流程」。

**教训**：A/B 的两边必须是**同一条代码路径**，只有被测变量不同。
我的两边差着 4 个属性设置 —— 那不是 A/B，那是两个不同的实验。

---

## 下一步（精确）

### 1. 读回注册表，看实际写了什么

实验脚本的注册表检查部分**什么都没打印**（说明写的路径不在我检查的位置）。
需要：

```powershell
# 用 SetupDi 读设备接口属性（而不是猜注册表路径）
Get-PnpDeviceProperty -InstanceId 'SWD\VCAMDEVAPI\<新实例>' \
    -KeyName '{6EDC630D-C2E3-43B7-B2D1-20525A1AF120} 3'
```

### 2. 逐个属性做 A/B

现在的实验是「一次设 5 个」，看不出是哪个起了作用。
必须**逐个单独测**：

| 只设这个 | Start 错误码 |
| --- | --- |
| 只设 `IsVirtualCamera`（REG_DWORD） | ? |
| 只设 `VCamClsid` | ? |
| 只设 `CustomCaptureSourceClsid` | ? |
| 都不设（基线） | 0x80070057 |

### 3. 若 `MF_E_INVALIDTYPE` 来自属性类型

对照 `vcam-pitfalls.md` 第 7 条的修法（`VT_LPWSTR` → `VT_UI4`），
检查我们所有 `AddProperty` 的类型是否与 `DEVPKEY` 声明的类型一致：

| 属性 | SDK 声明类型 | 我们用的 |
| --- | --- | --- |
| `IsVirtualCamera` | `DEVPROP_TYPE_BOOLEAN` | ✅ BOOLEAN |
| `IsWindowsCameraEffectAvailable` | `DEVPROP_TYPE_BOOLEAN` | ✅ BOOLEAN |
| `VirtualCameraAssociatedCameras` | `DEVPROP_TYPE_STRING_LIST` | 未设 |
| `VCamCreate_SourceId` | 未知（SDK 无定义） | STRING |
| `VCamCreate_FriendlyName` | 未知 | STRING |
| `VCamCreate_Lifetime` | 未知 | INT32 |
| `VCamCreate_Access` | 未知 | INT32 |

**`VCamCreate_*` 那四个的类型是「猜的」**（因为 SDK 没有定义）。
如果系统期望的类型不同，就会报 `MF_E_INVALIDTYPE`。

### 4. 用 Process Monitor 抓 Start 期间的注册表读取

这是唯一能直接看到「FrameServer 读了哪个键、期望什么类型」的手段。

## 一个反直觉的观察

`AddProperty(IsVirtualCamera, BOOLEAN, 1)` 在**管理员权限下**仍然返回
`ACCESS_DENIED`，而同一次调用的 `AddProperty(IsWindowsCameraEffectAvailable,
BOOLEAN, 0)` **成功**。

同一 GUID、同一 DEVPROPTYPE、同一权限 —— 一个成功一个失败。

说明 `IsVirtualCamera` **被系统保护**，用户态不能通过 `AddProperty` 设。
必须走 `AddRegistryEntry`（第 2/5 条证明这条路能写）。

这解释了为什么官方示例代码里找不到 `IsVirtualCamera` 的设置 ——
它可能由系统在 `Start` 成功后自己写。
