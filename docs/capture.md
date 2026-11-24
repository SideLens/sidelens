# 屏幕采集层（`sidelens-capture`）

> 产出 **NV12 + 16 对齐尺寸**，与 `sidelens-codec-mf` 的编码器输入直接对接。

## 三个后端

| 后端 | 实现 | 4K p50 | 什么时候用 |
| --- | --- | --- | --- |
| `Dxgi` | Desktop Duplication + staging texture | **29.62 ms** | 正常交互式会话（默认） |
| `Gdi` | `BitBlt` + `CreateDIBSection` | **66.42 ms** | 远程桌面 / 会话 0 / 老显卡 / 输出已被占用 |
| `Synthetic` | 纯 CPU 画图案 | **12.16 ms** | 剥离平台成本，做基线对照 |

（各 100 帧，release 构建，3840x2160）

### 为什么 GDI 仍然是必需的

DXGI 的 Desktop Duplication 在下列环境**拿不到**：

1. 远程桌面（RDP 会话）
2. 会话 0（服务）
3. 老显卡 / 驱动不支持
4. 输出已被别的进程复制（Desktop Duplication 同一时刻只能有一个消费者）

`BitBlt` 从 Win3.1 就有，任何交互式 Windows 会话都能跑。它是「至少能出图」的底线。
所以 `ScreenCapture::auto()` 会先试 DXGI，失败自动退回 GDI 并把原因返回给调用方。

## 性能分解：转换才是大头

```text
DXGI       4K  p50  22-30 ms       ← 平台调用 + 转换
Synthetic  4K  p50  12.05 ms       ← 纯 CPU，不碰任何平台 API
差值               10.21 ms        ← CopyResource + Map 的净成本
```

单独测纯转换（不含任何平台调用）：

```text
纯 BGRA→NV12 转换成本：
    640x360  p50   0.45 ms  p95   0.48 ms  （353280 字节 NV12）
  1920x1080  p50   4.12 ms  p95   4.59 ms  （3133440 字节 NV12）
  3840x2160  p50  15.48 ms  p95  16.16 ms  （12441600 字节 NV12）
```

**4K 下转换是 15.48ms，占 DXGI 总耗时（22-30ms）的一半以上。**
平台调用（CopyResource + Map）只占 10.21ms。

这个分解直接指出优化方向：**转换要 SIMD 化或搬到 GPU**，而不是优化 DXGI 调用。

1080p 下转换是 4.12ms，而端到端实测的 `capture_avg_us` 是 24.7ms ——
差了 20ms，说明**那 20ms 是等 `AcquireNextFrame` 的时间**，不是真实成本。
见下面「`capture_avg_us` 的疑点」。

## 五个实现要点

### 1. coded size 16 对齐：填充而不是留黑

屏幕 1920x1080，H.264 的 coded size 是 1920x1088。多出来的 8 行必须**填充**。

留黑会让编码器把底部黑边真的编进码流，接收端看到「画面底部有一条黑带」。
填充（复制最后一行）则不会 —— 那些行的内容与第 1080 行相同，
编码器用极少的比特就能表示。

### 2. BT.709，不是 BT.601

接收端 `sidelens-render` 的 `Matrix::Bt709` 是配套实现，两端必须一致。

不一致的后果是**颜色整体偏移**，而且这种偏移在单端测试里完全看不出来 ——
编码端和解码端各自都「对」，只有端到端对比原始画面才能发现。

### 3. NV12 的 UV 是交错的

是 `U0 V0 U1 V1...` 一个平面，不是 U、V 两个独立平面。

写成两个平面能编译、能跑，但**画面会变成绿色** —— 因为亮度对、色度全错。

### 4. DXGI 的 `AcquireNextFrame` 超时是正常返回

桌面没变化时返回 `DXGI_ERROR_WAIT_TIMEOUT`，必须翻成 `Ok(None)` 而不是 `Err`。

而且每次 Acquire 后**必须** ReleaseFrame，否则下一次 Acquire 永远失败
（`DXGI_ERROR_INVALID_CALL`），表现为「第一帧之后再也拿不到画面」。

### 5. DXGI 输出纹理不能直接 Map

