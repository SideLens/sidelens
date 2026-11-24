# 渲染层（sidelens-render）

> 第三十八轮新建。把解码后的 NV12 变成可见画面，或转成 RGBA 交给其他消费端。

## 为什么单独一个 crate

解码器（`sidelens-codec-mf`）输出的是 **NV12** —— 那是编码器的原生格式，
不是任何 UI 框架能直接画的格式。中间需要一层转换，而这一层有两种消费方：

| 消费方 | 需要什么 |
| --- | --- |
| 调试窗口 | RGBA 直接 blit |
| 虚拟摄像头（将来） | 可以只要 NV12，不转换 |
| 录制 / 截图 | RGBA 或 NV12 落盘 |

所以拆成「跨平台转换层 + 平台相关显示层」两块：

```text
crates/sidelens-render/
├── src/convert.rs     跨平台：Nv12View / to_rgba / crop_to_display
├── src/win32.rs       cfg(windows)：GDI 窗口 + headless
├── tests/convert.rs   26 个纯转换测试（CI 必跑，零平台 API）
├── tests/render_win.rs 7 个 headless 测试
└── examples/
    ├── bench_convert.rs    性能实测
    └── decode_to_render.rs 真解码器 → 真转换
```

## coded vs display：最容易出错的点

`H264Decoder` 有三个尺寸相关的方法，含义不同：

| 方法 | 含义 | 用途 |
| --- | --- | --- |
| `output_size()` | **coded**（宏块 16x16 对齐） | 分配 / 校验缓冲长度 |
| `output_display_size()` | SPS cropping 之后 | **显示 / 转换** |
| `output_frame_len()` | `coded_w * coded_h * 1.5` | 校验解码输出字节数 |

实测差异：

| 请求分辨率 | coded | 显示 | 多出行 |
| --- | --- | --- | --- |
| 320x240 | 320x240 | 320x240 | 0（240 = 15x16） |
| 640x360 | **640x368** | 640x360 | **8** |
| 1280x720 | 1280x720 | 1280x720 | 0（720 = 45x16） |
| 1920x1080 | **1920x1088** | 1920x1080 | **8** |

360 和 1080 都不是 16 的倍数，所以需要裁剪。**按 coded 输出会在底部多一条绿边。**

`Nv12View` 同时携带两个尺寸，`to_rgba` 只读 display 区域。

## 颜色矩阵：BT.709 limited（有实测依据）

三条依据，按可信度排序：

### 1. 码流自己没声明

用独立实现的 bit reader 解析 `golden-720p.h264` 的 SPS VUI：

```json
{ "profile_idc": 77, "level_idc": 31,
  "vui": { "aspect_ratio_info_present_flag": 1,
           "video_signal_type_present_flag": 0 } }
```

`video_signal_type_present_flag = 0` → `video_full_range_flag` /
`colour_primaries` / `matrix_coefficients` **一个都不存在**。
MF 的 H.264 编码器（mfh264enc）不写颜色描述，所以「照码流声明走」这条路不成立。

### 2. 编码链路不碰颜色

`decode_probe` 实测 Y 往返 `|ΔY|` 平均 0.43 / 最大 1 —— 编解码对 YUV 数值近似恒等。
矩阵只影响显示端，选哪个由我们定。

### 3. 行业默认

无 VUI 色彩描述时，FFmpeg swscale / Chrome / Windows video processor
对 HD（>= 720p）一律按 BT.709 limited 处理，SD 才回落 601。
本项目主场景是 720p/1080p。

BT.601 通过 `to_rgba_with(out, Matrix::Bt601)` 可选。

**测试钉住**：`bt709_red_pins_matrix` 用 BT.709 标准红 `(Y=63,Cb=102,Cr=240)`
断言输出 `(255,1,0)`；同三元组走 BT.601 是 `(233,...)`，
`bt601_differs_from_bt709_on_red` 断言两者必须不同。

> 注意期望值是 `(255,1,0)` 而不是 `(255,0,0)`。
> 代入矩阵算：`G = 54.726 + 5.544 - 59.686 = 0.584`，四舍五入就是 1。
> 硬压成 0 反而错 1 LSB —— 颜色期望值必须算，不能凭印象。

## 性能实测

```text
场景                           尺寸           p50        吞吐
---------------------------------------------------------------------
720p coded==display          1280x720     1089.4 µs   846 MP/s   3.38 GB/s
720p coded 728 → 720         1280x720     1094.8 µs   842 MP/s   3.37 GB/s
1080p coded 1088 → 1080      1920x1080    2474.4 µs   838 MP/s   3.35 GB/s
360p coded 368 → 360          640x360      277.8 µs   829 MP/s   3.32 GB/s
4K coded 2176 → 2160         3840x2160    9966.5 µs   832 MP/s   3.33 GB/s
720p BT.601（对照）           1280x720     1064.1 µs
```

**720p p50 1094.8 µs，目标是 < 2000 µs，余量 1.8 倍。**

### 优化过程（617 → 730 → 843 MP/s）

