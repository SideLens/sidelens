# SideLens 文档索引

> 46 篇文档。按「想解决什么问题」组织，不按时间顺序。

## 从这里开始

| 文档 | 什么时候读 |
| --- | --- |
| [progress.md](progress.md) | 想知道项目做到哪了、每一轮的实测数字。**62 轮的完整记录** |
| [blockers.md](blockers.md) | 想知道什么做不了、为什么、解除条件 |
| [protocol.md](protocol.md) | 改协议、加帧类型、理解 32 字节帧头 |
| [release.md](release.md) | **打包 / 安装 / 卸载**：per-user 布局、三条不变量、49 项自动断言 |
| [update.md](update.md) | **应用内更新与手机端升级提示**：两段式落地、手写 HTTP、去重与撤销 |
| [release-manifest.md](release-manifest.md) | 发布清单契约：字段、路径安全、版本判据 |

## 延迟（项目的核心指标）

| 文档 | 内容 |
| --- | --- |
| [latency-budget.md](latency-budget.md) | **延迟预算分解**：每一毫秒花在哪，哪些可优化。含跨网络预期推算 |
| [latency-matrix.md](latency-matrix.md) | 延迟基准矩阵（工具 + 历史数据）。注意部分历史数字已被证伪 |
| [rust-latency-report.md](rust-latency-report.md) | Rust 重写后的延迟报告 |

**一句话结论**：回环 p50 从 20.95ms 降到 **11.92ms（WebSocket）/ 13.77ms（UDP）**，
降幅全部来自抖动缓冲自适应（`target_ms` 从常量 20 变成按实测抖动调整）。

视频与音频两条自适应都已接入：
- 视频：第二十八轮接 UDP、第三十二轮补 WebSocket
- 音频：第三十六轮接入（下限 20ms / 单步 10ms / 驻留 500ms，与视频不同）

H.264 编解码往返 p50 **882 µs**（编码 529 + 解码 398），33ms 帧间隔下占 2.7%。

3% 丢包下的延迟（第三十八轮修掉 AvSync 主时钟 bug 后）：
**p50 12.02ms / p95 21.72ms**，此前是 20.97 / 57.70。

## 传输与可靠性

| 文档 | 内容 |
| --- | --- |
| [udp-nack-report.md](udp-nack-report.md) | NACK 重传实测 + **附录 C：一个被证伪的假设**（重放窗口是否误杀重传） |
| [fec-link.md](fec-link.md) | FEC 与分片的联动、自适应冗余度、组大小策略、**FEC 的价值边界表**、**关键帧保护（精确模型 + 0.999 目标）** |
| [congestion.md](congestion.md) | 拥塞控制：延迟梯度 + AIMD，以及反馈回路 |
| [crypto.md](crypto.md) | X25519 + ChaCha20-Poly1305 帧加密、重放防护 |

## 编解码

| 文档 | 内容 |
| --- | --- |
| [h264.md](h264.md) | Media Foundation H.264 **编解码双通**、动态码率、VARIANT 类型坑 |
| `tests/data/golden-720p.h264` | **golden 码流**（31 帧 / 136 KB），用三方交叉验证解析器 |
| [sps.md](sps.md) | SPS 解析（Exp-Golomb）与分辨率变更检测 |
| [nalu-bugs.md](nalu-bugs.md) | NALU 切分里修掉的两个真 bug |
| [opus.md](opus.md) | Opus 音频链路、帧长/DTX/in-band FEC 实测矩阵 |
| [audio-out.md](audio-out.md) | **音频输出**：WASAPI vs waveOut 实测决策、真设备时钟验证 |
| [audio-in.md](audio-in.md) | **音频采集**：WASAPI loopback、四个必踩的坑、真实回环验证 |
| [capture.md](capture.md) | **屏幕采集**：DXGI vs GDI 实测、BT.709 约定、16 对齐填充、**缩放** |
| [codec.md](codec.md) | 编解码层总览 |
| [render.md](render.md) | **渲染层（GDI）**：NV12 转 RGBA、coded vs display 裁剪、BT.709 依据、性能实测 |
| [render-gpu.md](render-gpu.md) | **GPU 渲染层**：D3D11 + flip-model，**p50 7456→437µs（17.1x）** |
| [ui.md](ui.md) | **原生 UI 层**：纯 Win32 控件 + DPI 感知，`pump` **297ns**，不用 webview |
| [frame-transform.md](frame-transform.md) | **NV12 几何变换**：旋转/裁切的三条硬约束、stride bug、与 DirectShow 模式的冲突 |
| [frame-effects.md](frame-effects.md) | **画面效果**：不依赖 ONNX 的自动取景/背景虚化/人像平滑，以及为什么这样取舍 |