必须 `CopyResource` 到一个 CPU 可读的 staging texture，再 `Map`。
这一步的拷贝成本已经算进上面的 17.46ms 里了。

## 发送端怎么接（第四十八轮）

`--screen <dxgi|gdi|off>`，默认 `off`。

**默认关掉的理由**：屏幕采集是有副作用的 —— DXGI 占 GPU 拷贝队列，
GDI 让桌面合成器做全屏读回。所有历史基线都在合成源下测的，
默认值一改那些数字全部失去可比性。

### 尺寸从编码器来，不从命令行来

```rust
// 编码器的 coded size 是 16 对齐的（1920x1080 -> 1920x1088），
// 采集必须产出同一个尺寸，否则 encode_nv12_into 会因长度不符报错。
```

### 桌面静止时：复用上一帧，不要跳过

| 处理 | 后果 |
| --- | --- |
| 跳过这一帧不编码 | 时间线出现空洞（PTS 跳跃），解码端看到帧率抖动 |
| **复用上一帧 NV12 继续编码** | 正常的静止画面，帧率稳定 |

选后者。这也是屏幕共享的常规做法：静止桌面仍然按帧率推流，只是内容不变。

## 端到端实测（1080p，30fps）

```text
发送端：
  sent=240  keyframes=4  avg_fps=29.9  encode_frames=240
  encode_avg_us=3270            ← 编码 3.27ms
  capture_avg_us=24733          ← 采集 24.7ms（**当前最大瓶颈**）
  capture_empty=1               ← 只有 1 次「桌面没变化」

接收端：
  video_decoded=197  video_decode_failed=0
  render_frames=197  render_failed=0
  decode_us=4949     kbps=862     playable_ms=24.38/30.92
```

### `capture_avg_us=24733` 的疑点

30fps 帧间隔 33.3ms，采集吃 24.7ms（74%），编码 3.3ms（10%），
**留给网络与调度只剩 5.3ms**。

但 4K 基准才 29.6ms，1080p 按比例应该约 8ms。24.7ms 对不上，说明
`capture_avg_us` 里含了别的东西：

1. `AcquireNextFrame` 的**等待**时间被算进采集耗时（桌面静止时要等满超时）
2. staging texture 的 `Map` 在等 GPU 完成拷贝

如果是第 1 条，采集应该改成「非阻塞轮询 + 复用上一帧」而不是「阻塞等一帧」，
那样 30fps 下采集耗时应该接近 0。**下一轮第一件要查的事。**

## 已知缺口

| 缺口 | 影响 |
| --- | --- |
| `bench_capture --size` 没被接收 | 只有 4K 基准数字，没有 1080p/720p 的 |
| 脏矩形未利用 | 每次全量拷贝。DXGI 的 `TotalMetadataBufferSize` 非 0 时只更新了局部，全量拷贝多付了带宽 |
| BGRA→NV12 是标量实现 | 4K 下 12ms，是最大单项成本。SIMD 化或 GPU shader 能降一个数量级 |
| 只支持主显示器 | `CaptureSource::Monitor(n)` / `Window(hwnd)` 已定义但未实测 |
| 没有光标 | Desktop Duplication 的光标是单独返回的（`PointerPosition`），当前忽略了 |

## 六个实测坑（不是推演出来的）

### 坑 1：DPI 虚拟化让 GDI 和 DXGI 报不同分辨率

进程默认 DPI-unaware 时，Windows 把显示器坐标虚拟化 —— 4K 屏 + 200% 缩放时：

| 来源 | 报的分辨率 |
| --- | --- |
| `EnumDisplayMonitors` / `BitBlt`（GDI） | 1920x1080（虚拟化后） |
| DXGI 的 `ModeDesc` | 3840x2160（真实） |

后果：**「DXGI 失败降级到 GDI」这条路径直接坏掉** —— 编码器按 4K 配好，
降级后帧变成 1080p，每帧都尺寸不符。

修法：两个后端构造函数里都先调 `SetProcessDpiAwarenessContext(PER_MONITOR_AWARE_V2)`，
失败不算错误（宿主可能已设过，返回 `ACCESS_DENIED`）。