| 改动 | 收益 |
| --- | --- |
| `chunks_exact` 替代下标索引 | 每像素一次边界检查 → 循环外一次 |
| `clamp()` 替代 if 链 | 生成 pmaxsd/pminsd，内层无跳转 |
| 去掉 `clear()+resize()` 的冗余 memset | 720p 每帧白清 3.7MB，占三成耗时 |

吞吐在 360p 到 4K 全程稳定在 ~830 MP/s —— 说明现在**写带宽受限**而非分支受限，
SIMD 收益有限，没上。

## headless 模式

`Renderer::headless(w, h)` 不注册窗口类、不创建窗口、不碰任何 Win32 API，
`present` 只做转换 + 累计帧数/耗时。用于 CI / 无桌面环境。

```rust
let mut r = Renderer::headless(1280, 720)?;
r.present(&nv12_view)?;
assert_eq!(r.frames(), 1);
assert_eq!(r.blit_ns(), 0);  // headless 下不 blit
```

## 真链路验证

`examples/decode_to_render` 用**真解码器**输出，不是合成数据：

```text
640x360   请求 → coded (640, 368)  显示 (640, 360)  一帧 353280 字节
          转换 30 帧，平均 289 µs/帧
          [OK] 裁剪生效：底部 8 行宏块填充已丢弃
1920x1080 请求 → coded (1920, 1088) 显示 (1920, 1080) 一帧 3133440 字节
          转换 20 帧，平均 2609 µs/帧
          [OK] 裁剪生效：底部 8 行宏块填充已丢弃
```

## 已知限制

| 限制 | 说明 |
| --- | --- |
| 真窗口已跑通但未肉眼验证 | 实测窗口创建成功、271 帧 `present` 全返回 Ok、`render_failed=0`。**画面是否正确显示需要人眼看** |
| GDI 而非 D3D | 保持依赖最小。4K 下 blit 可能成为瓶颈（未测） |
| 32bpp BI_RGB 是 BGRA | GDI 的通道序与 RGBA 相反，`swap_rb` 有 3 个单测守着（通道搞反在 CI 里看不见，在真窗口上是「人脸发蓝」） |
| `biHeight` 用负数 | 自上而下，与 NV12/RGBA 内存序一致，省一次整帧翻转 |

## 用法

```rust
use sidelens_render::{Nv12View, Renderer};

// 转换（不依赖任何平台 API）
let view = Nv12View::new(&nv12_bytes, coded_w, coded_h, display_w, display_h)?;
let mut rgba = Vec::new();
view.to_rgba(&mut rgba)?;

// 显示（Windows）
let mut r = Renderer::new("SideLens", 1280, 720)?;
r.present(&view)?;
if !r.pump_events() { /* 用户关窗 */ }
```

## 接入 daemon（第三十九轮）

### 三种模式

| `SIDELENS_RENDER` | 行为 |
| --- | --- |
| `1` / `true` | 开真窗口（GDI）。创建失败自动退回 headless |
| `headless` | 只做 NV12→RGBA 转换，不显示。CI / 无桌面环境用 |
| 未设置 | 不渲染 |

### 接线位置

```rust
// udp_server.rs 的 SyncVerdict::Play 分支
if let Some(r) = renderer.as_mut() {
    let (cw, ch) = dec.output_size();          // 必须在 decode 之前取！
    let (dw, dh) = dec.output_display_size();
    match sidelens_render::Nv12View::new(nv12, (cw, ch), (dw, dh)) {
        Ok(view) => { r.present(&view)?; }
        Err(e) => { /* 尺寸不匹配，计入 render_failed */ }
    }
}
```

**必须在 `decode` 之前取尺寸**：`decode` 返回的 `&[u8]` 借用了 `dec`，
之后再调 `dec.output_size()` 会 E0502。

### `pump_events` 必须放在 tick 里

不泵消息的话，GDI 窗口在 5 秒后被系统标成「未响应」（标题栏变灰）。
用户关窗时只销毁渲染器、不中断接收。

### 实测

```text
10 秒，1280x720，60fps，headless：
video_decoded   601
render_frames   601      <- 完全一致
render_failed   0
decode_us       3089     <- 含 NV12→RGBA 转换（2440 解码 + 1094 转换）
```

### 首帧渲染失败一次（已知，可接受）

640x360 这类「coded 比 display 大 8 行」的分辨率，**首帧渲染会失败一次**：

```text
coded_w=1280 coded_h=720   <- decode 前取到的还是候选尺寸
nv12_len=353280            <- 实际是 640x368
error=NV12 缓冲不足：需要 1382400 字节，只有 353280 字节
```

根因：解码器建的时候用候选尺寸，要等首个 SPS 才知道真实分辨率。
而 `decode` 返回的 `&[u8]` 借用了 `dec`，之后不能再调 `output_size()`（E0502）。

实测影响：

| 分辨率 | decoded | rendered | failed |
| --- | --- | --- | --- |
| 640x360 | 361 | 360 | 1（0.28%） |
| 1280x720 | 361 | 361 | **0** |
| 1920x1080 | 359 | 359 | **0** |

首帧本来就在等关键帧，用户感知不到。

### 还没做的

- **真窗口未肉眼验证**（需要交互桌面）
- 4K 下 GDI blit 可能成为瓶颈（未测）
- 音频没有播放（只解码不输出到声卡）
