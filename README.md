# SideLens

把一台设备的**屏幕、麦克风、系统声音**以超低延迟共享给另一台设备。
原生界面、GPU 渲染、开源。

> **当前状态：桌面端功能闭环，可端到端运行。**
>
> 视频与音频两条链路的两端都是真实设备，中间是真实网络协议。
> 界面是原生 Win32 控件（不用 webview），视频走 D3D11 GPU 渲染。
>
> **45,000 行 Rust / 16 个 crate / 1059 个测试 / 5 个平台交叉编译通过。**

---

## 实测数字

| 指标 | 值 | 条件 |
| --- | --- | --- |
| 端到端可播放延迟 p50 | **19.6 ms** | 720p 60fps，含界面 + GPU 渲染 |
| 端到端可播放延迟 p50 | **12.0 ms** | 3% 丢包 + NACK |
| GPU 渲染单帧 p50 | **437 µs** | 1080p，D3D11 flip-model（GDI 是 7456µs，**17.1x**） |
| 屏幕采集 p50 | **22.7 ms** | 4K DXGI（GDI 60.4ms） |
| 屏幕采集 p50 | **11.2 ms** | 4K → 720p 缩放（带宽从 15.2 降到 2.1 Mbps） |
| H.264 编码 / 解码 | 529 / 2440 µs | 1080p，Media Foundation |
| Opus 音频带宽 | **45 kbps** | 原始 PCM 是 768kbps，压 17 倍 |
| 音频采集延迟 | **10.0 ms** | WASAPI loopback |
| 音频输出延迟 | **30 ms** | WASAPI 共享模式 |
| UI 消息泵 | **297 ns/次** | 1ms tick 的 0.03% |

---

## 这个软件能做什么

| 能力 | 状态 | 实现 |
| --- | --- | --- |
| 屏幕采集 | ✅ | DXGI Desktop Duplication（4K 22.7ms）/ GDI 保底（60.4ms） |
| 屏幕缩放 | ✅ | BGRA 域面积平均（4K→1080p 比不缩放更快） |
| 音频采集 | ✅ | WASAPI loopback（系统声音）/ 麦克风 / headless |
| H.264 编解码 | ✅ | Media Foundation MFT，编解码双通 |
| Opus 编解码 | ✅ | audiopus，帧长 2.5-60ms 可配，支持 DTX |
| GPU 渲染 | ✅ | D3D11 + DXGI flip-model swap chain，NV12 零拷贝 |
| 原生界面 | ✅ | 纯 Win32 控件 + DPI 感知，不用 webview |
| UDP 传输 + 分片 | ✅ | 多分片重组，SAFE_MTU 1200 |
| NACK 重传 | ✅ | 重传缓存 + 重放窗口 |
| FEC 前向纠错 | ✅ | XOR + 高斯消元，**关键帧保护**（精确模型 + 0.999 目标） |
| 拥塞控制 | ✅ | 延迟梯度 + AIMD，通过 RttReply 驱动编码器 |
| 帧加密 | ✅ | X25519 + ChaCha20-Poly1305，重放防护 |
| 抖动缓冲自适应 | ✅ | 视频/音频两套参数，p50 从 20.95 降到 13.77ms |
| 音画同步 | ✅ | 音频主时钟 + 偏移限速 |
| 虚拟摄像头 | ⚠️ 结案 | API 路线在当前 Windows build 上走不通，需 DriverPackage |

---

## 快速开始

> 本项目 JS 侧用 **Vite+（`vp`）** 管理，不要用 npm / pnpm / yarn。
> Rust 侧就是标准 cargo。

### 1. 编译

```bash
cargo build --release
```

产物：`target/release/sidelensd.exe`（接收端）、`sidelenssend.exe`（发送端）。

### 2. 跑起来（推荐配置：真实屏幕 + GPU 渲染 + 原生界面）

