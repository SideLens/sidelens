# GPU 渲染层（`sidelens-render-gpu`）

> D3D11 + DXGI flip-model swap chain，NV12 零拷贝上屏。
> **1080p p50 437 µs vs GDI 7456 µs（17.1x）**。

## 为什么需要它

用户对 SideLens 的原始要求：「优先考虑软件的使用体验，比如超低的连接延迟，**极致的原生 ui 性能体验**」。

GDI 路径的链路是：

```text
NV12 → CPU 转 RGBA → 拷进 GDI DIB → StretchDIBits → DWM 合成 → 屏幕
        ↑ 2.51ms @1080p   ↑ 全帧拷贝      ↑ 再拷一次
```

三次全帧拷贝。GPU 路径把它压成一步：

```text
NV12 纹理 → ID3D11VideoProcessor（GPU 着色器做 YUV→RGB）→ swap chain 呈现
           ↑ 零 CPU 参与          ↑ flip-model，不经 DWM 额外拷贝
```

## 实测（RTX 5070 Ti，1920x1088 → 1920x1080，各 200 帧）

```text
--- GDI ---
  纯 CPU 转换：p50=2437us p95=2796us
  端到端：avg=7559us p50=7456us p95=8137us
    其中 convert=2518us blit=956us

--- GPU ---
  适配器：NVIDIA GeForce RTX 5070 Ti（software=false）格式=B8G8R8A8_UNORM
  200 帧：p50=437us p95=1042us avg=726us
    其中 upload=355us（CPU memcpy） gpu p50=232us p95=353us
```

| 指标 | GDI | GPU | 提升 |
| --- | --- | --- | --- |
| p50 | 7456 µs | **437 µs** | **17.1x** |
| p95 | 8137 µs | **1042 µs** | **7.8x** |

`window_demo` 960x540 跑 10 秒：**30631 帧 / 3063 fps**，p50=255µs，无撕裂。

### 437µs 里还有什么

```text
upload=355us   ← CPU memcpy 到 staging 纹理（最大的一块）
gpu=232us      ← 视频处理器 + Present
```

**下一步最大的性能空间是省掉这 355µs 的上传** —— 让 Media Foundation 直接解码到
共享的 D3D11 纹理（DXVA2/D3D11VA）。这需要改 `sidelens-codec-mf` 暴露纹理句柄。

## 六个关键技术决策

### 1. YUV→RGB 用视频处理器，不写 shader

`ID3D11VideoDevice::CreateVideoProcessor` + `ID3D11VideoProcessorBlt`：

- 自动处理 BT.709 / BT.601 色彩矩阵（`VideoProcessorSetStreamColorSpace`）
- 自动处理 limited/full range
- 硬件加速，几乎零开销

自己写 pixel shader 也能做，但要手写矩阵、处理 range、处理色度上采样 ——
容易出错且不利用硬件。

### 2. 必须 flip-model，不能 blt-model

`DXGI_SWAP_EFFECT_FLIP_DISCARD` + `DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT`。

flip-model 不经过 DWM 的一次额外拷贝 —— 这是「极致低延迟」的关键。
再用 `SetMaximumFrameLatency(1)` 把排队深度压到 1（默认 3，会多出 2 帧延迟）。

### 3. NV12 上传必须走 STAGING + 逐行写

**不能 Map 一个 NV12 纹理**（`D3D11_USAGE_DEFAULT` 不允许 Map，且它是 planar 的）。
**也不能用 `R8_UNORM` 中转**（`CopyResource` 要求源/目标格式完全一致，一律 `E_INVALIDARG`）。

正确做法：NV12 的 STAGING 纹理 `Map` 后**逐行写**（驱动给 `RowPitch`，可能 > 宽度，
填充区域不能覆盖），再 `CopyResource` 到 NV12 纹理。

### 4. coded vs display 的裁剪

解码器输出 1920x1088，真实画面只有 1920x1080。视频处理器的输出矩形用 `display`，
输入矩形用 `coded`，让它做裁剪。这是 `ID3D11VideoProcessorBlt` 的
`pOutputRect` / `pStreamFrameSourceRect` 参数。

### 5. 离屏模式必须显式 `Flush`

窗口模式有 Present 推队列，离屏没有 —— 驱动会一直攒着不执行，
timestamp query 永远读不到。加显式 `Flush` 后 `avg_gpu_us` 从 0 → 232µs。

### 6. 每个窗口独立的 quit 标志

任何 `DestroyWindow` 都会 `PostQuitMessage` 到**线程**队列。全局静态标志在
多窗口场景下互相踩（`cargo test` 并行跑两个窗口测试时 A 关窗把 B 也关了）。
改用 `GWLP_USERDATA` 挂 `Arc<AtomicBool>`。

## 怎么用