### 坑 2：DXGI 首帧一定是全黑的假帧

`DuplicateOutput` 之后第一次 `AcquireNextFrame` 会**成功返回**，但带的是占位帧：

```text
LastPresentTime == 0
AccumulatedFrames == 0
纹理格式是 R16G16B16A16_FLOAT（不是 B8G8R8A8_UNORM）
内容全 0
```

不判这一条，第一帧会被拷成 Y 恒为 16 的纯黑送进编码器 ——
而 **`min == max == 16` 恰好是合法的全黑帧**，下游完全看不出异常。

修法：`LastPresentTime == 0` 直接返回 `Ok(None)`；另外每帧校验纹理格式必须是
`B8G8R8A8_UNORM`（`CopyResource` 对格式不匹配是**静默失败**，不返回错误）。

### 坑 3：NV12 缓冲的初值不能是全 0（接线时踩的）

坑 2 的连锁后果。采集层正确返回 `Ok(None)`，编码层正确「复用上一帧」，
但**第一帧复用到的就是初始缓冲** —— 如果它是 `vec![0u8; n]`：

| 平面 | 全 0 的含义 | 应该是 |
| --- | --- | --- |
| Y | full-range 的黑，limited range 下是「比黑更黑」的非法值 | 16 |
| UV | 色度 = 0，彻底错的色度 | 128（中性） |

结果是画面偏绿且色度块状。必须用 `fill_black_nv12` 填成合法的 limited-range 黑。

**这个 bug 的形态值得记：两个各自正确的组件拼在一起出了错。**

### 坑 4：`zip` 链在 4K 上反而更慢

三层嵌套 `zip` 拆 BGRA 字节流：1080p 快了 25%，**4K 慢了 20%**。
巨大的 tuple 类型让 LLVM 在 4K 循环上放弃了向量化。
改回 `chunks_exact(4)` + 常量下标两边都稳。

### 坑 5：`D3D11CreateDevice` 的 driver type

传了 adapter 时 driver type 必须是 `D3D_DRIVER_TYPE_UNKNOWN`，
传 `HARDWARE` 返回 `E_INVALIDARG` 且不提原因。

### 坑 6：`windows` crate 没给 `RECT` 实现 `Eq`

所以 `MonitorDesc` 只能 derive `PartialEq`。小事，但会让「用 HashMap 缓存显示器信息」写不出来。

## 验证数据：`dump_frame` 的 Y 平面统计

```text
GDI   3840x2160  Y min=16 max=235 mean=46.04  判定：正常（有动态范围）
DXGI  3840x2160  Y min=16 max=235 mean=63.64  判定：正常（有动态范围）
```

`Y min=16 / max=235` 正是 limited-range 的完整范围 —— 这同时证明了：

1. BT.709 转换系数正确（错了 min/max 会超出或不到边界）
2. 16 对齐填充正确（如果填 0 或 255，min/max 会露出来）
3. 没有额外的增益/偏移

## 缩放（第五十轮）

采集层支持在 BGRA 域缩放到目标尺寸，**和色彩转换在同一个遍历里做**。

```rust
pub fn new_dxgi_sized(source: CaptureSource, target: Option<(u32, u32)>) -> Result<Self>;
pub fn auto_sized(source: CaptureSource, target: Option<(u32, u32)>) -> (Self, Option<CaptureError>);
pub fn set_target_size(&mut self, size: Option<(u32, u32)>) -> Result<()>;  // 会重建后端
pub fn source_size(&self) -> Option<(u32, u32)>;
pub fn scale_mode(&self) -> Option<ScaleMode>;
```

| 目标 | 模式 | p50 | 输出字节 |
| --- | --- | --- | --- |
| 3840x2160 | 无缩放 | 15.61 ms | 12,441,600 |
| 1920x1080 | 面积平均 2:1 | **14.44 ms** | 3,133,440 |
| 1280x720 | 面积平均 3:1 | **8.29 ms** | 1,382,400 |
| 1920x1081 | 最近邻 | 7.29 ms | 3,133,440 |

**2:1 降采样比不缩放更快** —— 读的源数据一样多，写的目标数据只有 1/4。

### 为什么不能「先转 NV12 再缩放」