```powershell
# 接收端：环境变量开各功能
$env:SIDELENS_UI='1'              # 原生界面
$env:SIDELENS_RENDER='gpu'        # GPU 渲染（D3D11 flip-model）
$env:SIDELENS_H264_DECODE='1'     # H.264 解码
$env:SIDELENS_OPUS='1'            # Opus 解码
$env:SIDELENS_AUDIO_OUT='1'       # 声卡输出
.\target\release\sidelensd.exe --udp 27200

# 发送端：共享屏幕 + 系统声音
.\target\release\sidelenssend.exe --udp 127.0.0.1:27200 60 8192 8 `
    --screen dxgi --size 1920x1080 `
    --h264 --gop 30 `
    --audio --opus --audio-in loopback
```

界面会显示连接状态、延迟分位数、丢包率、分辨率、帧率，视频区里是实时画面。

### 3. 端到端回归（一条命令验证链路没坏）

```powershell
cargo build --release
.\tools\verify-e2e.ps1              # 8 个场景 / 25 项断言，约 2 分钟
.\tools\verify-e2e.ps1 -Only h264   # 只跑某个场景
```

覆盖：基础 UDP、3% 丢包 + NACK、H.264 编解码、Opus 音频、帧加密、FEC 关键帧保护、**真实屏幕采集**、**真实音频 loopback**。
退出码 0/1，可直接接 CI。

### 4. 最小验证（不需要界面）

```bash
./target/release/sidelensd.exe --udp 27200
./target/release/sidelenssend.exe --udp 127.0.0.1:27200 60 8192 10 --loss 0.03
```

接收端每 2 秒打印统计：

```text
UDP 统计 frames=1024 net_ms="0.13/65.44/81.84" playable_ms="15.65/24.43"
  jitter=1 target_ms="10.0" nack_sent=316 healed=152 dropped=229
  loss="0.079" fec_reds=18 fec_recovered=11 fec_redundant=11
  video_decoded=888 render_frames=888 render_failed=0 render_mode="gpu-window"
```

---

## 环境变量速查

| 变量 | 取值 | 作用 |
| --- | --- | --- |
| `SIDELENS_UI` | `1` | 原生界面（默认关） |
| `SIDELENS_RENDER` | `gpu` / `1` / `headless` | GPU 渲染 / GDI / 只转换不显示 |
| `SIDELENS_H264_DECODE` | `1` | H.264 解码（发送端用 `--h264` 时必须开） |
| `SIDELENS_OPUS` | `1` | Opus 解码 |
| `SIDELENS_OPUS_FRAME_MS` | `2.5`..`60` | 音频帧长（必须与发送端一致） |
| `SIDELENS_AUDIO_OUT` | `1` / `headless` | 声卡输出 / 只计数 |
| `SIDELENS_AUDIO_TARGET_MS` | 毫秒 | 音频抖动缓冲目标 |
| `SIDELENS_PSK` | 64 位 hex | 帧加密密钥（两端必须一致） |

**参数配对表**（配错会得到误导性的失败）：

| 发送端 | 接收端 | 配错的后果 |
| --- | --- | --- |
| `--h264` | `SIDELENS_H264_DECODE=1` | 不开：`video_decoded=0` 但不报错 |
| 不给 `--h264` | 却开了解码 | `video_decode_failed` 飙升（拿合成字节喂 H.264） |
| `--audio --opus` | `SIDELENS_OPUS=1` | 不开：`audio_decode_failed` 飙升 |
| `--psk X` | `SIDELENS_PSK=X` 或 `--psk X` | **`decrypt_failed=0` 但两路解码全废**（根本没解密） |

---

## 架构

```text
发送端                                         接收端
─────────                                      ─────────
屏幕采集 (DXGI/GDI)                             UDP 收包
  ↓ NV12                                         ↓ 分片重组
可选缩放 (面积平均)                              ↓ NACK / FEC 恢复
  ↓                                              ↓ 解密 + 重放防护
H.264 编码 (MF)                                  ↓ 抖动缓冲 (自适应)
  ↓ Annex-B                                      ↓ AvSync 裁决
分片 + FEC + 加密                                ↓ H.264 解码
  ↓ UDP                                          ↓ NV12
                                                GPU 渲染 (D3D11)
                                                  ↓
                                                窗口

声卡回环 (WASAPI loopback)                       UDP 收包
  ↓ PCM                                           ↓ 抖动缓冲 (按 PTS)
Opus 编码                                         ↓ Opus 解码
  ↓ UDP                                           ↓ WASAPI
                                                声卡