```bash
# 独立基准（对比 GDI）
cargo run --release -p sidelens-render-gpu --example bench_present

# 真窗口演示
cargo run --release -p sidelens-render-gpu --example window_demo

# 在 daemon 里启用
$env:SIDELENS_RENDER='gpu'
$env:SIDELENS_H264_DECODE='1'
./target/release/sidelensd.exe --udp 27200
```

`SIDELENS_RENDER` 取值：

| 值 | 行为 |
| --- | --- |
| `gpu` | D3D11 + flip-model |
| `1` / `true` | GDI `StretchDIBits`（历史路径） |
| `headless` | 只转换不显示 |
| 未设置 | 不渲染 |

**为什么 GPU 不是默认值：** 第三十九到五十一轮的所有渲染数字都是 GDI 测的，
默认值一改那些数字全部失去可比性；而且 GPU 依赖 D3D11 设备，
远程桌面 / 虚拟机 / 老显卡上可能拿不到硬件设备（退 WARP 慢一个数量级），
显式选择比静默降级好。

## 端到端效果

720p，60fps 源，各 9 秒：

| 后端 | `render_mode` | `render_frames` | `render_failed` | `playable_ms` p50 |
| --- | --- | --- | --- | --- |
| GDI | `gdi-window` | 409 | 0 | 24.02 ms |
| GPU | `gpu-window` | 410 | 0 | **20.73 ms** |

**只降了 14% 而不是 17 倍** —— 因为端到端 `playable_ms` 里渲染只占 7.5ms（GDI）
/ 0.4ms（GPU），剩下的是抖动缓冲 + AvSync + 网络。渲染的 17 倍提升作用在一个
本来就不是瓶颈的环节上。

## 已知缺口

| 缺口 | 影响 | 修法 |
| --- | --- | --- |
| **没有硬件解码器零拷贝** | `upload=355µs` 是最大单项成本 | 让 MF 直接解码到共享 D3D11 纹理（DXVA2/D3D11VA） |
| ~~没有窗口 resize 处理~~ | **已修（第五十九轮）**：`resize()` + daemon 接线 | — |
| ~~视频按 1:1 显示~~ | **已修（第五十五轮）**：`dest_rect_fit` 保持宽高比填满 | — |
| ~~没有设备丢失恢复~~ | **已完成（第五十九/六十轮）**：`recover()` + `DeviceRemoved` 错误分类 + daemon 自动触发（限 3 次） | 真实触发未实测 |
| GPU 计时在离屏路径延迟一帧 | 最后一帧样本读不到 | 已知限制，窗口路径不受影响 |
| WARP 路径未实测 | 无独显环境的行为未验证 | 需要在无 GPU 机器上跑 |
| 未测多显示器 / HDR / 10bit | HDR 显示器上会被 DWM 做 tone mapping | 需要 HDR swap chain（`R10G10B10A2` + `DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020`） |

## 子窗口模式（第五十四轮）

```rust
pub fn new_child(
    parent: isize,
    title: &str,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
) -> Result<Self, GpuError>;
```

参数全是**父窗口客户区的物理像素**，与 `sidelens_ui::MainWindow::video_rect()` 同口径。

### 为什么不能直接建 `WS_CHILD`

`CreateSwapChainForHwnd` 在真正的 `WS_CHILD` 窗口上会**同步**给窗口过程发
`WM_SIZE` / `WM_WINDOWPOSCHANGED` 等消息并**等待它们处理完**。而窗口过程
运行在创建它的线程上 —— 也就是调用 `new_child` 的那个线程。

于是：`new_child` 卡在 `CreateSwapChainForHwnd` 里等窗口过程，
窗口过程在等 `new_child` 返回。**永久死锁。**

实测症状：daemon 打到「GPU 渲染器嵌入面板视频区」这行日志后就再无输出，
进程活着但不干活（不是崩溃）。加 `pump_once` 也救不了 —— 消息是在
`CreateSwapChainForHwnd` **内部**才发出的。

### 解法：先顶层，后挂载

```text
1. 建无边框顶层窗口（WS_POPUP）   ← swap chain 能正常创建
2. 建 D3D11 设备 + swap chain
3. SetParent 挂到面板上            ← 系统自动加 WS_CHILD、去 WS_POPUP
4. MoveWindow 摆到视频区坐标
```

`SetParent` 会自动补 `WS_CHILD` 并去掉 `WS_POPUP`，不需要手动 `SetWindowLong`。

### 另一个坑：排空消息队列会吃掉别人的消息

`Window::build` 里有一段「清掉队列里残留的 `WM_QUIT`」的逻辑（用于修
「先建 GDI 渲染器、销毁、再建 GPU 渲染器时 GPU 侧 0 帧」）。

第一版是：

```rust
while PeekMessageW(&mut stale, None, 0, 0, PM_REMOVE).as_bool() {
    // 丢弃一切
}
```