NV12 的 UV 已经是 2x2 下采样。先转再缩放会让 UV 多一次降采样（总共 4x），
颜色会糊。正确做法是**在 BGRA 域缩放到目标尺寸，再做一次标准的 2x2 UV 下采样**。

### 为什么必须单遍

第一版 `scale_area_y` + `scale_area_uv` 分开跑，UV 那遍要把 33MB 源数据**再读一次**。
实测 4K→1080p **22.3ms**，比不缩放的 15.5ms 还慢 —— 完全违背「写的目标只有 1/4」的直觉。
融合成单遍（按目标 2x2 块遍历，一次产出 4 个 Y + 1 对 UV）后降到 **14.4ms**。

### 端到端实测（4K 屏 → 三档目标）

| `--size` | 缩放模式 | `capture_avg_us` | `encode_avg_us` | `kbps` | 接收端 |
| --- | --- | --- | --- | --- | --- |
| 1280x720 | AreaAverage 3:1 | **11,154** | 1,778 | 2,144 | 181 / 0 失败 |
| 1920x1080 | AreaAverage 2:1 | **18,376** | 3,365 | 2,019 | 180 / 0 失败 |
| 3840x2160 | 无缩放 | **23,134** | 13,636 | **15,184** | 145 / 0 失败 |

**最有价值的一列是 `kbps`：** 4K 不缩放要 15.2 Mbps，降到 1080p 只要 2.0 Mbps
（**1/7**），而 `capture_avg_us` 只从 23.1ms 降到 18.4ms。

对屏幕共享来说，**降分辨率是最高性价比的优化** —— 带宽收益远大于延迟收益。

## 一个只在非 16 倍数分辨率下暴露的 bug

接线时我把 `EncoderConfig::width/height`（**显示**尺寸）当成 coded size 传给采集层，
采集层又按**显示**尺寸做了一次 16 对齐：

```text
1920x1080 → 采集层算出 coded 1920x1088 → Frame.coded_height = 1088
          → 与 cfg.height = 1080 比对 → 永远不符
```

**为什么 720p 完全正常：** 720 恰好是 16 的倍数，`align16(720) == 720`，
对齐是恒等操作。只有 1080/1440/2160 这类分辨率才暴露。

修法三处：

1. 目标尺寸传**显示**尺寸，让采集层自己对齐（这是它的职责）
2. 比对时两边都 `(x + 15) & !15`
3. NV12 缓冲长度按 `layout.nv12_len()`（coded）算，不是 `cfg.nv12_len()`（display）

**这个 bug 的形态值得记：两个组件的「尺寸」语义不同，但类型都是 `u32`。**
一个说「显示尺寸」、一个说「编码尺寸」，编译器完全无法区分。

## 已知缺口

| 缺口 | 影响 | 修法 |
| --- | --- | --- |
| **`capture_avg_us` 混合了等待和处理** | 看它判断不了瓶颈 | 拆成 `capture_wait_us` / `capture_process_us` |
| 非整数倍缩放走最近邻 | 细文字和高对比边缘有锯齿 | `ScaleMode` 加 `AreaFractional` 变体 |
| `set_target_size` 会重建后端 | DXGI 重建要 10-30ms，期间丢帧 | 构造时定尺寸（`new_dxgi_sized`） |
| DXGI 旋转屏未处理 | 竖屏显示器给的是未旋转的横向画面 | 按 `DXGI_MODE_ROTATION` 旋转或明确拒绝 |
| 脏矩形未实现 | 4K 下 PCIe 流量约 500 MB/s | 用 `TotalMetadataBufferSize` 判部分更新 |
| 没有光标 | Desktop Duplication 的光标单独返回（`PointerPosition`），当前忽略 | 合成光标到 NV12 |
| 窗口采集只有 GDI | DXGI 的 Desktop Duplication 按输出复制，没有单窗口能力 | API 限制，已返回明确的 `UnsupportedSource` |
| 缩放只在 CPU 做 | DXGI 有 GPU 路径（`ID3D11VideoProcessor` 可直出 NV12） | 省不掉 GPU→CPU 回读（5-7ms），收益不明确 |