## 平台与移植

| 文档 | 内容 |
| --- | --- |
| [cross-platform.md](cross-platform.md) | 九平台能力矩阵、cfg 门控的判据 |
| [ipc.md](ipc.md) | 命名管道 + 共享内存（seqlock 防撕裂） |

## 虚拟摄像头（已结案）

这五篇是 18 个假设的完整排查记录。**结论：API 路线在当前 Windows build 上走不通**，
需换 DriverPackage + 桩驱动。

| 文档 | 内容 |
| --- | --- |
| [android.md](android.md) | **Android 端**：Camera2 + MediaCodec 零拷贝、USB 链路、实测数据 |
| [dshow.md](dshow.md) | **DirectShow 路线**：源滤镜实现、注册表登记、6 个踩坑 |
| [vcam-summary.md](vcam-summary.md) | **MFCreateVirtualCamera 结案**：19 个假设的排除表 |
| [vcam-pitfalls.md](vcam-pitfalls.md) | 逐轮踩坑记录 |
| [vcam-registry.md](vcam-registry.md) | 注册表取证（DeviceClasses / Enum / Properties） |
| [vcam-sdk-gaps.md](vcam-sdk-gaps.md) | SDK 头文件与官方示例的不一致处 |
| [vcam-regentry.md](vcam-regentry.md) | `AddRegistryEntry` 的严格 A/B（证伪） |

## 性能

| 文档 | 内容 |
| --- | --- |
| [perf-audit.md](perf-audit.md) | 热路径审计：3 个真问题 + 4 个「看起来是问题、实测不是」 |
| [stability.md](stability.md) | **长时间运行**：60 秒全功能数据、内存收敛验证、历史泄漏记录 |

## 工具索引

| 工具 | 用途 |
| --- | --- |
| `tools/h264-inspect.mjs` | **独立** Annex-B 解析器（零依赖，支持 `--json` / `--summary` / `--expect` 断言） |
| `tools/verify-h264-stream.ps1` | H.264 码流回归（720p/1080p/360p + 3 个参数交互场景） |
| `tools/verify-e2e.ps1` | **端到端回归（统一入口）**：8 场景 / 25 断言 / 2 分钟，含真实采集，退出码可接 CI |
| `tools/package-release.ps1` | **打包**：产出 dist/<版本>/ 与 zip，逐文件算 sha256 写进清单 |
| `tools/verify-release.ps1` | **发布验证**：打包→装→升→卸→更新→版本一致性，**49 项断言**，全在临时沙箱 |
| `tools/verify-cross-platform.ps1` | **跨平台编译回归**（自动发现所有 target，逐个 cargo check） |
| `tools/audit-cfg.sh` | cfg 门控静态审计（4 类 HIGH 问题 + 单边 cfg） |
| `tools/verify-jitter-adapt.ps1` | 抖动缓冲自适应实测 |
| `tools/verify-bitrate-feedback.ps1` | 码率反馈回路实测 |
| `tools/verify-replay-retx.ps1` | 重放窗口 vs 重传（负结果验证） |
| `tools/bench/run-matrix.ps1` | 完整延迟矩阵（需两台设备） |
| `tools/bench/fec-ab.ps1` | FEC A/B 实测 |
| `tools/run-category-ab.ps1` | 虚拟摄像头类别 A/B |

## 阅读顺序建议

**想理解延迟怎么优化的**：`latency-budget.md` → `progress.md` 第二十八轮 →
`crates/sidelens-core/src/jitter.rs` 的 `adapt` 方法

**想理解 FEC 该不该开**：`fec-link.md` 的「价值边界表」→ `progress.md` 第二十六轮

**想接手虚拟摄像头**：`dshow.md`（当前可用路线）→ `vcam-summary.md`（为什么放弃 MF 路线）

**想加新功能**：`protocol.md` → `cross-platform.md`（cfg 门控判据）→ `progress.md` 最近的轮次

**想发一个版本**：`release.md`（一条命令）→ `release-manifest.md`（清单字段与判据）→ `update.md`（用户怎么拿到这个版本）