`PeekMessageW(None, ...)` 取的是**整个线程**的消息。接进 daemon 后同一线程上
已经有 UI 面板在跑，它的 `WM_PAINT` / `WM_SIZE` 被吃掉且不派发 —— 面板
永远收不到，而死锁在等它们。

**正确做法：只丢 `WM_QUIT`，其余照常 `DispatchMessageW`。**

### 实测

```text
INFO 状态面板已创建（原生 Win32 控件） dpi=192 client=(1800, 1280)
INFO GPU 渲染器嵌入面板视频区（子窗口） video_x=32 video_y=192 video_w=1736 video_h=928
INFO GPU 渲染已启用 adapter="NVIDIA GeForce RTX 5070 Ti"

窗口层级：
main 1826x1351
  子窗口 1736x928   ← GPU 渲染（正好等于 video_rect）
  子窗口 1720x48    ← 状态标签
  子窗口 1720x44 ×2 ← 延迟 / 视频信息标签
  子窗口 280x52     ← 复选框
```

端到端：`render_frames=327 / render_failed=0`，`playable_ms` p50 **21.14ms**。

## 窗口 resize 与设备丢失恢复（第五十九轮）

### `resize(width, height)`

```rust
pub fn resize(&mut self, width: u32, height: u32) -> Result<(), GpuError>;
```

**为什么必须显式调用**：`WM_SIZE` 到达时 swap chain 的 back buffer 还是旧尺寸。
继续 `Present` 会被 DWM 拉伸到新窗口 —— 表现为「拖动窗口时画面变形」。

而 GPU 渲染器**收不到** `WM_SIZE`（窗口过程在 `window.rs`，只处理退出标志），
所以必须由持有窗口的一方转发：

```rust
// daemon 主循环
if let Some((vw, vh)) = panel.pump() {
    if let Some(r) = renderer.as_mut() {
        r.resize(vw, vh)?;
    }
}
```

三件事：`ResizeBuffers` 重建 back buffer、丢弃旧管线（输出视图绑在旧纹理上）、
下一次 `present` 时按新尺寸重建。**同尺寸幂等**（`WM_SIZE` 会重复发）。

实测：

```text
改前 1826x1351 → 改后 1200x800
INFO 渲染器已跟随窗口尺寸 width=1768 height=1104
render_frames=467  render_failed=0  render_mode="gpu-window"
```

### `recover()`

```rust
pub fn recover(&mut self) -> Result<Option<isize>, GpuError>;
```

设备丢失（`DXGI_ERROR_DEVICE_REMOVED`）时所有 D3D11 对象都失效，唯一恢复办法是重建。

触发场景：显卡驱动更新/TDR、独显切集显、远程桌面连接/断开、GPU 过热复位。

**返回值是新的窗口句柄** —— 重建要重新创建窗口（`SwapChain` 绑在 HWND 上），
调用方持有的旧 `hwnd` 会失效。

### 为什么 `Window` 要记住挂载位置

```rust
pub struct Window {
    // ...
    /// 子窗口模式下的 `(父句柄, x, y)`；顶层窗口为 `None`
    child: Option<(isize, i32, i32)>,
}
```

如果重建出来的是**顶层窗口**，它会盖住 UI 面板 —— 用户看到的是
「拖动窗口后突然弹出一个独立窗口」。`recover()` 用 `child_placement()` 还原。

### 测试

| 测试 | 覆盖 |
| --- | --- |
| `gpu_resize_keeps_rendering` | resize 后仍能正确出画（**最重要**） |
| `gpu_resize_same_size_is_idempotent` | 同尺寸不重建 |
| `gpu_resize_zero_is_error` | 尺寸 0 报错且不破坏原尺寸 |
| `gpu_headless_resize_updates_size` | headless 只改尺寸 |

`gpu_resize_keeps_rendering` 的意图：画一帧（白）→ 读回 → resize(640,480) →
再画一帧（黑）→ 读回。如果漏了重建管线，第二次会画到失效纹理上（画面变黑）。

### 已接入（第六十轮）

daemon 现在会自动触发：`present` 返回 `DeviceRemoved` 时调 `recover()`，
**限 3 次**（否则「每帧都丢失、每帧都重建」会死循环）。

配套的错误分类：

```rust
pub enum GpuError {
    // ...
    /// 设备丢失（DEVICE_REMOVED / DEVICE_HUNG / DEVICE_RESET）
    DeviceRemoved { hresult: u32 },
}
```

三个 HRESULT 归成一类（都是「对象全失效、必须重建」）：

| HRESULT | 常量 |
| --- | --- |
| `0x887A0005` | `DXGI_ERROR_DEVICE_REMOVED` |
| `0x887A0006` | `DXGI_ERROR_DEVICE_HUNG` |
| `0x887A0007` | `DXGI_ERROR_DEVICE_RESET` |

**真实触发无法在此环境构造**（需要拔显卡 / TDR / 切显卡），
所以验证只覆盖错误分类逻辑，未实测真实恢复效果。