```

---

## 目录结构

```text
crates/
  sidelens-proto/       帧协议（32B 头，12 种帧类型）
  sidelens-core/        抖动缓冲、时钟同步、NACK、FEC、拥塞控制
  sidelens-transport/   WebSocket + UDP
  sidelens-codec/       H.264 NALU/SPS 解析、Opus 抖动缓冲、码率统计
  sidelens-codec-mf/    Media Foundation H.264 编解码
  sidelens-codec-opus/  Opus 编解码（audiopus）
  sidelens-crypto/      X25519 + ChaCha20-Poly1305
  sidelens-capture/     屏幕采集（DXGI / GDI / 合成）+ 缩放
  sidelens-audio-in/    音频采集（WASAPI loopback / 麦克风 / headless）
  sidelens-audio-out/   音频输出（WASAPI 共享模式）
  sidelens-render/      渲染（GDI StretchDIBits，历史路径）
  sidelens-render-gpu/  渲染（D3D11 + flip-model，**17.1x**）
  sidelens-ui/          原生界面（Win32 控件 + DPI 感知）
  sidelens-ipc/         命名管道 + 共享内存（seqlock）
  sidelens-vcam/        虚拟摄像头（结案）
  sidelens-vcam-dll/    虚拟摄像头媒体源 DLL
apps/
  sidelensd/            接收端常驻服务
  sidelenssend/         发送端
packages/               JS 参照实现（协议 golden 向量对齐 Rust）
tools/                  独立验证脚本（H.264 解析器、跨平台检查、A/B 基准）
docs/                   31 篇设计文档，约 700 KB
```

---

## 文档

从 [docs/README.md](docs/README.md) 开始，按「想解决什么问题」组织：

| 想知道什么 | 读哪篇 |
| --- | --- |
| 项目做到哪了、每一轮的实测数字 | [progress.md](docs/progress.md)（**54 轮**） |
| 什么做不了、为什么 | [blockers.md](docs/blockers.md) |
| 延迟怎么优化的 | [latency-budget.md](docs/latency-budget.md) |
| GPU 渲染为什么快 17 倍 | [render-gpu.md](docs/render-gpu.md) |
| 界面为什么不用 webview | [ui.md](docs/ui.md) |
| FEC 该不该开 | [fec-link.md](docs/fec-link.md) |
| 屏幕采集踩过什么坑 | [capture.md](docs/capture.md) |

---

## 设计决策

### 为什么协议自己定帧格式

WebSocket / TCP 的 message 边界不是「帧」的边界。接收端必须能从任意切分的
字节流里重新切出帧，所以帧头必须有 **magic + 长度**，并且流被污染时能**重新同步**。

### 为什么渲染用 GPU 而不是 GDI

GDI 路径是「NV12 → CPU 转 RGBA → 拷进 DIB → StretchDIBits → DWM」——
**三次全帧拷贝**。GPU 路径把这三步合成一步：NV12 纹理 → 视频处理器（GPU 着色器）
→ flip-model swap chain。实测 7456µs → 437µs。

### 为什么界面不用 webview

用户要求「极致的原生 UI 性能体验」。webview 启动要数百毫秒、内存几十 MB、
滚动有掉帧；纯 Win32 控件建窗 2.4ms、消息泵 297ns。而这个软件界面只是
「几行状态 + 一块视频 + 几个开关」，原生控件成本并不高。

### 为什么抖动缓冲是自适应的

固定 `target_ms` 在好链路上下不来（白等），在差链路上又不够（卡顿）。
自适应后 p50 从 20.95ms 降到 13.77ms（**降 34%**）。

---

## 已知限制

| 限制 | 说明 |
| --- | --- |
| 只在 Windows 上运行 | 5 个 target 编译通过，但 macOS/Linux 没有平台实现 |
| 没有真实摄像头采集 | 本机无摄像头（见 blockers B1），屏幕采集已通 |
| 虚拟摄像头走不通 | API 路线需 DriverPackage + 桩驱动（见 blockers B3） |
| 视频按 1:1 显示 | 不填满视频区，刻意不拉伸（便于排查裁剪问题） |
| 跨设备未实测 | 时钟同步协议已实现，但只在同机验证 |

---

## 许可

Apache-2.0
