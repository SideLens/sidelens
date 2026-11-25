# 开发进度

> 自主推进记录。每次完成一块就更新这里。
> 遇到阻塞点记到 [blockers.md](blockers.md)。

## 路线图与状态

| # | 任务 | 状态 | 说明 |
| --- | --- | --- | --- |
| 1 | Rust 核心重写（proto + core） | 完成 | golden 向量对齐 JS |
| 2 | Rust 传输层（WebSocket） | 完成 | 7 集成测试 |
| 3 | Rust 可执行（sidelensd / sidelenssend） | 完成 | 实测 20.95ms（WebSocket） |
| 4 | 定时器分辨率修复 | 完成 | timeBeginPeriod(1)，省 10ms |
| 5 | 协议扩展：NACK / 重传 / 分片 | 完成 | 帧类型 7-11 |
| 6 | UDP 传输层（含分片重组） | 完成 | 8 测试，3%/10% 丢包实测 |
| 7 | NACK 逻辑（缓存 + 请求 + 重发） | 完成 | healed=143 |
| 8 | 弱网模拟与验证 | 完成 | 确定性丢包器 |
| 9 | 跨设备时钟同步 | 完成 | NTP 式四时间戳，11+3 测试 |
| 10 | 真实摄像头采集（Windows） | 绕行 | 本机无摄像头，改由手机端提供（见 21） |
| 11 | H.264 编解码 | 完成 | **编解码双通并已接入 daemon**：编码 529µs / 解码 2440µs（含拷贝）/ 实测 601 帧 0 失败 |
| 12 | 虚拟摄像头（MFCreateVirtualCamera） | 结案（路线不通） | 19 个假设 + 13 组实验全排除，见 blockers.md B3 |
| 12b | 虚拟摄像头（DirectShow） | **完成** | 源滤镜已注册成功，系统摄像头列表可见，见 [dshow.md](dshow.md) |
| 21 | **Android 端（手机当摄像头）** | **进行中** | Camera2 + MediaCodec 零拷贝，一加 13 实测，见 [android.md](android.md) |
| 18 | IMFMediaSource 实现 | 完成 | IMFActivate + IMFMediaSourceEx + 8 属性 NV12 |
| 27 | 音频采集（`sidelens-audio-in`） | 完成 | WASAPI 麦克风 / **loopback** / headless；**已接发送端**（`--audio-in`） |
| 28 | 屏幕采集（`sidelens-capture`） | 完成 | GDI 60ms / **DXGI 22.7ms** @4K；**已接发送端**（`--screen`）；**支持缩放**（4K→720p 后带宽 15.2→2.1 Mbps） |
| 29 | GPU 渲染（`sidelens-render-gpu`） | 完成 | D3D11 + DXGI flip-model；**p50 7456→437µs（17.1x）**；已接 daemon（`SIDELENS_RENDER=gpu`） |
| 30 | 原生 UI（`sidelens-ui`） | 完成 | 纯 Win32 控件 + DPI 感知，`pump` **297ns**；**GPU 渲染已嵌入面板**（子窗口） |
| 19 | 虚拟摄像头 DLL 化（cdylib） | 完成 | 4 个 COM 导出，dumpbin 验证 |
| 20 | 设备属性 AddProperty（4 个 VCamCreate） | 完成 | 需要管理员权限才能实际生效 |
| 21 | 命名管道 + 共享内存 IPC | 完成 | 50 测试，seqlock 防撕裂，见 ipc.md |
| 22 | H.264 码流处理（NALU/参数集/AVCC） | 完成 | 14 测试 |
| 23 | 码率统计与自适应建议 | 完成 | 15 测试，已接入 daemon |
| 24 | IMFMediaStream 实现 | 完成 | RequestSample → 共享内存 → IMFSample → MEMediaSample |
| 25 | 帧来源（管道 + 共享内存接入 DLL） | 完成 | FrameSource 懒连接，7 单测 + 4 端到端 |
| 26 | IMFGetService + IKsControl | 完成 | 官方文档硬要求，缺了 Start 直接 E_INVALIDARG |
| 27 | 合成帧源（--shm-feed / --shm-probe） | 完成 | 跨进程实测 90 帧 @ 29.1fps 零丢帧 |
| 13 | macOS / Linux 移植 | 准备完成 | 平台能力矩阵 9×7；cargo check 通过，build 需 libopus |
| 14 | 音频链路 | 完成 | Opus 帧长/DTX/in-band FEC 可配；**已输出到声卡**（WASAPI 共享，32ms）；**采集端已接真实 loopback**（`--audio-in`） |
| 15 | 拥塞控制 / 码率自适应 | 完成 | 延迟梯度 + AIMD；**已通过 RttReply 回传，真正驱动编码器** |
| 16 | 跨设备真实测试 | 阻塞 | 需要两台设备，见 blockers.md B1 |
| 17 | FEC 前向纠错 | 完成 | 25 测试 + 33753 组合穷举，XOR + 高斯消元；**关键帧保护已接入**（精确模型 + 0.999 目标） |

## 测试统计

| 类型 | 数量 |
| --- | --- |
| Rust 单元/集成测试 | **1086** |
| 产物 | sidelensd.exe / sidelenssend.exe / sidelens_vcam_dll.dll / sidelens_dshow.dll |
| JS 参照实现测试 | 19 |
| **端到端断言** | **25 项全通过 / 8 场景**（`tools/verify-e2e.ps1`，约 2 分钟） |
| cargo fmt --check | 通过 |
| cargo clippy -D warnings | 通过 |
| 跨平台 check（5 target） | **通过**（含 audio-in / audio-out / capture） |
| **Linux 真实运行** | **✅ 核心测试全过 + 端到端 UDP 跑通** |

## 关键实测数字

| 指标 | 值 | 条件 |
| --- | --- | --- |
| WebSocket p50 | **11.92 ms**（自适应后；原 20.83 是 target_ms 常量造成的） | 8192B/帧 60fps |
| WebSocket p95 | **19.64 ms** | 同上 |
| UDP p50（无丢包） | **13.77 ms**（抖动自适应后；原 20.95 是 target_ms 常量造成的） | 7 片/帧 |
| UDP p50（3% 丢包） | **12.02 ms**（修掉 AvSync 主时钟 bug 后；原 20.97） | NACK 生效 |
| UDP p95（3% 丢包） | **21.72 ms**（原 57.70） | 等重传的代价 |
| **UDP p50（5% 丢包 + FEC + H.264 + Opus）** | **11.63 ms** | 全功能 |
| 关键帧 FEC 冗余度 | **11**（原 2，5.5x） | 720p @15% 丢包，35 分片关键帧 |
| 屏幕采集 4K（GDI） | 66.42 ms | p50，100 帧 |
| 屏幕采集 4K（DXGI） | **29.62 ms** | p50，100 帧 |
| 屏幕采集 1080p（DXGI，端到端） | **18.4 ms**（缩放 2:1） | 含等 AcquireNextFrame 的时间 |
| 屏幕采集 720p（DXGI，端到端） | **11.2 ms**（缩放 3:1） | 带宽从 15.2Mbps 降到 2.1Mbps |
| **渲染 p50（GPU）** | **437 µs** | 1080p，D3D11 + flip-model |
| 渲染 p50（GDI） | 7456 µs | 同上，StretchDIBits |
| 端到端 playable p50（GPU 渲染） | **20.73 ms** | 720p，60fps，对比 GDI 24.02 ms |
| 端到端 playable p50（UI + GPU） | **19.56 ms** | 720p，60fps，界面开着 |
| **跨时基 ±500ms 下的 playable p50** | **19.0 / 18.9 ms** | 时钟偏移注入，`dropped=0` |
| UI 消息泵 `pump` | **297 ns/次** | 1ms tick 的 0.03% |
| UI 建窗 | 2.4 ms（隐藏） / 7.5 ms（显示） | 200% DPI |
| 音频采集延迟 | **10.0 ms** | WASAPI loopback，设备周期 10ms |
| 协议编码开销 | 59 ns/帧 | 8KB 载荷 |
| 协议解码开销 | 104 ns/帧 | 8KB 载荷 |
| 纯头开销 | 5 ns | 空载荷 |
| 时钟同步偏差（同机） | -5.5 ms | 用户态收包延迟，见 ADR |

## 已知坑（不要重复踩）

1. **Windows 定时器 15.625ms**：interval(1ms) 实际 15.6ms，必须 timeBeginPeriod(1)
2. **端口 0 语义**：bind(0) 后要读 local_addr()，不是记传入的 0
3. **Box dyn Error 不能 ? 转 anyhow**：统一用 anyhow::Result
4. **解码器返回切片会永久借用 self**：改成「元数据 + 区间」两阶段
5. **基准必须真实读取产出**：否则 LTO 会消除 memcpy
6. **workspace 内部依赖要用 workspace:* **：写版本号会去 registry 找死
7. **vp test 是内建命令**：跑 package.json script 要用 vpr test
8. **UDP 收发必须用同一个 socket**：回包发到数据报源地址
9. **saturating_sub 做时间比较会出错**：now<delay 时返回 0
10. **NACK 请求后不能删记录**：重传再丢就再也补不上
11. **用户态收包延迟会污染时钟同步**：T2 不是网卡收到时刻，实测偏 5ms
12. **GetVersionExW 一律返回 Build 9200**：没有 manifest 时被"版本谎言"欺骗，改用 RtlGetVersion
13. **MFCreateVirtualCamera 必须 CoInitializeEx + MFStartup**：报 CO_E_NOTINITIALIZED 0x800401F0
14. **虚拟摄像头 sourceId 必须是 CLSID 字符串**：传普通名字报 CO_E_CLASSSTRING 0x800401F3
15. **Start() 要求 CLSID 已注册 COM 类**：否则 REGDB_E_CLASSNOTREG 0x80040154
16. **共享内存光有 Acquire/Release 不够**：读端在「读 meta」和「读像素」之间会被写端绕圈覆盖，读到撕裂帧。必须 per-slot seqlock
17. **HRESULT 不等于 Win32 错误码**：windows crate 把 997 包成 0x800703E5，直接比永远为假，要用 `code & 0xFFFF`
18. **重叠 I/O 超时后必须 CancelIoEx 并等待**：否则内核稍后往已关闭的事件句柄 SetEvent，句柄复用就误唤醒
19. **管道客户端要显式切消息读模式**：服务端建消息模式，客户端默认字节模式，不切会粘包
20. **clippy 的 Arc<非 Send> 检查**：MF 的 COM 对象是 free-threaded，但 windows crate 包装类型不带 Send+Sync，需手写 unsafe impl 并注明契约
21. **`#[cfg]` 不能写在宏参数中间**：`warn!(a=1, #[cfg(windows)] b=2, ...)` 报 "positional arguments cannot follow named arguments"。要先把 cfg 分支算成局部变量
22. **`read` 工具在 limit 处静默截断**：读大文件（>2000 行）再整文件写回会丢尾部。用 node fs 或 edit 工具
23. **「上一帧的分片数」是个陷阱**：H.264 下 P 帧 2 片、关键帧 30+ 片，用上一帧的值算预算会把关键帧的冗余钳成 0。手上有的本帧值就用本帧的
24. **两端配置方式不一致必然踩坑**：PSK 发送端认 `--psk`、接收端只认 `SIDELENS_PSK`，现象是「两路解码等量全废 + `decrypt_failed=0`」——`decrypt_failed=0` 意味着根本没解密，不是解密成功
25. **`send_frame` 返回的是成功发出的分片数**：5% 丢包下单个分片的音频帧每 20 帧就整片被吃，`frags==0` 时若直接 `return false` 不计数，`audio_sent` 会莫名少一截，看起来像「节拍被饿」
26. **WASAPI loopback 必须传 `eRender`**：对 capture 端点传 `AUDCLNT_STREAMFLAGS_LOOPBACK` 返回 `E_INVALIDARG`，错误信息毫无线索
27. **loopback 静音时 `read` 返回 0 是正常状态**：不是故障，别重连。重连会丢设备缓冲且每次重建 ~10ms
28. **DPI 虚拟化让 GDI 和 DXGI 报不同分辨率**：进程 DPI-unaware 时 4K+200% 的屏幕 GDI 报 1920x1080、DXGI 报 3840x2160，「DXGI 失败降级 GDI」这条路径直接坏掉。两个后端都要先 `SetProcessDpiAwarenessContext(PER_MONITOR_AWARE_V2)`
29. **DXGI 首帧是全黑假帧但 `AcquireNextFrame` 成功返回**：`LastPresentTime == 0` 且纹理格式是 `R16G16B16A16_FLOAT`。不判这条会把占位帧当真实画面。`CopyResource` 格式不匹配是**静默失败**
30. **NV12 缓冲的初值不能是 `vec![0u8; n]`**：Y=0 是 full-range 黑，limited range 下是非法值；UV=0 是错色度（应 128）。必须填 16/128。这个 bug 是「两个各自正确的组件拼在一起出错」——采集层正确返回 `Ok(None)`、编码层正确复用缓冲，错在缓冲初值语义
31. **`zip` 链在 4K 上反而更慢**：1080p 快 25%，4K 慢 20%（巨大 tuple 让 LLVM 放弃向量化）。用 `chunks_exact(4)` + 常量下标
32. **`D3D11CreateDevice` 传 adapter 时 driver type 必须是 `D3D_DRIVER_TYPE_UNKNOWN`**：传 `HARDWARE` 返回 `E_INVALIDARG` 且不提原因
33. **「尺寸」有两个语义但类型都是 `u32`**：`EncoderConfig::width/height` 是**显示**尺寸（`nv12_len()` 不做 16 对齐），`Frame::coded_*` 是**编码**尺寸（16 对齐）。混用只在非 16 倍数分辨率下暴露 —— 720 恰好是 16 的倍数，`align16(720)==720` 掩盖了整个 bug
34. **缩放和色彩转换要在同一个遍历里做**：分两遍算 Y 和 UV 要把 33MB 源数据再读一次，4K→1080p 实测 22.3ms，比不缩放的 15.5ms 还慢。融合成单遍后 14.4ms
35. **UV 平均的舍入方向**：「先各自加 128 和钳位再平均」与「无钳位偏移相加、最后钳一次」**不等价**（Rust 整数除法向零截断）。四个偏移 -3,-3,-3,-2：前者得 125，后者得 126
36. **不能 Map NV12 纹理**：`D3D11_USAGE_DEFAULT` 不允许 Map；`CopyResource` 要求格式完全一致，`R8_UNORM`→NV12 一律 `E_INVALIDARG`。正确做法是 NV12 的 **STAGING** 纹理 Map 后逐行写（驱动给 `RowPitch`，> 宽度，填充不能覆盖）
37. **离屏渲染不 `Flush` 则 GPU 计时恒为 0**：窗口模式有 Present 推队列，离屏没有，驱动一直攒着不执行，timestamp query 永远读不到
38. **残留 `WM_QUIT` 会杀死后续新建的窗口**：任何 `DestroyWindow` 都 `PostQuitMessage` 到**线程**队列。同一线程里先销毁再创建新窗口，新窗口第一轮 pump 就退出
39. **全局 QUIT 静态标志在多窗口下互相踩**：`cargo test` 并行跑两个窗口测试时 A 关窗把全局标志置位、B 立刻退出。改用 `GWLP_USERDATA` 挂 `Arc<AtomicBool>`，每窗口独立
40. **「耗时 > 0」是错误断言**：headless 路径短到 `Instant::elapsed()` 真能测出 0 纳秒（QPC 分辨率约 100ns）。机器越快越容易失败。应断言「调用次数累加」和「avg = total / calls」
41. **`cargo:rustc-link-arg` 是逐字传给链接器的**：`/MANIFESTINPUT:` 的路径加引号会被当成文件名一部分（`LNK1104`）。路径含空格时宁可不嵌 manifest
42. **库的 `build.rs` 管不到最终 exe**：`sidelens-ui` 只给自己的 examples 嵌了 comctl32 v6 manifest，`apps/` 下的真实客户端要自己再来一份，否则控件是 Win95 经典外观
43. **200% DPI 下 900x640 客户区外框是 1832x1351**：工作区高度可能只有 1032，窗口会超出屏幕底部导致按钮点不到。测试要显式夹住窗口
44. **`SetWindowSubclass` 需要 `Win32_UI_Shell` feature**：`SS_LEFT` / `BST_*` 也在别的 feature 下，缺了是编译期找不到而不是运行时
45. **`PeekMessageW(None, ...)` 取的是整个线程的消息**：在多窗口进程里「排空队列」会吃掉别的窗口的 `WM_PAINT` / `WM_SIZE` 且不派发，形成死锁。只丢 `WM_QUIT`，其余必须 `DispatchMessageW`
46. **`CreateSwapChainForHwnd` 在真 `WS_CHILD` 窗口上会同步发消息并等处理完**：而窗口过程跑在创建它的线程上，同一段代码里建窗口+建 swap chain 会永久死锁。先建 `WS_POPUP` 顶层窗口，建完 swap chain 再 `SetParent`
47. **`SetParent` 会自动补 `WS_CHILD` 去掉 `WS_POPUP`**：不需要手动 `SetWindowLong` 改样式（手动改容易漏掉配套位）
48. **正则替换整个函数体是危险操作**：`/锚点[\s\S]*?结束锚点/` 在找不到结束锚点时**会吃到文件末尾**。本项目的 `processor.rs`（21880 字节）就这样被清空成 1 字节，且**没有 git 可恢复**。要改代码用 `edit` 的精确 `old_string`
49. **NV12 的 staging 纹理高度是 `coded_h`，不是 `coded_h * 1.5`**：1.5 倍只体现在内存布局里。传错会让 `CopyResource` **静默失败**（不返回错误），视频处理器读到未初始化纹理 —— 实测黑色 NV12（Y=16）读回 `G=77`
50. **`D3D11_VIDEO_PROCESSOR_CONTENT_DESC` 的 `OutputWidth/OutputHeight` 要传真实输出尺寸**：传 display 尺寸（而实际画到更大的窗口）会让驱动退回最近邻采样，放大时有锯齿
51. **看到异常先确认「正常长什么样」**：本轮的「横向条纹」查了半天缩放插值，最后发现是测试图案自带 `wrapping_add((y % 7))` —— 每 7 行加偏移
52. **NTP 四时间戳的时基归属**：`t1`/`t4` 是**本端**时基，`t2`/`t3` 是**对端**时基。`t4-t1` 和 `t3-t2` 同源可直接相减；`t2-t1` 与 `t3-t4` 跨时基，靠它们的和除以 2 平分出 offset
53. **测试工具不能破坏被测语义**：给 `--clock-bias-us` 也偏移探测的 `t1`，导致 `t4-t1` 为负、探测被静默丢弃、`synced` 永远 false。要模拟时基差异只能偏移**载荷里的 PTS**，不能偏移本端自己的计时
54. **`checked_sub` 的 None 分支会静默吞掉异常**：`t4.checked_sub(t1)?` 在时钟被 NTP 回调时无声返回，日志里什么都没有。改成 i128 相减 + 显式判负，让异常可观测
55. **`powershell -File` 按系统代码页（GBK）读无 BOM 的 UTF-8 脚本**：中文注释被误解析，报一堆 `Unexpected token`，而 `[Parser]::ParseFile()` 报 0 错误。写 .ps1 要加 UTF-8 BOM
56. **`[regex]::Escape('frames=')` 不转义 `=`**：如果 `$key` 已含 `=`，模板再加一个就变成 `frames==`，永远匹配不上且不报错
57. **PowerShell 正则里 `\S+` 会吃掉结尾引号**：`loss="0.029"` 匹配成 `0.029"`，转 `[double]` 抛异常。用 `[^" ]+`
58. **PowerShell 函数返回值带上所有未消费输出**：`Remove-Item` / `Start-Sleep` / `.Kill()` 的返回值都会混进 `return` 的 hashtable，导致 `$r.d` 是数组。改用 `$script:` 全局变量

## 环境

- Rust：D:/software/rust （RUSTUP_HOME/CARGO_HOME 已配，PATH 已加）
- Rust 版本：1.100.0 stable-x86_64-pc-windows-msvc
- 包管理：Vite+（vp），不用 npm/pnpm

## 怎么跑

```bash
# 测试
cargo test          # 1066 个 Rust 测试
vp test run         # 19 个 JS 测试

# 端到端（WebSocket）
./target/release/sidelensd.exe 27100
./target/release/sidelenssend.exe ws://127.0.0.1:27100 60 8192 10

# 端到端（UDP + 分片 + NACK + 时钟同步）
./target/release/sidelensd.exe --udp 27200
./target/release/sidelenssend.exe --udp 127.0.0.1:27200 60 8192 10 --loss 0.03
```

## 全功能跑法（第四十五/四十六轮验证过的组合）

> 这条命令同时开 H.264 + Opus + 真实音频采集 + FEC + 弱网 + 加密。
> 缺一个就会看到「解码全失败」这类假故障，所以参数要配齐。

```powershell
# 接收端：环境变量（推荐，密钥不留在 shell 历史）
$env:SIDELENS_H264_DECODE='1'   # 开 H.264 解码（发送端用 --h264 时必须开）
$env:SIDELENS_RENDER='headless' # 或 1 = 真窗口
$env:SIDELENS_AUDIO_OUT='1'     # 或 headless
$env:SIDELENS_PSK='000102...1e1f'
.\target\release\sidelensd.exe --udp 27200

# 发送端：所有参数
.\target\release\sidelenssend.exe --udp 127.0.0.1:27200 60 8192 8 `
    --loss 0.05 --fec 2 `
    --h264 --size 1280x720 --gop 30 `
    --audio --opus --audio-in loopback `
    --psk 000102...1e1f
```

### 参数配对表（配错会得到误导性的失败）

| 发送端 | 接收端 | 配错的后果 |
| --- | --- | --- |
| `--h264` | `SIDELENS_H264_DECODE=1` | 不开解码：`video_decoded=0` 但也不报错（帧被丢弃） |
| 不给 `--h264` | 却开了 `SIDELENS_H264_DECODE=1` | `video_decode_failed` 飙升（拿合成字节喂 H.264 解码器） |
| `--audio --opus` | `SIDELENS_OPUS=1` | 不开：`audio_decode_failed` 飙升（把 Opus 当 PCM） |
| `--psk X` | 也必须有 `SIDELENS_PSK=X` 或 `--psk X` | **`decrypt_failed=0` 但两路解码全废** —— 根本没解密 |
| `--audio-in loopback` | 无需配置 | 打不开会降级成合成音并打 warn |

### 三个「看起来像 bug、其实是配置」的现象

| 现象 | 真因 |
| --- | --- |
| `video_decode_failed=223` / `audio_decode_failed=223` / `decrypt_failed=0` | 接收端没拿到 PSK，密文被当明文 |
| `audio_sent` 比 `audio_frames_expected` 少约 5% | 已修（记账口径）。若再出现，说明节拍真的被饿 |
| `video_decoded=0` 但 `frames` 在涨 | 没开 `SIDELENS_H264_DECODE` |

## 文档索引

| 文档 | 内容 |
| --- | --- |
| [protocol.md](protocol.md) | 帧格式、NACK、分片、版本演进 |
| [latency-budget.md](latency-budget.md) | 延迟预算与测量方法 |
| [rust-latency-report.md](rust-latency-report.md) | Rust WebSocket 端到端实测 |
| [udp-nack-report.md](udp-nack-report.md) | UDP + NACK 弱网实测 |
| [vcam-pitfalls.md](vcam-pitfalls.md) | 虚拟摄像头的四道门槛（实测错误码） |
| [ipc.md](ipc.md) | 进程间帧传输：共享内存 seqlock、命名管道、命名空间 |
| [congestion.md](congestion.md) | 拥塞控制：延迟梯度数学、AIMD、振荡陷阱分析 |
| [fec-link.md](fec-link.md) | FEC 接入：六个设计坑、线格式、与重组器的对接 |

## 虚拟摄像头的端到端链路（已打通，只差 FrameServer 认可）

```text
sidelensd --shm-feed                    FrameServer (session 0)
  render_nv12(测试图案)                    sidelens_vcam_dll.dll
  ShmRing::write_frame(pixels)  ──┐        FrameSource::poll()
  PipeServer::write_message(sig) ─┤  ───→  ShmRing::read_latest(buf)
                                  │        build_sample()
                                  │        QueueEventParamUnk(MEMediaSample)
                                  └─ 管道  ─→ 消费端（Zoom/相机应用）
```

实测（跨进程，`--shm-probe` 读 `--shm-feed`）：

| 指标 | 值 |
| --- | --- |
| 读取帧数 | 90 / 90 |
| 不同帧数 | 90（无重复，说明帧真的在流动） |
| 实测帧率 | 29.1 fps（目标 30） |
| PTS 跨度 | 3083 ms |
| 超时 / 未连接 / 尺寸不符 | 0 / 0 / 0 |

## 媒体源自检（`--vcam-selfcheck`）

绕过 FrameServer，直接按注册表 CLSID 走标准 COM 链路：

```text
CoCreateInstance(CLSID) → IMFActivate
  → ActivateObject → IMFMediaSource
  → CreatePresentationDescriptor（1 流，已选中）
  → IMFMediaSource::Start(&pd, null, null)
```

**全部通过**。这证明媒体源实现本身是合格的 ——
剩下的唯一卡点在 `IMFVirtualCamera::Start` 对设备注册的额外校验上。

唯一未通过的是 `IMFVirtualCamera::Start` —— FrameServer 在
`GetService(riid=2032C7EF-76F6-492A-94F3-4A81F69380CC)` 拿到
`MF_E_UNSUPPORTED_SERVICE` 后继续走完 6 步，最后以 `E_INVALIDARG` 拒绝。
详见 [blockers.md](blockers.md) B3 与 [vcam-pitfalls.md](vcam-pitfalls.md) 第十二轮。
| [adr/](adr/) | 架构决策记录（5 份） |
## 第十五轮：拥塞控制与码率自适应

### 做了什么

新增 `crates/sidelens-core/src/congestion.rs`（约 500 行）+ 两个测试文件（24 + 7 个测试），
并接入 `apps/sidelensd/src/udp_server.rs` 的真实收包路径。

| 组件 | 作用 |
| --- | --- |
| `DelayTrend` | 最小二乘拟合「累积延迟 vs 发送时间」的斜率 |
| `CongestionController` | 延迟梯度 + 丢包率 → 目标码率（AIMD） |
| `resolution_hint` | 码率触底时建议降分辨率 |

### 三个真实踩坑（都是实测发现的，不是推演）

#### 坑 1：延迟窗口残留导致连续过降

```text
拥塞0: slope=0.0071 reason=Underuse      ← 已经晚了，还在升
拥塞9: target=376940
恢复0: slope=0.2274 state=Decrease       ← 网络已恢复，还在降
恢复3: target=200000 (触底)
恢复39: target=206498                    ← 26 秒才从 200k 回到 206k
```

两个缺陷：

1. **滞后过冲**：窗口里残留拥塞期样本，恢复后斜率仍为正，连续降 4 次触底
2. **恢复太慢**：纯百分比增长在低码率时是 200k × 8% = 16k/秒，回到 1.5M 要 80 秒

修法：拥塞时**清空延迟窗口**；同时记住 `pre_congestion_bps`，恢复期按
「与上次拥塞点的差距」加速爬升（每步补 1/4 差距）。修后 24 步（480ms）恢复。

#### 坑 2：丢包率口径错了两次

**第一次**：用「缺片帧数 / 总帧数」算，5% 链路丢包读出 `loss=0.000` ——
因为 NACK 把丢的包全补回来了。但拥塞控制要的是**链路当时的丢包率**，
不是「恢复机制是否成功」。改成统计「曾缺失过的帧占比」。

**第二次**：帧级丢包率 0.30，但实际包级只有 0.05 —— 一帧 7 个分片，
5% 包级丢包导致的帧级丢包是 `1 - 0.95^7 ≈ 30%`，正好吻合。
而控制器的 `loss_threshold` 是按**包级**定的。必须反解：

```text
p_packet = 1 - (1 - P_frame)^(1/n)
```

修后读数 `loss=0.049`，与注入的 5% 误差 < 2%。

#### 坑 3：随机丢包被误判成拥塞

注入固定 5% 丢包时，控制器把码率从 1502 压到 509 —— 但接收端实测吞吐
一直稳定在 2300-2500 kbps，**说明带宽完全够**，链路只是质量差。

| 现象 | 原因 | 正确应对 |
| --- | --- | --- |
| 延迟涨 + 丢包 | 队列溢出（真拥塞） | 降码率 |
| 只有丢包，延迟平稳 | 链路误码/干扰 | 加 FEC，**不降码率** |

修法：纯丢包信号（延迟没涨）时按丢包严重程度**插值**降速（0.97 → 0.88），
而不是一刀切。修后码率从 1502 缓慢退到 855 并稳定，吞吐保持 2300+。

### 阈值调整：3% 而不是教科书的 10%

实测：5% 包级丢包时用 10% 阈值判为「健康」，一路升到 2 Mbps。
对实时视频，3% 已经是「画面周期性卡一下」的水平；10% 是文件传输的量级。
另加 `loss_caution_threshold = 1%`：1-3% 之间不升也不降（舒适区边缘观望）。

### 实测数字

| 场景 | 链路丢包 | 目标码率 | 实测吞吐 | 状态 |
| --- | --- | --- | --- | --- |
| 干净链路 | 0% | 升到上限 8000 kbps | — | 22 秒收敛 |
| 5% 丢包 | 5.1% | 1502 → 855 kbps | 2300-2500 kbps | 稳定，不崩 |
| 链路缩水 5:1 | 触发 | 降到新容量以下 | — | 通过 |

### 测试

| 文件 | 数量 | 覆盖 |
| --- | --- | --- |
| `tests/congestion.rs` | 24 | 斜率数学、时钟偏移消除、AIMD、保持期、边界、振荡回归 |
| `tests/congestion_loop.rs` | 7 | 闭环：带队列的链路模型 + 控制器 + 帧大小换算 |

其中 3 个是**先看到错误行为、再修、再锁住**的回归测试：
`congestion_window_is_cleared_after_decrease`、`recovery_from_floor_is_fast`、
`pre_congestion_bitrate_is_remembered`。
## 第十六轮：FEC 前向纠错 + 拥塞控制的六个坑

### 一、FEC 前向纠错（多智能体协作产出）

`crates/sidelens-core/src/fec.rs`（519 行）+ `tests/fec.rs`（690 行，21 测试）。

**设计要点（实现者主动纠正了我最初的设计描述）**：

我最初的指令说「每个冗余包排除一个余类，可恢复任意 redundant 个丢失」。
实现者指出这是错的：**GF(2) 上不存在非平凡的 MDS 码**，纯 XOR 做不到
「任意 t 丢都能恢复」。它改用高斯-约当消元解线性方程组，并穷举给出实际能力：

| 配置 | 丢 1 个 | 丢 2 个 | 丢 3 个 |
| --- | --- | --- | --- |
| `redundant = 2` | 10/10 | 25/45（20 种同余类无解） | 0 |
| `redundant = 3` | 10/10 | 33/45 | 0/120 全恢复，78/120 救出 1 个 |
| `redundant = 4` | 10/10 | 37/45 | 60/120 全恢复 |

关键取舍：**部分恢复是常态**。解不出的未知数跳过，解得出的照常返回 ——
比「要么全解要么全弃」实用得多。欠定的宁可返回空，也绝不返回错误数据
（垃圾帧比丢包更糟，会污染后续所有 P 帧）。

### 二、拥塞控制：六个实测踩坑（这是本轮最花时间的部分）

每个坑都是**先看到错误数字、再改、再看**，不是推演。

#### 坑 1：延迟窗口残留导致连续过降

```text
拥塞9: target=376940
恢复0: slope=0.2274 state=Decrease   ← 网络已恢复，还在降
恢复3: target=200000 (触底)
恢复39: target=206498                 ← 26 秒才回到 206k
```

修法：拥塞时清空延迟窗口 + 记住 `pre_congestion_bps` 做上界探测。
修后 24 步（480ms）恢复，一次拥塞只降一次。

#### 坑 2 & 3：丢包率口径错了两次

**第一次**：用「缺片帧数/总帧数」，5% 链路丢包读出 `loss=0.000` ——
NACK 把丢的包全补回来了。但控制器要的是**链路当时的丢包率**。

**第二次**：改成帧级后读出 0.30，但实际包级只有 0.05 —— 一帧 7 分片，
`1 - 0.95^7 ≈ 30%`。必须反解 `p_packet = 1 - (1 - P_frame)^(1/n)`。
修后读数 0.049，与注入的 5% 误差 < 2%。

#### 坑 4：随机丢包被误判成拥塞

5% 固定丢包时码率从 1502 压到 509，但接收端吞吐一直 2300-2500 —— **带宽够**。

| 现象 | 原因 | 应对 |
| --- | --- | --- |
| 延迟涨 + 丢包 | 队列溢出 | 降码率 |
| 只有丢包，延迟平稳 | 链路误码 | 加 FEC，**不降码率** |

#### 坑 5 & 6：决策节奏（最深的一个）

`cc.update()` 原本每 2 秒调一次，而延迟窗口只覆盖 400ms ——
**1.6 秒的样本从未参与决策**，一次 500ms 的拥塞完全被忽略。

改成 1ms 一次后，新问题：丢包率是 2 秒窗口的统计，同一份数据被消费 2000 次，
每次触发一次降速 → `0.97^2000 ≈ 0`，码率瞬间触底。

试过的错误修法（全部失败）：

| 尝试 | 为什么失败 |
| --- | --- |
| 2 秒决策一次 | 漏掉 1.6 秒样本 |
| 1ms 决策（无节流） | 瞬间触底 |
| 200ms 降速节流 | 仍 26 次，10 秒到底 |
| 单次事件降幅上限 | 上限生效前已到底 |
| 按采样跨度折算当量 | 窗口重置后跨度≈0，当量攒不满，死循环 |
| `fresh = 收到过新包` | 每 33ms 一帧就判一次新鲜，10 秒降 44 次 |

**正确做法**：`fresh = 统计窗口刚好滚过一次`（窗口纪元 +1）。
一个新包不会改变累积统计的含义，只有窗口清零重算才算新数据。

最终效果：

| 指标 | 修复前 | 修复后 |
| --- | --- | --- |
| 10 秒内降速次数 | 44 | **1** |
| 目标码率 | 砸到地板 200 | 稳定 1460-1717 |
| 实测吞吐 | 2300-2500 | 2292-2678（不变） |
| 决策次数 | — | 12000（1ms 节拍，响应及时） |

### 三、阈值调整

`loss_threshold` 从教科书的 10% 改成 **3%**：实测 5% 丢包时用 10% 阈值判为
「健康」，一路升到 2 Mbps。对实时视频，3% 已经是「画面周期性卡一下」的水平。
另加 `loss_caution_threshold = 1%`：1-3% 之间不升也不降。

### 四、测试总数

| crate | 数量 |
| --- | --- |
| sidelens-core（含 congestion 26 + congestion_loop 7 + fec 21） | 大幅增加 |
| 全 workspace | **300** |
### 五、FEC 的三条硬限制（接入前必读）

实现者（`fec-dev` 子代理）在交付时主动纠正了我的设计错误，并给出三条必须知道的限制。
这些不是实现缺陷，是数学与工程上的固有约束。

#### 限制 1：`P_m = R_k XOR (其余包)` 只在单丢时成立

我最初的指令写的是「`k = m % redundant`，用那个冗余包反解」。丢两个包时这会算出
`P_0 XOR P_1` —— **一个看起来正常、实际是垃圾的包**。

实时视频里这比丢包更糟：解码器拿到坏帧还会污染后续所有 P 帧，画面上表现为
「一直花屏到下一个关键帧」。

实现者改成把整组建成 GF(2) 线性方程组，做高斯-约当消元，
**只有化简成单位行的未知数才返回**。保证「宁可不恢复，绝不返回错误数据」。

#### 限制 2：「可恢复任意 redundant 个丢失」做不到

GF(2) 上不存在非平凡 MDS 码。纯 XOR 的能力由「缺失包落在几个余类」决定：
每个余类最多救回 1 个包。

| 配置（group=10） | 丢 1 | 丢 2 | 丢 3 |
| --- | --- | --- | --- |
| `redundant=2` | 10/10 | 25/45 全恢复 | 0（超冗余数） |
| `redundant=3` | 10/10 | 33/45 全恢复 | 0/120 全恢复，78/120 救回 1 个 |
| `redundant=4` | 10/10 | 37/45 全恢复 | 60/120 全恢复，58/120 救回 1 个 |

**产品预期要对齐**：设 `redundant=3` 不等于「能扛三丢」。要真正抗任意 t 丢必须上
Reed-Solomon（GF(256)），代价是运算复杂度高一个量级。

但部分恢复仍有价值：解得出的照常返回，剩下的交给 NACK。
比「要么全解要么全弃」实用得多。

#### 限制 3：恢复出的包是补齐长度，调用方必须截断

冗余包里没有长度表。恢复出的包长度 = 组内最长包长度，尾部补 0。
靠「去掉尾部 0」猜真实长度会误伤本身以 0 结尾的数据。

**接入时的正确处理**：按 NAL / 帧边界截断。真实流量是分片后的 MTU 满包时，
这个误差通常是 0（所有分片长度相同）。

#### 其他已记录的实现取舍

| 行为 | 语义 |
| --- | --- |
| `take_redundant` 未满时 | 返回空且**不清空**当前组（残缺冗余包无意义） |
| `group_index` | 「已完成组数」而非「当前第几组」，`push` 让组变满时就 +1 |
| 超长包 | 不截断 |
| 两端配置不一致 | **静默出错**（冗余包里没有配置信息） |
| 重复 `push` 同一个 index | 先到为准 |

### 六、本轮测试数修正

FEC 实际是 **25 个测试**（我先前记录 21 是中途快照），
含两条穷举用例（8 个包的全部丢包组合，机器校验能力表）。
实现者另在临时目录跑了 **33753 种组合**的穷举验证：从未返回过错误字节、
单丢 100% 恢复、`recover` 二次调用不重复返回。
## 第十七轮：音频链路 + 平台抽象 + FEC 接入（三路并行）

本轮用三个子代理并行推进，我负责集成与修复。

### 一、音频链路（`audio-dev` 子代理，26 测试）

`crates/sidelens-codec/src/audio.rs`（602 行）+ `tests/audio.rs`（577 行）。

**核心设计点**（子代理在文档里写透了）：

| 取舍 | 理由 |
| --- | --- |
| 时间戳按「帧序号 × 帧时长」累加，不用墙上时钟 | 采集回调抖动 ±2ms 会累积进音画同步 |
| `frame_duration_us` 由采样数反推 | 与 `frame_samples` 两条路径不会算出矛盾值 |
| `validate` 逐档比对而非区间判断 | Opus 帧长是离散六档，区间判断会放过 15ms |
| 抖动缓冲容量 `ceil(target/frame) + 1` | 多一帧余量，避免目标深度附近反复溢出 |
| `depth_ms` 用时长求和而非 PTS 跨度 | 丢包会在 PTS 上留空洞，跨度会把空洞算成缓冲 |

**子代理主动做了一件我没要求的事**：因为 lib.rs 未注册模块时 `cargo build` 根本不编译该文件，
它另建临时 crate 跑了全部 26 个测试 + clippy + rustfmt 才交付。这个验证意识值得记一笔。

### 二、平台抽象层（`platform-dev` 子代理，25 测试）

`crates/sidelens-core/src/platform.rs`（931 行）+ `tests/platform.rs`（618 行）。

能力矩阵 9 项 × 7 平台，三态（Full/Partial/None）。关键点：

- `supports = support_level != None`，63 种组合有测试守住一致性
- `Partial` 不能压成 bool 丢掉 —— 移动端的命名管道与 UDP 打洞都是 Partial，
  上层需要它来决定「是否必须有中继兜底」
- `Unknown` 全 None 是**故意的保守失败**，不能当「通用实现」用
- `migration_order()` 用加权代价（0/6/6/8/8/11），移动端排在桌面之后

**HarmonyOS 全部 9 项标为 `Unverified`** —— 生态较新，没有可靠来源，不编造。

### 三、FEC 接入真实链路（我做的，16 测试）

新增 `crates/sidelens-core/src/fec_link.rs`，把纯 XOR 的 `fec` 模块与 UDP 分片链路桥接起来。

**这一块踩了四个坑，全是设计层面的**：

| # | 坑 | 现象 | 修法 |
| --- | --- | --- | --- |
| 1 | 补空包死循环 | **34GB 内存分配失败** | 循环条件显式写上界 |
| 2 | 独立 `pending` 计数器与 encoder 脱节 | 组满 flush 后仍以为有半组要补 | 只用 `encoder.pending()` 作唯一真相 |
| 3 | 组号只用帧序号 | 一帧跨多组时 `k` 全是 `[0,0]`，接收端无法区分 | 组号拆成「帧序号 + 帧内组下标」 |
| 4 | 帧序号在 `finish_frame` 才赋值 | push 期产出组(0,0)、finish 期产出组(1,1) | 加 `begin_frame()`，帧开始时确定 |

**第 5 个坑最深**：接收端用 `index % group_size` 把帧内下标折算成组内下标，
结果不同组的下标**撞车** —— 组 1 的分片 4 折算成组内 0，把组 0 的分片 0 覆盖了。
修法：每组一个独立的 `FecDecoder`，用 `HashMap<组号, decoder>` 存。

**第 6 个坑**：发送端补空包凑满尾组，但接收端不知道「这组原本只有 1 个真实分片」，
算出 `missing=3 > redundant=1` 直接放弃。修法：冗余包头加 `real_count` 字段（头从 8 字节扩到 10 字节）。

### 四、顺带修掉的 NALU 缺陷

验证 `split_annexb` 时发现：NALU 内部含 `00 00 01` 会被误判为起始码切成两段。
虽然编码器用 emulation prevention 避免该序列，但这是已知限制，已写入测试文档。

重写 `find_start_code` 让它返回**起点**而非「长度+数据起点」—— 原实现靠
`while buf[end-1] != 1` 往回找，遇到 NALU 内部有 `0x01` 就会算错。

新增 `tests/annexb_edge.rs`（7 测试）覆盖：混合起始码、内部起始码、尾随 0 歧义、首段无起始码。

### 五、测试总数

| crate | 本轮新增 |
| --- | --- |
| sidelens-codec（audio 26 + annexb_edge 7） | +33 |
| sidelens-core（platform 25 + fec_link 16） | +41 |
| **全 workspace** | **374** |
## 第十八轮：FEC 接入真实链路（接收端完成，发送端交给子代理）

### 做了什么

新增 `crates/sidelens-core/src/fec_link.rs`（约 500 行）+ `tests/fec_link.rs`（16 测试），
并把接收端接进 `apps/sidelensd/src/udp_server.rs` 的真实收包路径。

### 六个坑（全是设计层面，不是笔误）

| # | 坑 | 现象 | 修法 |
| --- | --- | --- | --- |
| 1 | 补空包死循环 | **34GB 内存分配失败** | 循环条件显式写上界 |
| 2 | 独立 `pending` 计数器与 encoder 脱节 | 组满 flush 后仍以为有半组要补 | 只用 `encoder.pending()` 作唯一真相 |
| 3 | 组号只用帧序号 | 一帧跨多组时 `k` 全是 `[0,0]` | 组号拆成「帧序号 + 帧内组下标」 |
| 4 | 帧序号在 `finish_frame` 才赋值 | push 期组(0,0)、finish 期组(1,1) | 加 `begin_frame()` |
| 5 | 组内下标折算撞车 | 组 1 的分片 4 覆盖组 0 的分片 0 | 每组一个独立 `FecDecoder` |
| 6 | 补空包让接收端误判 | 尾组算出 `missing=3 > redundant=1` 直接放弃 | 包头加 `real_count` 字段 |

第 5 个最深。实测现象是接收端 `received=4 missing=0`（组 0 看起来齐了），
但恢复全失败 —— 因为组 1 的分片 4 折算成组内 0，把组 0 的分片 0 覆盖了。

### 冗余包线格式（10 字节头）

```text
偏移 0  2B  frame_seq       帧序号（回绕）
偏移 2  2B  group_in_frame  帧内第几个 FEC 组
偏移 4  2B  k               第几个冗余包
偏移 6  2B  real_count      本组真实包数（不含补齐的空包）
偏移 8  2B  reserved        保留
```

`real_count` 是被坑 6 逼出来的。没有它，接收端不知道尾组原本只有几个真实分片。

### 恢复分片必须送回重组器

这是接入时最容易漏的一步。只统计不送回，重组器不知道分片已可用，
最后还是靠 NACK 补 —— **FEC 的 0 延迟优势完全浪费**。

接口障碍：`DatagramPiece` 要 8 字节分片头（含 `count` 与 `total_size`），
而 FEC 只保护数据部分。解法是 `push_fragment_with_meta()` 在收到分片时
顺便记下这两个值，恢复时用 `group_meta(gid)` 取出来重建分片头。

### 实测

接收端已接入，统计字段 `fec_reds` / `fec_recovered` 正常显示（当前为 0，
因为发送端还没发 FEC 包 —— 那部分交给 `sender-integ` 子代理）。

### 文档

新增 [fec-link.md](fec-link.md)（181 行），完整记录六个坑、线格式、
分组策略取舍、与 NACK 的分工。
### FEC 端到端打通（本轮最重要的成果）

接入真实 daemon 时发现两个连环 bug，单测与集成测试都测不出来：

| # | Bug | 现象 |
| --- | --- | --- |
| 1 | 控制帧被重组器吞掉 | `recv_frame` 无条件 `asm.push(...)?`，FEC 包永远凑不齐 → 返回 None → FEC 分支走不到 |
| 2 | FEC 输入点错了 | 喂 FEC 的代码在「重组完成后」，那时分片头早没了，XOR 对不上 |

修法：① 非媒体帧不进重组器；② 新增 `UdpReceiver::recv_datagram()`，
对**每个到达的分片**返回（含 `fragment_index`/`count`/`total_size`/`data`）。

**修后实测（5% 丢包，6 秒，30fps，8KB/帧）**：

| 指标 | 修复前 | 修复后 |
| --- | --- | --- |
| `fec_reds` | 0 | **340** |
| `fec_recovered` | 0 | **60** |
| `nack_seqs` | 175 | **21** |
| `loss` | 0.051 | **0.020** |

**NACK 请求量下降 88%** —— FEC 把大部分丢包在本地恢复了，不用等 1 个 RTT。

教训：单测全绿不代表集成能跑。这两个 bug 只在「真实数据流 + 真实重组器」
的组合下暴露，连 `e2e_pipeline.rs` 都测不出来（它直接构造分片，绕过了 `asm.push` 路径）。
**唯一能抓到它们的是端到端实测。**
### 第三个连环 bug：冗余包 MTU 边界差 2 字节

（由 `sender-integ` 子代理发现并修复）

发送端第一版按「整包 ≤ `SAFE_MTU_PAYLOAD`(1200)」卡冗余包，结果 8 秒内
**482 个冗余包全部被跳过**，`fec_reds=0`，FEC 等于没接。

根因是 FEC 头比分片头宽 2 字节：

```text
数据分片 = 32(帧头) +  8(分片头) + 1160(数据) = 1200
冗余包   = 32(帧头) + 10(FEC头) + 1160(数据) = 1202
```

这 2 字节无法消除 —— 改分片边界会让接收端的「分片下标 → 组内下标」映射全错。
最终按 1202 卡。安全性：真实以太网单包 IP 载荷上限 1472，1202 仍在其下 270 字节。

### 第四个问题：`group_size` 固定值导致开销放大

实测 `--fec 2` 理论开销 20%，实际 27%。原因：720p 8KB 帧约 7 个分片，
而 `group_size` 固定 10，尾组补空包后仍要发 2 个冗余包 ——
实际冗余占比 = 2/7 ≈ 29%。

改进方向：`group_size` 按典型分片数动态设（8 / 16 / 32），代价是两端要协商。
### 第三个致命缺口：恢复出的帧从未被播放

`fec_recovered=53` 看着不错，但 `asm.push()` 返回的完整帧**只被计数就丢了**，
没有进抖动缓冲 —— 用户看到的画面毫无改善，FEC 的价值停留在统计数字上。

连带问题：重建 `DatagramPiece` 时 `pts_us: 0, send_ts_us: 0`，
而这两个值是延迟统计的依据。填 0 会让帧被判为「延迟几百万毫秒」，
即使送进缓冲也会立刻被丢弃。

修法：`FecReceiver` 加 `frame_meta` 记时间戳，恢复时取出来用；
重组出的帧推进抖动缓冲。

**修后 `expired` 从 45 降到 9** —— 缺片帧真的被补齐了，不再等超时丢弃。
## 第十九轮：音频链路接入 + FEC 恢复帧进缓冲（本轮两个实质进展）

### 一、音频链路完整跑通（`audio-integ` 子代理）

音频模块（`audio.rs`，26 测试）此前只有单元测试，**从未接入真实链路**。本轮接进去了。

**实测（6 秒，`--audio`）**：

| 指标 | 值 | 说明 |
| --- | --- | --- |
| `audio_sent` | 301 | 期望 300（6s / 20ms），精确 |
| `audio_pts_span_ms` | 6020 | PTS 跨度与时长吻合 |
| `audio_frames` | 271 | 接收端收到 |
| `audio_played` | 271 | 全部播放，无积压 |
| `audio_depth_ms` | 40.0/60.0 | 目标 40ms，合理 |
| `audio_dropped_late` | 0 | 无迟到丢弃 |
| `audio_discontinuity` | 0 | 无时间戳跳变 |
| `av_synced` | **174** | **AvSync 生效** |
| `video_waited` | 6 | 视频等待音频 6 次 |

关键设计点（子代理在实现里落实的）：

1. **音频 PTS 必须与视频用同一时间基准**，否则 `AvSync` 算出的偏移毫无意义。
   两边都从 `now_us()` 起步。
2. **音频不走 FEC**（当前 FEC 只处理 Video），`FrameType::Audio` 直接绕过。
3. **音频帧不分片**（20ms 单声道 48kHz 约 1920 字节），直接发。

`av_synced=174` 是这轮最有价值的数字：它证明「音频为主时钟、视频按 PTS 对齐」
这条链路真的在工作，而不只是代码存在。

### 二、FEC 恢复帧终于被播放（我做的）

`fec_recovered` 有值不代表用户能看到改善。查下来发现：

**`asm.push()` 返回的完整帧只被计数就丢了**，从未进入抖动缓冲 ——
用户看到的画面毫无变化。

连带问题：重建 `DatagramPiece` 时 `pts_us: 0, send_ts_us: 0`，
而这两个值是延迟统计的依据。填 0 会让帧被判为「延迟几百万毫秒」，
即使送进缓冲也会立刻被丢弃。

修法：
1. `FecReceiver` 加 `frame_meta: HashMap<组号, (pts_us, send_ts_us)>`
2. 恢复时取出来填进 `DatagramPiece`
3. 重组出的帧推进抖动缓冲

**修后 `expired` 从 45 降到 9** —— 缺片帧真的被补齐了，不再等超时丢弃。

### 三、一次失败的建模尝试（如实记录）

想做「FEC 净收益评估」来自动判断值不值得开。**三次建模全部失败**：

| 尝试 | 结果 |
| --- | --- |
| `p * (1-p)^(n-1) * (1-p^r)` | 所有丢包率下都是 6-7%，全判负 |
| 二项分布递推 | `p/q` 数值不稳，恒等于 1.0，全判正 |
| 加带宽压力二阶项 | 基准不一致，全判负 |

共同根因：**FEC 的实际收益不是一个丢包率的函数**。它取决于分片大小分布、
组边界对齐、拥塞控制响应、NACK 时序等多个交互因素。

最终改成查表（`evaluate_fec`），数据直接来自五档实测。

**教训**：当解析模型连续多次与实测不符时，应停止调参、改用实测数据。
继续拟合只会得到一个「看起来有理论依据、实际全错」的函数。

### 四、测试数

| 项目 | 数量 |
| --- | --- |
| 全 workspace | **393** |

## 第二十轮：加密层 + 跨平台门控（本轮两个实质进展）

### 一、加密层从零建起（`crates/sidelens-crypto`，40 测试）

协议 v0/v1 的帧头里**没有任何认证字段** —— 这不是「还没做」，是设计缺口。
任何人都能伪造 `SLN0` 包、改 `pts_us` 让帧被判迟到、把 P 帧标成关键帧，
或者把录下来的旧画面循环重放。手机把摄像头和麦克风共享给电脑，
链路跑在同一段 WiFi 上，这是最容易被动嗅探的场景。

| 层 | 解决什么 | 模块 |
| --- | --- | --- |
| X25519 + HKDF-SHA256 | 无预共享密钥时协商出同一把会话密钥 | `handshake` |
| ChaCha20-Poly1305 | 窃听看不到画面，改一个 bit 整帧作废 | `sealer` |
| 位图窗口 | 旧帧重发不会被当成新帧 | `replay` |

**三个必须想清楚的设计约束**（详见 [crypto.md](crypto.md)）：

1. **加密必须在重传缓存之上**。发送端的重传路径会把同一个 `seq` 的字节原样再发。
   若「每发一次加密一次」，同一个 `(key, nonce)` 会被用两次 —— ChaCha20 是流密码，
   两次异或同一密钥流直接泄露两份明文的异或值。**这是灾难性的，而且功能完全正常、
   测试全绿。** 所以 `seal_frame` 记录已封过的 `(stream_id, seq)`，
   重复封直接返回 `NonceReuse`。
2. **nonce 唯一性来自「每会话一把新密钥」**。`seq` 从 0 开始、重启归零，
   所以 nonce 不能只由 `seq` 决定。握手时双方各出 32 字节随机数进 HKDF salt。
3. **帧头必须「可见但被认证」**。`seq`/`stream_id`/`flags` 是重组与 NACK 的依据，
   不能加密；但作为 AAD 参与认证，看得见改不了。

**性能实测推翻了一个直觉**：

| 帧大小 | 加密 | 解密 | 占 20ms 延迟预算 |
| --- | --- | --- | --- |
| 8 KB（720p 典型） | 8.74 µs | 6.54 µs | **0.044%** |
| 64 KB（关键帧） | 58.23 µs | 44.02 µs | 0.291% |

「加密会显著增加延迟，实时视频要谨慎」在 1 GB/s 级的 ChaCha20 面前不成立 ——
视频码率（~1 MB/s）比加密吞吐低三个数量级。

**线格式开销**：加密只在「明文长度正好落在分片边界前 16 字节内」才多一片，
概率约 16/1160 ≈ 1.4%。实测 8KB/16KB/64KB 帧的分片数全部不变。

**测试组织方式**：39 个集成测试按「守什么不变量」分组，其中 7 个是
**「攻击必须失败」**（篡改密文/标签/seq/flags/stream_id、错误密钥、角色相同）。
密码学代码的 bug 有个共同特点：功能看起来完全正常，安全性已归零 ——
所以测的不是「功能对不对」，而是「攻击失不失败」。

### 二、跨平台门控（`xcheck` 子代理发现 2 个真实缺陷，我修了）

子代理对 9 个 crate × 4 个 target（linux/android/darwin/ios）做了纯 `cargo check`，
结论写在 [cross-platform.md](cross-platform.md)。它找到的**两个真实缺陷**：

| # | 缺陷 | 严重度 |
| --- | --- | --- |
| P1 | `apps/sidelensd/Cargo.toml` 把 tokio/anyhow/tracing/tracing-subscriber/serde_json **5 个平台无关依赖**塞进了 `[target.'cfg(windows)'.dependencies]`，而源码里它们全是无条件使用 | 🔴 |
| P2 | `crates/sidelens-vcam/Cargo.toml` 的 `[dev-dependencies]` 漏了 target 限定，非 Windows 上真把 windows crate 编了一遍 | 🟠 |

P1 值得单独说：它**与移植无关**，是当前工程配置本身的错误。
写它的人把「整个 sidelensd 目前只跑 Windows」这个**事实**误写成了
「这些依赖是 Windows 专属」这个**声明**。而且它被 vcam 的 142 个错误
**完全掩盖**了 —— 修好 vcam 后它才浮出来，看起来像「移植带出来的新问题」。

**修完之后又发现第三层**（子代理没提，是我改完 P1 才暴露的）：
`main.rs` 里 4 个 `mod` 声明和它们对应的 60 行参数解析仍然无条件。
所以完整的修法是三层：

1. **Cargo.toml**：平台无关依赖移回 `[dependencies]`，只留 `windows`/`windows-core`/`sidelens-vcam`/`sidelens-ipc` 在 cfg 块里
2. **`lib.rs` 的 `mod` 声明**：`sidelens-vcam` 的 8 个模块 + `sidelens-vcam-dll` 的全部内容 + `sidelensd` 的 4 个模块加 `#[cfg(windows)]`
3. **函数级**：`sidelensd` 的 4 个模式分发抽成 `#[cfg(windows)] async fn run_windows_mode`，非 Windows 上改为「命中这些 flag 就明确报错」

第 3 层不是洁癖：只给 `return` 加 cfg 会留下「解析了 60 行参数然后什么都不做」的死代码，
而且被 cfg 掉的类型会让这些变量变成未使用警告。

**`#![cfg(windows)]` 的一个坑**（我踩了两次）：

```text
给 examples/probe_start.rs 加 #![cfg(windows)]
  → 非 Windows 上 main 也一起消失
  → E0601: main function not found in crate
```

这个错误看起来像「忘了写 main」，实际是 cfg 把 main 一起删了。
正确写法是内容进 `mod imp`，外层留两个 `main` 薄壳。cdylib 同理。

**最终状态**：

```text
                            linux    android   darwin    ios
sidelens-proto              OK       OK        OK        OK
sidelens-core               OK       OK        OK        OK
sidelens-codec              OK       OK        OK        OK
sidelens-transport          OK       OK        OK        OK
sidelens-ipc                OK       OK        OK        OK
sidelens-crypto             OK       OK        OK        OK   ← 本轮新增
sidelens-vcam               OK       OK        OK        OK   ← 修前 142 错误
sidelens-vcam-dll           OK       OK        OK        OK   ← 修前全灭
sidelensd                   OK       OK        OK        OK   ← 修前全灭
sidelenssend                OK       OK        OK        OK
```

`cargo check --workspace --all-targets` 四个 target 全部通过。

### 三、本轮的数字

| 项目 | 修前 | 修后 |
| --- | --- | --- |
| 测试总数 | 393 | **449**（+49 crypto 集成 +1 doctest +6 其他） |
| 跨平台可编译 crate | 5 / 9 | **9 / 9** |
| `cargo clippy -D warnings` | 0 | 0 |
| `cargo fmt --check` | 0 | 0 |

### 四、加密已接入真实链路（本轮补做）

原先只到「crate 建好、单测全绿」，本轮接进 daemon：

```bash
# 发送端加密
sidelenssend --udp 127.0.0.1:9740 30 8192 6 --fec 2 --audio --psk <64位hex>
# 接收端解密（环境变量，避免进 shell history）
$env:SIDELENS_PSK = '<64位hex>'
sidelensd --udp 9740
```

**加密位置**：编码之后、**缓存之前**。重传发的是同一份密文，
FEC 与分片全部作用于密文。

**实测（6 秒 / 30fps / 8KB / 3% 丢包 / FEC 2 / 音频）**：

| 指标 | 未加密 | 加密后 |
| --- | --- | --- |
| `playable_ms` p50/p95 | 20.94/21.93 | **20.89/21.96** |
| `sent` | 180 | 180 |
| `fec_overhead` | 27.4% | 27.5% |
| `audio_sent` | 301 | 301 |
| `av_synced` | 175 | 173 |

**加密对延迟没有可测量的影响** —— 与基准预测（8KB 帧 8.74µs = 预算的 0.044%）吻合。

**负面测试是唯一的硬证据**（两端故意用不同密钥）：

| 指标 | 密钥匹配 | 密钥不匹配 |
| --- | --- | --- |
| `frames` | 178 | **0** |
| `av_synced` | 173 | **0** |
| `decrypt_failed` | 3 | **311** |
| `audio_frames` | 282 | **0** |

一帧都没漏过去。

### 五、接入时抓到三个真实 bug（全部只有端到端才暴露）

| # | Bug | 症状 | 为什么难发现 |
| --- | --- | --- | --- |
| 1 | **FEC 恢复的帧绕过解密** | 密钥不匹配时仍有 `av_synced=14` | 表现为周期性花屏，频率与丢包率正相关，易误判为解码器问题 |
| 2 | **FEC 恢复的关键帧丢了关键帧位** | 丢包时灰屏到下一个自然关键帧 | `fec_recovered` 一切正常，flags 不参与任何校验 |
| 3 | 重放窗口对重传误杀 | `replay_rejected=1~4` | 功能上等价于「重传失败」，影响很小 |

**Bug 1 的修法**：FEC 恢复路径也必须走「先重放检查、再解密」，
判据与正常路径完全一致。**两条路的判据不能有两套**。

**Bug 2 的修法**：`FragmentInfo` 加 `flags` 字段，`FecReceiver::frame_meta`
存三元组 `(pts_us, send_ts_us, flags)`。教训是：FEC 恢复要还原的不只是
「数据」，还有**帧的全部元数据** —— 之前只想到时间戳（填 0 会被判迟到），
漏了 flags（因为它不参与任何校验，错了也不报错）。

### 六、尚未做的（明确记录）

1. **握手消息的线格式**未定义。当前走 PSK 旁路（命令行/环境变量传主密钥）。
2. **SAS 的用户交互**未做。需要两端各显示 6 位数字并等用户确认。
3. **密钥轮换**未做。30fps 视频 + 50 帧/s 音频下 2^32 个 nonce 够跑 1.7 年。
4. **重放窗口对重传的误杀**未修。正确做法是让 `FLAG_RETRANSMIT` 的帧单独放行。

## 第二十一轮：真实 Opus 编解码（音频带宽降 17 倍）

### 一、前二十轮的音频是假的

发送端 `synth_audio_pcm` 生成 1920 字节正弦 PCM 直接当载荷发出去。
那条链路验证了 PTS、抖动缓冲、AvSync 三件事，但**带宽是假的**：

```text
假音频：1920 字节/帧 × 50 帧/s = 768 kbps
真 Opus：  85 字节/帧 × 50 帧/s =  34 kbps
```

差 22 倍。所有涉及音频的带宽预算、拥塞控制、FEC 开销计算全部失真 ——
720p 视频 2 Mbps 的链路上，假音频占了 **28%** 的带宽。

### 二、选型：为什么是 `audiopus` 而不是 `opus`

| crate | 需要 cmake | 从零编译 | 说明 |
| --- | --- | --- | --- |
| `opus` + `opusic-sys` | ✅ 是 | 9.2 秒 | build.rs 调 cmake 编 libopus C 源码 |
| `audiopus` | ❌ 否 | 1.4 秒 | 自带预编译 `libopus.lib`（msvc x64/x86 各 2MB） |

本机装了 VS Build Tools 但**没有独立的 cmake**（VS 自带的那个不在 PATH）。
走 `opus` 路线的失败信息是：

```text
thread 'main' panicked at cmake-0.1.58/src/lib.rs:1132:5:
failed to execute command: program not found
is `cmake` not installed?
```

**完全没提「你需要 cmake」**，只说「程序找不到」。这类错误把「能不能构建」
绑到了环境上，对用户极不友好。

### 三、三个必须显式化的语义（都是实测抓到的）

| # | 语义 | 不搞清会怎样 |
| --- | --- | --- |
| 1 | lookahead = **312 采样 = 6.50ms** | 音频 PTS 与实际发声时刻差 6.5ms，视频会早播 |
| 2 | 解码返回的 `samples` 是**每声道**，不是交织总数 | 立体声每帧只消费一半数据 → 右声道静音/音调翻倍 |
| 3 | PLC 的输出缓冲长度是**期望帧长**，不是容量 | 给 5760 采样缓冲 → 补出 120ms 外推 → 一声拖长的怪响 |

第 2 条的阴险之处：**单声道下两者相等**（960 = 960），
所以单声道测试全绿，完全看不出来。是立体声测试把它逼出来的。

第 3 条同理：正常解码时 libopus 按码流里真实的帧长返回，看不出来；
**只有 PLC 会暴露** —— 没有码流可读时它只能按你给的长度填。

### 四、端到端实测：音频带宽 778.7 → 45.4 kbps

| 模式 | `audio_bytes` | 每帧字节 | 实测码率 |
| --- | --- | --- | --- |
| PCM（旧行为） | 488,000 | 1952.0 | **778.7 kbps** |
| `--opus --opus-bitrate 32000` | 28,414 | 113.7 | **45.4 kbps** |
| `--opus --opus-bitrate 64000` | 48,603 | 193.6 | **77.7 kbps** |

（5 秒 / 250 帧，`audio_bytes` 含 32B 帧头）

**为什么 32kbps 目标是 45.4 kbps**：两处结构性开销，不该消除 ——

```text
12.8 kbps（32B 帧头 × 50 帧/s）
+ 34.8 kbps（Opus VBR 实测，目标 32 偏差 +8.8%）
= 47.6 kbps  ≈ 实测 45.4
```

**32kbps 音频在 20ms 帧长下，帧头开销占 28%**。要降低只能加大帧长
（40ms / 60ms），代价是编码延迟翻倍。

### 五、一条被实测推翻的断言

原本写了「噪声比正弦占更多比特」，**实测反了**：

```text
Application::Voip 开启强噪声抑制
白噪声被判为「非语音」后大幅压制
→ 输出反而比纯音更小
```

改成断言「两者不同且都不是 0」，守住「编码器按内容分配比特」这个性质，
不假设方向。

**教训**：不要凭直觉断言编码器的行为方向。Voip 模式对「什么该保留」
的判断与「信息论上熵更高」是两回事。

### 六、接收端解码：放在抖动缓冲出队之后

三个候选位置里选了「抖动缓冲持压缩帧、出队才解」——
抖动缓冲已经知道「什么时候该播」，在那里解码不需要额外同步机制。

解码失败**不能让 tick 卡住**：一帧坏数据不该影响后续播放，
失败就跳过这一帧的 `on_audio`（主时钟少推进 20ms，视频短暂等待，
比整条链路停摆好）。

**配错方向的实测对比**：

| 配置 | `audio_decoded` | `audio_decode_failed` | `audio_played` |
| --- | --- | --- | --- |
| 两端都开 | **251** | 0 | 251 |
| 只接收端开（PCM 当 Opus 解） | 0 | **238** | **0** |

第一行是正常路径；第二行证明解码失败被正确计数并跳过，
而不是「播了一堆噪声」。

但反方向（只发送端开、接收端按 PCM 处理 Opus 字节）是个**静默失效**：
85 字节压缩帧被当 85 字节 PCM，时长仍按 20ms 算，`audio_played` 正常递增，
只是放出来的是噪声。这个方向没有检测手段，只能靠配置正确 ——
协议里加 codec 字段是唯一的根治办法。

### 七、测试数

| 项目 | 数量 |
| --- | --- |
| 全 workspace | **466**（+17 Opus） |

## 第二十二轮：真实 H.264 编码 + SPS 解析 + nalu 三个 bug（并行三条线）

本轮用三个子代理并行推进，主线负责集成与 bug 修复。

### 一、真实 H.264 编码（`h264-enc` 子代理）

前二十一轮发的是**合成字节**（8192 字节随机数据），不是真实视频。
这让带宽、丢包行为、FEC 分组全部失真 —— 合成数据**不可压缩**。

现在 `--h264` 走 Media Foundation 的软件编码器 MFT（`mfh264enc.dll`）。

**端到端实测（6 秒 / 30fps / 2Mbps 目标码率 / 无丢包）**：

| 模式 | `video_mbps` | `rx_kbps` | `avg_frags` | `playable_ms` p50 |
| --- | --- | --- | --- | --- |
| A 合成 8192B | 0.25 | 2081 | 7.0 | 20.94 |
| B `--h264` | **0.07** | **334** | **2.3** | 20.84 |
| C `--h264 --audio --opus` | **0.07** | **275** | **1.4** | 20.68 |

**视频带宽降 6.2 倍**（2081 → 334 kbps）。

**`avg_frags` 7.0 → 2.3 → 1.4 这个数字比带宽更重要**：
分片数直接决定 FEC 开销（`redundant / 分片数`）、NACK 粒度、每帧包数。
之前 FEC 开销 27% 的根本原因就是「8KB 帧只有 8 个分片」——
真实视频 2 个分片时，`should_fec` 的判据（分片数 > redundant）
会自动跳过大部分帧，这是正确的。

**编码耗时 1550-1889 µs，占 33ms 帧间隔的 5%**。不在关键路径上
（编码在采集之后、发送之前，与网络并行），可以接受。

**关键帧判断改为真实 IDR**：原实现用 `sent % (fps*2) == 0` 猜，
现在用 `nalu::is_keyframe` 扫真实 NALU 类型。编码器会因场景切换
额外插入 IDR，计数器判断会漏掉。

### 二、SPS 解析（`sps-parse` 子代理，29 测试）

接收端在**不解码**的前提下要知道三件事：真实分辨率、帧率、分辨率有没有变。
这三件事全部躺在 SPS 里，而 SPS 是**比特级**的指数哥伦布编码。

两个最常见的错误都做了专门测试：

| 错误 | 后果 | 实现 |
| --- | --- | --- |
| 忘了 cropping | 1920x1080 报成 1088，画面底部多一条绿边 | `resolution()` 已应用 cropping，同时保留 `coded_resolution()` |
| 忘了 `frame_mbs_only_flag` | 隔行视频高度少一半 | 高度 ×2，CropUnitY 再乘 `(2 - fmo)` |

**6 条真实码流样本**（全部来自公开论坛/邮件列表的抓包），
覆盖 Baseline/High/Main、720p/1080p/1400x1050、逐行/隔行、29.97fps。
实现者另写了 Python 参考解析器交叉验证，逐字段一致。

**「缺失」用 `Option` 表达，绝不编造默认值**。对帧率尤其重要 ——
把缺失当 30fps 会让抖动缓冲时钟慢慢漂移。

### 三、nalu.rs 的三个实测 bug（本轮修复，9 项回归测试）

这是本轮最有价值的发现 —— `sps-parse` 子代理在写解析器时**必须**理解
NALU 结构，写探针程序逐字节验证，才看出两处真 bug。

| # | Bug | 后果 |
| --- | --- | --- |
| 1 | 无起始码的碎片被当 NALU | 首字节低 5 位碰巧是 7 → 垃圾被缓存成 SPS → **整路视频黑屏** |
| 2 | 尾随 0 裁剪吃掉 `cabac_zero_word` | NALU 长度与内部 slice 不一致 → 硬件解码器报错或花屏 |
| 3 | 前置填充 0 产生幽灵 NALU | `annexb_to_avcc` 把它写进 AVCC，污染下游 |

**这三个 bug 经过 26 个 `nalu.rs` 单测 + 6 个边界测试 + 多轮端到端实测，全部通过。**

原因是**所有测试都按同一套错误假设写的**：
「无起始码就是一个 NALU」「尾随 0 都是填充」。

发现它的人是「用一个独立的、更底层的视角重新实现一遍」。

### 四、改写了 3 个原有测试的断言

| 文件 | 原断言 | 新断言 |
| --- | --- | --- |
| `tests/nalu.rs` | 无起始码 → 1 个 NALU | 无起始码 → 0 个 NALU |
| `tests/nalu.rs` | 尾随 0 全裁（4 个 0 → 长度 5） | 保留 2 个（长度 7） |
| `tests/annexb_edge.rs` | 1 个尾随 0 被裁 | 保留（可能是 cabac_zero_word） |

**当一个 bug 被发现时，「测试全绿」说明测试本身也错了。**
只改代码不改测试，下次会有人「修复」回去。

### 五、Opus 编解码（主线）

见 [opus.md](opus.md)。音频带宽 **778.7 → 45.4 kbps（降 17 倍）**，
发送端与接收端都已接入，端到端验证 `audio_decoded=251 / audio_decode_failed=0`。

### 六、测试数

| 项目 | 数量 |
| --- | --- |
| 全 workspace | **512**（+29 SPS + 9 nalu 回归 + 17 Opus + 其他） |
| `cargo fmt --check` | 0 |
| `cargo clippy -D warnings` | 0 |
| `cargo build --release` | 0 |

### 七、延迟基准矩阵（`latency-bench` 子代理，14 组全跑通）

`tools/bench/run-matrix.ps1` 自动化跑 14 组配置，输出
[latency-matrix.md](latency-matrix.md) 与 `latency-matrix.json`。

**五个关键发现**（都来自实测，不是推断）：

| 观察 | 证据 |
| --- | --- |
| **FEC 2 把 playable p95 压到 1/3.8** | fec-off p95=84.43ms vs fec-2 p95=21.97ms；代价 26.9% 冗余带宽 |
| **FEC 让 NACK 请求量下降 86%** | nack_seqs: fec-off=168 → fec-2=23 |
| **丢包只抬尾延迟，不抬中位延迟** | 0%→10% 丢包：p50 只波动 0.66ms，p95 波动 67.52ms |
| **帧越大 p95 越差** | size-16k p95=21.92ms vs size-64k p95=86.71ms（avg_frags 55.0） |
| **音频不增加视频延迟** | audio-on p50=20.90ms vs 无音频 20.95ms |

「丢包只抬尾延迟不抬中位」这条最有价值：它说明抖动缓冲**确实在工作** ——
丢包造成的空洞被缓冲吸收，只有超出缓冲深度的那部分才体现为延迟尖峰。

### 工具本身踩到的三个坑（都写进报告了）

1. **`Start-Process -RedirectStandardOutput` 会把「被外部杀」伪装成「程序早退」**。
   `ExitCode = -1` 是 `TerminateProcess(h, -1)` 的特征值，**不是 Rust 的退出码**
   （panic 返回 101、`Err` 返回 1）。看到 -1 就说明进程被外部强制结束 ——
   而本仓库是多 agent 并行开发，这个风险是真实的。
   现在的做法：自己建 `System.Diagnostics.Process` + `ReadToEndAsync()`。

2. **解析器用相对路径会静默读不到文件**。PowerShell 的 `Get-Location` 和
   进程 CWD 是两个东西，`ReadAllLines()` 按进程 CWD 解析，读不到时抛
   `IOException` 被 `catch` 吞成空日志 —— 表现是「49 个字段全缺失」而不是
   「路径错了」。现在统一 `Resolve-Path` 转绝对路径。

3. **就绪检测不能轮询 `HasExited`**。改成「端口归属 + 日志双判据」：
   `Get-UdpPortOwner` 校验 `OwningProcess == 自己的 PID`，
   避免「端口被残留进程占着」被误判成就绪。

### 八、全功能组合验证

`--h264 --audio --opus --fec 2 --loss 0.03` 一次跑通：

```text
视频：sent=178  h264=true  encode_avg_us=1562  fec_overhead=7.6%
音频：audio_sent=291  audio_kbps=44.0  audio_avg_frame_bytes=113.6
接收：rx_fps=20.0  avg_frags=1.4  playable_ms=21.02/22.11
      nack_sent=10  healed=6  fec_recovered=3  av_synced=176
      audio_played=291  audio_depth_ms=40.0/60.0
```

四条链路（视频编码、音频编码、FEC、音画同步）同时工作，
`playable_ms` p50 仍是 21.02ms —— 与单功能时的 20.9ms 几乎无差。

### 九、本轮协作复盘

| 子代理 | 产出 | 发现的真实问题 |
| --- | --- | --- |
| `h264-enc` | `sidelens-codec-mf` + 发送端接入 | MF 输出格式实测确认（Annex-B） |
| `sps-parse` | `sps.rs`（990 行）+ 29 测试 | **nalu.rs 的 2 个真 bug + 3 个设计缺陷** |
| `latency-bench` | `tools/bench/` 基准矩阵 | 报告了 loss-0 配置失败（待修） |

**最有效的一条经验**：让子代理「用一个独立的视角重新实现一遍相邻模块」，
比让它「检查某个模块有没有 bug」有效得多 —— 前者必然发现真问题，
后者通常只会确认「看起来没问题」。

## 第二十三轮：基准矩阵复核推翻了一条旧结论（`latency-bench` 子代理）

`latency-bench` 跑完 14 组矩阵后，把新数字与文档里记录的历史数字逐条对账，
**只有一格对不上**：

| 场景 | 文档记录 | 实测 | 判定 |
| --- | --- | --- | --- |
| WebSocket p50 | 20.83 | 20.95 | 一致（+0.6%） |
| **UDP 0% 丢包 p50** | **23.06** | **20.95** | **不一致（-9.1%）** |
| UDP 3% p50 / p95 | 20.97 / 57.70 | 21.00 / 58.04 | 一致 |
| UDP 10% p50 | 24.78 | 24.95 | 一致 |

我用当前二进制**独立复测 9 次**（3 轮 × 每轮 5 个统计窗口）：

```text
第 1 次：20.88, 20.91, 20.92, 20.91, 20.91
第 2 次：20.95, 20.95, 20.95, 20.96, 20.96
第 3 次：20.95, 20.96, 20.96, 20.95, 20.95
```

全部在 20.9 附近，**稳定复现不出 23.06**。子代理排除了三种解释：
逐窗查全程平坦（无启动斜坡）、`target_ms` 六行全是 20.0、同机同二进制连续跑。

### 实质影响：一条被推翻的结论

`udp-nack-report.md` 第 6 节原本写：

> UDP 版 p50 略高是因为分片带来了额外的一次遍历与拷贝。
> 但这是可接受的代价：换来的是丢包时不会队头阻塞。

**这条不成立。** 实测 UDP 与 WebSocket 的 p50 都是 20.95ms，没有差异。

分片开销确实存在，但落在 `net_ms`（0.2ms 量级），被 20ms 抖动缓冲完全淹没 ——
这正是抖动缓冲「吸收微小抖动」的设计目的。

**UDP 的代价在 p95 而不是 p50**：3% 丢包时 p95 从 22ms 跳到 58ms。
取舍仍然成立，只是论据要换。

### 这一轮最值得记的方法论

**「跑新基准时顺手和历史数字对账」比「跑新基准」本身更有价值。**

如果只是把 14 组新数字写进报告，那条错误的 23.06 会一直躺在
`udp-nack-report.md` 里，继续被引用、继续误导优化方向
（会有人去优化「分片遍历与拷贝」，而那根本不是瓶颈）。

子代理做了这件事，是因为我在任务里明确要求了
「**有没有发现现有的实测数字与 progress.md 里记录的不一致**」。
下次写子代理任务时，这条应该成为固定项。

### 交付物

- `tools/bench/run-matrix.ps1`（57KB）：14 组矩阵、`-Quick`、端口隔离、
  进程清理、自愈重试
- `tools/bench/parse-metrics.ps1`：容错解析，三种口径分开处理
- `docs/latency-matrix.md` / `.json`：幂等覆盖，JSON 含时间戳 + 二进制指纹

### 三个工具缺陷（子代理自己修的）

| # | 缺陷 | 修法 |
| --- | --- | --- |
| 1 | 就绪检测轮询 `HasExited` 不可靠 | 改用 `Get-NetUDPEndpoint` 只读查端口归属，校验 `OwningProcess == 自己的 PID` |
| 2 | `Start-Process -RedirectStandardOutput` 让日志只剩 1 行 | 改用原始 .NET `Process` + `ReadToEndAsync` 直接收流 |
| 3 | 解析器用相对路径静默读不到文件 | 统一 `Resolve-Path` 转绝对路径，单独输出 `IOErrors` |

**第 2 条的诊断过程值得记**：我一开始假设是 `-NoNewWindow` 与重定向互斥，
子代理用 A/B 实测**否定了这个假设**（Plain 模式反而坏 1 次、NoNewWindow 0 次），
找到真因是 `ExitCode=-1` = `TerminateProcess(-1)` 的特征值 ——
**进程被外部杀死**（本仓库多 agent 并行，其他 agent 会批量
`Get-Process sidelensd,sidelenssend | Stop-Process -Force`）。

这提醒一件事：**多 agent 并行开发时，「我的测试进程被别人的清理脚本杀掉」
是真实风险**，工具必须能区分「程序缺陷」与「被外部杀」。

## 第二十四轮：三路并行（FEC 自适应 / Opus 调优 / 性能审计）+ B3 两条假设被证伪

### 一、Opus 参数调优（`opus-tune` 子代理，35 新测试）

三条推翻原有认知的结论：

**1. DTX 线上只带来 1.9 倍收益，达不到「接近 0」**

| 场景 | 无 DTX | 有 DTX | 载荷比 | 线上比 |
| --- | --- | --- | --- | --- |
| 纯静音 5 秒 | 26.8 kbps | 14.3 kbps | **9.1x** | **1.9x** |
| 语音 5 秒 | 46.0 kbps | 46.0 kbps | 1.0x | 1.0x |

载荷降 9.1 倍，但 1 字节的 DTX 包要配 **32 字节协议帧头** ——
静音期帧头占总字节的 **89.3%**。要拿到全部收益必须让协议层支持「空帧不发」。

**更严重的问题**：`AvSync::on_audio` 只在 `pop_due` 出队时调用。
DTX 不发包 → 没有帧可出队 → **主时钟停滞 → 静音期视频全部判 Wait，卡住最长 420ms**。

所以正确做法是 **DTX 帧也发包**（1 字节），用 0.4 kbps 换主时钟连续。

**2. in-band FEC 有两道硬边界**

| 边界 | 判据 | 实测 |
| --- | --- | --- |
| 码率 < 16 kbps | LBRR 塞不下 | 6~15.5 kbps 全部 FEC==PLC（8/8 逐采样相同） |
| 纯 CELT 模式 | CELT 没有 LBRR | `LowDelay` + 128kbps：真修复 **0/6** |

默认 Voip + 32kbps 走 SILK/Hybrid，FEC 有效（胜 13/16，RMSE 提升 3.16 倍）。
但若改成 `Application::Audio` 或码率提到 64kbps 以上，**FEC 会静默失效**。

**3. lookahead 恒为 312 采样 —— 推翻文档原有假设**

2.5 / 20 / 40 / 60ms 四档实测**全部是 312 采样 = 6500µs**。

原因：40/60ms **不是新的编码模式，而是把 2/3 个 20ms 帧打进一个包**
（TOC 解析证实：`config=15 code=2` 表示 2 个 20ms 内部帧）。

lookahead 是解码器侧 CELT 的重叠窗长度，与编码帧长无关。
**加大帧长不改善也不恶化音画同步。**

### 二、帧长权衡（实测三档）

| 帧长 | 载荷码率 | 帧头占比 | 线上码率 | 编码延迟 | 丢一包损失 |
| --- | --- | --- | --- | --- | --- |
| 20ms | 33.2 kbps | **27.8%** | 46.0 kbps | 20ms | 20ms |
| 40ms | 33.3 kbps | 16.1% | 39.7 kbps | 40ms | 40ms |
| 60ms | 33.6 kbps | **11.3%** | 37.9 kbps | 60ms | 60ms |

**载荷码率三档几乎相同** —— Opus 按「每秒多少比特」算，与帧长无关。
省下来的全是帧头。

**建议保持 20ms**：40/60ms 省的 6.3/8.1 kbps 相对 2 Mbps 视频链路是 0.3-0.4%，
而单包丢失从 20ms 变成 40/60ms。**在视频主导的链路里不值得。**

### 三、B3 虚拟摄像头：两条最强假设被证伪

**假设 A：`AddRegistryEntry` 写的注册表项能救 Start** → ❌ 证伪

严格 A/B（走完整 `VirtualCamera::start()` 流程，只切换 `--regentry`）：

| 模式 | Start 错误码 | 13 步轨迹 |
| --- | --- | --- |
| `baseline` | `0x80070057` | 完整 |
| `--regentry`（写 IsVirtualCamera + VCamClsid） | `0x80070057` | **完全一样** |

**假设 B：`2032C7EF` 是必需的会话接口** → ❌ 证伪

把 `GetService(2032C7EF)` 的返回值从 `MF_E_UNSUPPORTED_SERVICE` 改成
`E_NOINTERFACE`（两者语义不同：「知道服务但不提供」vs「不认识这个接口」）：

| 版本 | Start 错误码 | 13 步轨迹 |
| --- | --- | --- |
| `MF_E_UNSUPPORTED_SERVICE` | `0x80070057` | 完整 |
| `E_NOINTERFACE` | `0x80070057` | **完全一样** |

FrameServer **不区分这两个错误码** —— 说明它只是「拿不到就往下走」，不做分支。

### 四、一次自我推翻（方法论）

我最初以为 `AddRegistryEntry` 让错误码从 `0x80070057` 变成 `0xC00D36BD`，
还写进了文档。随后的严格 A/B 发现：

```text
模式 none / iscam_dword / iscam_binary / vcamclsid / customclsid
→ 全部 0xC00D36BD
```

连「什么都不设」都是 `0xC00D36BD`。

真因：那个 experiment 是**手写的简化流程**，漏了 4 个 `VCamCreate_*` 属性。
`0xC00D36BD` 是「缺属性」的错误码，不是「`AddRegistryEntry` 起了作用」。

**教训：A/B 的两边必须是同一条代码路径，只有被测变量不同。**
手写的简化流程不能当对照组。

### 五、SDK 头文件里的三个被忽略的 API（本轮发现）

直接读 `mfvirtualcamera.h`（而不是只看官方示例代码）发现：

| API / 属性 | 我们的状态 |
| --- | --- |
| `IMFVirtualCamera::AddRegistryEntry` | **从未调用**（已试，无效果） |
| `DEVPKEY_DeviceInterface_IsVirtualCamera` | **从未设置**（实测为空） |
| `MF_FRAMESERVER_VCAMEVENT_EXTENDED_*`（6 个） | **从未使用** |

顺带纠正：`VCamCreate_*` 的 GUID `6ac1fbf7` 在 **SDK 头文件、`devpkey.h`、
本机 10 个 MF DLL、注册表**里全部 0 命中 —— 它是官方示例自造的约定值。

**方法论**：当示例代码与 SDK 头文件不一致时，**头文件更权威** ——
它是系统契约，示例只是其中一种实现。

### 六、测试数

| 项目 | 数量 |
| --- | --- |
| 全 workspace | **553**（+35 Opus 调优） |
| `cargo fmt --check` | 0 |
| `cargo clippy -D warnings` | 0 |

## 第二十五轮：性能审计 + 修复两个真问题

### 一、审计结论：瓶颈不在分配，在两个「每毫秒都做」的事

`perf-audit` 子代理用只读插桩（`#[cfg(feature = "perf-probe")]`）测出：

| 排名 | 问题 | 成本 |
| --- | --- | --- |
| 🥇 | `udp_server.rs:597` 每 tick 调 `net_latency.summary()` | **83.9 µs/tick × 999/s = 83.8 ms/s**（单核 8.4%） |
| 🥈 | FEC 组状态永不回收 | **+0.52 MB/s**，内存永不回落 |
| 🥉 | `UdpDatagram::payload` 的 `to_vec` | 240 KB/s（且该字段全仓库 0 次读取） |

**收包路径整条均摊只有 1.21 µs/包，占 wall clock 0.056% —— 比 tick 小 69 倍。**

### 二、四个「看起来是问题、实测不是」

这条比 top 3 更有价值 —— 误优化会白花力气：

| 伪问题 | 真相 |
| --- | --- |
| `payload: f.payload` 是拷贝 | **是 move**（`RX_COMPLETED_PAYLOAD_BYTES = 0` 佐证） |
| 重复分片分支有分配 | **已在 `to_vec()` 之前 return**，零分配 |
| 「Rust 分配器开销」明显 | **测不出来** —— 两次 `to_vec` ≤0.48 µs，百分位是 83.9 µs |
| 热路径有 `format!` | **没有** —— 全在 2 秒统计块里 |

### 三、修了两个（主线补做）

**修复 1：百分位移出 tick**

`p50_cc` 只在 `cc.update()` 里用，而 `cc` 只在 `fresh` 时真正决策。
把百分位算进 `fresh` 分支 → 每 2 秒一次而不是每毫秒一次。
tick 总耗时 85.1 µs → 约 1.2 µs（**÷70**）。

**修复 2：接上 `prune_stale`**

`FecReceiver` 的三张表（`groups` / `group_meta` / `frame_meta`）
**此前从未被清理过** —— daemon 里一次都没调过清理函数。

实测验证（30 秒 / 30fps / 64KB 帧 / 3% 丢包 / FEC 自适应）：

```text
内存: 9.6 MB → 10.4 MB（增长 0.8 MB）
fec_groups_pruned = 603          ← 清理在工作
fec_recovered = 1275             ← FEC 仍正常工作
playable_ms = 21.12 / 22.42      ← 延迟未受影响
```

修复前同场景 `tracked_groups` 与 `frames_in` 严格 1:1，会涨到 55 MB 以上。

**修复 3（删 `UdpDatagram::payload`）未做** —— 审计自己说「比 #1 小三个数量级」，
而动 `transport` 的公开 API 跨 crate 不值 0.1 µs/包。

### 四、修正 `latency-matrix.md` 的一个错误机制描述

原文说「`frames_in` 同时统计 `Video` 与 `Retransmit` 两个分支」。

**错了**：`FrameType::Retransmit` 分支**从未命中**（`FRAMES_IN_RETX_TYPE = 0`，三场景全 0）。

真因：**发送端重传时直接重发缓存里的原始字节，`ty` 仍是 `Video`**。
`FLAG_RETRANSMIT` 常量定义了但全仓库无写入点。

结论（`frames` 是到达帧次不是去重帧数）对，机制已修正。

### 五、FEC 自适应实测（`fec-adaptive` 子代理）

| 场景 | overhead | hyst_switches | recovered | nack_seqs | playable p50/p95 |
| --- | --- | --- | --- | --- | --- |
| 8KB / 3% 丢包 | 27.5% | 0 | 51 | 6 | 20.93 / 21.97 |
| 8KB / 10% 丢包 | 25.5% | 1 | 112 | 128 | 20.96 / 21.98 |
| 64KB / 3% 丢包 | 22.8% | 10 | 346 | 147 | 21.06 / 22.41 |
| 8KB / 无丢包 | 28.2% | 0 | 0 | 0 | 20.99 / 22.03 |

**「无丢包」时 `recovered=0` 但 `overhead=28.2%`** —— 我查到了根因并修了：

链路干净时 `suggest_redundant` 返回 0，调用方钳到 1。
而 `observe` 的降幅度闸门 `abs_diff(2,1) = 1 < min_delta_down = 2` **把它挡住了**
—— 永久卡在 2。

修法：降到下限（`want <= 1`）时不受幅度限制。
语义上也该放行：`min_delta_down` 是为了挡住 2↔3 的 ±1 抖动，
而降到 1 是**触底**，下面没有更小的值可以抖回来。

**修复后实测**：

```text
fec_redundant: 2 → 1
fec_overhead:  28.2% → 15.9%
fec_red_bytes: 723,604 → 407,478（省 316 KB / 10 秒）
```

理想值是 0（完全不发冗余），但 `FecConfig` 要求 `redundant >= 1` ——
这是类型层面的约束，改它要动已稳定的 `fec.rs` XOR 引擎。
所以 15.9% 是**结构下限**，不是调参空间。

加了 3 个回归测试钉住这个行为（含「3→2 仍应被挡住」的反向保护）。

### 六、多 agent 并行的一个真实教训

审计员用「整文件备份 → 回滚」清理插桩，**覆盖了 `fec-adaptive` 同期
写入的自适应代码**。它自己记录了这件事并告知 `fec-adaptive` 重贴。

**教训：多 agent 并行改同一文件时，整文件备份/回滚不可用。**
应按行号精确删除自己插入的行。本项目无 git，只能靠这个纪律。

### 七、测试数

| 项目 | 数量 |
| --- | --- |
| 全 workspace | **599** |
| `cargo fmt --check` | 0 |
| `cargo clippy -D warnings` | 0 |
| `cargo build --release` | 0 |

## 第二十六轮：FEC 自适应落地 + 结构下限的发现

### 一、自适应冗余度已接入（`fec-adaptive` 子代理）

`crates/sidelens-core/src/fec_link.rs` 新增：

| 项 | 内容 |
| --- | --- |
| `suggest_redundant(group_size, loss_rate, target)` | 用二项分布求「让 P(可恢复) ≥ target」的最小冗余度 |
| `binomial_cdf` | 二项累积分布，与穷举对拍验证 |
| `unresolve_loss` | 从帧级丢包率反解回包级 |
| `LossEstimator` | 用 NACK 量反解丢包率，滑动窗口 + 指数平滑 |
| `RedundantHysteresis` | 升降幅度 + 驻留帧数双重闸门 |

发送端 `--fec 2` 现在是**初始值**，之后由自适应接管（`fec_adaptive=true`）。

### 二、四档场景实测

| 场景 | overhead | hyst_switches | recovered | nack_seqs | playable p50/p95 |
| --- | --- | --- | --- | --- | --- |
| 8KB / 3% 丢包 | 27.5% | 0 | 51 | 6 | 20.93 / 21.97 |
| 8KB / 10% 丢包 | 25.5% | 1 | 112 | 128 | 20.96 / 21.98 |
| 64KB / 3% 丢包 | 22.8% | 10 | 346 | 147 | 21.06 / 22.41 |
| 8KB / 无丢包 | 28.2% → **15.9%** | 0 → 1 | 0 | 0 | 20.99 / 22.03 |

### 三、我修掉的一个真 bug：降到下限被幅度闸门挡住

**症状**：无丢包时 `fec_recovered = 0` 但 `fec_overhead = 28.2%`。

**根因**：`suggest_redundant` 返回 0 → 钳到 1 → `abs_diff(2,1) = 1 < min_delta_down = 2`
→ **永久卡在 2**。

**修法**：`at_floor = want <= 1` 时跳过幅度检查。

**修复后**：

```text
fec_redundant: 2 → 1
fec_overhead:  28.2% → 15.9%
fec_red_bytes: 723,604 → 407,478（省 316 KB / 10 秒）
```

加 3 个回归测试（含「3→2 仍应被挡住」的反向保护）。

### 四、一个结构下限（如实记录）

理想值是 `redundant = 0`（完全不发冗余），但 `FecConfig` 要求 `redundant >= 1`。
这是类型层面的约束，改它要动已稳定的 `fec.rs` XOR 引擎。

所以 **15.9% 是结构下限**，不是调参空间。要真正降到 0 需要让
`should_fec` 之外的路径能表达「这一帧完全不生成 FEC 组」。

### 五、当前状态

```text
cargo fmt --check         0
cargo clippy -D warnings  0
cargo test                599 passed / 0 failed
cargo build --release     0
端到端（5% 丢包 + FEC + Opus）  跑通
```


## 第二十七轮：FSM 类别 GUID 反推成功 → 假设证伪（B3 收敛）

### 一、上一轮留下的具体任务：找出 `KSCATEGORY_FSM_VIRTUAL_CAMERA` 的 GUID

上一轮只做到「知道 `mfsensorgroup.dll` 0x85e40 有个字符串，但不知道 GUID」。
本轮拿到了。

### 二、两次失败路径（值得记，因为都是「看起来对」的错法）

| 尝试 | 做法 | 结果 |
| --- | --- | --- |
| 1 | 假设「字符串表顺序 = GUID 表顺序」，从 `KSCATEGORY_VIDEO_CAMERA` 的 GUID 位置按槽位推 | ❌ 错位一个（推到 `CATEGORY_SENSOR_GROUP`），且槽位 1/2/4/5/7 是垃圾 `{80085DE8-0001-...}` —— 那是指针/重定位数据，不是 GUID |
| 2 | 搜「引用该字符串 VA 的绝对指针」 | ❌ 0 命中。x64 用 `lea rip-relative`，数据段里根本没有绝对地址 |

### 三、成功路径：24 字节步长的 `[GUID][名字指针]` 混合表

`mfsensorgroup.dll` `.rdata` 0x90cd8 起，每项 24 字节：前 16 字节 GUID，后 8 字节是指向名字字符串的 VA。

```text
0x90cd8  {24E552D7-6523-47F7-A647-D3465BF1F5CA}  ->  KSCATEGORY_SENSOR_CAMERA
0x90cf0  {5C7164F5-7E6A-42B6-81F3-68C0B1FAAA0C}  ->  KSCATEGORY_FRAMEPROVIDER
0x90d08  {588C8D20-C0E3-4FD3-B511-8F2F692156F8}  ->  KSCATEGORY_FSM_VIRTUAL_CAMERA  ★
0x90d20  {6994AD05-93EF-11D0-A3CC-00A0C9223196}  ->  KSCATEGORY_VIDEO
0x90d38  {65E8773D-8F56-11D0-A3B9-00A0C9223196}  ->  KSCATEGORY_CAPTURE
0x90d50  {00000000-...}                          ->  image
0x90d68  {00000000-...}                          ->  perception
0x90d80  {00000000-...}                          ->  stream
```

**关键洞察**：后半段 GUID 全零但名字是 `image`/`perception`/`stream`/`video`/`RGB32` ——
那是**媒体类型描述**，不是 KS 类别。所以这是**混合表**，
**必须靠名字指针确认身份，不能靠位置推**。前两次就是败在靠位置推。

**交叉验证**：全盘扫 `C:\Windows\System32` 4532 个 `.dll`/`.exe`，
这 16 字节只出现在 11 个 FrameServer 相关模块里（`FrameServer*.dll` ×5、
`FsNVSDeviceSource`、`MFCaptureEngine`、`mfcore`、`MFDshowReverseBridge`、
`mfsensorgroup`、`Windows.Media`）。随机猜的 GUID 不会这样。

**本机状态**：`Control\DeviceClasses\{588c8d20-...}` 键**存在、ACL 正常、
SubKeyCount=0** —— 系统备好了这个类别，没有设备登记进去。

### 四、A/B 实验：假设证伪，而且方向相反

给 `MFCreateVirtualCamera` 的 `categories` 参数加了三组对照
（新增 `VirtualCamera::start_with_categories` + `--category vc|fsm|both`，
走完整 `start_full` 同一条代码路径，只切换类别）：

```text
组     Start 错误码    DLL 是否被加载    DeviceClasses(FSM) 节点
vc     0x80070057      是，15 行          0
fsm    0x80070057      否                 0
both   0x80070057      否                 0
```

**第一版结论是错的**。直接连跑 `vc → fsm → both`，后两组「DLL 未加载」
看起来像因果，实际是**顺序污染**：`Start` 失败后设备记录不自动清除
（`register.rs` 默认 `remove_on_drop = false`），第一组留下的
`SWD\VCAMDEVAPI\<hash>` 幽灵节点类别写死为 `VIDEO_CAMERA`，后两组复用了它。

修正做法：**每组前 `pnputil /remove-device` 清幽灵设备，再跑两轮、第二轮顺序反转**。

```text
第一轮 vc → fsm → both          第二轮 both → fsm → vc
  vc   : DLL 加载 15 行            vc   : DLL 加载 15 行
  fsm  : DLL 未加载                fsm  : DLL 未加载
  both : DLL 未加载                both : DLL 未加载
```

两轮完全一致 → 不是顺序效应。但差异方向是「登记到 FSM 类别后 FrameServer
**连 DLL 都不加载**」—— 即 FSM 类别让情况**变坏**。

> **结论：`KSCATEGORY_FSM_VIRTUAL_CAMERA` 是输出侧类别**
> （FrameServer 把自己生成的虚拟摄像头**登记到**那里给消费端看），
> **不是输入侧的查找条件**。我们作为「提供者」登记进去，反而让设备不可识别。

保留 `register.rs::fsm_virtual_camera_category()`（含完整推导注释），
将来做「FrameServer 输出 → 我们消费」方向会用到。

### 五、18 个假设之后的一个判断

14 轮排查、18 个假设，每次都是「猜 → 试 → 错误码不变」。
这个模式本身在说：**`E_INVALIDARG` 不是「某个参数不对」，而是「这条路径根本不通」**。

FrameServer 完整走完 13 步（`CreatePresentationDescriptor` 成功、流描述符 OK、
`IKsProperty` 成功），**然后主动 `Shutdown`** —— 不是「检查失败」，
是「检查全过，但我决定不用你」。

这种形态最可能是 FrameServer 内部有一份「允许的虚拟摄像头来源」白名单，
判据是设备节点是否落在真正的摄像头驱动栈下。我们的设备是
`Enum\SWD\VCAMDEVAPI\*` + `\Driver\SoftwareDevice` —— **API 路线的结构性限制**。

**决策**：B3 暂停在「已充分排查、结论是需换 DriverPackage 路线」，记入
`blockers.md`。剩余时间投入能确定完成的功能。

### 六、本轮并行推进（两个子代理，写入范围不重叠）

| 子代理 | 范围 | 任务 |
| --- | --- | --- |
| `h264-bitrate` | 仅 `crates/sidelens-codec-mf/**` | `H264Encoder::set_bitrate` + 实测 mid-stream 改码率是否生效 |
| `opus-wire` | 仅 `apps/sidelenssend/**` | `--opus-frame-ms/--opus-dtx/--opus-inband-fec/--opus-loss` 四个开关 + 实测矩阵 |



## 第二十八轮：抖动估计公式的真 bug + 抖动缓冲自适应落地（p50 20.95 → 13.77ms）

### 一、一个藏了很久的公式错误

`crates/sidelens-core/src/jitter.rs` 的 `push_at` 里，抖动估计是这么写的：

```rust
let transit      = arrived_at_us   as i128 - frame.send_ts_us as i128;
let last_transit = last_arrival_us as i128 - frame.send_ts_us as i128;  // 同一个 send_ts
let delta = (transit - last_transit).unsigned_abs() as f64;
```

`send_ts_us` 在减式里**自己抵消掉了**：

```text
delta = |(arrived - send_ts) - (last_arrival - send_ts)|
      = |arrived - last_arrival|          <- 到达间隔，不是抖动
```

60fps 下帧间隔 16.67ms，于是 `jitter_ewma` 恒在 16.7ms 附近，
而回环真实抖动 < 1ms —— **差了 20 倍**。

正确形式要拿**上一帧自己的** `send_ts_us`（RFC 3550 §6.4.1 的 `|D(i-1,i)|`）：

```text
D(i-1,i) = (R_i - R_{i-1}) - (S_i - S_{i-1})
```

这才是「网络把间隔拉开了多少」，帧间隔被减掉了。为此新增字段
`last_send_ts_us`（原来只存了 `last_arrival_us`）。

**为什么这个 bug 一直没被发现**：`avg_jitter_ms` 只出现在日志里，
而 16.7ms 这个数字看起来「像个合理的抖动值」—— 没人会去质疑它。
直到把它接进 `adapt` 当输入，才发现整条自适应链路算出来的都是错的。

修完实测：`jitter_ewma` 从恒定的 ~16.7 变成 **0.04 到 4.23ms**（随链路真实波动）。

### 二、`shrink` / `grow` 从未被调用

全仓库搜索：

```text
crates/sidelens-core/src/jitter.rs:232  pub fn shrink(&mut self, elapsed_ms: f64)
crates/sidelens-core/src/jitter.rs:238  pub fn grow(&mut self, extra_ms: f64)
crates/sidelens-core/tests/jitter.rs:99 jb.shrink(1000.0);      <- 只有测试
```

**生产路径一次都没调用。** 于是 `target_ms` 永远是 `JitterConfig::default()`
的 20.0 —— 而实测 `playable_ms` p50 也永远是 20.95ms。

那 20.95 里 **20ms 是纯等待**，只有 0.95ms 是真实传输 + 处理。
**延迟优化的全部空间都在这个 `target_ms` 上**，而它是个常量。

### 三、新增 `JitterBuffer::adapt(now_us)`

不是简单地把 `shrink`/`grow` 接上 —— 那两个方法是**固定步进**（每秒正负 2ms），
从 20 降到 10 要 5 秒，而链路状态可能 1 秒就变了。

新方法按**实测抖动乘以安全系数**直接算出目标：

```text
want = (jitter_ewma_ms * 2.5).clamp(min_ms, max_ms)
```

系数 2.5 的依据：`jitter_ewma` 是 `0.9/0.1` 平滑后的**均值**，乘 2.5 大致覆盖 3 sigma。
小于 2 会在抖动轻微变大时开始丢帧，大于 3 则退化成「不敢降」。

**三个闸门（缺一个就震荡）**：

| 闸门 | 值 | 不加会怎样 |
| --- | --- | --- |
| 下限 `min_ms` | 10ms | UDP 乱序和调度抖动会直接把帧判迟到 |
| 单次降幅 `ADAPT_MAX_STEP_MS` | 2ms | 链路平静 100ms 就把目标砍到底，下波抖动全部丢帧 |
| 驻留期 `ADAPT_HOLD_US` | 200ms | `target_ms` 跟着抖动 EWMA 一起抖 —— 正反馈 |

**升降不对称**：升是全额立即生效（抖动变大时每一帧都在丢，等不起），
降才受单步上限约束。降错的代价是**丢帧**（可见卡顿），升错的代价只是多一点
延迟（不可见）。

### 四、实测：p50 20.91 降到 13.77ms（降 34%）

20 秒回环，60fps，8KB 帧，无丢包，2 秒一步的 `adapt`：

| 时刻 | target_ms | playable p50/p95 | jitter_ewma | adapt 次数 |
| --- | --- | --- | --- | --- |
| 2s | 18.0 | 20.91 / 21.86 | 0.04 | 1 |
| 4s | 16.0 | 19.21 / 21.56 | 0.07 | 2 |
| 6s | 14.0 | 18.47 / 21.32 | 0.12 | 3 |
| 8s | 12.0 | 17.46 / 20.89 | 0.20 | 4 |
| **10s** | **10.0** | **16.44 / 20.83** | 0.31 | 5 |
| 12s | 10.0 | 15.52 / 20.81 | 0.58 | 5 |
| 14s | 10.0 | 14.73 / 20.37 | 0.73 | 5 |
| 16s | 10.0 | 14.13 / 20.35 | 1.75 | 5 |
| 18s | 10.0 | 13.60 / 20.25 | 1.31 | 5 |
| 20s | 10.6 | 13.64 / 20.28 | 4.23 | 6 |
| 22s | 10.0 | **13.77 / 20.24** | 2.92 | 7 |

**target 在 10 秒内从 20 收敛到 10（下限）**，p50 跟着从 20.91 降到 13.77。

两点值得注意：

1. **p50 在 target 稳定后还继续降**（16.44 到 13.77）。因为 `playable_ms` 是
   `now - send_ts`，包含了「帧在队列里等待」的时长；target 降下来后队列积压
   也逐步排空，所以有滞后。最终 p50 约 13.77 仍略高于 target=10，差额是
   真实传输 + 处理 + 队列排空时间。
2. **抖动 EWMA 后期涨到 4.23ms**（回环上不该这么大）。原因是 target 降到 10 后
   队列变浅，同样的调度抖动更容易表现为「到达间隔变化」。`adapt` 在第 20 秒
   正确地把它抬到 10.6，第 22 秒又降回 10 —— 这是闸门在起作用，不是 bug。

### 五、测试（16 个，含 3 个针对公式 bug 的回归）

| 测试 | 守住什么 |
| --- | --- |
| `jitter_is_transit_delta_not_arrival_gap` | 网络稳定 + 60fps 时抖动必须约 0（若约 16.7 说明又算成到达间隔） |
| `jitter_reflects_only_network_delay_variation` | 帧间隔稳定、晚到 2ms 则抖动恰为 2.00ms |
| `jitter_ignores_frame_interval_changes` | 帧间隔从 16.7ms 变 33.3ms（掉帧）不算抖动 |
| `adapt_lowers_target_when_jitter_is_small` | 小抖动下调，且单次降幅不超过 2ms |
| `adapt_raises_target_immediately_when_jitter_grows` | 大抖动上调**全额生效**（不受单步上限约束） |
| `adapt_target_tracks_safety_factor` | 目标 = 抖动乘 2.5（用 12ms 抖动走上调路径才能测到） |
| `adapt_respects_floor` | 反复降不会低于 `min_ms` |
| `adapt_holds_off_within_dwell_window` | 驻留期内不动，期满可动 |
| `adapt_does_nothing_before_jitter_is_established` | 0 帧 / 1 帧时不动（样本不可信） |
| `adapt_is_idempotent_at_convergence` | 已收敛时再调不产生变化 |

**写测试时踩的坑**：第一版用 4ms 抖动测 `safety_factor`，断言
`target == 4 * 2.5 == 10`，实际得到 18 —— 因为 `10 < 20` 走的是**下调**路径，
被单步上限 2ms 挡住（20-2=18）。要用 12ms（12*2.5=30 > 20）才能走上调路径
测到系数本身。

### 六、本轮状态

```text
cargo fmt --check         0
cargo clippy -D warnings  0
cargo test --workspace    625 passed / 0 failed（+24）
cargo build --release     0
端到端（20s 回环）        playable p50 20.91 -> 13.77ms
```



## 第二十九轮：码率反馈回路打通（拥塞控制器 -> RttReply -> 编码器）

### 一、一个架构上的断点

拥塞控制器（`CongestionController`）在**接收端**跑 —— 它需要延迟梯度样本，
而那些样本只有收到包的一方才拿得到。但真正执行码率的是**发送端**的编码器。

中间缺一条反馈回路。此前：

```text
接收端: cc.update() -> target_bps   （算出来了，只打在日志里）
发送端: H264Encoder 固定 2 Mbps      （从来不读 cc 的结果）
```

`H264Encoder::set_bitrate` 本轮由 `h264-bitrate` 子代理实现（实测 mid-stream 生效），
但**没有任何调用点** —— 缺口在「怎么把接收端的决策传到发送端」。

### 二、方案：复用 `RttReply` 而不是新增帧类型

协议里已经有现成的反向通道：`RttProbe`（发送端 -> 接收端）/ `RttReply`（接收端 -> 发送端），
每 2 秒一次。两者语义上都是「接收端告诉发送端链路的状况」，
再开一个新帧类型不如复用它。

```rust
pub struct RttReply {
    pub nonce: u32,
    pub t1_us: u64,
    pub t2_us: u64,
    pub t3_us: u64,
    // 新增（第二十九轮）
    pub target_bps: Option<u64>,
    pub loss_permille: Option<u16>,
}
```

**向后兼容**靠 `Option` + `#[serde(default)]` + `skip_serializing_if`：

| 组合 | 行为 |
| --- | --- |
| 老接收端 -> 新发送端 | 缺字段反序列化成 `None`，发送端保持码率不变 |
| 新接收端 -> 老发送端 | 老版本 serde 忽略未知键，时钟同步照常 |
| 新 <-> 新 | 码率自适应生效 |

**丢包率用整数千分之一而不是 f64**：JSON 浮点在跨语言/跨版本解析时容易出精度问题，
而 0.1% 的精度差别不影响任何码率决策。

### 三、发送端的三道闸门

不是收到建议就设 —— 直接设会抖：

| 闸门 | 值 | 不加会怎样 |
| --- | --- | --- |
| 范围钳制 | 100 kbps - 50 Mbps | 老版本/异常对端可能给 0 或极大值，0 会让编码器输出空帧 |
| 变化幅度 | 5% | 控制器的百分比爬升每 2 秒只涨几个百分点，每次都设会让 `SetValue` 每秒调一次，编码器反复重算 QP 曲线，输出反而更不稳 |
| 实际值比较 | `ratio >= 1.05 || ratio <= 0.95` | 同上 |

### 四、实测：回路通了

10% 丢包 + H.264，15 秒：

```text
发送端:
  按接收端建议调整编码码率 from=2000000 to=1772631 hint=1772631 loss_permille=0
  按接收端建议调整编码码率 from=1772631 to=1256779 hint=1256779 loss_permille=73

接收端:
  target_kbps=1785  state=Hold  decisions=1
  target_kbps=1257  state=Hold  decisions=2
  target_kbps=1281  state=Hold  decisions=3
```

**接收端算出 1257 kbps，发送端把编码器从 2 Mbps 降到 1257 kbps。**
第二条日志里 `loss_permille=73` 说明当时实测丢包率 7.3% —— 与注入的 10%
（减去已被 NACK 补回的部分）吻合。

最终统计里有 `bitrate_adaptations=2 last_rate_hint=1971935`。
后者是最后一次收到的建议（1971 kbps，链路恢复后控制器又往上爬），
而 `bitrate_adaptations` 仍是 2 —— 因为 1971 与当时的 1257 差 57%，
**本该触发第 3 次调整**。查下来是发送端在收到那条建议之前就结束了（12 秒时长），
不是逻辑问题。

### 五、`h264-bitrate` 子代理的实测（并入本轮）

`H264Encoder::set_bitrate` 的关键结论：**SetValue 一定返回 S_OK，
但改码率有没有实际改变输出，取决于内容复杂度**。所以实验必须用**同内容同帧号**对照：

| 内容 | 对照后窗口 | 降档后窗口 | 比值 |
| --- | --- | --- | --- |
| 低熵（渐变+竖条） | 1051 B/帧 | 1051 B/帧 | 1.00 不生效 |
| 中熵（半幅噪声块平移） | 7918 B/帧 | 2936 B/帧 | **0.37 生效** |
| 高熵（逐帧全噪声） | 49233 B/帧 | 49233 B/帧 | 1.00 不生效 |

逐帧跳变干净：`28:8851 29:8588 30:9064 | 31:3078 32:2890 33:2719` —— **下一帧就生效**。

**升降不对称**（实测，对控制器调参重要）：

| 组 | 0-29 帧 | 30-59 | 60-119 | 120-209 |
| --- | --- | --- | --- | --- |
| 对照 全程 2M | 2937 | 1762 | 2065 | 2094 |
| 降档 2M->500k | 2937 | **725** | 679 | 679 |
| 升档 500k->2M | 1807 | **1555** | 1905 | 1969 |

降档瞬时且不回弹；升档要 **60-120 帧（2-4 秒）**才追上对照。
所以控制器「恢复期加速爬升」的逻辑是必要的，否则画面会糊很久。

**重建成本实测 `H264Encoder::new` 约 195 ms**（20 次：均值 195.7，最快 190.0，最慢 211.6）。
30fps 下约 6 帧黑屏 + 重发 SPS/PPS + IDR。所以运行中改码率（不用重建）是对的路线。

**两个参数实测无效**：

| 只改这个 | 后窗口字节/帧 | 相对对照 |
| --- | --- | --- |
| 对照 | 7918 | 1.00 |
| mean -> 500k | 2936 | **0.37** |
| max -> 500k | 7918 | 1.00 |
| buffer -> 500k | 7918 | 1.00 |

`mfh264enc` **只认 mean**，`max` 与 `buffer` 设什么都不改变输出。
保留 1.5x max 是为了给硬件/第三方 MFT 兜底（`with_info` 允许指定别的 MFT），实测无害。

### 六、踩到的两个坑

**1. `bps * 3 / 2` 会溢出。** 原写法在 `bps > u32::MAX/3` 时先溢出再除，
结果**小于入参**，`max < mean` 语义反了。改成 `bps.saturating_add(bps / 2)`。

**2. 替换代码时按「行数」而没按「内容」对齐。** 我把 7 行的旧块当成 6 行替换，
留下一个多余的 `}`，报 `unexpected closing delimiter`。教训：`splice(start, count, ...)`
的 `count` 必须用 `findIndex` 之后重新数，不能凭记忆。

### 七、本轮状态

```text
cargo fmt --check         0
cargo clippy -D warnings  0
cargo test --workspace    630 passed / 0 failed（+5）
cargo build --release     0
端到端                    接收端 target=1257kbps -> 发送端编码器 2Mbps 降到 1257kbps
```


## 第三十轮：音频 codec 声明缺口（audio_decoded=0 但 audio_played 正常）

### 一、怎么发现的

跑完整回归（H.264 + Opus + FEC + 丢包）时注意到一组矛盾的统计：

```text
audio_frames=502  audio_played=501  audio_decoded=0  audio_decode_failed=0
```

`audio_played` 在涨，说明音频在「播」；但 `audio_decoded=0` 且
`audio_decode_failed=0` —— **一次解码都没尝试过**。

### 二、根因：接收端不知道该不该解码

`udp_server.rs` 里创建 Opus 解码器的条件是环境变量：

```rust
let mut opus_dec = match std::env::var("SIDELENS_OPUS") {
    Ok(v) if v == "1" || v.eq_ignore_ascii_case("true") => { ... }
    _ => None,
};
```

没设这个变量 → `opus_dec = None` → 解码分支整块跳过 →
`audio_played` 照样 `+= 1`（它只表示「从抖动缓冲出队了」）。

**所以「播」的是 Opus 码流的字节被当成 16-bit PCM 的噪声。**

### 三、为什么不能自动检测

注释里已经写清楚了（不是我这次才想到）：

> 接收端**无法从码流判断**载荷是 Opus 还是原始 PCM —— 两者都是不透明的字节。
> 协议帧头里也没有编解码器字段。
>
> 自动检测（比如「长度像 Opus 帧就当 Opus」）会在「Opus 帧恰好 1920 字节」时
> 误判，而那个长度是可能的（1275 字节上限乘 2 声道 = 2550，1920 在范围内）。

结论：**必须由发送端显式声明**，而协议里缺这个字段。

### 四、方案：复用 Config 帧

`FrameType::Config` 本来就是「告诉接收端怎么解读后续数据」的通道
（现在用它协商 `fec_group_size` / `fec_redundant`）。音频 codec 属于同一类信息。

发送端 `send_config` 的 JSON 从两字段变三字段：

```json
{"fec_group_size":64,"fec_redundant":1,"audio_codec":"opus"}
```

接收端新增独立解析函数 `parse_audio_codec_payload`，**不塞进**
`parse_fec_config_payload`：

| 理由 | 说明 |
| --- | --- |
| 语义无关 | FEC 字段缺失/非法不该影响 codec 声明生效 |
| 契约冲突 | `parse_fec_config_payload` 的契约是「两个 FEC 字段都没解析出来就返回 None」，塞进去会破坏它 |

收到 `"opus"` 且当前没有解码器 → 创建；收到 `"pcm"` 且有解码器 → 销毁
（否则会把 PCM 当 Opus 解）。

### 五、实测：audio_decoded 从 0 变 952

最终回归（20 秒，H.264 + Opus + FEC-2 + 5% 丢包）：

```text
修复前:  audio_frames=502  audio_played=501  audio_decoded=0
修复后:  audio_frames=952  audio_played=952  audio_decoded=952  audio_decode_failed=0
```

daemon 日志里能看到声明生效：

```text
INFO sidelensd::udp_server: 按 Config 声明启用 Opus 解码（发送端说 audio_codec=opus）
```

**向后兼容**：老发送端不发这个字段 → `parse_audio_codec_payload` 返回 `None`
→ 保持原来的行为（不创建解码器），环境变量 `SIDELENS_OPUS=1` 仍然有效。

### 六、同一轮确认的延迟自适应

回归里同时确认抖动自适应在工作：

```text
target_ms="13.0"  jitter_adapt=11  jitter_up=1  jitter_down=10  jitter_ewma_ms="0.38"
playable_ms="25.52/35.51"
```

`target_ms` 从 20 降到 13（11 次调整，其中 10 次降 1 次升）。
这次 `playable p50` 是 25.52 而不是 13.77 —— 因为**这一轮同时开了 H.264 +
5% 丢包 + FEC + 音频**，`playable_ms` 包含了等待重传的时长。
纯回环无丢包的 13.77ms 见第二十八轮。

### 七、一个操作失误（值得记）

我用「按行号 splice」改代码时，把 `ensure_config` 的整个函数体当成了
`send_config` 的一部分删掉，编译报 7 个错误。

**根因**：`lines.findIndex(...)` 找到的是**注释块里的那行**而不是函数签名
（注释和代码里都出现了 `let config = self.config;`），后续 `splice` 的
`count` 就全错位了。

**修法**：从 `target/fec-adaptive-backup/` 里找备份 —— 但那个备份是
`fec-adaptive` 之前的版本，没有 `ensure_config`。最后是**按 rustc 的错误
信息 + 周围代码结构手工重建**的。

**教训**：

1. 用 `splice` 前必须打印出实际要删的行，确认内容而不是只信行号
2. 备份要**每次改动前**做，不能依赖「上次某个 agent 留下的备份」

### 八、本轮状态

```text
cargo fmt --check         0
cargo clippy -D warnings  0
cargo test --workspace    641 passed / 0 failed（+11）
cargo build --release     0
端到端                    audio_decoded 0 -> 952；bitrate_adaptations=5；jitter_adapt=11
H.264 dump                2,251,119 字节（20 秒，1201 帧）
```

## 第三十一轮：Opus 三个开关接入 + 两处硬编码帧长的真 bug

### 一、`opus-wire` 子代理交付的开关

| 开关 | 取值 | 默认 | 落点 |
| --- | --- | --- | --- |
| `--opus-frame-ms` | `2.5\|5\|10\|20\|40\|60` | 20 | `OpusConfig.frame_ms` → `frame_samples` / `frame_duration_us` / 音频节拍 |
| `--opus-dtx` | 布尔 | 关 | `OpusEncoder::set_dtx` |
| `--opus-inband-fec` | 布尔 | 关 | `OpusEncoder::set_inband_fec` |
| `--opus-loss` | `0-100` | 0 | `OpusEncoder::set_packet_loss_perc` |
| `--audio-silence-after <秒>` | f64 | 无 | 合成源改静音（DTX 必须靠真静音才触发） |

实测矩阵（8 秒/组，release）：

| 配置 | frame_ms | rx 音频 kbps | 每帧字节 | dtx 帧 |
| --- | --- | --- | --- | --- |
| f2.5 | 2.5 | 137.6 | 43.0 | 0 |
| f5 | 5 | 84.8 | 53.0 | 0 |
| f10 | 10 | 49.7 | 62.0 | 0 |
| **f20** | 20 | 45.5 | 113.4 | 0 |
| f40 | 40 | 39.0 | 193.9 | 0 |
| f60 | 60 | **36.8** | 274.6 | 0 |
| f20-fec10 | 20 | 29.5 | 73.8 | 0 |
| **f20-dtx** | 20 | 29.6 | 73.8 | **182** |

**帧长越长音频越省**（137.6 → 36.8 kbps，3.7 倍），代价是单帧丢失的空洞从
2.5ms 变成 60ms。**in-band FEC 降 35%**（113.4 → 73.8 字节），但只在
`--opus-loss > 0` 时产生冗余（`f20-fec0` 与 `f20` 字节数完全一致）——
这是 libopus 的设计，不是 bug。

**DTX 必须换输入才测得出**：DTX 靠 VAD 判静音，440Hz 恒定正弦永远过不了阈值。
加 `--audio-silence-after 3` 后 `opus_dtx_frames=182/401`（45%，正好对应后 5 秒静音期），
`opus_dtx_bytes=182` = 每帧 1 字节，与 `docs/opus.md` 里 20ms DTX 包 1 字节的记录一致。

### 二、子代理发现的真 bug：发送端从未调 `HighResTimer`

接 `--opus-frame-ms` 时才暴露：Windows 默认定时器分辨率 15.625ms，
`interval(2.5ms)` 实际只有 64Hz，配合 `MissedTickBehavior::Skip` 每 tick 只补一帧。

```text
修前: --opus-frame-ms 2.5 跑 8 秒 -> audio_pts_span_ms=1310（应 8000，只走了 16%）
      接收端 audio_dropped_late=469/509（几乎全丢）
修后: 2.5 / 5 / 10ms 三档 audio_pts_span_ms 全部 8000±10
```

**为什么一直没暴露**：daemon 早就调了 `HighResTimer`，而发送端的节拍原本固定 20ms
—— 20ms 在 15.625ms 分辨率之上，取整误差被 `Skip` 掩盖了。只有把帧长降到 2.5ms
才会踩到。

### 三、我修的两处接收端硬编码（子代理的写入范围外）

子代理报告了 5 处硬编码 `OpusConfig::default()`（= 20ms），其中 3 处是功能性的：

| 位置 | 发送端用 60ms 帧时的后果 |
| --- | --- |
| 解码器 `frame_samples` | 缓冲只要 960 采样，解码报 "buffer too small" |
| `opus_pcm` 长度 | 同上 |
| `AudioJitterBuffer` 的 `frame_duration_us` | AvSync 主时钟每帧少推进 40ms |
| `AudioFrame::new` 的 duration | 同上 |

实测证据（子代理）：`audio_decode_failed=134/134`，一帧都没解出来。

**修法**：引入 `opus_cfg` 单一来源，从 `SIDELENS_OPUS_FRAME_MS` 读（默认 20），
校验失败回退默认。五处全部改用它。

### 四、但环境变量不够 —— 又发现第三个真 bug

改成环境变量后仍然 `audio_decoded=0`。查下来：

```rust
let mut fec = FecOutbound::from_args(&args);  // 不加 --fec 时返回 None
```

而 `Config` 帧**只由 `FecOutbound::send_config` 在切换 FEC 配置时发出**。
没启用 FEC → `fec = None` → 那行代码永远不执行 → **音频 codec 声明与帧长声明
一个都送不到接收端**。

也就是说：我上一轮（第三十轮）做的 codec 声明，**只在加了 `--fec` 时才有效**。
这是我上一轮没测到的场景 —— 当时的验证脚本恰好都带了 `--fec 2`。

**修法**：在主循环开始前无条件发一次 `Config`（发三份而不是两份，因为它承载的
信息更多）。

### 五、实测：60ms 帧全链路打通

```text
发送端: INFO 已发送初始 Config（音频 codec 与帧长声明） audio_codec="opus" audio_frame_ms=60.0
接收端: INFO 按 Config 声明启用 Opus 解码（发送端说 audio_codec=opus）
接收端: INFO 按 Config 声明调整音频帧长，已重建解码器与抖动缓冲
        from_ms=20.0 to_ms=60.0 frame_samples=2880 frame_duration_us=60000

最终: audio_frames=134  audio_played=134  audio_decoded=134  audio_decode_failed=0
      audio_depth_ms="60.0/60.0"（缓冲深度跟着帧长走了）
```

对比修复前：`audio_decoded=0  audio_decode_failed=134`。

### 六、本轮修掉的三个 bug 汇总

| # | bug | 影响 | 谁发现 |
| --- | --- | --- | --- |
| 1 | 发送端从未调 `HighResTimer` | 短帧（2.5/5/10ms）节拍只跑 16%，音频几乎全丢 | 子代理 |
| 2 | 接收端 5 处硬编码 20ms 帧长 | 40/60ms 帧解码全失败 | 子代理报告，我修 |
| 3 | `Config` 帧只在 FEC 启用时发送 | **音频 codec 与帧长声明在无 FEC 时全部丢失** | 我（验证 60ms 时发现） |

第 3 个是我上一轮引入的缺陷 —— 当时验证脚本都带了 `--fec 2`，恰好绕过了它。
**教训：新增一条声明通道时，必须测「关闭其他功能」的组合**，否则它会寄生在
某个不相关的开关上。

### 七、本轮状态

```text
cargo fmt --check         0
cargo clippy -D warnings  0
cargo test --workspace    647 passed / 0 failed（+6）
cargo build --release     0
端到端 60ms 帧            audio_decoded=134/134，0 失败
```

## 第三十二轮：WebSocket 路径补上自适应（p50 20.92 → 11.92ms）

### 一、上一轮漏掉的一半

第二十八轮给 `JitterBuffer` 加了 `adapt()`，但只接进了 `udp_server.rs`。
默认的 WebSocket 路径（`sidelensd <port>`，不加 `--udp`）**完全没接** ——
`target_ms` 仍恒为 20，p50 仍恒为 20.95。

审计命令：

```text
grep "jb.adapt" apps/  ->  只有 udp_server.rs:1437 一处
grep "JitterBuffer::new" apps/  ->  main.rs:258（WebSocket 路径，没有 adapt）
```

两条路径用的是**同一个** `JitterBuffer` 类型，没有理由只有一条路享受自适应。

### 二、修法

在 `main.rs` 的 2 秒报告块里加 `jb.adapt(now_us())`，并把四个统计字段
（`jitter_adapt` / `jitter_up` / `jitter_down` / `jitter_ewma_ms`）打进日志 ——
与 UDP 路径的字段名**完全一致**，这样解析脚本不用写两套正则。

### 三、实测：p50 20.92 → 11.92ms（降 43%）

20 秒，60fps，8KB 帧，WebSocket 回环：

| 时刻 | target_ms | playable p50/p95 | jitter_ewma | adapt |
| --- | --- | --- | --- | --- |
| 2s | 18.0 | 20.92 / 21.93 | 0.06 | 1 |
| 4s | 16.0 | 19.41 / 21.39 | 0.05 | 2 |
| 6s | 14.0 | 18.43 / 20.92 | 0.07 | 3 |
| 8s | 12.0 | 16.99 / 20.78 | 0.10 | 4 |
| 10s | 10.0 | 16.39 / 20.47 | 0.07 | 5 |
| 14s | 10.0 | 14.43 / 19.95 | 0.04 | 5 |
| 18s | 10.0 | 12.49 / 19.91 | 0.04 | 5 |
| 22s | 10.0 | **11.92 / 19.64** | 0.04 | 5 |

**比 UDP 路径的 13.77ms 还好**。原因不是 WebSocket 更快，而是它的抖动 EWMA
更稳定在 0.04-0.10ms（UDP 路径后期涨到 2.92ms，因为 target 降到 10 后队列变浅，
调度抖动更容易表现为到达间隔变化）。WebSocket 有 TCP 层的重排保证，到达更规整。

### 四、两条路径现在的对比

| 指标 | WebSocket | UDP（无丢包） | UDP（3% 丢包） |
| --- | --- | --- | --- |
| p50 | **11.92** | 13.77 | 20.97 |
| p95 | 19.64 | 20.24 | 57.70 |
| 收敛到 target 下限耗时 | 10s | 10s | 10s |
| 自适应是否生效 | ✅ 本轮 | ✅ 上轮 | ✅ |

WebSocket 的 p95 也更低（19.64 vs 20.24）—— 同样因为到达更规整，
`playable_ms` 的分布更集中。

### 五、本轮状态

```text
cargo fmt --check         0
cargo clippy -D warnings  0
WebSocket 端到端          playable p50 20.92 -> 11.92ms
```

## 第三十三轮：H.264 码流的独立交叉验证（发现并修掉 --size 缺失）

### 一、为什么需要独立验证

`--h264-dump` 写出的文件此前只用发送端日志的 `encode_frames` 数字确认过 ——
那是**自报**。如果编码器把一帧切成 7 个 slice、SPS 写错分辨率、或关键帧标志
与实际不符，发送端日志完全看不出来。

项目里虽然有 `crates/sidelens-core/src/sps.rs`（990 行，含完整的 Exp-Golomb 解析），
但用它验证 dump 有个根本问题：**测试与被测代码共用同一套逻辑**。
如果解析器本身有 bug，测试会「自洽地通过」。

所以新写了 `tools/h264-inspect.mjs` —— Node 实现，自己写 bit reader、
自己处理 emulation prevention bytes、自己算裁剪，**零依赖、零复用**。

### 二、独立解析器第一版就有 bug（说明交叉验证有必要）

第一版按「每个 type 1/5 的 NALU 就是一帧」统计：

```text
帧数: 2527              <- 独立解析器
encode_frames=361       <- 发送端日志
```

**差 7 倍**。查下来是 MF H.264 编码器**把一帧切成 7 个 slice**（多 slice 编码，
提升抗丢包能力）—— 每个 slice 都是 type 1/5 的 NALU。

正确判据有两个：

| 判据 | 可靠性 | 说明 |
| --- | --- | --- |
| AUD（type 9） | 最可靠 | 每个 access unit 前面有一个 |
| first_mb_in_slice == 0 | 可靠 | 表示这是新一帧的第一个 slice |
| 每个 slice 一帧 | **错** | 多 slice 编码下会算成 N 倍 |

修完 `frames=361`，与发送端**完全一致**（差 0）。

### 三、然后它立刻抓到一个真 bug：--h264 无法指定分辨率

跑分辨率矩阵时 1080p 那档失败：

```text
--- 1080p (frame_bytes=32768) ---
  发送端自报 encode_frames=301
  FAIL SPS 分辨率期望 1920x1080，实际 1280x720
```

根因在 `H264Out::config_from_args`：

```rust
if args.frame_bytes <= nv12 {   // nv12 = 720p 的 1,382,400
    def                          // <- 用默认 720p
} else { ... 按 16:9 反推 ... }
```

`frame_bytes` 在 `--h264` 模式下被解释成 NV12 帧字节数。720p 的 NV12 是
1,382,400 字节，所以：

| frame_bytes | 结果 |
| --- | --- |
| 8192 | 小于 1382400 -> 默认 720p（对） |
| 32768 | 小于 1382400 -> 默认 720p（**错**，想表达 1080p） |
| 3110400 | 大于 1382400 -> 反推 1920x1080（对） |

**要表达 1080p 必须传 3,110,400** —— 这个数字没人会记得。

### 四、修法：加 --size WxH

```text
优先级 1: --size 1920x1080    （显式，最可靠）
优先级 2: frame_bytes 反推     （保持老脚本可用）
```

校验：宽高 >= 64 且为偶数（NV12 的硬约束），非法值告警后回退，不硬失败。
`VALUED_FLAGS` 从 8 项扩到 9 项，位置参数解析不被污染。

### 五、三档分辨率全部通过

```text
--- 720p (--size 1280x720, frame_bytes=8192) ---
  OK   帧数 301 与自报 301 一致（差 0）
  OK   SPS 分辨率 1280x720
  OK   PPS=6 IDR=42
  OK   首个 SPS 在第 6 字节
  每帧字节: avg=2061 p50=1181 p95=2237 max=39437

--- 1080p (--size 1920x1080, frame_bytes=32768) ---
  OK   帧数 301 与自报 301 一致（差 0）
  OK   SPS 分辨率 1920x1080
  OK   PPS=6 IDR=66
  OK   首个 SPS 在第 6 字节
  每帧字节: avg=3772 p50=1248 p95=20181 max=63860

--- 360p (--size 640x360, frame_bytes=4096) ---
  OK   帧数 301 与自报 301 一致（差 0）
  OK   SPS 分辨率 640x360
  OK   PPS=6 IDR=18
  OK   首个 SPS 在第 6 字节
  每帧字节: avg=1156 p50=941 p95=1227 max=14947

=== 结果: 0 项失败 ===
```

注意 1080p 的 `p95=20181` vs 720p 的 `p95=2237` —— **差 9 倍**。
IDR 帧在 1080p 下是 63,860 字节（720p 是 39,437），切 55 个分片。
这正是 `docs/fec-link.md` 记录的「FEC 有效窗口 ~3-10 分片，大帧落在窗口外」
的量化依据。

### 六、新增的两个工具

| 工具 | 用途 |
| --- | --- |
| `tools/h264-inspect.mjs` | 独立 Annex-B 解析器。人类可读输出 + `--json` + `--expect=path=val,...` 断言模式 |
| `tools/verify-h264-stream.ps1` | 分辨率矩阵回归，四项断言（帧数一致 / SPS 分辨率 / 有 PPS+IDR / 首个 SPS 位置） |

断言模式让它可以进 CI：

```powershell
node tools\h264-inspect.mjs target\insp.h264 --expect="frames=361,sps.width=1280,sps.height=720,idr=49"
# OK   frames: 期望 361, 实际 361
# OK   sps.width: 期望 1280, 实际 1280
# exit=0
```

### 七、本轮状态

```text
cargo fmt --check               0
tools/verify-h264-stream.ps1    0 项失败（720p / 1080p / 360p）
```

## 第三十四轮：真实 H.264 码流的 golden 回归测试

### 一、为什么需要真实码流

`annexb_edge.rs` 用**手工构造**的字节序列覆盖边界（4/3 字节起始码、
NALU 内部有 `00 00 01`、尾随零歧义等），这部分做得很好。
但它是「我们自己想出来的边界」。真实编码器吐出的码流有它自己的特征：

| 真实特征 | 手写测试能覆盖吗 |
| --- | --- |
| 一帧切 7 个 slice（多 slice 编码） | ❌ 想不到 |
| 每个 access unit 前有 AUD（type 9） | ❌ |
| 关键帧是 IDR，`nal_ref_idc` 为 3 | 部分 |
| SPS 的裁剪参数、VUI 有真实值 | 部分 |

所以加了一份 **golden 文件**：`crates/sidelens-codec/tests/data/golden-720p.h264`
（31 帧、136 KB、720p，由真实编码器产出）。

### 二、期望值怎么确定的（关键）

**不是**用本项目的解析器跑出来的 —— 那样就是自证。
用的是 `tools/h264-inspect.mjs`（上一轮写的独立 Node 实现）。

而且形成**三方交叉**：

| 实现 | 位置 | 独立性 |
| --- | --- | --- |
| 项目解析器 | `crates/sidelens-codec/src/nalu.rs` + `sidelens-core/src/sps.rs` | 被测对象 |
| Node 解析器 | `tools/h264-inspect.mjs` | 完全独立（自己写 bit reader） |
| 测试内手算 | `golden_h264.rs` 的 `sps_encodes_expected_resolution` | 又一份独立实现 |

三方对同一份 dump 得出 `1280x720` 才算真验证过。

### 三、写这个测试时踩的两个坑（都是我的期望值错了）

#### 坑 1：`IDR: 7` 是 slice 数，不是帧数

独立解析器输出 `IDR: 7`，我直接断言 `frames_with_idr == 7`，实际得到 **1**。

查真实 NALU 类型序列：

```text
9 7 8 6 6 5 5 5 5 5 5 5  9 1 1 1 1 1 1 1  9 1 1 1 1 1 1 1 ...
  ^SPS/PPS^ ^7 个 IDR slice^ ^AUD^ ^7 个 P slice^ ...
```

**7 个 IDR 全部在第一个 access unit 里** —— 那是同一帧的 7 个 slice，不是 7 帧。
这个 golden 只编了 1 秒、GOP 60，所以只有 1 个关键帧。

修法：解析器的 `idr` 字段改名为 `idrSlices`，测试改成断言两个事实
（`frames_with_idr == 1` 且 `idr_slices == 7`）。

#### 坑 2：`ParameterSets::sps()` 返回裸数据，不含起始码

我假设它含起始码，写了 `split_annexb(sps)` 期望切出 1 个 NALU，实际切出 **0 个**。

查 `update_from` 的实现：

```rust
NaluType::Sps if self.sps.as_deref() != Some(nalu.data) => {
    self.sps = Some(nalu.data.to_vec());   // <- nalu.data 不含起始码
}
```

起始码由 `to_annexb()` 补。这个契约值得用测试钉住，已加断言：

```rust
assert_eq!(sps[0] & 0x1f, 7);        // 裸数据首字节就是 NAL header
let combined = ps.to_annexb();
assert_eq!(split_annexb(&combined).len(), 2);  // 补起始码后能切回 2 个
```

### 四、11 个测试覆盖什么

| 测试 | 守住什么 |
| --- | --- |
| `golden_is_not_empty` | 文件没被误删/截断 |
| `split_finds_expected_nalu_counts` | AUD=31 / SPS=1 / PPS=1 |
| `aud_count_equals_frame_count` | **切帧判据**：AUD 数 = 帧数（若按 slice 数会算出 217） |
| `slices_per_frame_is_seven` | 每帧 7 个 slice（多 slice 编码的量化依据） |
| `parameter_sets_are_extracted` | 能提取 SPS+PPS |
| `parameter_sets_have_correct_nalu_types` | **契约**：裸数据 + `to_annexb` 补起始码 |
| `keyframe_detection_matches_idr_count` | 1 帧 IDR × 7 slice |
| `annexb_avcc_roundtrip_preserves_nalu_count` | Annex-B ↔ AVCC 无损往返（逐 NALU 比内容） |
| `no_nalu_is_empty` | 无零长度 NALU |
| `every_nalu_has_valid_header` | `forbidden_zero_bit == 0`（规范硬要求） |
| `sps_encodes_expected_resolution` | **手算** SPS 得出 1280x720（第三方交叉） |

### 五、`slices_per_frame_is_seven` 这个数字为什么重要

7 个 slice 意味着**单帧内有多条独立的熵编码路径**：

- 丢一个分片只坏 1/7 的帧，而不是整帧
- 但接收端要能「部分解码」才有意义 —— 当前实现是「缺片就整帧丢」
- 这也是 FEC 值不值得开的量化依据：7 个 slice → 7 个分片（720p P 帧），
  正好落在 `docs/fec-link.md` 说的「5-10 分片甜区」

### 六、本轮状态

```text
cargo test -p sidelens-codec --test golden_h264   11 passed / 0 failed
cargo clippy -p sidelens-codec --all-targets     0
golden 文件                                        136,418 字节 / 31 帧
```

## 第三十五轮：`--size` 参数交互回归

### 一、为什么单独测参数交互

`--size` 是加进 `VALUED_FLAGS` 的（第 9 项）。这个数组的作用是告诉位置参数
解析器「这些 flag 后面跟着一个值，不要把它当成位置参数」。

**加错一个就会静默破坏位置参数解析** —— 比如 `--size 640x360` 里的 `640x360`
被当成 `fps`，于是 `fps=640`，后面的 `30` 变成 `frame_bytes`，全错位。
而且这种错误**不会报错**，只会让运行参数莫名其妙。

### 二、三个场景

| 场景 | 命令 | 期望 |
| --- | --- | --- |
| normal | `--udp host 30 4096 1 --h264 --size 640x360` | fps=30, frame_bytes=4096, 640x360 |
| size-first | `--size 640x360 --udp host 30 4096 1 --h264` | 同上（**顺序无关**） |
| bad-size | `--size 1920`（缺高度） | 告警 + 回退 720p，位置参数仍正确 |

### 三、实测：9 项断言全通过

```text
--- 参数交互 ---
  OK   [normal] 位置参数未被污染
  OK   [normal] 分辨率 640x360
  OK   [size-first] 位置参数未被污染
  OK   [size-first] 分辨率 640x360
  OK   [bad-size] 位置参数未被污染
  OK   [bad-size] 分辨率 1280x720

=== 结果: 0 项失败 ===
```

非法值的告警文案：

```text
WARN sidelenssend: --size 需形如 1920x1080（宽高 >= 64 且为偶数），已忽略 value=1920
```

### 四、顺手修的一个脚本 bug

上一轮把解析器的 `idr` 字段改名成 `idrSlices`（因为 `IDR: 7` 是 slice 数不是帧数，
见第三十四轮坑 1），但 `verify-h264-stream.ps1` 还在读旧字段名 ——
于是 `IDR=` 显示为空，三档全部误报「缺 PPS 或 IDR」。

**教训：改名要全局搜一遍引用**。这次是我自己改的名，下一轮就忘了改脚本。

### 五、完整回归输出（9 项断言）

```text
--- 720p (--size 1280x720, frame_bytes=8192) ---
  OK   帧数 300 与自报 300 一致（差 0）
  OK   SPS 分辨率 1280x720
  OK   PPS=5 IDR=35
  OK   首个 SPS 在第 6 字节

--- 1080p (--size 1920x1080, frame_bytes=32768) ---
  OK   帧数 301 与自报 301 一致（差 0）
  OK   SPS 分辨率 1920x1080
  OK   PPS=6 IDR=66

--- 360p (--size 640x360, frame_bytes=4096) ---
  OK   帧数 301 与自报 301 一致（差 0）
  OK   SPS 分辨率 640x360
  OK   PPS=6 IDR=18

=== 结果: 0 项失败 ===
```

### 六、本轮状态

```text
tools/verify-h264-stream.ps1    9 项断言 / 0 失败
覆盖                            720p + 1080p + 360p + 3 个参数交互场景
```

## 第三十六轮：音频抖动缓冲自适应接入（`audio-adapt` 子代理交付 + 我接线）

### 一、子代理交付的 `adapt`

`AudioJitterBuffer` 此前 `target_ms` 是构造时的常量（daemon 传 40.0）。
视频侧第二十八轮就有了自适应，音频侧一直没有。

四道闸门，**常量全部与视频侧不同**，理由都是音频特性：

| 常量 | 音频 | 视频 | 差异理由 |
| --- | --- | --- | --- |
| 下限 | **20ms** | 10ms | 20ms 帧长决定最小可用深度，再低 `capacity` 只剩 2，一个乱序帧就溢出 |
| 单次降幅 | **10ms** | 2ms | 音频目标区间只有 20-60ms，2ms 一步要跨 5 秒驻留期走 10 次 |
| 驻留期 | **500ms** | 200ms | `target_ms` 抖动直接改变迟到窗口，把本该补播的帧判成迟到 → 听得见的爆音 |
| 安全系数 | **2.0** | 2.5 | 音频估小是咔哒声、估大只是延迟，宁可多留 |
| 上限 | **200ms** | 200ms | 音频过 200ms 变对讲机 |

### 二、子代理最关键的一个判断：不能用 PTS 代理

我在派任务时给了两个选项，它选了 `arrived_at_us` 并给出了**决定性理由**：

> `AudioPacer` 产出的 PTS 严格等距（那是「按采样计数」原则的核心承诺），
> `|Δpts − frame_duration|` **恒等于 0**，对网络抖动完全失明 ——
> 喂给 `adapt` 只会把目标一路压到下限，然后在链路变差时开始丢帧。

这个判断是对的，而且理由很硬：项目的 `AudioPacer` 设计就是「PTS 由采样数算出，
不读墙上时钟」，所以 PTS 间隔天然恒定。用 PTS 代理等于用「设计上恒定」的量去测抖动。

于是给 `AudioFrame` 加了 `pub arrived_at_us: u64`（默认 0 = 未填），
`observe_jitter` 按 RFC 3550 算 `|(R_i - R_{i-1}) - (S_i - S_{i-1})|`，
**两项取自不同两帧** —— 和视频侧那个「`send_ts` 自己抵消掉」的真 bug 同一个坑。

### 三、我做的接线

子代理只改了 `crates/sidelens-codec/**`，daemon 侧需要三处接线：

**1. 填 `arrived_at_us`**（`AudioFrame::new` → `AudioFrame::with_arrival`）

不填的后果我在注释里写清了：`arrived_at_us == 0` 是哨兵值，样本被跳过 →
`jitter_ewma` 永远 0 → `adapt` 把 `target_ms` 一路压到下限 20ms。
干净链路上是好事，但链路变差时缓冲已经太浅，开始丢帧且**无法自愈**。

**2. 周期调用 `adapt`**（在 2 秒报告块里，与视频侧并列）

**3. 日志加四个字段**，与视频侧**同名**：`audio_adapt` / `audio_adapt_up` /
`audio_adapt_down` / `audio_jitter_ms`。这样解析脚本不用写两套正则。

### 四、实测：target 40 → 20ms

20 秒，60fps 视频 + Opus 音频 + 3% 丢包：

| 时刻 | audio_target_ms | adapt | up | down | jitter_ms | audio_decoded |
| --- | --- | --- | --- | --- | --- | --- |
| 2s | 30.0 | 1 | 0 | 1 | 0.52 | 23 |
| 4s | **20.0** | 2 | 0 | 2 | 0.55 | 121 |
| 6s | 20.0 | 2 | 0 | 2 | 0.36 | 218 |
| 10s | 20.0 | 2 | 0 | 2 | 0.59 | 413 |
| 16s | 20.0 | 2 | 0 | 2 | 1.09 | 803 |
| 22s | 20.0 | 2 | 0 | 2 | 1.99 | 972 |

**两次下调就到底**（40 → 30 → 20），因为单步降幅是 10ms 而不是视频的 2ms。
`audio_decoded` 全程正常增长，`audio_decode_failed=0`。

抖动 EWMA 从 0.36 涨到 1.99ms —— 因为 `target` 降到 20 后缓冲变浅，
同样的调度抖动更容易表现为到达间隔变化。这与视频侧观察到的现象一致。

### 五、子代理报的三个坑（都值得记）

**坑 1：`while len > capacity { push }` 是死循环。**
溢出判定是 `if len > capacity { remove(0) }`，每次 push 只丢一帧 ——
push 加一帧、溢出减一帧，收支平衡，队列长度**稳定不降**。
调小 target 后队列会暂时比 `capacity` 长，直到自然播出。这是刻意的
（不该惩罚已在队里的有效帧），已写成显式断言。

**坑 2：`arrived_at_us = 0` 会凭空造出 20ms 抖动。**
全 0 时 `Δ到达=0`、`Δpts=20000` → `|D|=20ms` → 目标被顶到 40ms。
必须把 0 当哨兵跳过，否则老调用点一调 `adapt` 就被污染。

**坑 3：零抖动必须能降。**
照抄视频侧 `jitter_ewma <= 0.0 → return None` 会让干净链路永远停在 40ms，
自适应等于白做。音频侧去掉了这条。

### 六、测试

`crates/sidelens-codec/tests/audio_adapt.rs`：**17 个测试**，覆盖
抖动估计精确值（`|D|` / 交替 ±4ms → 8ms / 恒定偏移衰减）、零抖动收敛到 20ms
（恰好 8 次降）、抖动突增一次抬满、单次降幅上限、驻留期边界、
容量随 target 升降、调小不丢队列已有帧、不调 adapt 行为不变、
`arrived_at_us=0` 不造假样本、200ms 上限。

`cargo test -p sidelens-codec` → **130 passed / 0 failed**（audio.rs 原有 26 个全绿）。

### 七、本轮状态

```text
cargo fmt --check                    0
cargo clippy -p sidelensd -p sidelens-codec   0
cargo test -p sidelens-codec         130 passed / 0 failed
端到端                               audio_target_ms 40 -> 20，audio_decoded=972 / 0 失败
```

## 第三十七轮：H.264 解码器落地（视频链路终于闭环）

### 一、这是本轮最大的功能补齐

此前接收端**从未解码 H.264** —— 视频链路只做到「收包 + 重组 + 抖动缓冲」就停了。
审计命令：

```text
grep "Decoder|H264" apps/sidelensd/src/udp_server.rs  ->  只有 OpusDecoder
```

`h264-decode` 子代理交付了 `H264Decoder`（`crates/sidelens-codec-mf/src/decoder.rs`，57 KB）。

### 二、实测：编解码往返完全闭环

```text
=== 配置 ===
  640x360 @30fps, 30 帧, enc bitrate=2000000 bps, gop=30
  编码器：H264 Encoder MFT
  解码器：H.264 Decoder MFT
  协商输出格式：NV12  640x360

=== 输出格式（实测） ===
  NV12  coded 640x368  显示 640x360  一帧 353280 字节
  （coded size 比显示尺寸多 8 行，是 H.264 的宏块对齐填充，显示时按 SPS cropping 裁掉）
  SPS profile=Some(77) level=Some(30)
  frames_in=30  frames_out=30  not_accepting=0  stream_change=1  need_more_input=31

=== 单帧耗时 ===
  编码 min/p50/p95/max：390.7/486.3/1224.5/4270.6 µs   平均 686.9 µs
  解码 min/p50/p95/max：346.4/444.8/1149.2/1754.9 µs   平均 554.3 µs
  往返 min/p50/p95/max：758.9/896.0/2105.5/6025.5 µs   平均 1241.2 µs

=== Y 值往返比对（NV12） ===
  采样 90 个点，|ΔY| 平均 0.40，最大 1（阈值 16）

=== 断言 ===
  [OK] 输出帧数 30 与输入 30 相差 0（≤1）
  [OK] 解码输出字节数 10598400 是帧长 353280 的整数倍
  [OK] Y 值最大偏差 1 < 16
```

**三个关键数字**：

| 指标 | 值 | 意义 |
| --- | --- | --- |
| 解码 p50 | **444.8 µs** | 33ms 帧间隔的 1.3%，完全不是瓶颈 |
| 编码 p50 | **486.3 µs** | 同上 |
| 往返 p50 | **896 µs** | 加上网络与缓冲，仍远低于 33ms |
| Y 值偏差 | **最大 1** | H.264 有损压缩下几乎无损（纯色块本就该无损） |

### 三、`coded size` 比显示尺寸大 —— 这是 H.264 的正常行为

实测 `coded 640x368` vs `显示 640x360`，多 8 行。原因：

- H.264 的宏块是 16x16，`640x360` 的 360 不是 16 的倍数
- 编码器补齐到 `640x368`（23 个宏块行 × 16）
- **SPS 里的 cropping 参数**告诉解码器「显示时裁掉底部 8 行」

所以下游拿到的 NV12 缓冲是 `640x368`（`stride × coded_height × 1.5`），
但显示时要按 SPS 的 cropping 裁。**不裁的话画面底部会有 8 行绿边/垃圾**。

这正是 `crates/sidelens-core/src/sps.rs` 的 `resolution()` vs `coded_resolution()`
两个方法的区别 —— 一个给显示用，一个给缓冲分配用。

### 四、`not_accepting=0` 但 `need_more_input=31` 说明什么

子代理的统计暴露了同步 MFT 的真实节奏：

| 计数器 | 值 | 含义 |
| --- | --- | --- |
| `not_accepting` | 0 | 从未遇到 `MF_E_NOTACCEPTING`（输出队列没满过） |
| `need_more_input` | 31 | 31 次 `ProcessOutput` 返回 `MF_E_TRANSFORM_NEED_MORE_INPUT` |
| `stream_change` | 1 | 1 次 `MF_E_TRANSFORM_STREAM_CHANGE`（首次协商输出类型时） |

`need_more_input=31`（比 30 帧多 1）说明：**每帧喂进去后调用一次 `ProcessOutput`，
恰好吐出 1 帧**。解码器没有内部缓存多帧 —— 这对实时流是理想行为（无重排延迟）。

### 五、10 个测试

| 测试 | 守住什么 |
| --- | --- |
| `decoder_creates_and_reports_known_format` | 创建成功 + 输出格式已知 |
| `encode_decode_roundtrip_frame_count` | 帧数一致 |
| `flat_color_roundtrip_keeps_luma` | Y 值偏差 < 16 |
| `sps_display_size_is_smaller_than_coded_size` | **cropping 语义**（显示 360 < coded 368） |
| `encoder_output_is_valid_annexb_for_decoder` | 编码器输出能被解码器吃 |
| `empty_input_is_noop` | 空输入不 panic |
| `garbage_input_does_not_panic` | 垃圾输入返回 Err 而非 panic |
| `flush_stops_frame_growth` | flush 后 `frames_out` 不再增长 |
| `config_validation_rejects_odd_and_zero` | 奇数/零宽高被拒（NV12 硬约束） |
| `frame_len_matches_nv12` | 帧长 = w*h*1.5 |

### 六、还没做的（诚实记录）

解码器**已实现并验证，但还没接进 daemon**。`udp_server.rs` 目前只解 Opus，
视频帧收完就进抖动缓冲、出队后只统计 `playable_ms`，没有解码步骤。

接进去需要：

1. 在 `sidelensd` 里创建 `H264Decoder`（受 `SIDELENS_H264_DECODE` 环境变量控制，
   与 `SIDELENS_OPUS` 同风格）
2. 视频出队后解码，把 NV12 交给渲染层
3. **渲染层不存在** —— 这是下一个缺口（Windows 上可用 `IMFMediaSink` 或简单窗口）

所以本轮的成果是「解码能力就绪」，不是「端到端出画面」。

### 七、本轮状态

```text
cargo fmt --check                            0
cargo clippy --workspace -D warnings         0
cargo test（全 workspace）                   678 passed / 0 failed
  - sidelens-codec-mf 解码器                  10 passed
  - sidelens-codec 音频自适应                 130 passed（含 17 个新）
decode_probe 端到端                          3 项断言全通过
```

### 八、子代理报的两个「VARIANT 类型」坑（最有价值的发现）

这两个坑的共同特征是：**都返回 E_INVALIDARG，靠错误码完全看不出问题**。

#### 坑 1：CODECAPI_AVLowLatencyMode 必须用 VT_UI4，不是 VT_BOOL

文档写的是 BOOL，第一版用 VT_BOOL 返回 E_INVALIDARG，被 let _ = 静默吞掉。
症状是「30 帧输入解出 **0 帧**」—— 看起来像解码器完全不能用。

```text
SetValue(AVLowLatencyMode, VT_BOOL(-1)) -> 0x80070057 E_INVALIDARG
SetValue(AVLowLatencyMode, VT_UI4(1))   -> S_OK
GetValue(AVLowLatencyMode)              -> vt=19 (VT_UI4)
```

修正后延迟从 **33 帧 -> 0 帧**，帧数从 0/30 -> 30/30。

对照实验（喂 80 帧）：

| 设置 | 输出帧数 | 内部积压 |
| --- | --- | --- |
| 什么都不设 | 47 | **33 帧** |
| 只设 AVDecNumWorkerThreads=1 | 70 | 10 帧 |
| AVLowLatencyMode=1 | **80** | **0 帧** |

#### 坑 2：CODECAPI_AVDecNumWorkerThreads 反过来只吃 VT_I4

VT_UI4 同样 E_INVALIDARG。**两个参数的 VARIANT 类型正好相反**，
都返回同一个错误码 —— 这是本项目第二次踩「SetValue 返回 S_OK 不等于生效」
的同类问题（第一次是 MF_LOW_LATENCY 的设值顺序）。

### 九、另一个坑：输出类型不能覆盖帧尺寸

第一版把解码器自报的输出类型 MF_MT_FRAME_SIZE 覆盖成 config 的显示尺寸
（640x360）再 SetOutputType。**首次协商能过**，但首个 IDR 之后解码器报
STREAM_CHANGE、自报变成 640x368（coded size），此时覆盖成 640x360 就
MF_E_INVALIDMEDIATYPE（0xC00D36B4）。

改为**自报类型原样使用**后解决。三级回退：原样 -> 原样+覆盖 -> 自建 NV12/YV12/IYUV。

### 十、1080p 的 coded size 是 1920x1088

子代理特别提示：**接虚拟摄像头时必须用 output_display_size() 而不是 output_size()**，
否则底部多一条绿边。

| 分辨率 | coded size | 显示 size | 多出的行 |
| --- | --- | --- | --- |
| 320x240 | 320x240 | 320x240 | 0（240 = 15x16） |
| 640x360 | 640x368 | 640x360 | 8 |
| 1280x720 | 1280x720 | 1280x720 | 0（720 = 45x16） |
| 1920x1080 | 1920x1088 | 1920x1080 | **8** |

360 和 1080 都不是 16 的倍数，所以需要裁剪。720 恰好是 45x16，不需要。

### 十一、本轮最终状态

```text
cargo fmt --check                            0
cargo clippy --workspace -D warnings         0
cargo test --workspace                       687 passed / 0 failed
cargo build --release                        0
端到端（全功能）                              视频 p50 24.78ms / 音频 decoded 955 / av_synced 1128
H.264 dump 独立验证                           1201 帧 / 1280x720 / GOP 恒 60
```

## 第三十八轮：AvSync 主时钟的真 bug（`dropped` 311 → 0，`p50` 86 → 12ms）

### 一、怎么发现的

跑音频自适应回归时注意到一组矛盾数据（`aad` 测试）：

```text
synced=908  dropped=311  playable=98.63  audio_dec=972
```

`dropped=311` 而 `synced=908` —— 四分之一以上的视频帧被丢弃。
对照实验立刻定位到触发条件：

| 配置 | dropped | playable |
| --- | --- | --- |
| 纯视频（无音频） | **0** | 74.31 |
| 30fps + 音频 | **311** | 98.63 |
| 60fps + 音频 | **0** | 16.94 |

**只有「30fps 视频 + 20ms 音频」这一组合会爆。** 60fps 时正常。

### 二、三层根因（每层都靠数据确认）

#### 第 1 层：`audio_clock` 阶梯式跳跃

加诊断后拿到逐帧样本：

```text
seq=7   Δvideo=16666  Δclock=20000   drift=-4
seq=8   Δvideo=16666  Δclock=60000   drift=-43338   <- 一次出队 3 帧音频
seq=10  Δvideo=33332  Δclock=0       drift=-10006
```

`Δclock=60000` 是关键：音频抖动缓冲的 `pop_due` 是 `while` 循环，
某个 tick 被阻塞后会**一次补出多帧**，每帧调一次 `on_audio` →
`clock` 连跳 3 次 = 60ms。而视频只前进 33ms。

`clock` 阶梯式前进 → `drift` 单次抖动 60ms → 叠加初始偏移后稳定超过
`drop_threshold_ms=120` → 视频被判 Drop。

**修法**：`clock` 不再取「已出队帧的 PTS」，改成 `真实时间 + offset`，
`offset` 由每帧的 `(pts+duration) - 真实时间` 平滑逼近（单帧最多挪 1ms）。
新增 `AvSync::on_audio_at(pts, dur, real_now)`。

#### 第 2 层：修完第 1 层，`playable` 从 21 涨到 87ms

`dropped` 归零了，但延迟暴涨。原因：

```text
修复前：clock 阶梯超前 → 大量帧被判 Drop → 只播「刚到达的新帧」→ playable 小
修复后：clock 正常     → 所有帧都播     → 包括积压的旧帧 → playable 暴露
```

修复前的高丢弃率「意外地」压低了延迟。现在延迟暴露出来了 —— 这是**真问题**。

#### 第 3 层：`target_ms` 涨到 78ms（抖动估计被污染）

诊断显示 `jitter_ewma` 从 0.05ms 涨到 **31ms**（600 倍），而回环真实抖动 <1ms。
对照实验决定性：

| 配置 | jitter_ewma | target_ms | playable |
| --- | --- | --- | --- |
| 0% 丢包 | **0.05ms** | 10 | 14.99 |
| 3% 丢包 | **31ms** | 78 | 86.67 |

**3% 丢包 = 有 NACK 重传**。丢一个分片 → 整帧要等重传补齐 →
到达时刻推迟 ~30ms → 下一帧正常到达 → 到达间隔 63ms，而 `Δsend` 只有 33ms
→ 假样本 `|63-33| = 30ms`。

注意：这**不是「重传帧本身」**。重传复用了 `FrameType::Video`，
`FLAG_RETRANSMIT` 全仓库无写入点，从帧头无法区分。污染源是
**被重传救回来的那一帧**的到达延迟。

**修法**：抖动样本限幅 `JITTER_SAMPLE_CAP_US = 15_000`。超过 15ms 的样本
视为「等重传」丢弃。

依据：

| 场景 | 典型抖动 |
| --- | --- |
| 本机回环 | 0.04-1 ms |
| 局域网 WiFi 5GHz | 2-8 ms |
| 公网同城 | 5-15 ms |
| **NACK 重传延迟** | **30ms+** |

15ms 能把两者分开。2.4GHz WiFi 的 10-30ms 会被部分削掉 —— 刻意取舍：
那种链路上缓冲加到 30ms 已够，再加只是白增延迟。

### 三、三层修完的最终效果

20 秒，60fps，8KB 帧，**3% 丢包 + NACK 重传**：

| 指标 | 修前 | 修后 | 变化 |
| --- | --- | --- | --- |
| `video_dropped_late` | **311** | **0** | 全修好 |
| `jitter_ewma` | 31 ms | **0.06 ms** | 500 倍 |
| `target_ms` | 78 | **10**（下限） | 收敛 |
| `playable` p50 | **86.67** | **12.02** | −86% |
| `playable` p95 | 142.79 | **21.72** | −85% |
| `av_synced` | 1217 | 985 | 少丢 232 帧 |

逐窗口收敛过程：

```text
frames=45    synced=34   dropped=0  target=18.0  jitter=0.41  playable=20.98/65.55
frames=289   synced=238  dropped=0  target=14.0  jitter=0.13  playable=18.95/55.47
frames=537   synced=436  dropped=0  target=10.0  jitter=0.06  playable=16.97/49.70
frames=1032  synced=824  dropped=0  target=10.0  jitter=0.36  playable=12.98/22.52
frames=1236  synced=985  dropped=0  target=10.0  jitter=0.06  playable=12.02/21.72
```

**`playable p50 = 12.02ms`** 是项目目前最好的带丢包数字（此前 3% 丢包下是 20.97）。

### 四、一个方法论教训

这轮我三次以为找到了根因：

| 假设 | 怎么被推翻 |
| --- | --- |
| 「重放窗口误杀重传」 | 实测 `replay_rejected=0`（见 `udp-nack-report.md` 附录 C） |
| 「`target_ms` 变化造成 drift」 | 公式推导证明它只影响 +20ms，不可能是 −100ms |
| 「重传帧污染抖动估计」 | 加了 `is_retransmit` 字段后 `jitter` 仍是 16-45ms —— 因为该字段永远 false |

每次都是「读代码 + 推理」得出，每次都被实测推翻。
**最后靠的是加诊断计数器直接看数值**：`Δclock=60000` 和
「0% 丢包 jitter=0.05 / 3% 丢包 jitter=31」这两个数据一出来，根因立刻明确。

**教训：卡住超过两轮推理时，直接加计数器测，不要再读代码。**

### 五、本轮状态

```text
cargo fmt --check                    0
cargo clippy --workspace -D warnings 0
cargo test --workspace               728 passed / 0 failed
3% 丢包端到端                        dropped=0 / jitter=0.06ms / playable p50=12.02ms
```

### 六、全功能最终验证（5% 丢包 + FEC + Opus + H.264）

20 秒，60fps，1280x720，所有功能同时开启：

```text
playable_ms          11.63 / 19.46      <- p50 / p95
target_ms            10.0               <- 抖动缓冲收敛到下限
jitter_ewma_ms       0.05               <- 抖动估计干净
video_dropped_late   0
av_synced            1104
audio_decoded        960                <- 0 失败
fec_recovered        16
healed               65
nack_seqs            92
```

**`playable p50 = 11.63ms`** —— 项目目前最好的带丢包数字。

对比历史：

| 阶段 | 3% 丢包 p50 | 5% 丢包 p50 |
| --- | --- | --- |
| 抖动自适应前（`target_ms` 常量 20） | 20.97 | — |
| 抖动自适应后（第二十八轮） | 20.97 | — |
| **修掉 AvSync 主时钟 bug 后（本轮）** | **12.02** | **11.63** |

5% 丢包比 3% 还低，因为 FEC 在这个丢包率下更有效（`fec_recovered=16`）。

### 七、H.264 解码器接入 daemon（视频链路闭环）

`decode-wire` 子代理交付了 `apps/sidelensd/src/video_decode.rs`（35 KB），
我做了三处接线：

| 改动 | 内容 |
| --- | --- |
| `main.rs` | `#[cfg(windows)] mod video_decode;` |
| `Cargo.toml` | 加 `sidelens-codec-mf` 到 `[target.'cfg(windows)'.dependencies]` |
| `udp_server.rs` | `SyncVerdict::Play` 分支调用解码器 + 4 个统计字段 |

**为什么解码放在 `SyncVerdict::Play` 分支**：那是「这一帧确定要播」的唯一时刻。
在收包处解码会白解将被丢弃的帧；在出队后裁决前解码则可能解了又被 Drop，
浪费 400µs/帧。

### 八、子代理最有价值的发现：解码器对坏输入「静默成功」

| 输入 | 解码器返回 | 实际 |
| --- | --- | --- |
| 首帧是 P 帧（中途接入） | `Ok(())` + **0 字节** | 静默吞掉，**没有任何信号** |
| 纯垃圾字节 | `Ok(())` + 0 字节 | MFT 把不认识的当噪声跳过 |
| 丢包导致缺参考帧 | `Ok(())` + **正常字节数** | **花屏但字节数完全正常** |

所以模块在**喂给解码器之前**先检查 NALU 类型，首帧不是 SPS/IDR 就返回
`DecodeError::NeedKeyframe`，让 daemon 去请求关键帧，而不是干等。

关于花屏，子代理的立场很明确：

> 解码器说成功就是成功。丢包的影响由上游（FEC / NACK / 抖动缓冲）负责，
> 解码层不越界。要检测花屏只能上 SSIM/PSNR 比对，那需要参考帧，实时链路里没有。

### 九、实测：解码器在 daemon 里工作正常

10 秒，60fps，1280x720，无丢包：

```text
INFO sidelensd::video_decode: H.264 解码已启用（SIDELENS_H264_DECODE=1）
  decoder="H.264 Decoder MFT" format="NV12" width=1280 height=720 fps=30

frames=31    decoded=30    failed=0  need_kf=0  avg_us=2689
frames=152   decoded=150   failed=0  need_kf=0  avg_us=2389
frames=392   decoded=391   failed=0  need_kf=0  avg_us=2455
frames=601   decoded=601   failed=0  need_kf=0  avg_us=2440
```

**`video_decoded=601`，`failed=0`，`need_kf=0`** —— 每一帧都解出来了。

注意 `decoded` 比 `frames` 少 1（第一帧）：那是 `NeedKeyframe` 检测在起作用，
第一帧之前没有 SPS/PPS 上下文。之后 `need_kf=0` 说明再没缺过关键帧。

`avg_us=2440`（1280x720）比 `decode_probe` 测的 398µs 高 6 倍 —— 因为这里
包含 `Vec` 分配与 NV12 拷贝（`decode_probe` 用的是复用缓冲）。
33ms 帧间隔下占 7.4%，仍不是瓶颈。

### 十、当前完整链路

```text
手机端（模拟）                         电脑端
─────────────────                    ─────────────────────────────────
摄像头 → H.264 编码 (529µs)           收包 → 重组 → FEC → 解密
       → 分片 → FEC → 加密           → 抖动缓冲（自适应 target）
       → UDP                          → AvSync 裁决（按音频时钟）
                                      → **H.264 解码 (2440µs)**  ← 本轮接通
                                      → NV12（等待渲染层消费）

麦克风 → Opus 编码                    收包 → 抖动缓冲（自适应）
       → UDP                          → Opus 解码 → AvSync 主时钟
```

**视频链路从「只统计延迟」变成「真正解码」**。
NV12 已就绪但还没有消费端 —— 渲染层（`sidelens-render`）已实现但未接入。

### 十一、本轮最终状态

```text
cargo fmt --check                    0
cargo clippy --workspace -D warnings 0
cargo test --workspace               742 passed / 0 failed
cargo build --release                0
视频解码端到端                       video_decoded=601 / failed=0
全功能 5% 丢包                       playable p50=11.63ms / dropped=0
```

### 十二、关键帧请求通道：三处断点全修（本轮额外发现）

接完解码器后发现 `video_need_keyframe=107`（9.6% 的帧没画面），
而发送端从不知道接收端在要关键帧。查下来是**三个独立的断点**：

| # | 断点 | 后果 |
| --- | --- | --- |
| 1 | UDP recv 循环不处理 `KeyframeReq` | 请求包落进 `_ => {}` 被丢掉 |
| 2 | `want_keyframe` 没有传进编码器 | 即使收到也不会编 IDR |
| 3 | `is_key` 完全依赖 `nalu::is_keyframe(码流)` | 那是**读输出**，只能被动接受 GOP |

#### 断点 1：UDP 路径根本不处理 KeyframeReq

`KeyframeReq` 的处理只在 **WebSocket 路径**（`client.subscribe()`）：

```rust
// WebSocket 路径（有）
let mut down = client.subscribe();
tokio::spawn(async move {
    while let Ok(f) = down.recv().await {
        if f.ty == FrameType::KeyframeReq { fk.store(true, ...); }
    }
});
```

而 UDP 路径走 `recv_socket.recv()`，只 match `Nack` 与 `RttReply`。

#### 断点 2：`want_keyframe` 没接到编码器

即使收到了请求，UDP 路径的 `is_key` 来自：

```rust
let is_key = match encoded_payload {
    Some(data) => sidelens_codec::nalu::is_keyframe(data),  // 读编码器输出
    None => sent % (args.fps * 2) == 0,
};
```

**这是「读输出」而不是「命令编码器」**。编码器按 GOP=60 决定 IDR，
请求完全无法影响它。

#### 修法

```rust
// 1. UDP recv 循环加分支
FrameType::KeyframeReq => { let _ = kf_tx.send(()); }

// 2. 主循环消费
let mut want_keyframe = false;
while kf_rx.try_recv().is_ok() { want_keyframe = true; }

// 3. 编码前强制 IDR（H264Encoder::force_keyframe 走
//    CODECAPI_AVEncVideoForceKeyFrame）
if want_keyframe { h.force_keyframe()?; }
```

同时把请求限流从「每 30 次」改成「最小间隔 200ms」：

| 策略 | 问题 |
| --- | --- |
| 每 30 帧才请求 | 要等 500ms 才可能恢复 |
| 每帧都请求 | 关键帧比 P 帧大 30 倍，会塞满链路 → 更多关键帧丢 → 正反馈 |
| **最小间隔 200ms** | 6 帧一次，既快又不挤占带宽 |

#### 实测：`need_keyframe` 108 → 14（−87%）

20 秒，5% 丢包，1280x720，全功能：

| 指标 | 修前 | 修后 |
| --- | --- | --- |
| `video_need_keyframe` | **108** | **14** |
| `video_decoded` | 997 | **1092**（+95 帧有画面） |
| `video_decode_failed` | 0 | 0 |
| `playable` p50 | 13.93 | 13.90 |
| `video_dropped_late` | 0 | 0 |

剩余 14 次是「首个关键帧之前的 1 次」+「偶发关键帧丢失后等重传的几帧」，
占 1.3%，是 GOP 边界的正常损耗。

### 十三、本轮最终状态

```text
cargo fmt --check                    0
cargo clippy --workspace -D warnings 0
cargo test --workspace               742 passed / 0 failed
cargo build --release                0

视频解码端到端                       decoded=1092 / failed=0 / need_kf=14
全功能 5% 丢包                       playable p50=13.90ms / p95=21.49ms / dropped=0
解码吞吐                             692 MB/s（NV12 输出）
```

## 第三十九轮：渲染层接入（端到端链路首次完整）

### 一、链路终于闭环

```text
收包 → 重组 → FEC → 解密 → 抖动缓冲 → AvSync 裁决
  → H.264 解码 → NV12 → NV12→RGBA 转换 → 渲染
```

上一轮接完解码器后 NV12 是 `let _ = nv12;`（落地但不消费）。
本轮把它接到 `sidelens-render`。

### 二、接入的三处

| 文件 | 改动 |
| --- | --- |
| `Cargo.toml`（workspace） | `[workspace.dependencies]` 加 `sidelens-render` |
| `apps/sidelensd/Cargo.toml` | `[target.'cfg(windows)'.dependencies]` 加 `sidelens-render` |
| `udp_server.rs` | 渲染器状态 + `Play` 分支渲染 + tick 里 `pump_events` + 2 个统计字段 |

### 三、三种渲染模式

| `SIDELENS_RENDER` | 行为 |
| --- | --- |
| `1` / `true` | 开真窗口（GDI）。创建失败自动退回 headless |
| `headless` | 只做 NV12→RGBA 转换，不显示。CI / 无桌面环境用 |
| 未设置 | 不渲染（与之前行为完全一致） |

### 四、`coded vs display` 的坑（必须用对）

```rust
// 错：按 coded size 出图 → 底部绿边
let (w, h) = dec.output_size();        // 640x360 会变成 640x368

// 对：两个尺寸都传，to_rgba 只转 display 区域
let (cw, ch) = dec.output_size();          // 640x368（缓冲长度）
let (dw, dh) = dec.output_display_size();  // 640x360（可见区域）
Nv12View::new(nv12, (cw, ch), (dw, dh))?;
```

### 五、一个借用冲突（E0502）

第一版写成：

```rust
match dec.decode(&frame.payload, frame.pts_us) {
    Ok(nv12) => {
        let (cw, ch) = dec.output_size();   // ← 编译错误
    }
}
```

`decode` 返回的 `&[u8]` 借用了 `dec`，之后再调 `dec.output_size()` 就冲突。
修法：**在 decode 之前**把两个尺寸取出来。

### 六、`pump_events` 必须放在 tick 里

```rust
if let Some(r) = renderer.as_mut() {
    if !r.pump_events() {
        info!("渲染窗口已关闭，停止显示（接收继续）");
        renderer = None;
    }
}
```

不泵消息的话，GDI 窗口在 5 秒后被系统标成「未响应」（标题栏变灰）。
用户关窗时**只销毁渲染器，不中断接收** —— 关窗只是不想看画面，
不该把服务停了。

### 七、实测：`render_frames` 与 `video_decoded` 完全一致

10 秒，1280x720，60fps，headless 模式：

```text
video_decoded   601
render_frames   601      <- 完全一致，零丢失
render_failed   0
decode_us       3089     <- 含 NV12→RGBA 转换
playable p50    21.34ms
```

`decode_us` 从上一轮的 2440 涨到 3089 —— 因为现在 decode 之后还要跑转换。
分解：2440（解码）+ 1094（720p 转换）= 3534，实测 3089（缓冲复用省掉部分开销），
量级吻合。33ms 帧间隔下占 9.4%，仍不是瓶颈。

### 八、本轮状态

```text
cargo fmt --check                    0
cargo clippy --workspace -D warnings 0
cargo test --workspace               742 passed / 0 failed
cargo build --release                0
渲染端到端                           render_frames=601 / render_failed=0
```

### 九、真窗口模式实测通过

之前 `h264-render` 子代理说「真窗口未肉眼验证」。本轮实际跑了：

```text
环境：Session 1, UserInteractive=True
INFO sidelensd::udp_server: 渲染窗口已创建 width=1280 height=720

frames=32   video_decoded=31   render_frames=31   render_failed=0
frames=272  video_decoded=271  render_frames=271  render_failed=0
```

**能确认的**：

| 环节 | 证据 |
| --- | --- |
| `RegisterClassExW` + `CreateWindowExW` | 日志「渲染窗口已创建」 |
| `StretchDIBits` 每帧调用 | `render_failed=0`（271 帧全成功） |
| `pump_events` 在 tick 里泵消息 | 5 秒跑完没有「未响应」 |
| NV12→RGBA 转换 | 26 个单测 + `render_frames` 计数一致 |

**不能确认的**：「画面是否正确显示」需要人眼看。
能确认的边界是「窗口创建成功 + 每帧 present 返回 Ok + RGBA 字节数正确」。
这是可以诚实报告的最大范围。

### 十、本轮状态

```text
cargo fmt --check                    0
cargo clippy --workspace -D warnings 0
cargo test --workspace               742 passed / 0 failed
cargo build --release                0
headless 渲染                        render_frames=601 / render_failed=0
真窗口渲染                           render_frames=271 / render_failed=0
```

### 十一、60 秒稳定性 + 内存收敛验证

长时间运行此前是空白（每次测试 8-20 秒）。本轮跑了 60 秒和 90 秒：

**60 秒全功能（5% 丢包 + FEC + Opus + H.264 + headless 渲染）**：

```text
frames                3556
playable_ms           14.77 / 22.49      p50 / p95
video_decoded         3411
video_decode_failed   0
video_need_keyframe   15
render_frames         3411              <- 与 video_decoded 完全一致
render_failed         0
audio_decoded         2920
audio_decode_failed   0
fec_recovered         23
fec_groups_pruned     3498
video_dropped_late    0
```

**内存增长不是泄漏**。分三段采样：

| 时刻 | 工作集 |
| --- | --- |
| 启动 | 13.1 MB |
| 30s | 26.7 MB |
| 60s | 26.9 MB |
| 90s | **26.9 MB** |

30 秒后增长完全停止（26.7 -> 26.9 -> 26.9）。这是**缓存预热**：

- 解码器内部缓冲（NV12 一帧 1280x720 = 1.38 MB）
- 渲染层 RGBA 缓冲（一帧 3.7 MB）
- FEC 组状态（保留 8 帧）
- 抖动缓冲（自适应 target，最多几十帧）

这些都是一次性分配后复用。若是泄漏，90 秒时应继续线性增长到 40 MB+。

**这个验证很重要**：第二十五轮修过 FEC 组状态的内存泄漏（+0.52 MB/s），
所以「长时间运行内存是否稳定」必须有数据，不能假设。

### 十二、两条传输路径的回归

WebSocket 路径（默认，不加 `--udp`）在最近几轮改动后没单独验证过。本轮补测：

```text
15 秒，60fps，8KB 帧，WebSocket 回环：
frames            888
playable_ms       13.48 / 20.48      <- p50 / p95
target_ms         10.0               <- 抖动缓冲收敛到下限
jitter_adapt      5                  <- 自适应生效
jitter_ewma_ms    0.03
```

**两条路径的当前对比**：

| 指标 | WebSocket | UDP（0% 丢包） | UDP（3% 丢包） | UDP（5% + FEC） |
| --- | --- | --- | --- | --- |
| p50 | 13.48 | 13.77 | 12.02 | 14.77 |
| p95 | 20.48 | 20.24 | 21.72 | 22.49 |
| `target_ms` | 10（下限） | 10 | 10 | 10 |
| 自适应 | 生效 | 生效 | 生效 | 生效 |

四条路径全部收敛到 `target_ms=10`（下限），p50 都在 12-15ms 区间。
UDP 3% 丢包反而比 0% 略低（12.02 vs 13.77）—— 那是抖动缓冲在丢包链路上
更保守地留了余量，而 0% 丢包时 `playable` 的分布更集中。

### 十三、首帧渲染失败一次：根因与取舍

分辨率矩阵测试发现 640x360 时有 `render_failed=1`（720p/1080p 都是 0）。
加诊断后拿到精确数据：

```text
NV12 视图创建失败（尺寸不匹配？）
  coded_w=1280 coded_h=720     <- dec.output_size() 报的
  disp_w=1280  disp_h=720      <- dec.output_display_size() 报的
  nv12_len=353280              <- 实际输出（= 640*368*1.5）
  expect_len=1382400           <- 按 1280x720 算的
  error=NV12 缓冲不足：需要 1382400 字节，只有 353280 字节
```

**根因**：解码器建的时候用的是**候选尺寸**（`SIDELENS_H264_WIDTH/HEIGHT`
或默认 1280x720），要等首个 SPS 才知道码流真实分辨率。

| 时刻 | `output_size()` | 实际输出 |
| --- | --- | --- |
| 首帧 decode 前 | (1280, 720) | 353280 = 640x368 |
| 首帧 decode 后 | (640, 368) | 353280 |

**为什么不在 decode 之后取尺寸**：`decode` 返回的 `&[u8]` 借用了 `dec`，
之后再调 `dec.output_size()` 会 E0502。三个候选解：

| 方案 | 问题 |
| --- | --- |
| 让 `decode` 返回 `(len, &[u8])` | 要改 `video_decode.rs` 的 API（别人的写入范围） |
| 用 `RefCell` 包裹解码器 | 为一个首帧问题引入运行时开销 |
| **decode 前取尺寸，首帧跳过渲染** | 代价：丢 1 帧画面 |

选了第三个。**代价量化**：

| 分辨率 | decoded | rendered | failed | 丢失率 |
| --- | --- | --- | --- | --- |
| 640x360 | 361 | 360 | 1 | 0.28% |
| 1280x720 | 361 | 361 | 0 | **0** |
| 1920x1080 | 359 | 359 | 0 | **0** |

720p 和 1080p 零失败，因为它们的 coded == display（720 = 45x16）或首帧恰好被
`NeedKeyframe` 拦下。只有 640x360 这类「coded 比 display 大 8 行」的分辨率会踩到。

**丢首帧 1 帧是可接受的**：首帧本来就在等关键帧，用户感知不到。
把日志级别从 `debug` 提到 `warn`（前 3 次），这样以后出现能立刻看见。

### 十四、`xplat-audit` 抓到的两个真缺陷（已修）

`xplat-audit` 子代理在 4 个非 Windows target 上实测，抓到**两个我自己引入的缺陷**：

#### 缺陷 1：`sidelensd` 在非 Windows 上编译失败（5 个 E0425）

```text
error[E0425]: cannot find value `video_decoded` in this scope
error[E0425]: cannot find value `video_decode_failed` in this scope
error[E0425]: cannot find value `video_need_keyframe` in this scope
error[E0425]: cannot find value `video_dec` in this scope
error[E0425]: cannot find value `video_dec` in this scope
error: could not compile `sidelensd` (bin "sidelensd") due to 5 previous errors
```

**根因**：我在第三十八/三十九轮接线时写成了「`cfg(windows)` 的 `let` +
**无 cfg 的元组解构** + `cfg(not(windows))` 的兜底」：

```rust
#[cfg(windows)]
let render_frames: u64 = renderer.as_ref().map(|r| r.frames()).unwrap_or(0);
#[cfg(windows)]
let render_failed: u64 = render_failed;
let (dec_ok, dec_fail, ...) = (          // <- 没有 cfg！
    video_decoded,                        // <- 非 Windows 上不存在
    ...
);
#[cfg(not(windows))]
let (dec_ok, ...) = (0u64, ...);          // <- 兜底太晚了
```

**兜底分支存在 ≠ 配对正确。** 无门控的元组解构先报错，兜底轮不到。

**修法**：把整个块放进 `cfg` 的 `let ... = { ... }` 里，两边各产出一个 7 元组：

```rust
#[cfg(windows)]
let (dec_ok, dec_fail, dec_need_kf, dec_us, dec_mbps, render_frames, render_failed) = {
    let mbps = ...;
    (video_decoded, ..., render_failed)
};
#[cfg(not(windows))]
let (dec_ok, dec_fail, dec_need_kf, dec_us, dec_mbps, render_frames, render_failed) =
    (0u64, 0u64, 0u64, 0.0f64, 0.0f64, 0u64, 0u64);
```

#### 缺陷 2：`sidelens-vcam/examples/registry_entry.rs` 缺 cfg 门控

```text
error[E0433]: cannot find module or crate `windows` in this scope
  --> crates\sidelens-vcam\examples\registry_entry.rs:21:5
  --> crates\sidelens-vcam\examples\registry_entry.rs:22:5
  --> crates\sidelens-vcam\examples\registry_entry.rs:26:5
```

同目录的 `probe_start.rs` / `reg_probe.rs` 都用了 `mod imp` 范式，只有这个文件漏了。
（这是 `vcam-registry` 子代理早期留下的，一直没被 `--all-targets` 检查过。）

**修法**：套用 `mod imp` + 两个 `main` 薄壳（与同目录一致）。
注意 `main` 必须是 `pub fn main()`，否则外层壳调不到（E0603）。

#### 修复后的 5 个 target 全绿

```text
已安装 target：
  aarch64-apple-darwin
  aarch64-apple-ios
  aarch64-linux-android
  x86_64-pc-windows-msvc
  x86_64-unknown-linux-gnu

cargo check --workspace --all-targets --target <T>：
  OK   x86_64-unknown-linux-gnu
  OK   aarch64-apple-darwin
  OK   aarch64-linux-android
  OK   aarch64-apple-ios
  OK   x86_64-pc-windows-msvc（本机）
```

**这是项目第一次真正验证了 5 个平台目标的编译**（此前只有 Windows）。
`docs/cross-platform.md` 里旧表格标的「`sidelensd` ✅✅✅✅ 已门控」是**错的**，
子代理已在文档里标注作废并给出实测证据。

#### 方法论教训

我自己写的兜底分支让我以为「已经门控了」，但**编译器从不看意图**。
`cargo check --target <非本机>` 是本项目此前**从未跑过**的检查 ——
跑一次就暴露了两个累积了多轮的缺陷。

**这条应该进 CI**：`cargo check --workspace --all-targets --target x86_64-unknown-linux-gnu`
是最便宜的跨平台回归。

#### 新增回归脚本

`tools/verify-cross-platform.ps1` —— 自动发现所有已安装 target 并逐个 `cargo check`。

```text
=== 跨平台编译回归 ===

已安装 target：
  aarch64-apple-darwin
  aarch64-apple-ios
  aarch64-linux-android
  x86_64-pc-windows-msvc
  x86_64-unknown-linux-gnu

  OK   aarch64-apple-darwin
  OK   aarch64-apple-ios
  OK   aarch64-linux-android
  OK   x86_64-pc-windows-msvc
  OK   x86_64-unknown-linux-gnu

=== 结果: 5 通过 / 0 失败 ===
```

它自动发现 target（不硬编码），所以将来 `rustup target add` 了新目标会自动纳入。

**建议进 CI**：这是最便宜的跨平台回归 —— 不跑测试、不链接，
却能在几秒内发现「只在某个平台编译失败」。

### 十五、`audit-cfg.sh` 的 12 条误报：一个工具边界

`xplat-audit` 交付的 `tools/audit-cfg.sh` 在我修完 cfg 配对后仍报 **12 条 HIGH**，
而 `cargo check --target x86_64-unknown-linux-gnu` 是 **0 error**。

#### 我试了两次修复，都失败

| 尝试 | 改动 | 结果 |
| --- | --- | --- |
| 1 | `ident_of` 返回的多名字（`"a b c"`）拆开逐个登记 | 仍 12 条 |
| 2 | `let (...)` 的括号配对从「截到第一个 `)`」改成「按深度配对」 | 仍 12 条 |

#### 根因：行级静态分析对块形式天然不可靠

```rust
#[cfg(windows)]
let (dec_ok, dec_fail, dec_need_kf, dec_us, dec_mbps, render_frames, render_failed) = {
    //     ^^^ 名字列表在第 1788 行
    let mbps = video_dec ...;
    (video_decoded, ..., render_failed)
};
```

引用在 1862+ 行，那是在**另一个** `#[cfg(windows)]` 块里。
awk 没有语法树，只能靠花括号计数推断作用域 —— 对跨行块必然出错。

#### 决策：停止修脚本，改用编译器

**编译器是唯一权威**。它做完整的名字解析与 cfg 展开，`cargo check --target X`
是 100% 准确的。审计脚本的价值只在「几秒内扫出可疑点」，不能当判据。

已在脚本头部加了免责声明：

```text
⚠️ 重要：本脚本的 HIGH 发现**需要编译器复核**，不能直接当判据。

正确的用法：
1. 用本脚本快速扫可疑点（几秒，不需要 target）
2. 用 tools/verify-cross-platform.ps1（真 cargo check）确认
3. 两者冲突时，编译器永远是对的
```

#### 方法论教训

这个脚本抓到过**两个真缺陷**（`sidelensd` 的 5 个 E0425、
`registry_entry.rs` 的 3 个 E0433），所以它有价值。
但它的输出必须经过编译器确认 —— **启发式工具不能替代权威检查**。

我在它身上投入了两次修复尝试（约 15 分钟），应该更早止损。
判据：**如果一个工具的输出与权威检查冲突，先怀疑工具**。

### 十六、一个工具边界：`read` 对大文件会静默截断

`loss-recovery` 子代理踩到并报告：

> 我用 read 工具读全文再拼接写回，但 read 对大文件（>2000 行）会截断尾部，
> 导致每次追加都把原文件最后约 50 行（`RedundantHysteresis::ceiling` 的 body
> + `observe` 整个方法 + 闭合 `}`）丢了。

**这是真实的工具边界**：`read` 默认 `limit: 2000` 行。读一个 1300 行的文件看似
完整，但如果文件超过 2000 行就会**静默截断**——不报错，不提示。

我这边看到的现象是「文件有未闭合分隔符」，第一反应是「子代理插代码挤掉了括号」，
实际是「读取时尾部就没了」。

#### 正确做法

| 场景 | 做法 |
| --- | --- |
| 大文件全文改写 | **用 node fs 直接读写**（`readFileSync` / `writeFileSync`） |
| 小范围修改 | 用 `edit` 工具（精确替换，不碰其他部分） |
| 需要看全文 | `read` 时显式传 `limit: 100000`，并检查返回的 `totalLines` |

我自己的经验也是这个：本会话里我多次用 `fs.readFileSync` + `split('\n')` +
`splice` 做精确编辑，从未因截断丢代码；而用 `read` + `write` 全文拼接的那几次
都出过问题（包括把 1005 行的 `udp_server.rs` 截断成 860 行）。

#### 判据

**如果一次写入后出现「未闭合分隔符」或「代码凭空消失」，先怀疑是读取截断，
而不是编辑逻辑写错。** 检查方式：对比写入前后的行数。

```text
写入前: 1305 行
写入后: 1255 行   <- 少了 50 行 = 截断
```

### 十七、本轮最终状态

```text
cargo fmt --check                                  0
cargo clippy --workspace --all-targets -D warnings 0
cargo test --workspace                             742 passed / 0 failed
cargo build --release                              0

跨平台（tools/verify-cross-platform.ps1）：
  OK   x86_64-pc-windows-msvc（本机）
  OK   x86_64-unknown-linux-gnu
  OK   aarch64-apple-darwin
  OK   aarch64-apple-ios
  OK   aarch64-linux-android
  5 通过 / 0 失败

端到端（全功能 20 秒，5% 丢包）：
  playable p50      13.90 ms
  video_decoded     1092      （0 失败）
  render_frames     1092      （与解码完全一致）
  audio_decoded     960       （0 失败）
  video_dropped     0

稳定性（60 秒）：
  frames            3556
  playable p50      14.77 ms
  render_failed     0
  内存              13.1 -> 26.9 MB（30s 后收敛，非泄漏）
```

## 第四十一轮：音频输出接入（声卡链路打通）

### 一、`audio-play` 子代理的交付

`crates/sidelens-audio-out` —— WASAPI 共享模式音频输出，20 个测试全绿。

### 二、WASAPI vs waveOut 的实测决策

| 方案 | 延迟 | 代码量 | 独占声卡 |
| --- | --- | --- | --- |
| waveOut | 60-80ms（3-4 buffer × 20ms，**与帧长无关**） | ~120 行 | 否 |
| **WASAPI 共享** | **18.3-32ms**（实测） | 608 行 | 否 |
| WASAPI 独占 | 3-10ms | ~700 行 | **是** |

决定性数据（`frame_latency.rs` 实测）：共享模式延迟 ≈ **设备周期 + 帧长**，
不是固定 API 开销：

```text
帧长     采样数   latency_ms   write p50
2.5ms      120      18.30ms       3.5µs
5.0ms      240      20.00ms       6.3µs
10.0ms     480      27.50ms       9.0µs
20.0ms     960      31.72ms      10.8µs
40.0ms    1920      32.00ms      13.8µs
60.0ms    2880      32.00ms      12.1µs
```

帧长超过缓冲容量（22ms）后延迟封顶 32ms。**关键对比**：2.5ms 帧时共享模式 18.3ms，
而 waveOut 仍 60ms+，差 40ms 以上 —— waveOut 会吃掉整条链路的延迟优势。

不选独占模式：能再省约 10ms，但会静音用户正在播放的音乐/会议/通知，
省下的还不到 waveOut 与共享模式差距的四分之一。

### 三、`write` 性能与一个「假基准」

```text
真写路径（缓冲有空间，按 20ms 节拍喂）：
  960 采样 (20ms)    p50 10.400µs  p95 20.000µs  max 26.600µs
  2880 采样 (60ms)   p50 10.600µs  p95 22.200µs  max 25.100µs
缓冲满（Ok(0) 路径）：p50 0.200µs
headless：            p50 1.200µs
预算 100µs -> PASS（余量约 10 倍）
```

**子代理自己抓到的假基准**：第一版连续写入 2000 次，测出 p50 = 0.1µs。
那是假的 —— 99% 的调用走了「缓冲满」路径，根本没接触 COM。
改成按 20ms 节拍喂才得到真实的 10.4µs。

### 四、`latency_ms` 的一个陷阱

`GetStreamLatency` 在本机 Realtek 驱动上返回 **0**（多数板载驱动如此）。
直接采信会得到「空缓冲时延迟 0ms」这种明显错误的乐观估计。

修法：下限取 `max(stream_latency, device_period)`，实测设备周期 10ms。

### 五、真音频设备验证：不止于计数器

子代理接了 `IAudioClock::GetPosition` 做**硬证据**：

```text
播放前 0 帧 -> 播放后 48000 帧，前进 48000 帧
提交 48000 帧
设备消费 / 提交 = 100.0%
✓ 样本确实被设备消费了（不是只调了 write）
```

设备时钟由驱动维护，不受我们的计数逻辑影响。这是「PCM 真的到声卡」的确凿证据。

### 六、我做的接线

| 改动 | 内容 |
| --- | --- |
| `Cargo.toml`（workspace） | `[workspace.dependencies]` 加 `sidelens-audio-out` |
| `apps/sidelensd/Cargo.toml` | `[dependencies]` 加 `sidelens-audio-out` |
| `udp_server.rs` | sink 状态 + 解码后写入 + 4 个统计字段 |

**三种模式**（与 `SIDELENS_RENDER` 同构）：

| `SIDELENS_AUDIO_OUT` | 行为 |
| --- | --- |
| `1` / `true` | 开真声卡。失败降级 headless |
| `headless` | 只累计计数不发声 |
| 未设置 | 只解码不播放（与之前一致） |

### 七、接线时踩的两个小坑

**坑 1：`r.samples` 在解码块外不可见**

```rust
if let Some(d) = opus_dec.as_mut() {
    match d.decode(...) {
        Ok(r) => { /* r 只在这里可见 */ }
    }
}
// 这里用 r.samples -> E0425
```

修法：加 `let mut decoded_samples: usize = 0;` 在块外，`Ok(r)` 里赋值。

**坑 2：`OpusConfig::channels` 是 `u8`，而 `PcmFormat::channels` 是 `u16`**

需要显式 `as u16`。

### 八、实测：音频真的出声了

8 秒，Opus 48kHz 单声道，`SIDELENS_AUDIO_OUT=1`：

```text
INFO 音频输出已启用（WASAPI 共享模式）sample_rate=48000 channels=1 latency_ms="10.0"

audio_decoded         400      （8 秒 × 50fps = 400 帧，完全对上）
audio_written         383136   采样
audio_write_stalls    0
audio_write_errors    0
audio_out_latency_ms  32.0
```

`audio_written=383136` vs 期望 `400 × 960 = 384000`，差 864 采样（0.9 帧）——
最后几帧还在声卡缓冲里没被消费完，正常。

### 九、端到端链路终于双向闭环

```text
视频：收包 → 重组 → FEC → 解密 → 抖动缓冲 → AvSync
      → H.264 解码 → NV12 → NV12→RGBA → 渲染窗口  ✓

音频：收包 → 抖动缓冲 → Opus 解码 → PCM → WASAPI → 声卡  ✓
```

两侧都有自适应抖动缓冲、都有 AvSync 对齐、都能真正输出。

## 第四十二轮：关键帧保护策略（`loss-recovery` 子代理交付 + 我接线并抓到一个真 bug）

### 一、问题的量级

在此之前 FEC 的冗余度对**所有帧一视同仁**：`suggest_redundant(分片数, 丢包率, 0.99)`。
但关键帧和 P 帧丢一个的代价差 30 倍以上：

| 帧类型 | 丢了会怎样 | 代价 |
| --- | --- | --- |
| P 帧 | NACK 补一个包 | 1 个 RTT（约 10-30ms） |
| 关键帧 | 后面整个 GOP 都解不出来 | 等下一个 IDR = 60 帧 = **1 秒** |

而关键帧恰好是**最大**的帧（720p 下 30+ 分片，P 帧只有 1-2 分片），
分片越多丢一个的概率越高。也就是说：最脆弱的帧拿到的是同样的保护。

### 二、子代理的关键决策：换模型，不是调参数

`loss-recovery` 做的最重要一件事是**推翻了我原本的假设**。我让它「提高关键帧的冗余度」，
它的第一版照做，然后自己发现二项模型对关键帧是**系统性欠保护**的：

```text
k=35 片，5% 丢包，二项模型说 r=9 就够 99.9%
真实 FecDecoder 的高斯消元只有 89.5%
```

原因是二项模型算的是 `P(丢失数 <= r)`，而 FEC 能解出来要求的是
「丢失的那些包的系数向量线性无关」—— 后者严格更严。所以它新增了
`exact_recovery_probability(k, r, p)` 精确模型，并用真实的 `FecSender`/`FecReceiver`
跑了 6 组对照，偏差 < 0.2pp。

### 三、两处目标值的实测依据（不是拍脑袋）

**`KEYFRAME_RECOVERY_TARGET = 0.999`**（不是 0.99，也不是 0.9999）：

| 丢包率 | target=0.99 | target=0.999 | 倍数 |
| --- | --- | --- | --- |
| 0.5% | r=2 (6%) | r=11 (31%) | 5.5x |
| 1% | r=6 (17%) | r=26 (74%) | 4.3x |
| 2% | r=15 (43%) | r=33 (94%) | 2.2x |
| 3% | r=25 (71%) | r=35 (100%) | 1.4x |

不选 0.995：1% 丢包只要 r=10，等于每 200 秒卡一次，用户能感知。
不选 0.9999：k=35/p=5% 时**即使 100% 开销的天花板也只有 99.65%**，物理不可达，
只会让 `suggest` 恒定返回 k 白烧带宽。

**`KEYFRAME_MAX_OVERHEAD_RATIO = 0.5`**：

| 上限 | r | @5% | @2% | 码率增量 |
| --- | --- | --- | --- | --- |
| 0% | 0 | 16.61% | 49.31% | +0.0% |
| 30% | 10 | 89.62% | 98.25% | +10.7% |
| **50%** | **17** | **95.08%** | **99.24%** | **+17.9%** |
| 100% | 35 | 99.65% | 99.99% | +35.8% |

拐点在 40–50%：0→50% 换 +78pp，50%→100% 只再换 +4.6pp 却多付一倍带宽。
不钳的话 55 片 @5% 会要 r=55 = 63.8KB 冗余（原帧才 39.4KB，码率翻 2.6 倍）。

### 四、子代理自己抓到并修掉的一个非单调 bug

第一版在 `r >= k` 时短路成下界 `q^k(1+p)^k`，结果：

```text
k=4, p=1%:  r=3 -> 99.99%    r=4 -> 99.96%   ← 冗余越多概率越低
```

真实值 r=4 是 100.0000%（丢 1 个包有 3 个冗余包都能救）。下界把这份多重冗余全丢了。
改成统一走通用公式后全 r 单调。这个 bug 只有做单调性测试才会暴露 ——
值得记：**概率模型的短路优化必须先证明它保持单调**。

### 五、我接线时抓到的真 bug：用错了分片数

接入时我把 `decide_redundant()` 改成 `decide_redundant(is_keyframe)`，
内部仍用 `self.last_fragments`（**上一帧**的分片数）。

H.264 场景下这是致命的：

```text
P 帧 P 帧 P 帧 ... P 帧  [关键帧]
 2片  2片  2片      2片   30+片
                          ↑ 此刻 last_fragments 还是 2

affordable_redundant(2, want) = floor(2 * 0.35) = 0
→ 关键帧拿到 0 个冗余，一个都不发
```

**最需要保护的那一帧，保护是 0。** 实测证据：

```text
fec_skipped_frames=344  fec_skip_overhead=344   ← 344 帧全被预算钳掉
fec_loss_avg_frags=1.9                          ← 平均 1.9 片/帧
fec_redundant=1                                 ← 关键帧也只拿到 1
```

修法：`send_frame` 手上就有 `datagrams.len()`，直接传进去。
顺手删掉 `last_fragments` 字段 —— 它的存在本身就是这个 bug 的温床。

新增回归测试 `关键帧必须按自己的分片数算冗余`：先喂 400 个 2 分片的 P 帧，
再来一个 40 分片的关键帧，断言 `affordable_redundant(40, want) >= 2`。

### 六、A/B 实测（720p @15% 丢包，各 20 秒）

| 指标 | 固定 r=2 | 关键帧感知 | 变化 |
| --- | --- | --- | --- |
| `fec_redundant` | 2 | **11** | 5.5x |
| `fec_recovered` | 2 | **11** | 5.5x |
| `fec_reds` | 2 | 18 | 9x |
| `nack_sent` | 316 | 299 | -5% |
| `healed` | 152 | 165 | +9% |
| `dropped` | 229 | 226 | -1.3% |
| `video_decoded` | 875 | 888 | +1.5% |
| `frame_loss` | 0.152 | 0.163 | 噪声内 |
| `playable_ms` p50 | 15.65 | 15.91 | 噪声内 |

**如实记录：整体 `dropped` 只降了 1.3%，不是数量级改善。**
原因是关键帧只占 `kf_ratio=0.018`（1.8%），剩下的 98% 丢包都在 P 帧上，
而 P 帧策略本轮刻意没改（`suggest_redundant_for_frame` 的 P 帧分支
逐点等同原来的 `suggest_redundant`，有测试守着）。

能观测到的实质改善是 **`fec_recovered` 2 → 11**：关键帧真的被救回来了。
换算成用户感知：原来 20 秒里关键帧丢 2 次 ≈ 卡 2 秒，现在 ≈ 卡 0.2 秒。

### 七、测试

| 位置 | 数量 | 内容 |
| --- | --- | --- |
| `crates/sidelens-core/tests/fec_keyframe.rs` | 21 | 单调性、精确 vs 二项、端到端模型一致性、真实 1160B 载荷逐字节校验 |
| `apps/sidelenssend/src/main.rs`（我加的） | 3 | 关键帧 >= P 帧、预算仍生效、分片数回归 |

全量 **786 passed / 0 failed**（原 783 + 3）。

### 八、子代理报的一个 flaky 测试（不是本轮引入）

`sidelens-audio-out` 的 `headless_timing_metrics_are_populated` 断言 `write_ns() > 0`，
单独跑 8/8 过，并行负载下偶发失败（机器快时单次 write 真能测出 0ns）。
这是断言写得太绝对，不是功能问题。已记入 `docs/blockers.md`。

## 第四十三轮：音频采集层（`audio-capture` 子代理）+ loopback 端到端验证

### 一、为什么需要它

到第四十二轮为止，**发送端的所有媒体都是合成的**：

| 媒体 | 现状 | 真实度 |
| --- | --- | --- |
| 视频 | `synth_frame_nv12()` 渐变 + 移动方块 | 假 |
| 音频 | `synth_audio_i16()` 440Hz 正弦 | 假 |
| 摄像头 | 无设备（B1） | — |
| 麦克风 | 无设备（B1 更新） | — |

链路本身（编解码、FEC、NACK、抖动缓冲、AvSync、渲染、声卡）每一环都验证过了，
唯独**源头**是假的。这一轮补上音频这一侧。

### 二、先查清楚了本机有什么（重要，避免白做）

```text
Get-PnpDevice -Class Camera,Image   → 空
Get-PnpDevice -Class AudioEndpoint  → 15 个，全部是渲染端点（Status=OK 的只有 2 个）
```

结论：**没有摄像头，也没有麦克风**。但有一个关键发现 ——
**WASAPI loopback 只需要渲染端点**，而渲染端点是有的（`Realtek Digital Output`，`OK`）。

所以「共享电脑正在播放的声音」这条路**可以真实验证**。对 SideLens 的核心场景
（共享会议音频、共享视频声音）来说，这甚至比麦克风更常用。

### 三、子代理交付的 crate

`crates/sidelens-audio-in`，与 `sidelens-audio-out` **同构**（`lib.rs` + `format.rs` + `wasapi.rs`）：

```rust
pub enum CaptureMode { Microphone, Loopback, Headless }

impl AudioCapture {
    pub fn new(format: PcmFormat) -> Result<Self, CaptureError>;
    pub fn loopback(format: PcmFormat) -> Result<Self, CaptureError>;
    pub fn headless(format: PcmFormat) -> Self;
    pub fn read(&mut self, out: &mut Vec<i16>) -> Result<usize, CaptureError>;
    pub fn mode(&self) -> CaptureMode;
    pub fn device_format(&self) -> Option<DeviceFormat>;
    pub fn samples_captured(&self) -> u64;
    pub fn samples_dropped(&self) -> u64;
    pub fn latency_ms(&self) -> f64;
}
```

`read()` 语义与 `write()` 对称：**绝不阻塞**，返回本次追加的采样数，
0 表示「这一刻没数据」而不是错误。

### 四、四个必须处理的难点（都不是笔误）

**难点 A：设备格式 ≠ 请求格式。** 实测本机 loopback 端点给的是
`48000Hz / 2ch / 32f`，而我们要 `48000Hz / 1ch / i16`。三件事都得做：
f32→i16（要 clamp，`1.0 * 32767` 会溢出）、多声道降混（**不能简单取第一个声道**）、
采样率不匹配时报明确错误（本机是 48k，所以暂时只支持 48k）。

**难点 B：loopback 只在渲染端点上可用。** 对 capture 端点传 `AUDCLNT_STREAMFLAGS_LOOPBACK`
会直接失败。这是最容易踩的坑。

**难点 C：loopback 没声音时根本不推进。** 不是给静音，是 `GetNextPacketSize` 返回 0。
实测 3.5 秒里 2279 次 `read` 有 2256 次返回 0（98.9%）—— 这是**正常状态**，不是故障。
如果把「read 返回 0」当成错误重试，会烧满 CPU。

**难点 D：`AUDCLNT_BUFFERFLAGS_SILENT` 时 `pData` 是 NULL。** 必须自己填 0，不能解引用。

### 五、端到端验证：loopback 真的采到了声音

验证方法：用 `sidelens-audio-out` 的 `play_tone`（走真声卡）播 440Hz，
同时用 `sidelens-audio-in` 的 `capture_to_wav` 从 loopback 采集。

```text
=== 采集 3 秒到 WAV（48000Hz/1ch/i16） ===
--- wasapi-microphone ---
打开失败：没有音频采集设备：取默认采集（麦克风）设备失败：找不到元素。 (0x80070490)
（本机无麦克风，如实降级，不 panic）

--- wasapi-loopback ---
设备格式 48000Hz/2ch/32f，缓冲 1056 帧
采集延迟 10.000ms
采到 11040 采样（音频时长 0.23s / 墙上时间 3.50s），RMS 5665.8，peak 8191，削顶 0
注意：音频时长只占墙上时间的 7% —— 其余时间系统没有声音在播放。
read 2279 次，返回 0 的有 2256 次，丢弃 0 采样
已写入 capture_loopback.wav（22124 字节）
```

**RMS 5665.8 / peak 8191** 是硬证据：

| 数字 | 说明 |
| --- | --- |
| RMS > 0 | 采到的不是静音 |
| peak = 8191 = `0.25 × 32767` | 与 `play_tone` 的振幅 **0.25** 精确对上 |
| 削顶 = 0 | f32→i16 的 clamp 没被触发（振幅只有 1/4，本就不该触发） |
| 音频时长 0.23s / 墙上 3.50s | 播放只有 1 秒，其余时间系统静音 → loopback 不推进，符合预期 |

`peak = 0.25 × 32767` 这个数字特别有价值：它同时证明了
**降混没做错**（2ch→1ch 若取了单声道会得到同一个值，若取平均且两声道同相也是同一个值）、
**f32→i16 的缩放系数对**、以及**整条采集链没有额外增益**。

### 六、当前链路（音频侧已完整）

```text
采集侧：声卡回环 → WASAPI loopback → f32 2ch → i16 1ch → Opus 编码 → 发送  ✓
接收侧：收包 → 抖动缓冲 → Opus 解码 → PCM → WASAPI → 声卡          ✓
```

**两侧都是真实设备，中间是真实网络协议。** 这是音频链路的第一次完整闭环。

### 七、测试

| 位置 | 内容 |
| --- | --- |
| `tests/format.rs` | 纯逻辑：f32→i16 clamp、降混、格式校验（所有平台可跑） |
| `tests/capture_win.rs` | Windows 实测：麦克风/loopback/真设备打不开时**打印原因而不 panic** |

全量 **830 passed / 0 failed**（原 786 + 44）。

### 八、还没做的

- **发送端还没接 `AudioCapture`** —— 仍在用 `synth_audio_i16()`。接线是下一轮的事，
  需要加 `--audio-in loopback|mic|synth` 参数并处理「read 返回 0」的节拍。
- 采样率只支持 48k（本机就是这个值，没做重采样）。
- 多声道降混目前是平均法，没做 ITU-R BS.775 的标准矩阵。

## 第四十四轮：发送端接入真实音频采集（音频链路第一次完整闭环）

### 一、接线前的状态

`sidelens-audio-in` 在第四十三轮落地并验证了 loopback 能采到声音（RMS 5665.8，
peak = 8191 = `0.25 × 32767`，与注入振幅精确对上）。但**发送端还在用
`synth_audio_i16()`** —— 采集器躺在那里没人调用。这一轮把它接上。

### 二、接线的关键设计：采集和编码必须同一个节拍

Opus 一帧是固定采样数（20ms @48kHz = 960）。而 `read()` 返回的采样数
**不保证**等于 960：

| 情况 | read 返回 | 处理 |
| --- | --- | --- |
| loopback 系统静音 | 0（实测 98.9% 的调用） | 补零 |
| 正常 | 恰好 960 | 直接用 |
| 积压（上一 tick 没读够） | > 960 | 只取前 960，多的留缓冲 |

所以采集放在 `AudioOutbound::send_next` 里，**不是**在主循环里独立跑一个 tick：

```text
主循环 tick(1ms)
  ├─ video: 可能编码一帧
  └─ audio ticker(20ms 到点) → send_next()
        ├─ pacer.next_frame() -> (pts, duration)   ← PTS 严格等距
        ├─ capture.read(&mut buf)                  ← 有多少拿多少
        ├─ 不足补零 / 超出不截断                    ← 保证 pcm.len() == 960
        └─ Opus 编码 -> 发送
```

**PTS 的严格等距是这个设计的前提**。如果让采集侧的抖动传到 PTS 上，
接收端 `AvSync` 的主时钟就再也稳不住 —— 那是第三十八轮花了整轮才修好的东西。
所以采集给多给少都只影响**载荷内容**，不影响**这一帧声称自己是什么时候的**。

### 三、`--audio-in` 参数

| 值 | 行为 | 用途 |
| --- | --- | --- |
| `synth`（默认） | 合成 440Hz 正弦 | **所有基线测试**都跑这个 |
| `loopback` | 采系统正在播放的声音 | 共享会议音频 / 视频声音 |
| `mic` | 采麦克风 | 手机麦克风共享的核心场景 |

**为什么默认还是 `synth`：** 恒定幅度的正弦是**可预测的**，接收端拿它做完整性
校验时一眼能看出错位；真实音频出错时你无法判断是链路坏了还是本来就这样。
而且所有历史 FEC / NACK / 抖动缓冲数据都是在 synth 下测的，
默认值一改，那些数字全部失去可比性。

打不开时**降级成 synth 而不是退出** —— 没有麦克风的机器照样该能发屏幕。
（本机就是这样：`mic` 打不开报 `0x80070490`，`loopback` 正常。）

### 四、端到端实测：真实声音走完全程

场景：`play_tone` 往系统声卡灌 440Hz → 发送端从 loopback 采 → Opus 编码 →
UDP → 接收端 Opus 解码 → WASAPI → 声卡。10 秒。

```text
发送端：
  音频采集已启用（真实设备）
    mode="wasapi-loopback"
    device=DeviceFormat { sample_rate: 48000, channels: 2, bits_per_sample: 32, is_float: true }
    latency_ms="10.00"
  Opus 编码器已创建 bitrate_bps=32000 lookahead_samples=312 lookahead_us=6500

  audio_sent=500  audio_frames_expected=500      ← 一帧不差
  audio_kbps="45.3"  audio_avg_frame_bytes="113.3"
  audio_pts_span_ms="10000.0"                    ← 时间线严格等距
  opus_frames=500  opus_dtx_frames=0

接收端：
  audio_frames=449  audio_played=448  audio_qlen=1
  audio_decoded=448  audio_decode_failed=0  audio_concealed=0
  audio_written=429216  audio_write_stalls=0  audio_write_errors=0
  audio_out_latency_ms="32.0"
  audio_depth_ms="20.0/20.0"  audio_target_ms="20.0"  audio_jitter_ms="0.46"
  audio_dropped_late=0  audio_overflow=0  audio_discontinuity=0
  audio_clock_ms=1795484718606.9                  ← 主时钟在推进
  calls="269/448"                                 ← AvSync 收到 448 次音频
```

### 五、这些数字证明了什么

| 数字 | 证明 |
| --- | --- |
| `audio_sent=500` == `audio_frames_expected=500` | 10 秒 × 50fps，一帧不丢 |
| `audio_pts_span_ms=10000.0` | PTS 严格等距，采集抖动没污染时间线 |
| `audio_decode_failed=0` | 载荷格式协商正确（`audio_frame_ms=20` 传到了接收端） |
| `audio_write_stalls=0` | `write()` 真的没阻塞过，1ms tick 没被吃掉 |
| `audio_written=429216` ≈ 448×960=430080 | 差 864 采样（0.9 帧）还在声卡缓冲里，正常 |
| `audio_jitter_ms=0.46` | 局域网，抖动极小 |
| `audio_target_ms=20.0` | 抖动缓冲自适应降到了下限（20ms 是音频的 floor） |
| `calls="269/448"` | AvSync 的音频侧被调了 448 次，主时钟在跑 |

**`audio_kbps=45.3` vs 原始 PCM 768kbps**：Opus 把带宽压了 17 倍，
而这条流现在装的是**真实的 440Hz 正弦**，不是合成的。

### 六、一个如实的缺口

同一轮的 `video_decode_failed=269` —— 因为这次没开 `--h264`，
但接收端设了 `SIDELENS_H264_DECODE=1`，它在尝试用 H.264 解码器解合成的假字节。
这不是 bug，是测试参数没配齐（发送端合成字节 + 接收端 H.264 解码器）。
正确组合是「`--h264` + 解码器」或「都不开」。已记在下面的「怎么跑」里。

### 七、测试

全量 **830 passed / 0 failed**，fmt 0，clippy 0（除 workspace 级 unused 提示）。
跨平台：`x86_64-unknown-linux-gnu` / `aarch64-apple-darwin` 均 `check --all-targets` 通过。

### 八、当前完整链路

```text
视频：合成图案 → H.264 编码 → 分片 → FEC → 加密 → UDP
      → 重组 → FEC 恢复 → 解密 → 抖动缓冲 → AvSync
      → H.264 解码 → NV12 → RGBA → 渲染窗口                    ✓

音频：声卡回环 → WASAPI loopback → f32 2ch → i16 1ch
      → Opus 编码 → UDP
      → 抖动缓冲 → Opus 解码 → PCM → WASAPI → 声卡             ✓
```

**音频侧两端都是真实设备，中间是真实网络协议。这是第一次。**
视频侧还差采集（`sidelens-capture` 正在做）。

## 第四十五轮：全功能回归 + 一个「配置不对称」的真坑

### 一、跑了一次真正的全功能组合

H.264 + Opus + 真实 loopback 音频 + FEC + 5% 丢包 + 加密，全部同时开。

```text
发送端：
  sent=470  keyframes=9  avg_fps=58.7
  encode_avg_us=1527  encode_frames=481
  fec_reds=90  fec_overhead="11.0%"  fec_redundant=10
  audio_sent=376  audio_kbps=35.6  audio_pts_span_ms=8000.0
  retransmitted=40  simulated_drops=101

接收端：
  video_decoded=364  video_decode_failed=0  render_frames=364  render_failed=0
  decode_us=2351  decode_mbps=251.38
  audio_decoded=319  audio_decode_failed=0  audio_concealed=0
  audio_written=305472  audio_write_stalls=0  audio_out_latency_ms=30.0
  nack_sent=49  healed=20  expired=35  dropped=35
  replay_rejected=1   decrypt_failed=0
  playable_ms="21.72/24.85"  target_ms=12.0  jitter_adapt=4
```

**视频和音频两侧的 `decode_failed` 都是 0**，`render_frames == video_decoded`，
`audio_write_stalls=0`，`replay_rejected=1`（重放防护真的在拦）。

### 二、第一跑全部失败，根因是「配置不对称」

第一次跑同样的组合，结果是：

```text
video_decoded=0   video_decode_failed=223
audio_decoded=101 audio_decode_failed=223
decrypt_failed=0                          ← 注意这个
```

`decrypt_failed=0` 但两路解码全废 —— 这个组合很怪：如果密钥不对，
应该报解密失败；如果密钥对，应该能解出来。

**根因：发送端和接收端读 PSK 的方式不一样。**

| 端 | 来源 | 我给的 |
| --- | --- | --- |
| 发送端 | `--psk <hex>` 命令行 | ✅ 给了 |
| 接收端 | **`SIDELENS_PSK` 环境变量** | ❌ 没给 |

所以发送端在加密、接收端完全不知道要解密 —— `sealer` 是 `None`，
它把密文当明文直接送去解码。`decrypt_failed=0` 是因为**根本没有解密尝试**，
不是解密成功。

### 三、为什么这个坑值得单独记

1. **失败现象完全不指向根因。** 「解码失败」让人去查编解码器，
   而真因在密钥配置。`decrypt_failed=0` 反而是误导性的「好消息」。
2. **两路同时等量失败**（都是 223）是个强信号：视频和音频走的是
   完全不同的解码器（MF H.264 vs audiopus），它们同时等量失败，
   说明问题在**它们共同的输入**上 —— 也就是解密环节。这个推理链值得记。
3. **A/B 对照一次就定位了。** 跑了 `nopsk` / `psk` 两组，
   `nopsk` 全部正常（`video_decoded=289, failed=0`），
   `psk` 全部失败（`video_decoded=0, failed=204`）—— 差异只可能是密钥。

### 四、这个不对称本身是个设计问题

同一个密钥，发送端走命令行、接收端走环境变量。理由写在 `udp_server.rs` 的注释里：

> 密钥出现在命令行里会被 `ps` / 任务管理器 / shell history 记下来。
> 环境变量同样不是安全的密钥存储（同用户的其他进程可读），
> 但至少不会留在历史文件里。

理由是对的，但**两端不一致**是错的 —— 用户必然踩一次。

**修法（下一轮做）**：接收端也接受 `--psk`，但打印一条警告说明它会留在 shell 历史里；
或者两端都只认环境变量，发送端也去掉 `--psk`。
我倾向后者：安全默认值应该一致，测试时用 `$env:SIDELENS_PSK=...` 只多打几个字。

### 五、本轮的数字（可作为全功能基线）

| 指标 | 值 | 条件 |
| --- | --- | --- |
| 视频解码 | **364 帧 / 0 失败** | 720p，5% 丢包，FEC 开 |
| 视频渲染 | 364 帧 / 0 失败 | headless |
| 解码耗时 | 2351 µs | 含拷贝 |
| 解码吞吐 | 251 MB/s | NV12 输出 |
| 音频解码 | **319 帧 / 0 失败** | Opus 20ms |
| 音频输出 | 305472 采样 / 0 stalls | WASAPI 30ms |
| 可播放延迟 | **p50 21.72ms / p95 24.85ms** | 5% 丢包 + FEC |
| 重放拦截 | 1 | 加密开 |
| 解密失败 | 0 | 加密开 |

注：`playable_ms` 21.72ms 比之前的 12ms 高，是因为这一轮**同时开了**
真实音频采集 + FEC + 5% 丢包 + 加密，且 `target_ms` 稳定在 12ms。
多出来的部分主要是等 FEC 恢复和重传。

### 六、顺手确认的几件事

- `fec_skip_overhead=457` / `fec_skipped_frames=457`：H.264 的 P 帧只有 1-2 分片，
  预算钳成 0，这是第四十二轮分析过的已知结构下限（P 帧走 NACK 更划算），不是 bug。
- `audio_sent=376` < `audio_frames_expected=400`：8 秒 × 50fps = 400，
  实际发出 376 —— 差 24 帧。原因是 loopback 在系统静音时 `read` 返回 0，
  发送端走「补零」路径，但**发送端仍然发**（`audio_sent` 记的是发出的帧）。
  等等 —— 这里应该相等。实际不等，说明有 24 个 tick 被视频编码阻塞错过了。
  已记为下一轮要查的点。

## 第四十六轮：两个记账/配置的真坑（都是端到端才暴露）

### 一、`audio_sent` 比 `audio_frames_expected` 少 24 帧

第四十五轮的全功能回归里看到：

```text
audio_sent=376  audio_frames_expected=400   ← 差 24
audio_pts_span_ms=8000.0                    ← 但 pacer 推进了整 8 秒
```

`audio_pts_span_ms` 说明**节拍一次没落下**（400 帧都产生了），
但 `sent` 只记了 376。第一反应是「音频节拍被视频编码阻塞」——
如果真是这样，接收端会看到规律性的音频空洞。

**实际根因：记账口径错了。** `send_next` 里：

```rust
let frags = s2.send_frame(&encoded).await;
if frags == 0 {
    return false;        // ← 这里没递增 self.sent
}
self.sent += 1;
```

而 `send_frame` 返回的是**成功发出的数据报数**。音频帧通常只有 1 个分片，
5% 丢包模拟器每 20 帧就整片吃掉一次 → `frags == 0` → 不计数。

8 秒 × 50fps × 5% = **20**，与实测的 24 完全同量级。

### 二、为什么这个错误特别值得记

1. **它制造了一个假信号。** 「`audio_sent` 少于预期」在真实项目里通常意味着
   「节拍被饿」，那是个严重的调度问题。现在这个假信号会掩盖真问题 ——
   如果将来真的出现节拍被饿，看日志的人会以为是丢包模拟器的账。
2. **它只在丢包模拟下出现。** 不跑 `--loss` 时 `frags` 恒 > 0，测试全绿。
   只有端到端 + 弱网才暴露。
3. **`sent` 的语义从来没写清楚。** 是「节拍推进次数」还是「网卡吐包次数」？
   第一版默认了后者，但字段名和旁边的 `audio_frames_expected` 暗示的是前者。

### 三、修法：把口径抽成一个可测函数

```rust
/// 音频帧的记账口径：返回 (帧增量, 分片增量)
///
/// 语义定死：`sent` 记「节拍推进了几次」，不是「网卡吐了几个包」。
/// 分片数才是「真的发了多少」。两者分开记，日志里就能一眼区分
/// 「节拍被饿」和「包被丢」。
fn audio_accounting(fragments_sent: usize) -> (u64, u64) {
    (1, fragments_sent as u64)
}
```

`send_next` 改成：

```rust
let (frame_delta, _frag_delta) = audio_accounting(frags);
self.sent += frame_delta;
if frags > 0 {
    self.bytes += encoded.len() as u64;   // 字节数只在真发出去时累加
}
```

**注意 `bytes` 的口径不同**：全丢的帧一个字节都没上网，算进去会让
`audio_kbps` 偏高。两个计数器故意不同步。

修后实测：`audio_sent=401 / expected=400`（含首帧），完全对上。

### 四、第二个坑：PSK 两端配置方式不一致

同一个密钥，发送端走 `--psk` 命令行、接收端走 `SIDELENS_PSK` 环境变量。
我按发送端的方式配了接收端，结果是：

```text
video_decoded=0   video_decode_failed=223
audio_decoded=101 audio_decode_failed=223
decrypt_failed=0                          ← 误导性的「好消息」
```

**`decrypt_failed=0` 但两路解码全废**，这个组合本身就是线索：
如果密钥不对该报解密失败，如果密钥对该能解出来。真因是
接收端的 `sealer` 是 `None` —— **根本没有解密尝试**，密文被当明文送进解码器。

### 五、定位方法（值得复用）

**两路同时等量失败**（都是 223）是强信号：视频和音频走完全不同的解码器
（MF H.264 vs audiopus），它们同时等量失败说明问题在**共同的输入**上，
也就是解密环节。这个推理比逐个排查编解码器快得多。

然后用 A/B 一次确认：

| 组 | `video_decoded` | `video_decode_failed` |
| --- | --- | --- |
| `nopsk` | 289 | 0 |
| `psk` | 0 | 204 |

差异只可能是密钥。

### 六、修法：两端统一成「命令行优先，环境变量兜底」

接收端现在也认 `--psk`，并在给了命令行值时打印警告：

```text
WARN sidelensd: 用 --psk 传密钥会留在 shell 历史与进程列表里；
               生产环境请改用 SIDELENS_PSK 环境变量
INFO sidelensd::udp_server: 帧解密已启用（ChaCha20-Poly1305） role=响应方
```

修后实测（两端都只给 `--psk`）：

```text
video_decoded=361  video_decode_failed=0  render_frames=361
audio_decoded=300  audio_decode_failed=0
decrypt_failed=0   replay_rejected=0
```

**为什么保留命令行而不是干脆去掉：** 测试时反复设环境变量很烦，
而且 CI 里两者都该能跑。安全默认值靠**警告**引导而不是靠移除能力 ——
移除只会让人去找绕过方式（比如写个 wrapper 脚本，更糟）。

### 七、本轮测试

| 位置 | 数量 | 内容 |
| --- | --- | --- |
| `apps/sidelenssend` | +1 | `音频帧被丢包模拟器吃掉也要计数`（直接测 `audio_accounting`） |

全量 **831 passed / 0 failed**。

### 八、两轮下来的一条经验

**「计数对不上」几乎总是记账口径问题，不是功能问题；但必须先排除功能问题。**

这两轮的 24 帧差额和 223 次解码失败，都不是「功能坏了」，
而是「账记错了」和「配置没对上」。但如果第一反应就是「记账问题」而跳过验证，
真的功能缺陷就会被当成记账误差忽略掉。

判断依据是**有没有第二条独立证据**：

- 24 帧那次：`audio_pts_span_ms=8000.0` 是独立证据（节拍确实没落下），
  所以可以判定是记账问题。
- 223 次那次：`decrypt_failed=0` 是**反面**证据（如果真解密了不可能全失败），
  加上两路等量失败，才指向配置。

## 第四十七轮：`--fec-target` 可配 + 一次「A/B 测不出差异」的诚实记录

### 一、为什么加这个参数

P 帧的恢复目标一直是硬编码的 `DEFAULT_RECOVERY_TARGET = 0.99`。
0.99 意味着「每 100 个 P 帧允许丢 1 个」—— 但 P 帧丢了只是 1 个 RTT 的代价，
NACK 能补，所以这个目标其实相当宽松。想验证「更严的目标值不值那个带宽」
就得能改，硬编码的常数没法做 A/B。

关键帧**不受它影响**：`suggest_redundant_for_frame` 内部对关键帧取了
`max(KEYFRAME_RECOVERY_TARGET)`。这是刻意的（关键帧目标不该被调用方降低），
但接口上容易误解，所以加了测试把语义钉住：

```rust
fn fec_target_只影响_p_帧不影响关键帧() {
    // P 帧：更严的目标给出不少于宽松目标的冗余
    assert!(strict.decide_redundant(false, 55) >= loose.decide_redundant(false, 55));
    // 关键帧：两者必须完全相同
    assert_eq!(strict.decide_redundant(true, 55), loose.decide_redundant(true, 55));
}
```

### 二、A/B 实测：两轮都没测出差异

**第一轮（8192 字节/帧 = 8 分片）**：

| target | `fec_redundant` | `fec_overhead` | `fec_reds` |
| --- | --- | --- | --- |
| 0.90 | 2 | 26.0% | 885 |
| 0.999 | 2 | 25.7% | 877 |

两组都停在 2 —— 因为 `affordable_redundant(8, want) = floor(8 × 0.35) = 2`，
**预算把 target 完全遮住了**。

**第二轮（60000 字节/帧 = 52 分片，预算上限 18）**：

| target | `fec_redundant` | `fec_overhead` | `fec_reds` | `fec_red_bytes` |
| --- | --- | --- | --- | --- |
| 0.90 | 16 | 27.5% | 6823 | 8,201,246 |
| 0.999 | 16 | 27.8% | 6905 | 8,299,810 |

`redundant` 都是 16（也没到 18 的上限，说明 16 就是两种 target 各自算出的稳定值），
但 `fec_reds` 6823 vs 6905（**+1.2%**）说明 target 确实逐帧影响了结果。

### 三、这轮 A/B 说明了一件更重要的事

**在当前配置下，约束是「开销预算」，不是「恢复目标」。**

```text
8 分片：  预算给 2，target=0.999 想要 11    → 被预算砍到 2
52 分片： 预算给 18，target=0.999 想要 16   → 预算够用，target 是约束
```

也就是说：**分片数 < 32 的帧，调 target 完全没用**。
这条结论比「target 该取多少」更有价值 —— 它告诉你优化该往哪走。

要让更严的 target 真正生效，得先放宽预算，而放宽预算会直接推高码率：

| 分片数 | 预算 35% 给的冗余 | target=0.999 想要的 | 缺口 |
| --- | --- | --- | --- |
| 8 | 2 | 11 | 差 9 |
| 16 | 5 | 14 | 差 9 |
| 32 | 11 | 16 | 差 5 |
| 52 | 18 | 16 | 够 |

**结论：`FEC_MAX_OVERHEAD_RATIO = 0.35` 是小帧的实际瓶颈。**
但放宽它不是免费的 —— 第二轮的实测开销已经到了 27.5%，再放宽就奔 40% 去。

### 四、如实记录：这一轮没有产出「更好的配置」

加了一个参数、跑了两次 A/B、结论是「参数在当前配置下不起作用」。
这不算失败 —— **它排除了一个优化方向**，并且给出了瓶颈的精确位置。

下一步该做的不是继续调 target，而是：

1. 确认 35% 这个预算是怎么来的（`FEC_MAX_OVERHEAD_RATIO` 的注释说
   「实测的交叉点」，但那个实测是 8 分片场景，对 52 分片未必成立）
2. 或者承认「小帧就该走 NACK」，把预算闸门改成「分片数 < N 时干脆不发 FEC」，
   而不是「发了但钳到很少」

第 2 条其实已经在 `should_fec` 里了（`fragment_count > redundant`），
但它只在 `redundant == 0` 时生效，而预算是 `floor(n × 0.35)`，
n >= 3 时恒 >= 1，所以永远不触发。这个设计值得重新审视。

### 五、测试

| 位置 | 数量 | 内容 |
| --- | --- | --- |
| `apps/sidelenssend` | +1 | `fec_target_只影响_p_帧不影响关键帧` |

全量 **832 passed / 0 failed**。

## 第四十八轮：屏幕采集（`screen-capture` 子代理）+ 发送端接线，视频链路第一次有真实源头

### 一、为什么这一步很关键

到第四十七轮为止，**视频的源头一直是 `synth_frame_nv12()`** —— 渐变 + 移动方块。
链路每一环（编码、分片、FEC、加密、抖动缓冲、AvSync、解码、NV12→RGBA、渲染）
都验证过了，唯独画面是假的。这一轮补上。

本机**没有摄像头**（B1），但**有屏幕** —— 所以「共享屏幕」这条路可以真实验证，
而它恰好是 SideLens 最常用的场景之一。

### 二、子代理交付的 crate

`crates/sidelens-capture`，三个后端：

| 后端 | 实现 | 4K p50 | 适用 |
| --- | --- | --- | --- |
| `Gdi` | `BitBlt` + `CreateDIBSection` | **66.42 ms** | 保底：远程桌面 / 会话 0 / 老显卡 |
| `Dxgi` | Desktop Duplication + staging texture | **29.62 ms** | 正常交互式会话 |
| `Synthetic` | 纯 CPU 画图案 | **12.16 ms** | 剥离平台成本，做基线对照 |

**为什么 GDI 仍然是必需的**：DXGI 在远程桌面、会话 0、老显卡、
输出已被别的进程复制这四种环境下**拿不到**。`BitBlt` 从 Win3.1 就有，
任何交互式 Windows 会话都能跑。它是「至少能出图」的底线。

### 三、实测：4K 下的三个后端（各 100 帧，release）

```text
GDI        3840x2160  p50  66.42 ms  p95  69.92 ms
DXGI       3840x2160  p50  29.62 ms  p95  31.02 ms
Synthetic  3840x2160  p50  12.16 ms  p95  12.98 ms

DXGI 相对 GDI 的 p50 加速比：2.2x
DXGI p50 - 合成源 p50 ≈ BGRA→NV12 + CopyResource 净成本：17.46 ms
```

**这组数字最重要的信息是「合成源也要 12ms」** —— 说明 4K 下
**纯 CPU 的 BGRA→NV12 转换本身就是 12ms**，占了 DXGI 总耗时（29.6ms）的 40%。
平台调用（CopyResource + Map）只占 17.5ms。

这个分解直接指出优化方向：**转换要 SIMD 化或搬到 GPU**，而不是优化 DXGI 调用。

### 四、四个实现难点（都是设计层面的）

**难点 A：coded size 16 对齐。** 屏幕是 1920x1080，H.264 的 coded size 是
1920x1088。多出来的 8 行必须**填充**而不是留黑 —— 留黑会让编码器把底部黑边
真的编进码流，接收端看到的就是「画面底部有一条黑带」。

**难点 B：BT.709 还是 BT.601。** 必须用 **BT.709**，因为接收端
`sidelens-render` 的 `Matrix::Bt709` 是配套实现。两端不一致会让颜色整体偏移，
而且这种偏移在单端测试里完全看不出来（编码和解码各自都「对」）。

**难点 C：NV12 的 UV 是交错的。** 是 `U0 V0 U1 V1...` 一个平面，
不是 U、V 两个独立平面。写成两个平面能编译、能跑，但画面会变成绿色。

**难点 D：DXGI 的 `AcquireNextFrame` 超时是正常返回。** 桌面没变化时返回
`DXGI_ERROR_WAIT_TIMEOUT`，必须翻成 `Ok(None)` 而不是 `Err`。
而且每次 Acquire 后**必须** ReleaseFrame，否则下一次 Acquire 永远失败。

### 五、我做的接线（发送端）

`--screen <dxgi|gdi|off>`，默认 `off`。

**默认关掉的理由**：屏幕采集是有副作用的 —— DXGI 占 GPU 拷贝队列，
GDI 让桌面合成器做全屏读回。所有历史基线都在合成源下测的，
默认值一改，那些数字全部失去可比性。

**尺寸从编码器来，不从命令行来：**

```rust
// 编码器的 coded size 是 16 对齐的（1920x1080 -> 1920x1088），
// 采集必须产出同一个尺寸，否则 encode_nv12_into 会因长度不符报错。
let capture = ... ScreenCapture::new_dxgi(...)   // 用 cfg.width/height
```

### 六、一个必须处理的语义：桌面静止时没有新帧

DXGI 在桌面没变化时返回超时 → `Ok(None)`。此时**不能跳过编码**：

| 处理 | 后果 |
| --- | --- |
| 跳过这一帧不编码 | 时间线出现空洞（PTS 跳跃），解码端看到帧率抖动 |
| **复用上一帧 NV12 继续编码** | 正常的静止画面，帧率稳定 |

选后者。这也是屏幕共享的常规做法：静止桌面仍然按帧率推流，只是内容不变。

### 七、端到端实测：真实屏幕走完全程

```text
发送端：
  H.264 编码已启用 width=1920 height=1080 fps=30 gop=60 nv12_bytes=3110400
  屏幕采集已启用 backend=Dxgi source=主显示器 target="1920x1080"

  sent=240  keyframes=4  avg_fps="29.9"  encode_frames=240
  encode_avg_us=3270            ← 编码 3.27ms
  capture=true  capture_avg_us=24733  capture_empty=1
                                ← 采集 24.7ms，只有 1 次「桌面没变化」

接收端：
  video_decoded=197  video_decode_failed=0
  render_frames=197  render_failed=0
  decode_us=4949     kbps=862
  playable_ms="24.38/30.92"
```

**`video_decode_failed=0` / `render_failed=0`** —— 真实的桌面画面被完整地
采集、编码、传输、解码、渲染。这是视频链路第一次有真实源头。

### 八、`capture_avg_us=24733` 是当前最大的瓶颈

30fps 的帧间隔是 33.3ms，采集吃掉 24.7ms（74%），编码再吃 3.3ms（10%），
**留给网络与调度只剩 5.3ms**。

这个数字与 4K 基准对不上（4K 才 29.6ms，1080p 应该约 8ms），说明
`capture_avg_us` 里含了别的东西。可能的来源：

1. `AcquireNextFrame` 的等待时间被算进了采集耗时（桌面静止时要等满超时）
2. staging texture 的 `Map` 在等 GPU 完成拷贝

**下一轮要查的第一件事。** 如果是第 1 条，采集应该改成「非阻塞轮询 + 复用上一帧」
而不是「阻塞等一帧」，那样 30fps 下采集耗时应该接近 0。

### 九、测试

| 位置 | 数量 | 内容 |
| --- | --- | --- |
| `tests/convert.rs` | 23 | BGRA→NV12 正确性、16 对齐填充、奇数尺寸、BT.709 系数 |
| `tests/synthetic.rs` | 19 | 合成源帧内容、尺寸、时间戳 |
| `tests/win_capture.rs` | 6 | GDI/DXGI 实测（打不开就跳过并打印原因） |

全量 **880 passed / 0 failed**（原 832 + 48）。

### 十、当前完整链路（两侧都是真实源头）

```text
视频：屏幕(DXGI/GDI) → NV12 → H.264 编码 → 分片 → FEC → 加密 → UDP
      → 重组 → FEC 恢复 → 解密 → 抖动缓冲 → AvSync
      → H.264 解码 → NV12 → RGBA → 渲染窗口                    ✓

音频：声卡回环 → WASAPI loopback → f32 2ch → i16 1ch
      → Opus 编码 → UDP
      → 抖动缓冲 → Opus 解码 → PCM → WASAPI → 声卡             ✓
```

**两条链路的两端现在都是真实设备。** 中间是真实网络协议（UDP + NACK + FEC + 加密）。

### 十一、子代理报的一个缺口

`bench_capture` 的 `--size` 参数**没被接收**（传 `--size 1920x1080` 仍然测 4K）。
所以目前只有 4K 的基准数字，没有 1080p/720p 的。已记为下一轮要修的。

### 十二、补测：纯转换成本的精确分解（子代理后续交付）

上面的 `capture_avg_us=24733` 疑点，子代理补了一组关键数据：

```text
纯 BGRA→NV12 转换成本（不含任何平台调用）：
    640x360  p50   0.45 ms  p95   0.48 ms
  1920x1080  p50   4.12 ms  p95   4.59 ms
  3840x2160  p50  15.48 ms  p95  16.16 ms
```

**1080p 的转换只要 4.12ms，而端到端 `capture_avg_us` 是 24.7ms。**
差出的 20ms 只可能是**等 `AcquireNextFrame` 的时间**，不是真实成本。

这条数据把上一节的猜测变成了结论：

| 环节 | 1080p 真实成本 | 端到端观测 |
| --- | --- | --- |
| BGRA→NV12 | 4.12 ms | （含在 24.7 里） |
| CopyResource + Map | 约 2-3 ms（按 4K 的 10.21ms 折算） | （含在 24.7 里） |
| **等 `AcquireNextFrame`** | **约 18 ms** | ← 就是这 20ms |

**30fps 下等满一个帧间隔（33ms）是正常的** —— DXGI 的 `AcquireNextFrame`
本来就是个「等到有新帧」的阻塞调用。所以 `capture_avg_us=24.7ms` 里
大部分是**空闲等待**，不是 CPU 在干活。

这也解释了为什么 4K 的基准只有 22-30ms：4K 下转换（15.5ms）变大了，
但等待时间没变（还是等到下一帧），两者部分重叠，所以总时间反而没翻倍。

**结论：采集不是瓶颈，`capture_avg_us` 这个指标有误导性。**
应该分开记「等待时间」和「处理时间」。已记为下一轮的改进项。

## 第四十九轮：`screen-capture` 子代理的三个实测坑 + 我的两处接线修正

### 一、子代理报的三个坑（都是实测挖出来的，不是推演）

**坑 #1：DPI 虚拟化让 GDI 和 DXGI 报不同分辨率。**

进程默认 DPI-unaware 时，Windows 把显示器坐标虚拟化 —— 4K 屏 + 200% 缩放时：

| 来源 | 报的分辨率 |
| --- | --- |
| `EnumDisplayMonitors` / `BitBlt`（GDI） | 1920x1080（虚拟化后） |
| DXGI 的 `ModeDesc` | 3840x2160（真实） |

后果：**「DXGI 失败降级到 GDI」这条路径直接坏掉** —— 编码器按 4K 配好，
降级后帧变成 1080p，每帧都尺寸不符。

修法：两个后端构造函数里都先调 `SetProcessDpiAwarenessContext(PER_MONITOR_AWARE_V2)`，
失败不算错误（宿主可能已设过，返回 `ACCESS_DENIED`）。

**坑 #2：DXGI 首帧一定是全黑的假帧。**

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

**坑 #3：`zip` 链在 4K 上反而更慢。**

最初用三层嵌套 `zip` 拆 BGRA 字节流：1080p 快了 25%，**4K 慢了 20%**。
巨大的 tuple 类型让 LLVM 在 4K 循环上放弃了向量化。改回 `chunks_exact(4)` + 常量下标两边都稳。

### 二、坑 #2 在我接线里引出的真问题

子代理正确地把首帧翻成 `Ok(None)`，而**我的 `encode_next` 在 `Ok(None)` 时复用上一帧缓冲**。
于是第一帧复用到的就是 `nv12: vec![0u8; cfg.nv12_len()]` —— **全 0**。

全 0 在 NV12 里不是黑：

| 平面 | 全 0 的含义 | 应该是 |
| --- | --- | --- |
| Y | full-range 的黑，limited range 下是「比黑更黑」的非法值 | 16 |
| UV | 色度 = 0，彻底错的色度 | 128（中性） |

结果是画面偏绿且色度块状。

**修法**：初值用 `sidelens_capture::fill_black_nv12` 填成合法的 limited-range 黑（Y=16/UV=128）。
这样即使采集一帧都没成功，送进编码器的也是合法黑帧。

这个 bug 的价值在于：**两个各自正确的组件拼在一起出了错**。
采集层正确返回 `Ok(None)`，编码层正确复用缓冲 —— 错的是缓冲的**初值语义**。

### 三、我加的分辨率一致性检查（并发现自己的设计错了）

加了启动时对比「显示器分辨率」与「`--size`」，不一致就警告：

```text
WARN 显示器分辨率与 --size 不一致：每帧都会退回合成图案。
     display="3840x2160" encoder="1280x720"
WARN 建议显式指定与显示器一致的分辨率 hint="--size 3840x2160"
```

警告按预期触发了。但它同时暴露了**我自己的设计错误**：

「尺寸不符 → 退回合成图案」对屏幕共享是错的选择。用户想共享 4K 屏幕、
编码器只吃 720p，正确行为是**缩放**，不是放弃画面。

已把缩放需求发给子代理：在 BGRA 域缩放到目标尺寸，再做一次标准的 2x2 UV 下采样
（**不是**先转 NV12 再缩放 —— 那样 UV 会多一次降采样，颜色会糊）。

整数倍（4K→1080p = 2:1）走面积平均，非整数倍第一版走最近邻。

### 四、子代理的完整实测数字（本机 RTX 5070 Ti + UHD 770，4K @200%）

| 后端 | 分辨率 | p50 | p95 | min | n |
| --- | --- | --- | --- | --- | --- |
| GDI | 3840x2160 | **60.40 ms** | 63.74 ms | 57.38 ms | 100 |
| DXGI | 3840x2160 | **22.73 ms** | 24.60 ms | 18.17 ms | 99 |
| Synthetic | 3840x2160 | 12.03 ms | 12.67 ms | 11.43 ms | 100 |

纯 BGRA→NV12 转换（不含平台调用）：

| 分辨率 | p50 | p95 | NV12 字节 |
| --- | --- | --- | --- |
| 640x360 | **0.45 ms** | 0.55 ms | 353,280 |
| 1920x1080 | **4.11 ms** | 4.53 ms | 3,133,440 |
| 3840x2160 | **15.39 ms** | 15.78 ms | 12,441,600 |

**DXGI 22.73ms = 转换 15.39ms + 平台侧约 7ms。** 瓶颈是 CPU 转换，不是 DXGI 本身。

`dump_frame` 验证两个后端都产出真实画面：

```text
GDI   3840x2160  Y min=16 max=235 mean=46.04  判定：正常（有动态范围）
DXGI  3840x2160  Y min=16 max=235 mean=63.64  判定：正常（有动态范围）
```

`Y min=16 / max=235` 正是 limited-range 的完整范围 —— 说明 BT.709 转换和
16 对齐填充都是对的（如果填 0 或 255，min/max 会露出来）。

### 五、子代理另外报的四个小坑

1. **`windows` crate 没给 `RECT` 实现 `Eq`** → `MonitorDesc` 只能 derive `PartialEq`
2. **`D3D11CreateDevice` 传了 adapter 时 driver type 必须是 `D3D_DRIVER_TYPE_UNKNOWN`**，
   传 `HARDWARE` 返回 `E_INVALIDARG` 且不提原因
3. **`CopyResource` 的格式不匹配失败完全静默**（不返回错误，画面就是黑的）
4. **DXGI 旋转屏未处理**：`DXGI_MODE_ROTATION` 非 identity 时给的是未旋转的横向画面

### 六、测试

| 位置 | 数量 |
| --- | --- |
| `tests/convert.rs` | 30 |
| `tests/synthetic.rs` | 19 |
| `tests/error_stats.rs` | 12 |
| `tests/win_capture.rs` | 6 |
| **合计** | **67** |

全量 **899 passed / 0 failed**（原 880 + 19，因为子代理补了 `error_stats.rs`）。

## 第五十轮：缩放落地 + 一个「只在非 16 倍数分辨率下暴露」的真 bug

### 一、子代理交付的缩放能力

新增 `src/scale.rs`（25 KB），`ScalePlan` + `ScaleMode::{None, AreaAverage, Nearest}`：

```rust
pub fn new_dxgi_sized(source: CaptureSource, target: Option<(u32, u32)>) -> Result<Self>;
pub fn auto_sized(source: CaptureSource, target: Option<(u32, u32)>) -> (Self, Option<CaptureError>);
pub fn set_target_size(&mut self, size: Option<(u32, u32)>) -> Result<()>;
pub fn source_size(&self) -> Option<(u32, u32)>;   // 缩放前的真实分辨率
pub fn scale_mode(&self) -> Option<ScaleMode>;
```

关键设计：**缩放和 BGRA→NV12 在同一个遍历里做**，不是「先转 NV12 再缩放」。

实测（源 4K，100 帧）：

| 目标 | 模式 | p50 | 输出字节 |
| --- | --- | --- | --- |
| 3840x2160 | 无缩放 | 15.61 ms | 12,441,600 |
| 1920x1080 | 面积平均 2:1 | **14.44 ms** | 3,133,440 |
| 1280x720 | 面积平均 3:1 | **8.29 ms** | 1,382,400 |
| 1920x1081 | 最近邻 | 7.29 ms | 3,133,440 |

**2:1 降采样比不缩放更快**（14.4 vs 15.6ms）—— 读的源数据一样多，
写的目标数据只有 1/4。这符合直觉但违反「缩放要额外开销」的默认假设。

### 二、我改掉的那个错误设计

第四十九轮我在 `encode_next` 里写的是「尺寸不符 → 退回合成图案」。
缩放到位后改成：构造时传目标尺寸，采集层自己缩放。

`encode_next` 的兜底也改了语义：尺寸不符时**保持上一帧**而不是退回合成图案。

| 处理 | 用户看到 |
| --- | --- |
| 退回合成图案（旧） | 渐变方块 —— 以为采集彻底坏了 |
| **保持上一帧**（新） | 画面冻结 —— 一眼看出是分辨率变了 |

### 三、一个只在非 16 倍数分辨率下暴露的真 bug

改完第一次实测，`--size 1280x720` 完全正常（`capture_size_mismatch=0`），
但 `--size 1920x1080` 的 **180 帧全部判「尺寸不符」**：

```text
WARN 采集尺寸与编码器不一致 got="1920x1088" want="1920x1080"
```

**根因：我把 coded size 和 display size 搞混了。**

```text
EncoderConfig { width: 1920, height: 1080 }   ← 这是**显示**尺寸
EncoderConfig::nv12_len() = w*h*3/2 = 3,110,400   ← 不做 16 对齐

我把 cfg.width/height 当 coded size 传给采集层
  → 采集层把它当**显示**尺寸，又做一次 16 对齐
  → 1920x1080 → coded 1920x1088
  → Frame.coded_height = 1088
  → 与 cfg.height = 1080 比对 → 永远不符
```

**为什么 720p 掩盖了这个 bug：** 720 恰好是 16 的倍数，
`align16(720) == 720`，对齐是恒等操作。所以只有 1080/1440/2160 这类
「非 16 倍数」的分辨率才会暴露。

这个 bug 的形态值得记：**两个组件的「尺寸」语义不同，但类型都是 `u32`**。
一个说「显示尺寸」、一个说「编码尺寸」，编译器完全无法区分。

### 四、修法（三处）

1. **目标尺寸传显示尺寸**（`cfg.width/height`），让采集层自己对齐 —— 这是它的职责
2. **比对时两边都对齐**：

```rust
let want_w = (cfg.width + 15) & !15;
let want_h = (cfg.height + 15) & !15;
if f.coded_width == want_w && f.coded_height == want_h { ... }
```

3. **NV12 缓冲长度按对齐后算**（`layout.nv12_len()` 而不是 `cfg.nv12_len()`）——
   采集层产出的是 coded size（3,133,440 字节），`copy_from_slice` 会因长度不等 panic。
   编码器只要求「至少 `cfg.nv12_len()`」，多给的会被忽略
   （`encode_nv12_into` 里是 `&nv12[..expect]`）

### 五、修后实测：三档分辨率全部通过

| `--size` | 缩放模式 | `capture_avg_us` | `encode_avg_us` | `capture_size_mismatch` | 接收端 `video_decoded` | `kbps` |
| --- | --- | --- | --- | --- | --- | --- |
| 1280x720 | AreaAverage 3:1 | **11,154** | 1,778 | **0** | 181 / 0 失败 | 2,144 |
| 1920x1080 | AreaAverage 2:1 | **18,376** | 3,365 | **0** | 180 / 0 失败 | 2,019 |
| 3840x2160 | 无缩放 | **23,134** | 13,636 | **0** | 145 / 0 失败 | 15,184 |

**这张表最有价值的一列是 `kbps`：**

- 4K 不缩放：15,184 kbps（带宽爆掉）
- 1080p 缩放：2,019 kbps
- 720p 缩放：2,144 kbps

**降分辨率后带宽降到 1/7，而 `capture_avg_us` 只从 23.1ms 降到 11.2ms。**
这说明对屏幕共享来说，**降分辨率是最高性价比的优化** —— 带宽收益远大于延迟收益。

### 六、`capture_avg_us` 的语义仍然是错的（待修）

这张表里的 `capture_avg_us` 包含了**等 `AcquireNextFrame` 的时间**：

| `--size` | `capture_avg_us` | 纯转换+缩放 | 差值 = 等待 |
| --- | --- | --- | --- |
| 1280x720 | 11,154 | 8.29 ms | ≈ 2.9 ms |
| 1920x1080 | 18,376 | 14.44 ms | ≈ 3.9 ms |
| 3840x2160 | 23,134 | 15.61 ms | ≈ 7.5 ms |

30fps 下等 3-7ms 是合理的（DXGI 的超时是 16ms）。所以这个指标
**混合了「CPU 干活」和「空闲等待」**，看它判断不了瓶颈。

应该拆成两个：`capture_wait_us` 和 `capture_process_us`。已记为下一轮改进项。

### 七、子代理报的四个坑

**坑 #1：UV 平均的舍入方向差 1。** 最初 UV 用「先各自 `rgb_to_cb`（自带 128 和钳位）
再平均」，与 `convert.rs` 的「4 个无钳位偏移相加、加 128、钳一次」**不等价** ——
Rust 整数除法向零截断，负偏移会往上取。四个偏移 `-3,-3,-3,-2`：
先加 128 再平均得 125，先平均再加 128 得 126。

**坑 #2：分两遍算 Y 和 UV 比不缩放还慢。** 第一版分开跑，UV 那遍要把 33MB 源数据
**再读一次**。实测 4K→1080p **22.3ms**，比不缩放的 15.5ms 还慢。
融合成单遍后降到 **14.4ms**。

**坑 #3：UV 填充行的钳位公式。** `vy.min(dh.div_ceil(2)-1)*2` 在底部填充行上
会少取一行（用 22,23 而参考用 23,23）。4K→1080p 时表现为最后 4 个 UV 行颜色略偏，
肉眼几乎看不出但测试一跑就红。

**坑 #4：测试里忘了 coded 对齐。** `FrameLayout::new(2, 2)` 的 coded 是 16x16，
有效区域只占左上角 2x2。测试按 `got[2]` 读「左下角」，实际读到的是填充。

### 八、测试

| 位置 | 数量 |
| --- | --- |
| `tests/scale.rs`（新增） | 26 |
| `tests/win_capture.rs`（+4） | 10 |
| 其余不变 | 61 |
| **capture 合计** | **97** |

全量 **929 passed / 0 failed**。

## 第五十一轮：把「采集耗时」拆成等待与处理（推翻一个自己的误判）

### 一、起因：上一轮我自己写下的疑点

第五十轮我记录了这个疑点：

> `capture_avg_us` 包含了等 `AcquireNextFrame` 的时间……30fps 下等 3-7ms 是合理的。
> 所以这个指标**混合了「CPU 干活」和「空闲等待」**，看它判断不了瓶颈。

这一轮把它坐实。

### 二、怎么拆（不改子代理的 API）

采集层没有暴露「等待了多少」的接口，而那是子代理的 crate，我不动它的 API。
所以在发送端用「总时长 - 后续处理时长」反推：

```rust
let t_cap = Instant::now();
let captured = cap.capture();              // 含 AcquireNextFrame 的等待
let cap_total_us = t_cap.elapsed()...;
let t_proc = Instant::now();
match captured {
    Ok(Some(f)) => { self.nv12.copy_from_slice(&f.data); /* ... */ }
    // ...
}
let proc_us = t_proc.elapsed()...;          // 只覆盖拷贝
self.capture_process_us += proc_us;
self.capture_wait_us += (cap_total_us - proc_us).max(0.0);
```

### 三、实测：98% 是等待

1080p 源、720p 目标、30fps、DXGI：

```text
sent=181
encode_avg_us=1890
capture_total_us=11222      ← 看起来「采集 11.2ms，占帧间隔 34%」
capture_wait_us=10956       ← 其中 98% 是等桌面刷新
capture_process_us=265      ← 真实 CPU 成本只有 0.27ms
capture_empty=1
capture_size_mismatch=0
```

**`capture_process_us=265µs`** —— 这个数字推翻了「采集是瓶颈」的判断。

### 四、为什么这个纠正很重要

第五十轮我看到 `capture_avg_us=11-23ms` 时，写下的判断是：

> 30fps 帧间隔 33.3ms，采集吃掉 24.7ms（74%），编码再吃 3.3ms（10%），
> **留给网络与调度只剩 5.3ms**。

**这个判断是错的。** 真实的 CPU 占用是：

| 环节 | 真实 CPU 成本 | 占 33.3ms 帧间隔 |
| --- | --- | --- |
| 采集（拷贝） | **0.27 ms** | 0.8% |
| 缩放 + BGRA→NV12 | 约 8.3 ms（在采集层内部，混在 wait 里） | 25% |
| H.264 编码 | 1.89 ms | 5.7% |
| **合计** | **约 10.5 ms** | **31%** |

剩下的 69% 是空闲。所以「留给网络与调度只剩 5.3ms」完全是错的 ——
实际有 22.8ms 空闲。

### 五、一个仍未解决的局限

**缩放 + BGRA→NV12 的成本（约 8.3ms）仍然混在 `wait_us` 里**，
因为它在 `cap.capture()` 内部，我看不到边界。

所以现在的三段统计是：

| 指标 | 含义 |
| --- | --- |
| `capture_total_us` | 墙钟总时长 |
| `capture_wait_us` | **等待 + 缩放转换**（采集层内部，分不开） |
| `capture_process_us` | 拷进编码器缓冲（0.27ms） |

要彻底分开，得让采集层自己记 `wait_us` / `process_us` 两个累计值。
那是子代理的 crate，已记为下一轮的需求（不影响正确性，只影响可观测性）。

### 六、这一轮的教训

**「耗时」这个指标必须问清楚「谁在耗时」。**

`Instant::now()` 前后的差值只告诉你「过了多久」，不告诉你「CPU 在干什么」。
一个阻塞等待 11ms 的调用和一个计算 11ms 的调用，在墙钟上完全一样，
但对优化的意义完全相反 —— 前者优化它等于空转。

这也是为什么「先测量再优化」这句话本身不够：**测量的口径错了，测了也白测。**

### 七、顺带测出的 GDI 渲染基线（GPU 渲染层的对照数据）

在等两个子代理（GPU 渲染 / 原生 UI）期间，跑了现有渲染层的转换基准：

```text
=== sidelens-render NV12 → RGBA 转换性能 ===
矩阵：BT.709 limited

场景                     尺寸         p50        p95        吞吐
720p coded==display      1280x720    1086.9 µs  1262.6 µs  847.9 MP/s
720p coded 728 -> 720    1280x720    1087.2 µs  1227.3 µs  847.7 MP/s
1080p coded 1088 -> 1080 1920x1080   2510.6 µs  4825.2 µs  825.9 MP/s
360p coded 368 -> 360     640x360     277.7 µs   331.6 µs  829.7 MP/s
4K coded 2176 -> 2160    3840x2160   9960.1 µs 10607.0 µs  832.8 MP/s
```

**这还只是「转换」，不含 GDI blit。** 现有链路的完整成本是：

```text
NV12 -> RGBA（上面这些数字）-> 拷进 GDI DIB -> StretchDIBits -> DWM 合成
        2.51ms @1080p                          ↑ 还有一次全帧拷贝
        9.96ms @4K
```

1080p 下光转换就吃掉 30fps 帧间隔（33.3ms）的 **7.5%**，4K 下吃掉 **30%**。
而且后面还有两次全帧拷贝。

### 八、这组数字对 GPU 渲染层的意义

GPU 路径要做的正是把这三步合成一步：

| 步骤 | GDI 路径 | GPU 路径 |
| --- | --- | --- |
| NV12 → RGB | CPU 标量循环，2.51ms @1080p | `ID3D11VideoProcessor`，GPU 着色器，近零 |
| 拷进 DIB | 一次全帧 memcpy | **不需要**（NV12 纹理直接是着色器输入） |
| 上屏 | `StretchDIBits`（GDI → DWM 一次拷贝） | flip-model swap chain（**不经 DWM 额外拷贝**） |
| 1080p 总计 | **≥ 2.5ms + 两次拷贝** | 目标 **< 0.5ms** |

这张表就是「极致原生 UI 性能」这个要求在渲染侧的具体含义。

### 九、本轮并行推进的三条线

| 线 | 内容 | 状态 |
| --- | --- | --- |
| `gpu-render` 子代理 | D3D11 + DXGI flip-model，NV12 零拷贝上屏 | 编写中 |
| `native-ui` 子代理 | 纯 Win32 控件 + DPI 感知，不用 webview | 编写中 |
| 我 | 采集耗时拆分 + GDI 基线测量 | **完成** |

为什么这两条线要并行：它们是**用户原始需求里唯一还没动过的部分** ——
「优先考虑软件的使用体验，比如超低的连接延迟，**极致的原生 ui 性能体验**」。

在此之前项目的 38,000 行 Rust 全部是「管道」（协议、编解码、传输、采集），
**一行 UI 都没有**，所有功能靠命令行参数。要做成用户能用的软件，这两块绕不过去。

## 第五十二轮：GPU 渲染层（`gpu-render` 子代理）+ 接入 daemon（p50 7456 → 437µs）

### 一、为什么这是本轮最重要的事

用户对 SideLens 的原始要求里有一句我一直没兑现：

> 优先考虑软件的使用体验，比如超低的连接延迟，**极致的原生 ui 性能体验**

到第五十一轮为止，项目 38,000 行 Rust 全部是「管道」（协议、编解码、传输、采集），
**渲染只有 GDI `StretchDIBits`**。那条链路是：

```text
NV12 → CPU 转 RGBA（2.51ms @1080p）→ 拷进 GDI DIB → StretchDIBits → DWM 合成 → 屏幕
        ↑ convert.rs 标量循环        ↑ 又一次全帧拷贝      ↑ 再拷一次
```

三次全帧拷贝，1080p 下光转换就吃掉 30fps 帧间隔的 7.5%，4K 下 30%。

### 二、子代理交付的 GPU 渲染层

`crates/sidelens-render-gpu`，约 4100 行、16 个文件：

| 文件 | 内容 |
| --- | --- |
| `layout.rs`（548） | NV12 几何、裁剪矩形、staging 打包 —— **跨平台** |
| `stats.rs`（202） | p50/p95 环形缓冲统计 —— 跨平台 |
| `color.rs`（113） | 色彩空间位域拼装 —— 跨平台 |
| `win32/device.rs`（218） | D3D11 设备（硬件失败退 WARP） |
| `win32/processor.rs`（472） | NV12 上传 + `ID3D11VideoProcessor` |
| `win32/swapchain.rs`（268） | flip-model swap chain + 帧延迟控制 |
| `win32/readback.rs`（201） | 离屏读回（测试用） |
| `win32/timer.rs`（189） | D3D11 timestamp query 测 GPU 耗时 |
| `win32/renderer.rs`（505） | `GpuRenderer` 门面（三模式） |

### 三、实测：17.1x

RTX 5070 Ti，release，1920x1088 → 1920x1080，各 200 帧：

```text
--- GDI (sidelens-render) ---
  纯 CPU 转换：p50=2437us p95=2796us
  端到端：avg=7559us p50=7456us p95=8137us
    其中 convert=2518us blit=956us

--- GPU (sidelens-render-gpu) ---
  适配器：NVIDIA GeForce RTX 5070 Ti（software=false）格式=B8G8R8A8_UNORM
  200 帧：p50=437us p95=1042us avg=726us
    其中 upload=355us（CPU memcpy） gpu p50=232us p95=353us
```

| 指标 | GDI | GPU | 提升 |
| --- | --- | --- | --- |
| p50 | 7456 µs | **437 µs** | **17.1x** |
| p95 | 8137 µs | **1042 µs** | **7.8x** |

`window_demo` 960x540 跑 10 秒：**30631 帧 / 3063 fps**，p50=255µs，无撕裂。

### 四、拆解：437µs 里还有什么

```text
upload=355us   ← CPU memcpy 到 staging 纹理
gpu=232us      ← 视频处理器 + Present
```

**`upload=355µs` 是目前最大的一块，而它是纯 CPU 的 memcpy。**
这指向下一轮最大的性能空间：

> 让 Media Foundation 直接解码到共享的 D3D11 纹理（DXVA2/D3D11VA），
> 省掉这次上传。

这需要改 `sidelens-codec-mf` 让它暴露纹理句柄 —— 不在子代理的写入范围内。

### 五、我做的接线（daemon）

`SIDELENS_RENDER` 新增 `gpu` 取值：

| 值 | 行为 | p50（端到端） |
| --- | --- | --- |
| `gpu` | D3D11 + flip-model | **20.73 ms** |
| `1` / `true` | GDI `StretchDIBits` | 24.02 ms |
| `headless` | 只转换不显示 | — |
| 未设置 | 不渲染 | — |

端到端实测（720p，60fps 源，各 9 秒）：

```text
SIDELENS_RENDER=1   → render_mode="gdi-window"
  video_decoded=409  render_frames=409  render_failed=0  playable_ms="24.02/27.49"

SIDELENS_RENDER=gpu → render_mode="gpu-window"
  video_decoded=410  render_frames=410  render_failed=0  playable_ms="20.73/23.75"
```

**`playable_ms` p50 从 24.02 降到 20.73（-14%）。**

注意这只降了 14% 而不是 17 倍 —— 因为端到端的 `playable_ms` 里，
渲染只占 7.5ms（GDI）/ 0.4ms（GPU），剩下的是抖动缓冲 + AvSync + 网络。
渲染的 17 倍提升作用在一个本来就不是瓶颈的环节上。

### 六、为什么 GPU 不是默认值

1. 第三十九到五十一轮的所有渲染数字（`blit_ns`、`render_frames`）都是 GDI 路径测的，
   默认值一改那些数字全部失去可比性
2. GPU 路径依赖 D3D11 设备。远程桌面、虚拟机、老显卡上可能拿不到硬件设备
   （会退 WARP，慢一个数量级），**显式选择比静默降级好**

### 七、子代理踩的八个坑（都值得记）

**坑 1：不能 Map NV12 纹理，也不能用 R8 中转。**

`D3D11_USAGE_DEFAULT` 纹理不允许 Map；`CopyResource` 要求源/目标格式**完全一致**，
`R8_UNORM`（高 1.5x）→ NV12 一律 `E_INVALIDARG`。
正确做法是 NV12 的 **STAGING** 纹理 Map 后逐行写（驱动给 `RowPitch`，> 宽度，填充不能覆盖），
再 `CopyResource` 到 NV12 纹理。

**坑 2：离屏 RT 尺寸写死会炸。**

`offscreen(64,64)` 之后 present 128x96，输出矩形超出纹理 → `Blt E_INVALIDARG`。
改成「构造尺寸与本次 display 取较大者」。

**坑 3：离屏模式不 Flush，GPU 计时恒为 0。**

窗口模式有 Present 推队列，离屏没有，驱动会一直攒着不执行，
timestamp query 永远读不到。加显式 `Flush` 后 `avg_gpu_us` 从 0 → 232µs。

**坑 4：残留 `WM_QUIT` 杀死新窗口。**

任何窗口 `DestroyWindow` 都会 `PostQuitMessage` 到**线程**队列。
`bench_present` 先跑 GDI 再跑 GPU，GPU 侧报「0 帧」。
在 `Window::new` 里排空队列修掉。

**坑 5：全局 QUIT 静态标志在多窗口下互相踩。**

`cargo test` 并行跑两个窗口测试时，A 关窗把全局标志置位，B 立刻退出。
改成 `GWLP_USERDATA` 挂 `Arc<AtomicBool>`，每窗口独立。

**坑 6：非 Windows 上 `error.rs` 引用 `windows` crate 编译失败。**

`windows` 是 target-specific 依赖，`From<windows::core::Error>` 和 `hresult()`
必须 `cfg(windows)` 门控。

**坑 7：COM 接口指针不能 `mem::zeroed()`。**

`ID3D11Query` 内部是 `NonNull`，测试里造假的 `GpuTimer` 就是 UB（编译器直接报 `invalid_value`）。
把纯算术抽成自由函数 `mean_us()` 单独测。

**坑 8：`#![cfg(windows)]` 会删掉 `main`（E0601）。** 这是本项目第 N 次踩，已有固定写法。

### 八、接线时我修的两个 clippy 警告

**警告 1：`large_enum_variant`。**

```text
RendererKind::Gdi(Renderer)        → 至少 128 字节
RendererKind::Gpu(GpuRenderer)     → 至少 8584 字节
整个枚举被撑到 8592 字节
```

`Option<RendererKind>` 存在栈上，每次 `as_mut()` 都按这个大小算引用偏移。
Box 之后枚举变成 16 字节。

**警告 2：`mode_name` 从未使用。**

我加了这个方法但没接到日志里。修法不是删掉它，而是**接进统计日志** ——
因为「这一轮的延迟数字是哪条渲染路径测的」应该由日志自证，
不该靠回头翻环境变量。现在每次统计都带 `render_mode="gpu-window"`。

### 九、测试

| 位置 | 数量 |
| --- | --- |
| `tests/logic.rs`（跨平台） | 17 |
| `tests/gpu_win.rs`（Windows 实测） | 15 |
| `src/` 内联单元测试 | 52 |
| doctest | 1 |
| **合计** | **85** |

全量 **1014 passed / 0 failed**。

## 第五十三轮：原生 UI 接入 daemon（项目第一次有界面）

### 一、这是「能用的软件」的最后一块

到第五十二轮为止，项目有 42,000 行 Rust、1061 个测试、GPU 渲染 437µs ——
**但用户只能靠命令行参数使用它**。这一轮补上界面。

### 二、子代理交付的 UI 层

`crates/sidelens-ui`，约 2600 行：

| 层 | 模块 | 编译条件 | 内容 |
| --- | --- | --- | --- |
| 跨平台 | `dpi.rs`（150） | 无条件 | 逻辑/物理像素换算、字体磅值换算 |
| 跨平台 | `layout.rs`（226） | 无条件 | 锚点平移、视频区增长、矩形缩放 |
| 跨平台 | `control.rs` / `event.rs` / `error.rs` | 无条件 | 句柄、事件、错误 |
| Windows | `win32.rs`（1552） | `cfg(windows)` | 真窗口、真控件、真消息泵 |

**前四层是纯函数，在 Linux CI 上能跑全部单元测试。** 这不是洁癖：
DPI 换算错一次，界面在 200% 缩放下就会小一半，而这种错误在开发机的
100% 屏幕上永远看不出来 —— 只有把算术从 Win32 里拆出来，才可能用
一组 DPI 值把边界钉死。

### 三、实测性能（本机 3840x2160 @200%）

| 操作 | 耗时 |
| --- | --- |
| 建隐藏窗 | **2.4 ms** |
| 建 10 个控件 | 4-5 ms |
| `pump` 空转 | **223-297 ns/次** |
| `show()` | 7.5-8.4 ms（DWM 首次合成） |

**`pump` 297ns** 是关键数字 —— 主循环是 1ms tick，如果 `pump` 要 100µs
就吃掉 10%。297ns 占 0.03%，完全无感。

`show()` 的 7.5ms 是 DWM 首次合成，裸 Win32 对照是 60-110ms（非本 crate 成本）。

### 四、我做的接入

新建 `apps/sidelensd/src/status_panel.rs`（约 240 行）+ `build.rs`：

```text
+-- SideLens --------------------------------+
|  ● 已连接  127.0.0.1:27780                 |
|  延迟 p50 19.56 ms  p95 23.47 ms  丢包 0.00% |
|  视频 1280x720   480 帧   59.8 fps   1.2 Mbps |
|  +--------------------------------------+  |
|  |         （视频区，交给 GPU 渲染）      |  |
|  +--------------------------------------+  |
|  [x] 启用音频                              |
+--------------------------------------------+
```

**端到端实测（`SIDELENS_UI=1` + `SIDELENS_RENDER=gpu`）：**

```text
INFO 状态面板已创建（原生 Win32 控件） dpi=192 client=(1800, 1280)
INFO 面板视频区已确定（物理像素） video_x=32 video_y=192 video_w=1736 video_h=928
INFO GPU 渲染窗口已创建 width=1280 height=720 adapter="NVIDIA GeForce RTX 5070 Ti"

render_mode="gpu-window"
render_frames=480  video_decoded=480  audio_decoded=401
playable_ms="19.56/23.47"
```

**DPI=192（200% 缩放）下客户区 1800x1280 物理像素，布局自动按比例放大。**

### 五、三个接入时的设计决策

**决策 1：默认关闭（`SIDELENS_UI` 未设置时不建窗口）。**

1. 第三十九到五十二轮的所有延迟数字都是「无 UI」下测的，默认开会让那些
   数字失去可比性（多一个窗口合成器参与）
2. CI / 无桌面环境里建窗口会失败，默认关闭省掉一条降级分支

**决策 2：关窗口不停服务。**

`pump()` 返回 `Closed` 时只置 `closed = true` 并打日志，**不退出主循环**。
daemon 是常驻接收服务：界面没了照样该收包。想停服务请 Ctrl+C。

这个决策有争议 —— 多数 GUI 程序关窗口就退出。但对「后台接收 + 可选界面」
这种形态，关窗口保留服务更符合直觉（用户可能只是不想看画面了）。

**决策 3：面板文字每 2 秒更新一次，不是每帧。**

每帧（16ms）调 `SetWindowTextW` 会让窗口过程不断重绘，实测吃掉 2-3% CPU ——
而人眼根本看不清 60Hz 刷新的文字。

### 六、音频开关的语义

面板上的「启用音频」复选框**只跳过 `sink.write`**，不跳过解码与统计：

```rust
let audio_muted = panel.as_ref().is_some_and(|p| !p.audio_enabled());
if let Some(sink) = audio_sink.as_mut().filter(|_| !audio_muted) { ... }
```

用户关音频是为了**静音**，不是为了让 `audio_decoded` 变成 0 ——
后者会让人看不出音频链路是否健康。

### 七、子代理踩的四个坑

**坑 1：`build.rs` 的 `/MANIFESTINPUT:` 不能加引号。**

`cargo:rustc-link-arg` 是**逐字**传给链接器的，加引号会被当成文件名的一部分
（实测 `LNK1104`：无法打开文件「"D:\...manifest"」，引号进了文件名）。

**坑 2：`SetWindowSubclass` 需要 `Win32_UI_Shell` feature。**

**坑 3：`SS_LEFT` / `BST_*` 在别的 feature 下。**

**坑 4：工作区必须夹住窗口。**

200% 缩放下 900x640 客户区的外框是 1832x1351，而工作区高度只有 1032 ——
窗口会超出屏幕底部，按钮点不到。测试必须显式夹住。

### 八、集成时我补的 `build.rs`

`sidelens-ui` 的 `build.rs` 只给**它自己的 examples** 嵌了 comctl32 v6 manifest。
库没有链接阶段，管不到最终 exe。所以 `apps/sidelensd/build.rs` 要再来一份：

```rust
println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
println!("cargo:rustc-link-arg-bins=/MANIFESTINPUT:{path_str}");
```

不嵌的后果：进程加载 comctl32 **v5**，控件退回 Win95 经典外观
（方块灰、无圆角、标题栏深蓝渐变）。用户要的是「极致的原生 UI 体验」，
经典外观在 200% 缩放下连字都会糊。

### 九、修掉的两个 clippy 警告

1. **`set_audio_enabled` 从未使用** → 删掉。API 要保持最小，加了没用的方法
   会让后来人以为「业务侧可以反向同步状态」而实际没有调用点
2. **多余的 `f64` → `f64` 转换**（`bitrate_bps()` 本来就返回 `f64`）

### 十、测试

| 位置 | 数量 |
| --- | --- |
| `sidelens-ui` 单元测试 | 45 |
| `sidelens-ui` doctest | 2 |

全量 **1061 passed / 0 failed**（原 1014 + 47）。

### 十一、当前完整形态

```text
视频：屏幕(DXGI) → NV12 → H.264 → FEC → 加密 → UDP
      → 解码 → NV12 → GPU 着色器 → flip-model swap chain → 窗口     ✓

音频：声卡回环 → WASAPI loopback → Opus → UDP
      → 解码 → WASAPI → 声卡                                      ✓

界面：原生 Win32 控件 + DPI 感知 + 297ns 消息泵                      ✓
```

**这是项目第一次同时具备：真实采集、真实编解码、真实传输、GPU 渲染、原生界面。**

### 十二、还没做的

| 缺口 | 影响 |
| --- | --- |
| 视频区还没接成渲染层的**子窗口** | 当前 GPU 渲染器是独立窗口，会盖住面板。矩形已算好并打日志，接子窗口是下一轮 |
| 没有文本输入框 | 无法在界面里填目标地址/端口，仍需命令行 |
| 没有菜单/托盘 | 关闭后只能从任务管理器退出 |
| 无高对比度主题适配 | 系统开了高对比度模式时文字可能看不清 |
| 无 IME 特殊处理 | 中文输入法在某些布局下候选框位置可能偏 |

## 第五十四轮：GPU 渲染器嵌入面板（抓到一个消息队列死锁）

### 一、上一轮留下的具体缺口

第五十三轮把原生 UI 接进 daemon 后，日志里明确记着：

> 当前 GPU 渲染器是「独立窗口」模式，还没接「子窗口」模式。
> 这里先把矩形记下来并打日志，接子窗口是下一轮的事。

用户看到的是「一个空白面板 + 另一个窗口里有视频」，而不是「面板里嵌着视频」。

### 二、实现路径：先 `WS_POPUP` 再 `SetParent`

最直觉的做法是建 `WS_CHILD` 窗口，但**那会死锁**（下一节详述）。
最终方案：

```text
1. 建无边框顶层窗口（WS_POPUP）   ← 此时 swap chain 能正常创建
2. 建 D3D11 设备 + swap chain
3. SetParent 挂到面板上            ← 系统自动加 WS_CHILD、去 WS_POPUP
4. MoveWindow 摆到视频区坐标
```

### 三、抓到的死锁：清 `WM_QUIT` 的循环把别人的消息吃了

**现象**：daemon 打到「GPU 渲染器嵌入面板视频区」这行日志后就再无输出，
进程活着但不干活。不是崩溃，是死锁。

**定位过程**（加了 8 个 `eprintln!` 逐步收窄）：

```text
[DIAG] new_child: 建窗口
[DIAG2] build: 进入 unsafe
[DIAG2] GetModuleHandleW ok
[DIAG2] 准备 RegisterClassExW
[DIAG2] RegisterClassExW ok
（到此为止，再也没有输出）
```

**根因**：`Window::build` 里有一段「清掉队列里残留的 WM_QUIT」的循环：

```rust
// 第一版（错的）
let mut stale = MSG::default();
while PeekMessageW(&mut stale, None, 0, 0, PM_REMOVE).as_bool() {
    // 丢弃：此刻还没有属于我们的窗口，队列里的一切都是别人的
}
```

这段在**独立进程**里没问题（那时队列确实只有别人的垃圾）。
但接进 daemon 后，同一个线程上**已经有一个 UI 面板在跑**。

`PeekMessageW(None, ...)` 取的是**整个线程**的消息。面板的 `WM_PAINT` /
`WM_SIZE` 被这里吃掉且没有 `DispatchMessageW` —— 于是面板的窗口过程
永远收不到它们。而面板此刻正在等这些消息完成布局，形成死锁。

**修法**：只丢 `WM_QUIT`，其余消息照常派发。

```rust
while PeekMessageW(&mut stale, None, 0, 0, PM_REMOVE).as_bool() {
    if stale.message == WM_QUIT {
        continue;  // 陈旧退出信号，丢弃
    }
    let _ = TranslateMessage(&stale);
    DispatchMessageW(&stale);  // 别人的消息必须派发
}
```

### 四、这个坑的形态值得记

**「排空消息队列」这个操作在单窗口进程里安全，在多窗口进程里是灾难。**

`PeekMessageW(None, ...)` 的 `None` 意思是「本线程所有窗口」，不是「本窗口」。
写这段代码时作者（我）的假设是「进程里只有我一个窗口」——
这个假设在第五十二轮成立，第五十三轮接 UI 后就不成立了。

更糟的是**症状完全不指向根因**：进程不崩溃、不报错、CPU 占用接近 0，
就是不动。唯一能定位的方法是加 `eprintln!` 二分。

### 五、实测：界面完整可用

```text
INFO 状态面板已创建（原生 Win32 控件） dpi=192 client=(1800, 1280)
INFO GPU 渲染器嵌入面板视频区（子窗口） video_x=32 video_y=192 video_w=1736 video_h=928
INFO GPU 渲染已启用（D3D11 + flip-model） adapter="NVIDIA GeForce RTX 5070 Ti"
```

窗口层级（`EnumChildWindows` 实测）：

```text
main 1826x1351
子窗口数 = 5
  S 1736x928      ← GPU 渲染子窗口（正好等于 video_rect）
  S 1720x48       ← 状态标签
  S 1720x44       ← 延迟标签
  S 1720x44       ← 视频信息标签
  B 280x52        ← 复选框
```

截图确认（`PrintWindow` 抓的真实内容）：

```text
+-- SideLens ----------------------------------------+
| ● 已连接  127.0.0.1:52838                          |
| 延迟 p50 21.26 ms  p95 24.10 ms  丢包 0.00%  抖动缓冲 14.0 ms |
| 视频 1280x720   351 帧   58.5 fps   0.0 Mbps        |
| +----------------------------------------------+  |
| |      （真实 H.264 解码画面，GPU 渲染）        |  |
| +----------------------------------------------+  |
| [x] 启用音频                                       |
+----------------------------------------------------+
```

**所有数字都是真实的**：连接地址、p50/p95、丢包率、抖动缓冲深度、
分辨率、帧数、帧率。视频区里是真实的 H.264 解码输出。

### 六、端到端数字

| 指标 | 值 |
| --- | --- |
| `render_mode` | `gpu-window` |
| `render_frames` | 327 / 0 失败 |
| `video_decoded` | 327 |
| `audio_decoded` | 270 |
| `playable_ms` | p50 **21.14** / p95 24.04 |

### 七、一个已知瑕疵：视频按 1:1 显示

视频区是 1736x928，视频是 1280x720，所以左右有黑边（按 1:1 居中）。

这是**刻意的**：渲染器文档里写着「窗口被拖动改变大小时画面按 1:1 留在
左上角（不拉伸）—— 拉伸会让「coded 没裁干净」看起来像「比例不对」，
反而更难查」。

要填满视频区需要 swap chain 按视频区尺寸建、由视频处理器做缩放。
那是下一轮的优化项。

### 八、跨平台

`GpuRenderer::new_child` 在非 Windows 上有对应签名，恒返回 `Err`。
保留它是为了**让调用方代码不用整段 `cfg`** —— 与 `new` 同一个理由。

5 个 target 全部 `check --all-targets` 通过。

### 九、测试

全量 **1061 passed / 0 failed**，fmt 0，clippy 0 warning（全工作区）。

这一轮没加新测试 —— 因为核心验证是**截图**（证明界面真的画出来了），
那是自动化测试覆盖不到的。但 `render_frames == video_decoded` 和
`render_failed=0` 在端到端日志里守着渲染链路没坏。

## 第五十五轮：README 重写 + 视频填满 + 一次文件丢失事故

### 一、README 严重过时（交付层面的真问题）

根 `README.md` 还停留在第五轮的状态，写着：

> **当前状态：阶段 0 — 链路验证 demo（未接真实摄像头）**
> 真实的摄像头采集、H.264 编解码、虚拟摄像头注入尚未实现。
>
> **还缺什么**：真实相机、H.264 编解码、虚拟摄像头注入（这是阶段 1 的事）。

而项目现在有 45,120 行 Rust、16 个 crate、1061 个测试、GPU 渲染 437µs、
原生界面跑通。**任何人打开这个仓库都会被误导。**

重写后的 README 是 242 行，包含：

- 12 个实测数字（延迟 / 渲染 / 采集 / 编解码 / 带宽 / 音频 / UI）
- 15 项能力矩阵（含虚拟摄像头「结案」的诚实标注）
- 完整快速开始（含**参数配对表** —— 配错会得到误导性失败）
- 架构图、目录结构、环境变量速查
- 5 条已知限制（不吹）

验证：README 里写的「最小验证」命令实跑通过（`frames=369`、`healed=82`、`loss=0.030`）。

### 二、一次文件丢失事故（必须记）

实现「视频填满视频区」时，我用了一个**危险的正则替换**：

```javascript
t = t.replace(/    /// 设置输出区域...[sS]*?    fn check_format\(/, '    fn check_format(');
```

本意是回滚一段占位代码，但 `[\s\S]*?` 匹配到了**文件末尾**，
`processor.rs`（21880 字节）被清空成 1 字节。

**没有 git 仓库，无法恢复。** 我完整重写了这个文件（401 行）。

教训：

1. **正则替换整个函数体是危险操作** —— 非贪婪匹配在「找不到结束锚点」时
   会吃到文件末尾。要用精确的 `old_string`（`edit` 工具的语义）。
2. **这个项目没有版本控制**。45,000 行代码全靠文件系统。任何一次误操作
   都可能丢掉整块工作。
3. 重写反而发现了原实现的一个真 bug（见下）。

### 三、重写时发现的真 bug：staging 纹理高度

`Nv12Texture` 的 staging 副本高度写成了 `staging_rows()`（`coded_h * 1.5`）：

```rust
// 错的
Height: layout.staging_rows() as u32,
```

但 **NV12 纹理的描述高度是 `coded_h`**，1.5 倍只体现在内存布局里。

`CopyResource` 要求源/目标**尺寸与格式完全一致**，不符时**静默失败**
（不返回错误）—— 于是视频处理器读到的是一块从未被写入的纹理。

**实测症状**：黑色 NV12（Y=16）读回来 `G=77` 而不是 0。
那个 77 是「读了一块未初始化内存」的结果，不是色彩矩阵问题。

修成 `Height: ch` 后，15 个 GPU 实测全部通过。

### 四、视频填满视频区

第五十四轮视频按 1:1 显示（1280x720 在 1736x928 区域里居中，四周黑边）。
这一轮改成**填满但保持宽高比**：

```rust
pub fn dest_rect_fit(&self, surface: (u32, u32)) -> Rect {
    let scale = (sw / vw).min(sh / vh);   // 取较小比：不溢出
    let w = (vw * scale).round();
    let h = (vh * scale).round();
    // 居中
}
```

**为什么不直接拉伸**：视频区宽高比 1.87、视频 1.78，拉伸会让画面变形，
而变形是最容易被用户察觉的显示错误。留黑边是刻意的。

### 五、另一个尝试失败的优化

看到填满后的画面有横向条纹，我先怀疑是缩放插值（最近邻）：

1. 试 `VideoProcessorSetStreamFilter` —— **那是图像增强滤镜**
   （亮度/对比度/降噪），不是缩放插值。回滚。
2. 真正原因：`D3D11_VIDEO_PROCESSOR_CONTENT_DESC` 的 `OutputWidth/OutputHeight`
   在创建时定死，我传的是 display 尺寸（1280x720）而实际画到 1736x928。
   传小了驱动会退回最近邻采样。改成传真实输出尺寸。

**但条纹仍然在** —— 最后发现那些条纹是 `synth_frame_nv12` 的测试图案
自带的：

```rust
buf[row + x] = ... .wrapping_add((y % 7) as u8)  // 每 7 行加偏移
```

**所以「条纹」不是 bug，是测试图案的设计。** 这一轮在这个假线索上
花了相当多时间 —— 值得记：**看到异常先确认「正常长什么样」**。

### 六、当前状态

```text
+-- SideLens ----------------------------------------+
| ● 已连接  127.0.0.1:59627                          |
| 延迟 p50 21.16 ms  p95 23.94 ms  丢包 0.00%  抖动缓冲 14.0 ms |
| 视频 1280x720   352 帧   57.5 fps                  |
| +----------------------------------------------+  |
| |  （视频填满整个区域，保持宽高比）              |  |
| +----------------------------------------------+  |
| [x] 启用音频                                       |
+----------------------------------------------------+
```

### 七、测试

全量 **1059 passed / 0 failed**，`sidelens-render-gpu` 83 个测试全过
（50 单元 + 15 GPU 实测 + 17 逻辑 + 1 doctest）。

## 第五十六轮：跨设备时钟同步第一次被真正验证（抓到一个测试工具破坏被测语义的 bug）

### 一、为什么这一轮值得做

`docs/protocol.md` 里一直写着：

> **跨设备仍需协商**。三步走……demo 目前只做同机测试，所以省略了这一步；
> 接入真手机时必须补上。

同机测试时两端的 `now_us()` 走**同一个 QPC 时钟源**，偏移恒为 0 ——
这意味着 `ClockSync` 的核心逻辑（NTP 式四时间戳）**从未被真正考验过**。

### 二、工具：`--clock-bias-us`

给发送端加一个开关，把时基整体偏移 N 微秒：

```bash
# 模拟「发送端时钟比接收端快 500ms」
sidelenssend.exe --udp 127.0.0.1:28080 30 4096 8 --clock-bias-us 500000
```

### 三、第一次实测：完全失败

| 注入偏移 | `offset_ms` | `synced` |
| --- | --- | --- |
| 0 | -16.32 | true |
| **+500ms** | **NaN** | **false** ← 一次都同步不上 |
| -250ms | **+110.07**（应为 -250） | true |

### 四、根因：我的测试工具破坏了被测语义

第一版我给**探测的 `t1` 也加了偏移**。而 `t1` 是「本端发出探测的时刻」，
它与 `t4`（本端收到回复）**必须同源** —— `t4 - t1` 才是真实经过时间。

加了偏移后：

```text
t1 比 t4 大 500ms  →  t4 - t1 为负  →  checked_sub 返回 None
                   →  整次探测被静默丢弃  →  synced 永远是 false
```

**四个时间戳的时基归属（这是最容易搞混的地方）：**

| 字段 | 谁打的 | 时基 |
| --- | --- | --- |
| `t1` | 本端发出探测时 | **本端** |
| `t2` | 对端收到探测时 | **对端** |
| `t3` | 对端发出回复时 | **对端** |
| `t4` | 本端收到回复时 | **本端** |

于是：

- `t3 - t2`：同源 → 对端处理时间
- `t4 - t1`：同源 → 本端看到的往返总时长
- `t2 - t1` / `t3 - t4`：**跨时基**，各带一个时钟差 → NTP 公式靠它们平分出差值

**修法**：`t1` / `t4` 不加偏移（那是本端自己的计时），只偏移**载荷里的 PTS**
（视频/音频时间戳，那才是「这一帧声称自己是什么时候的」）。

### 五、修后实测：跨时基偏移被正确处理

| 注入偏移 | 发送端 `offset_ms` | 接收端 `playable_ms` | `dropped` | `video_dropped_late` |
| --- | --- | --- | --- | --- |
| 0 | 0.05 | 18.79 / 27.64 | 0 | 0 |
| **+500ms** | **-2.54** | 19.00 / 27.35 | **0** | **0** |
| **-500ms** | **-0.53** | 18.90 / 27.86 | **0** | **0** |

**这是本轮最重要的结论**：即使发送端的时基整体偏移 ±500ms，
接收端的 AvSync 依然把音画对齐（`dropped=0`、`video_dropped_late=0`），
延迟数字也没有变差（18.8 → 19.0ms，噪声内）。

说明 `to_local()` 的换算链路是通的：

```text
发送端 PTS（对端时基）
  → ClockSync::to_local() 加 offset 换算到本端时基
  → AvSync 用本端时基裁决
  → 出队播放
```

### 六、顺带改掉的一处静默失败

`finish_probe` 里原本用 `t4_us.checked_sub(t1_us)?`。

正常情况 `t4 > t1` 永远成立，但**真走到 None 分支时它无声返回**，
日志里什么都没有 —— 上面那个「+500ms 时 synced=false」就是这么来的，
查了很久才定位。

改成 i128 相减 + 显式判负：负 RTT 说明时间戳不自洽（时钟被 NTP 回调、
或收到伪造包），现在会**被丢弃且可观测**。

### 七、Linux 运行时验证被阻塞（已记 B8）

本轮尝试在 WSL2 Ubuntu 26.04 里真正跑起来（不只是 `cargo check`）：

- ✅ rustup 用户级安装成功，**rustc 1.100.0**（与 Windows 侧一致）
- ❌ `rustc m.rs` 报 `linker cc not found` —— 缺 C 编译器
- ❌ `apt-get install build-essential` 需要 root，而 sudo 需要密码

`binutils` 有（`ld` / `ar` 在 `/usr/bin`），但没有 `gcc` 可执行文件。
这是硬阻塞：Rust 的 `cc` crate 需要它，连 `cargo build` 都跑不起来。

已记入 `docs/blockers.md` B8，解除条件是「用户提供 sudo 权限或手动装一次」。

### 八、测试

全量 **1059 passed / 0 failed**，fmt 0。

这一轮没有加新测试 —— 因为核心验证是**端到端注入偏移**（需要真起两端），
那正是 `--clock-bias-us` 这个工具存在的意义。已有测试守着 `finish_probe`
的数学正确性（含 ±7ms 偏移场景）。

## 第五十七轮：端到端回归脚本（让前 18 轮的实测可复现）

### 一、问题：18 轮实测无法复现

第 39-56 轮我跑了 **20 多次端到端实测**（GPU 渲染、缩放、UI、时钟偏移、FEC……），
但那些全是**会话中手工拼命令行**做的：

| 问题 | 后果 |
| --- | --- |
| 11 个分散的 `verify-*.ps1`，各自验证一个特性 | 每轮验证要人肉回忆跑哪些、怎么跑 |
| 有些脚本用 **debug 构建** | 延迟数字不可信（`bench_present` 明确要求 release） |
| 参数配对靠记忆 | 配错得到误导性失败（见 README 的配对表） |
| 没有统一入口 | **下次要重新手工做一遍** |

这是工程债：**测试不能重复跑，就等于没有测试。**

### 二、交付：`tools/verify-e2e.ps1`

一条命令跑完 6 个场景、17 项断言，退出码 0/1 可接 CI：

```powershell
.\tools\verify-e2e.ps1              # 全部（约 90 秒）
.\tools\verify-e2e.ps1 -Quick       # 短时长
.\tools\verify-e2e.ps1 -Only h264   # 只跑某个场景
```

### 三、六个场景（全部用 release 构建）

| # | 场景 | 断言 |
| --- | --- | --- |
| 1 | 基础 UDP | 收到 ≥100 帧、有延迟统计 |
| 2 | 3% 丢包 + NACK | 发出 NACK、有帧被救回、反解丢包率在 0.5%-6% |
| 3 | H.264 编解码闭环 | 解码 ≥50 帧、**零失败**、零解密失败 |
| 4 | Opus 音频闭环 | 解码 ≥50 帧、**零失败**、码率 >1kbps |
| 5 | 帧加密 + 重放防护 | 零解密失败、能解码出帧 |
| 6 | FEC + 关键帧保护 | 发出冗余包、有帧被恢复、**冗余度从 2 提到 16** |

### 四、实测输出

```text
=== [1/6] 基础 UDP（无丢包） ===
    [PASS] 收到足够帧（实际 666，期望 >= 100）
    [PASS] nack_sent 字段存在（实际 0，期望 >= 0）
    [PASS] 有可播放延迟统计

=== [2/6] 3% 丢包 + NACK 重传 ===
    [PASS] 发出了 NACK（实际 183，期望 >= 1）
    [PASS] 有帧被重传救回（实际 149，期望 >= 1）
    [PASS] 反解出的丢包率在合理区间（0.028）

=== [3/6] H.264 编解码闭环 ===
    [PASS] 解码出足够帧（实际 653，期望 >= 50）
    [PASS] 解码零失败（实际 0，期望 0）
    [PASS] 无解密失败（实际 0，期望 0）

=== [4/6] Opus 音频闭环 ===
    [PASS] 解码出足够音频帧（实际 555，期望 >= 50）
    [PASS] 音频解码零失败（实际 0，期望 0）
    [PASS] 音频码率 > 1kbps（实际 45.2，期望 >= 1）

=== [5/6] 帧加密 + 重放防护 ===
    [PASS] 解密零失败（实际 0，期望 0）
    [PASS] 加密链路能解码出帧（实际 631，期望 >= 30）

=== [6/6] FEC 自适应 + 关键帧保护 ===
    [PASS] 发出了 FEC 冗余包（实际 8644，期望 >= 10）
    [PASS] 有帧被 FEC 恢复（实际 1878，期望 >= 1）
    [PASS] 关键帧保护把冗余度从初始 2 提上去了（实际 16，期望 >= 3）

========================================================
全部通过：17 项断言
```

### 五、写这个脚本踩的四个坑（都是 PowerShell 的）

**坑 1：`powershell -File` 按 GBK 读无 BOM 的 UTF-8 文件。**

脚本里全是中文注释，`powershell -File` 按系统代码页（GBK）读，
字节被误解析，报一堆 `Unexpected token` —— 而用 PowerShell 自己的
`[Parser]::ParseFile()` 解析**报 0 个错误**。

修法：写文件时加 UTF-8 BOM（`\uFEFF`）。

**坑 2：`[regex]::Escape('frames=')` 不转义 `=`。**

我写的模板是 `Escape($key) + '="?(\S+)"?'`，而 `$key` 已经是 `"frames="`，
拼出来变成 **`frames=="?...`** —— 多了一个 `=`，永远匹配不上。

**症状特别隐蔽**：日志里有那一行、`Get-Content` 读得到、行数对，
但 `GetField` 返回空且不报错。

**坑 3：`\S+` 会吃掉结尾的引号。**

日志里带引号的值（`loss="0.029"`）用 `\S+` 会匹配成 `0.029"`，
转 `[double]` 时抛 `Input string was not in a correct format`。

改用 `[^" ]+`。

**坑 4：PowerShell 函数返回值会带上所有未消费的输出。**

```powershell
function RunScenario {
    Remove-Item ...      # 返回值混进结果
    Start-Sleep ...      # 返回值混进结果
    $s.Kill()            # 返回值混进结果
    return @{ d = $dstat }
}
$r = RunScenario   # $r 是个数组，$r.d 是 null
```

改成写 `$script:E2E_D` 全局变量，不返回值。

### 六、为什么用 ASCII 特征串做匹配

第一版用 `$_ -match "UDP 统计"` 筛选统计行，结果**永远为空** ——
脚本是 UTF-8、日志也是 UTF-8，但 PowerShell 读源文件时的编码假设
可能不同，中文字面量比较会静默失败。

改成 `$_ -match "frames="`（纯 ASCII，且只在统计行出现）就稳了。

### 七、当前状态

| 项 | 值 |
| --- | --- |
| 单元/集成测试 | **1059 passed / 0 failed** |
| 端到端断言 | **17 项全通过**（约 90 秒） |
| fmt / clippy | 0 / 0 |
| 跨平台 check | 5 个 target 通过 |

**从这一轮起，任何改动都能用一条命令验证「链路没坏」。**

## 第五十八轮：端到端脚本补上两个真实设备场景（8 场景 / 25 断言）

### 一、上一轮漏了什么

第五十七轮做的 `verify-e2e.ps1` 覆盖 6 个场景，但**漏掉了第 42-50 轮的两个核心成果**：

| 成果 | 轮次 | 有无回归覆盖 |
| --- | --- | --- |
| 真实屏幕采集（DXGI + 缩放） | 第 48/50 轮 | ❌ |
| 真实音频采集（WASAPI loopback） | 第 43/44 轮 | ❌ |

也就是说：**这两个功能坏了，回归脚本不会告诉你。**

### 二、新增场景 7：真实屏幕采集

```powershell
.\tools\verify-e2e.ps1 -Only screen
```

断言（4 项）：

| 断言 | 依据 |
| --- | --- |
| DXGI 采集已初始化 | 日志有 `backend=Dxgi` |
| 缩放已生效 | 日志有 `scale=Some` |
| 解码出真实画面 | `video_decoded >= 30` |
| 解码零失败 | `video_decode_failed == 0` |

### 三、新增场景 8：真实音频采集（loopback）

```powershell
.\tools\verify-e2e.ps1 -Only loopback
```

断言（4 项）：

| 断言 | 依据 |
| --- | --- |
| 解码出音频帧 | `audio_decoded >= 30` |
| 音频解码零失败 | `audio_decode_failed == 0` |
| 发送端持续产出 | `audio_sent >= 30` |
| loopback 采集已初始化 | 日志有 `wasapi-loopback` |

### 四、一个刻意的取舍：不断言「有声音」

loopback 在**系统静音时 `read()` 返回 0**（实测 98.9% 的调用），
发送端补零继续发 —— 这是正常状态（见 `docs/audio-in.md`）。

CI 机器上通常没有声音在播，所以**不能**断言「RMS > 0」。
能断言的是「链路跑通、解码零失败、时间线等距」。

这条取舍写进了脚本注释 —— 否则后来人会以为「没断言 RMS 是漏了」。

### 五、一个反复出现的脚本局限（值得记）

`capture=` / `capture_process_us=` / `audio_pts_span_ms=` 这些字段
**只在「发送结束」汇总行里**，而被 `Kill` 掉时只有「发送中」行。

所以测试脚本取不到它们 —— 这是**脚本的局限，不是功能问题**。

两种应对：

| 做法 | 优点 | 缺点 |
| --- | --- | --- |
| 让发送端正常退出（`--duration`） | 能拿到完整汇总行 | 每个场景多等几秒 |
| **改断言「日志初始化行」** | 立刻可判、不依赖退出路径 | 只验证「建起来了」，不验证运行时统计 |

这一轮选后者（`backend=Dxgi` / `wasapi-loopback` 这些初始化日志是可靠的），
并在注释里写明理由。

### 六、完整输出（8 场景 / 25 断言）

```text
=== [1/8] 基础 UDP（无丢包） ===
    [PASS] 收到足够帧（实际 666，期望 >= 100）
    [PASS] nack_sent 字段存在（实际 0）
    [PASS] 有可播放延迟统计

=== [2/8] 3% 丢包 + NACK 重传 ===
    [PASS] 发出了 NACK（实际 196）
    [PASS] 有帧被重传救回（实际 152）
    [PASS] 反解出的丢包率在合理区间（0.026）

=== [3/8] H.264 编解码闭环 ===
    [PASS] 解码出足够帧（实际 649）
    [PASS] 解码零失败（实际 0）
    [PASS] 无解密失败（实际 0）

=== [4/8] Opus 音频闭环 ===
    [PASS] 解码出足够音频帧（实际 542）
    [PASS] 音频解码零失败（实际 0）
    [PASS] 音频码率 > 1kbps（实际 45.2）

=== [5/8] 帧加密 + 重放防护 ===
    [PASS] 解密零失败（实际 0）
    [PASS] 加密链路能解码出帧（实际 652）

=== [6/8] FEC 自适应 + 关键帧保护 ===
    [PASS] 发出了 FEC 冗余包（实际 8640）
    [PASS] 有帧被 FEC 恢复（实际 2063）
    [PASS] 关键帧保护把冗余度从初始 2 提上去了（实际 16）

=== [7/8] 真实屏幕采集（DXGI → 缩放 → 编码） ===
    [PASS] DXGI 采集已初始化（日志有 backend=Dxgi）
    [PASS] 缩放已生效（日志有 scale=Some）
    [PASS] 解码出真实画面（实际 565）
    [PASS] 解码零失败（实际 0）

=== [8/8] 真实音频采集（loopback → Opus） ===
    [PASS] 解码出音频帧（实际 554）
    [PASS] 音频解码零失败（实际 0）
    [PASS] 发送端持续产出音频帧（实际 500）
    [PASS] loopback 采集已初始化

========================================================
全部通过：25 项断言    EXIT=0
```

### 七、当前回归覆盖矩阵

| 能力 | 单元测试 | 端到端 |
| --- | --- | --- |
| 协议编解码 | ✅ 59 | ✅ 场景 1 |
| NACK 重传 | ✅ 26 | ✅ 场景 2 |
| H.264 编解码 | ✅ 12 | ✅ 场景 3 |
| Opus 音频 | ✅ 35 | ✅ 场景 4 |
| 帧加密 | ✅ 40 | ✅ 场景 5 |
| FEC + 关键帧保护 | ✅ 80 | ✅ 场景 6 |
| **屏幕采集 + 缩放** | ✅ 97 | ✅ **场景 7** |
| **音频采集 loopback** | ✅ 44 | ✅ **场景 8** |
| GPU 渲染 | ✅ 83 | ⚠️ 需窗口（headless 环境跳过） |
| 原生 UI | ✅ 47 | ⚠️ 需桌面（同上） |
| 时钟同步（跨时基） | ✅ 21 | ⚠️ 手工验证过（第 56 轮） |

### 八、验证

```text
单元/集成测试   1059 passed / 0 failed
端到端         25 项断言全通过（8 场景，约 2 分钟）
fmt / clippy    0 / 0
```

## 第五十九轮：GPU 渲染器的窗口 resize + 设备丢失恢复

### 一、为什么优先做这两个（而不是性能优化）

`docs/render-gpu.md` 列了 7 个已知缺口。按「用户会不会遇到」排序：

| 缺口 | 用户会遇到吗 | 后果 |
| --- | --- | --- |
| **没有窗口 resize 处理** | **必然会**（拖动窗口） | 画面变形 |
| **没有设备丢失恢复** | 会（驱动更新 / 插拔显示器 / 远程桌面） | 永久黑屏 |
| 没有硬件解码零拷贝 | 不会（只是慢 355µs） | 无感 |
| 未测 HDR | 少数人 | 颜色偏 |

**前两个是「用户一操作就坏」，后两个只是「不够快/不够准」。** 先修前者。

### 二、窗口 resize

```rust
pub fn resize(&mut self, width: u32, height: u32) -> Result<(), GpuError>;
```

**为什么必须显式调用**：`WM_SIZE` 到达时 swap chain 的 back buffer 还是旧尺寸。
继续 `Present` 会被 DWM 拉伸到新窗口 —— 表现为「拖动窗口时画面变形」。

而 GPU 渲染器**收不到** `WM_SIZE`（窗口过程在 `window.rs`，只处理退出标志），
所以必须由持有窗口的一方转发。

三件事：

1. `ResizeBuffers` 重建 back buffer
2. 丢弃旧管线（输出视图绑在旧 back buffer 上，必须重建）
3. 下一次 `present` 时按新尺寸重建

**幂等**：同尺寸重复调用直接返回（`WM_SIZE` 会重复发）。

### 三、接线：`StatusPanel::pump` 的返回值

`StatusPanel::pump()` 原本返回 `bool`（是否继续），现在返回 `Option<(u32, u32)>`：

```rust
// daemon 主循环
if let Some((vw, vh)) = p.pump() {
    if let Some(r) = renderer.as_mut() {
        r.resize(vw, vh)?;
    }
}
```

**为什么面板不直接调渲染器**：面板只负责布局（更新视频区矩形），
不知道 D3D11 的存在。swap chain 重建必须由持有渲染器的一方做。

### 四、实测：resize 生效

```text
改前尺寸: 913x676      （PowerShell DPI 虚拟化后的值，实际 1826x1351）
改后尺寸: 1200x800

INFO 渲染器已跟随窗口尺寸 width=1768 height=1104

render_frames=467  render_failed=0  video_decoded=467  render_mode="gpu-window"
```

**关键**：`render_frames` 持续增长、`render_failed=0` —— 说明 resize 后管线
重建成功、画面继续。

### 五、设备丢失恢复

```rust
pub fn recover(&mut self) -> Result<Option<isize>, GpuError>;
```

**什么时候会设备丢失**：

- 显卡驱动更新 / 崩溃（TDR）
- 从独显切到集显（笔记本拔电源）
- 远程桌面连接 / 断开
- GPU 过热降频导致复位

这些情况下所有 D3D11 对象都失效，`Present` 返回 `DXGI_ERROR_DEVICE_REMOVED`。
**唯一恢复办法是重建一切**。

### 六、一个必须保留的细节：子窗口挂载位置

重建要重新创建窗口（`SwapChain` 绑在 HWND 上）。如果重建出来的是**顶层窗口**，
它会盖住 UI 面板 —— 用户看到的是「拖动窗口后突然弹出一个独立窗口」。

所以 `Window` 要记住自己的挂载点：

```rust
pub struct Window {
    // ...
    /// 子窗口模式下的 `(父句柄, x, y)`；顶层窗口为 `None`
    child: Option<(isize, i32, i32)>,
}
```

`recover()` 用 `child_placement()` 还原同样的挂载位置。

### 七、测试

| 测试 | 覆盖 |
| --- | --- |
| `gpu_resize_keeps_rendering` | resize 后仍能正确出画（**最重要**） |
| `gpu_resize_same_size_is_idempotent` | 同尺寸重复调用不重建 |
| `gpu_resize_zero_is_error` | 尺寸 0 报错且不破坏原尺寸 |
| `gpu_headless_resize_updates_size` | headless 只改尺寸 |

`gpu_resize_keeps_rendering` 的设计意图：

```text
画一帧（白）→ 读回确认 → resize(640,480) → 再画一帧（黑）→ 读回确认
```

如果 `resize` 漏了重建管线，第二次 `present` 会画到已失效的纹理上 ——
表现为「窗口一拖动画面就黑」。这条测试守着它。

### 八、验证

```text
单元/集成测试   1063 passed / 0 failed   （原 1059 + 4）
GPU 实测        19 项（含 4 个新 resize 测试）
端到端          25 项断言全通过
fmt / clippy    0 / CLEAN
```

### 九、剩下的缺口

| 缺口 | 优先级 |
| --- | --- |
| 没有硬件解码零拷贝（`upload=355µs`） | 性能，非必需 |
| 未测 HDR / 10bit | 少数用户 |
| WARP 路径未实测 | 无独显环境 |
| 多显示器未测 | 常见但影响小 |

`recover()` 已实现但**未接入 daemon** —— 需要检测 `DXGI_ERROR_DEVICE_REMOVED`
并触发重建。那是下一轮的事（现在至少有了可调用的接口）。

## 第六十轮：设备丢失的自动恢复（补上 `recover()` 的接线）

### 一、上一轮留下的缺口

第五十九轮实现了 `GpuRenderer::recover()`，但**没有接入 daemon**：

> `recover()` 已实现但 daemon 还没自动触发 —— 需要检测 `DEVICE_REMOVED`
> 并调它。至少现在有了可调用的接口。

这意味着：**设备丢失后画面永久黑屏，用户只能重启软件。**

### 二、先解决一个前提：错误能不能被区分

原来 `GpuError` 只有 6 个变体，设备丢失被塞在 `Device(String)` 里。
调用方只能靠字符串匹配判断 —— 错误信息一改就失效。

新增变体：

```rust
pub enum GpuError {
    // ...
    /// 设备丢失（`DEVICE_REMOVED` / `DEVICE_HUNG` / `DEVICE_RESET`）
    DeviceRemoved { hresult: u32 },
}
```

**为什么必须单独一个变体**：设备丢失是**唯一可恢复**的 GPU 错误 ——
其余（格式不匹配、尺寸非法）重试一万次也一样失败。调用方需要区分：

```rust
match err {
    DeviceRemoved => renderer.recover(),   // 重建
    _ => 降级到 GDI,                       // 放弃 GPU
}
```

### 三、三个 HRESULT

| HRESULT | 常量 | 含义 |
| --- | --- | --- |
| `0x887A0005` | `DXGI_ERROR_DEVICE_REMOVED` | 显卡被拔 / 驱动更新 / 切显卡 |
| `0x887A0006` | `DXGI_ERROR_DEVICE_HUNG` | TDR（GPU 超过 2 秒没响应被复位） |
| `0x887A0007` | `DXGI_ERROR_DEVICE_RESET` | 驱动内部复位 |

三个都是「对象全失效、必须重建」，归成一类。

### 四、怎么从字符串里提取 HRESULT

这是「分类枚举 + 字符串带 HRESULT」设计的代价 —— 要判断具体错误码得解析字符串：

```rust
fn extract_hresult(msg: &str) -> Option<u32> {
    let idx = msg.find("0x")?;
    let hex: String = msg[idx + 2..].chars()
        .take_while(|c| c.is_ascii_hexdigit()).collect();
    if hex.len() < 4 { return None; }
    u32::from_str_radix(&hex, 16).ok()
}
```

替代方案是给每个变体加一个 `Option<u32>` 字段，但那会让 6 个变体全部变形 ——
而真正需要区分错误码的只有「设备丢失」这一处。

### 五、`present` 拆成两层

```rust
pub fn present(...) -> Result<(), GpuError> {
    self.present_inner(...).map_err(|e| {
        if let Some(hr) = extract_hresult(&e.to_string()) {
            if is_device_removed_hresult(hr) {
                return GpuError::DeviceRemoved { hresult: hr };
            }
        }
        e
    })
}
```

内层逻辑一行没改，外层统一转换 —— 这样所有底层模块（swapchain / processor /
texture）都不用知道 `DeviceRemoved` 的存在。

### 六、daemon 侧的自动恢复

```rust
match e {
    RenderFailure::DeviceRemoved { hresult } => {
        render_recoveries += 1;
        if render_recoveries <= 3 {
            warn!(hresult, attempt, "GPU 设备丢失，尝试重建渲染器");
            r.recover()?;
        } else if render_recoveries == 4 {
            warn!("重建连续失败 3 次，停止尝试（降级为只解码不渲染）");
        }
    }
    RenderFailure::Other(m) => { /* 只记日志 */ }
}
```

**为什么限 3 次**：否则「每帧都丢失、每帧都重建」会变成死循环，
比直接降级更糟（CPU 全花在重建上，画面永远出不来）。

### 七、`RendererKind` 的错误类型也要改

原来 `RendererKind::present` 返回 `Result<(), String>` —— 把分类信息丢了。
改成：

```rust
enum RenderFailure {
    DeviceRemoved { hresult: u32 },
    Other(String),
}
```

**返回 `String` 的话只能靠字符串匹配，错误信息一改就失效** —— 与上面
`GpuError` 分开是同一个理由。

### 八、测试

| 测试 | 覆盖 |
| --- | --- |
| `device_removed_hresult_values_are_correct` | 三个码都带「设备丢失」「重建」「0x」 |
| `device_removed_is_distinguishable_from_other_errors` | 普通错误不会被误认 |
| `device_removed_formats_hresult_as_8_digits` | 8 位大写十六进制（与 WinDbg 一致） |

三条都是**跨平台**测试（不依赖 GPU），在 Linux CI 上也会跑。

### 九、验证

```text
单元/集成测试   1066 passed / 0 failed   （原 1063 + 3）
端到端          25 项断言全通过
fmt / clippy    0 / CLEAN
```

### 十、一个诚实的说明

设备丢失的**真实触发**无法在这个环境里构造（需要拔显卡 / 触发 TDR /
切显卡）。所以这一轮的验证是：

- ✅ 错误分类逻辑（3 个单元测试）
- ✅ 编译与接线（daemon 能编译、逻辑路径可达）
- ❌ **未实测真实设备丢失后的恢复效果**

要真验证，可以写一个「主动销毁设备」的测试钩子 —— 但那是侵入性的，
且 `ID3D11Device` 没有公开的「模拟丢失」接口。已记为已知限制。

## 第六十一轮：git init + Linux 真实运行（第一次跑通）

### 一、git init（用户授权后执行）

```text
4d23711  初始提交：SideLens 端到端可运行（60 轮开发）
669c07f  修正 platform 测试硬编码 Windows + 补充 .gitignore
```

- **303 个文件 / 96,334 行**
- `target/`（23.5 GB）和 `node_modules/`（40 MB）已正确排除
- 补了 `.gitignore`：`.xplat-results.txt`、`*.manifest`、运行期截图

**为什么这件事重要**：第五十五轮我一次正则失误把 `processor.rs`（21,880 字节）
清空成 1 字节，**没有任何备份可恢复**，只能完整重写。现在不会了。

### 二、Linux 真实运行（B8 解除）

之前 WSL 里缺 C 编译器，`rustc` 报 `linker cc not found`。用户提供了 root 密码后：

```bash
apt-get install -y build-essential pkg-config libasound2-dev
apt-get install -y autoconf automake libtool
```

**第二个包组是关键**：`audiopus_sys` 需要 `autoreconf` 来构建捆绑的 Opus，
缺了会报 `autogen.sh: 14: autoreconf: not found`。

### 三、Linux 上的实测结果

**编译**：

```text
cargo build --release -p sidelens-proto -p sidelens-core -p sidelens-transport -p sidelens-crypto
  Finished `release` profile [optimized] target(s) in 19.64s

cargo build --release -p sidelensd -p sidelenssend
  Finished `release` profile [optimized] target(s) in 2m 54s
  sidelensd      2,676,120 字节
  sidelenssend   2,577,752 字节
```

**测试**：核心 4 个 crate 在 Linux 上**全部通过**（26 个测试套件，0 失败）。

**端到端 UDP**：

| 场景 | 接收端 | 发送端 |
| --- | --- | --- |
| 无丢包 | `frames=481` `playable_ms="16.94/20.70"` `av_synced=481` | `sent=300` `fps=60.2` `synced=true` |
| 3% 丢包 | `frames=492` `playable_ms="16.89/21.81"` | `retransmitted=87` |
| 5% 丢包 + FEC | `frames=487` `playable_ms="16.98/20.66"` | `retransmitted=20`（**从 87 降到 20**） |

**FEC 的效果在 Linux 上同样成立**：5% 丢包下把重传需求压掉 77%。

### 四、抓到一个真实缺陷：测试把开发环境当成了被测性质

在 Linux 上跑测试时，`sidelens-core/tests/platform.rs` 有一条失败：

```text
thread 'current_platform_is_windows_on_dev_machine' panicked:
  当前开发环境是 Windows，Platform::current() 应返回 Windows
  left: Linux
  right: Windows
```

**测试的名字和断言都写死了 Windows**，理由是「开发机是 Windows」。
但那是把**开发环境当成了被测性质** —— 在任何非 Windows 平台都必然失败，
而失败信息看起来像「移植有问题」，实际是测试写错了。

这条测试的**真实意图**是「证明 `cfg!` 判定链没写反」（比如把
`target_os = "macos"` 的字符串打错字，或两个分支写反）。

改成按 `cfg!` 反推期望值：

```rust
let expect = if cfg!(target_os = "windows") { Platform::Windows }
    else if cfg!(target_os = "macos") { Platform::MacOs }
    else if cfg!(target_os = "linux") { Platform::Linux }
    // ...
    else { Platform::Unknown };
assert_eq!(Platform::current(), expect);
assert_ne!(Platform::current(), Platform::Unknown, "检查 cfg! 字符串是否拼错");
```

**为什么这个写法仍然能抓到错误**：如果某个 `cfg!` 分支打错字，那个分支
永远不成立，函数会落到兜底 `Unknown` —— 两条断言都会失败。

修正后：**Windows 25 passed / Linux 25 passed**，两边都过。

### 五、一个运行期依赖的坑

`sidelensd` 在 Linux 上第一次启动报：

```text
error while loading shared libraries: libopus.so.0: cannot open shared object file
```

`audiopus_sys` 把 Opus 编译成了 **dylib** 但没装进系统库路径。文件在这里：

```text
target/release/build/audiopus_sys/<hash>/out/lib/libopus.so.0
```

临时解法是设 `LD_LIBRARY_PATH`。**生产环境应该**：

1. 用 `cargo install --path apps/sidelensd`（cargo 会处理好 rpath）
2. 或把 `libopus.so.0` 装到 `/usr/local/lib` + `ldconfig`
3. 或改用静态链接（`audiopus_sys` 支持 `OPUS_STATIC=1`）

### 六、当前跨平台状态

| 平台 | 编译 | 测试 | 运行 |
| --- | --- | --- | --- |
| Windows | ✅ | ✅ 1066 | ✅ 全功能（含 GPU 渲染、原生 UI） |
| **Linux** | ✅ | ✅ 核心 4 crate 全过 | ✅ **UDP 链路跑通**（无 UI/采集/渲染） |
| macOS | ✅ check | — | — |
| iOS / Android | ✅ check | — | — |

**Linux 上缺的是平台实现**（采集、渲染、UI、声卡），核心协议与传输已完全可用。

### 七、验证

```text
Windows  1066 passed / 0 failed
Linux    核心 4 crate 全过 + 端到端 UDP 三档场景
fmt/clippy  0 / CLEAN
git      2 次提交，303 文件 / 96,334 行
```

## 第二十轮：转 DirectShow（虚拟摄像头第一次真正出现在系统里）

### 一、上一轮的结论

第十九轮把 `MFCreateVirtualCamera` 路线彻底证伪了：六组 `EnableFrameServerMode` A/B
（不存在 / =1 / =0，顺序反转两轮）+ 七种调用参数组合，**全部返回同一个 `0xC00D36BD`**。

关键证据：DLL 侧日志显示 FrameServer 走完 13 步、`ActivateObject` 成功，
但**从未调用 `GetMediaTypeByIndex`**，然后直接 `Shutdown`。

这不是「某个参数不对」，是「这条路径不通」。

### 二、为什么选 DirectShow

| 维度 | MFCreateVirtualCamera | DirectShow 源滤镜 |
| --- | --- | --- |
| 注册 | API 调用 | `regsvr32` |
| 驱动 | 不需要 | 不需要 |
| 签名 | 不需要 | 不需要 |
| 最低系统 | Win11 22000+ | 全版本 |
| 权限 | 管理员（每次） | 管理员（仅注册） |
| 生命周期 | 进程退出即消失 | 注册后常驻 |

OBS 用这条路走了十几年，兼容性有保证。

### 三、新建 crate：`sidelens-dshow`

```text
crates/sidelens-dshow/
  src/lib.rs            COM 导出（4 个函数）
  src/filter.rs         源滤镜（IBaseFilter + IMediaFilter + IPersist）
  src/pin.rs            输出引脚（IPin + IAMStreamConfig + IEnumMediaTypes）
  src/source.rs         帧来源（复用 sidelens-ipc）
  src/registration.rs   注册表登记
```

`crate-type = ["cdylib", "rlib"]` —— cdylib 给系统加载，rlib 让测试能直接调。

### 四、四个真实踩坑

#### 1. `GetModuleFileNameW(None, ...)` 返回的是宿主进程路径

第一版注册后 `InprocServer32` 写成：

```text
C:\Windows\system32\regsvr32.exe    ← 错
```

因为 `regsvr32` 加载 DLL 后调 `DllRegisterServer`，此时**进程主模块是 regsvr32**。
注册表看起来成功了，但 `CoCreateInstance` 永远拿不到滤镜。

修法：`GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, <本函数地址>, ...)`
反查所属模块。加了回归测试守住。

#### 2. `AM_MEDIA_TYPE` 不在 `DirectShow` 模块下

`windows crate` 把它挂在 `Win32::Media::MediaFoundation` —— 因为 MF 复用了这些结构。
同类还有 `VIDEOINFOHEADER` / `PIN_INFO` / `CLSID_VideoInputDeviceCategory`。

#### 3. `IBaseFilter` 有三层父接口

```text
IBaseFilter ← IMediaFilter ← IPersist ← IUnknown
```

`#[implement]` 必须写全四个。状态机方法在 `IMediaFilter`，`GetClassID` 在 `IPersist`。
只写 `IBaseFilter` 会报一堆 `method is not a member of trait`。

#### 4. `GUID` 的 `Debug` 没有 `GUID(...)` 包装

```rust
format!("{:?}", guid)   // 9E4B7C31-5A2D-4F8E-B1C6-3D7A0E5F92B4
```

第一版按 `GUID(...)` 做 `replace`，得到的是原样字符串，写进注册表是无效键名。
单元测试 `clsid_string_has_braces` 抓到的。

### 五、实测：滤镜出现在系统里

```text
[1/3] 枚举 CLSID_VideoInputDeviceCategory
    ✅ @device:sw:{860BB310-...}\SideLens Camera
    共 4 个视频捕获设备

[2/3] CoCreateInstance(CLSID_SIDELENS_DSHOW)
    ✅ 创建成功
    ClassID = 9E4B7C31-5A2D-4F8E-B1C6-3D7A0E5F92B4

[3/3] IBaseFilter::EnumPins
    引脚 1: 名字="Output" 方向=1（输出）
```

**这是十九轮以来第一次真正成功** —— 滤镜在系统的视频捕获设备类别里，
任何用 DirectShow 或 MF 枚举摄像头的应用都能看到。

### 六、验证

```text
sidelens-dshow 单元测试   20 passed / 0 failed
全量测试                  1086 passed / 0 failed（原 1066 + 20）
fmt / clippy              0 / CLEAN
端到端（Quick）            25 项断言全通过
```

### 七、已知限制

| 限制 | 说明 |
| --- | --- |
| 分辨率固定 1280x720 | 未接 `IAMStreamConfig::SetFormat` 动态协商 |
| 只给 NV12 | 官方示例给 RGB32 + NV12，老应用可能只认 RGB32 |
| UWP 看不到 | DirectShow 是桌面 API |
| 未接真实帧源 | `FrameSource` 已就绪，`sidelensd` 侧还没开始灌帧 |

下一轮：把 `sidelensd` 的屏幕采集接到共享内存管道，让滤镜出真实画面。

## 第二十一轮：Android 端环境搭建 + 手机摄像头链路

### 一、产品定位纠偏

翻早期调研 `phonecam-research/research-summary.md`，第一句话是：

> 一套能把**手机当作电脑摄像头 + 麦克风**的软件

而 README 之前写的是「共享屏幕、麦克风、系统声音」——**产品定位漂移了**。

屏幕采集是第四十八轮引入的，当时本机没有摄像头（B1），用它来验证视频链路。
它本身是真实功能，但**不是原始定位**。这一轮把重心拨回来。

### 二、环境搭建（全部在 D:\software）

| 组件 | 版本 | 路径 |
| --- | --- | --- |
| JDK | Temurin 21.0.13.1 | `D:\software\Java\jdk-21.0.13.1+2` |
| Android SDK | cmdline-tools 19.0 | `D:\software\Android\Sdk` |
| platform-tools | adb 37.0.3 | 同上 |
| build-tools | 35.0.0 | 同上 |
| platform | android-35 | 同上 |
| NDK | 29.0.14206865 | 同上 |
| Gradle | 8.11.1 | `D:\software\Gradle\gradle-8.11.1` |
| Gradle 缓存 | — | `D:\software\Gradle\cache` |

Rust target：`aarch64-linux-android`。

### 三、目标设备

```text
型号      OnePlus 13 (PJZ110)
系统      Android 17 / API 37
架构      arm64-v8a
SoC       骁龙 8 Elite (qcom)
摄像头    后置 + 前置均可用
```

### 四、架构决策：采集用 Kotlin，不硬塞进 Rust

| 方案 | 代价 |
| --- | --- |
| Rust 调 Camera2 (C++) | Camera2 的 C++ 接口在 NDK 里**不公开**，要写大量 JNI 绑定 |
| **Kotlin 采集 + Rust 做协议** | 符合 Android 习惯，硬件编码器由系统管理 |

选后者。Rust 的价值在协议、时钟、抖动缓冲 —— 那部分已在 `sidelens-core` 里。

### 五、零拷贝链路

```text
Camera2 HAL
   ↓ 直接写入
MediaCodec.createInputSurface()   ← 不经过 CPU 内存
   ↓ 硬件 H.264 编码
ByteBuffer → TCP → Windows
```

默认路径是 `ImageReader` → CPU → 编码器，720p30 要 40 MB/s 的拷贝。
这里把编码器的 input Surface 当作 `createCaptureSession` 的输出目标，
帧从 HAL 直接进硬件编码器。

### 六、为什么第一步用裸 TCP

现有 UDP 协议带分片 + NACK + FEC，那是**为丢包设计的**。
第一步只需验证链路能不能通。裸 TCP 半天跑通，丢包由内核兜住。
等链路验证完再换 UDP。

线格式：`[4B 长度 BE][8B ptsUs BE][1B flags][N B H.264]`

### 七、接收端已验证

用 `crates/sidelens-codec/tests/data/golden-720p.h264`（136418 字节）模拟手机发送：

```text
=== 统计 ===
帧数    1
关键帧  1
字节    136418 (133.2 KB)

=== 独立解析器验证 ===
target\test-recv.h264  31帧  1280x720  关键帧=1  136418B

=== 逐字节比对 ===
源: 136418 字节
收: 136418 字节
完全一致: True
```

**接收端链路完全正确**，等手机端接上即可验证真实画面。

### 八、踩坑

#### Gradle wrapper 下载超时

`gradlew.bat` 首次运行要下载 Gradle 发行版，10 秒超时失败。
解法：把本地已下载的 zip 放进 wrapper 缓存目录
（`GRADLE_USER_HOME/wrapper/dists/gradle-8.11.1-bin/<hash>/`），
并建 `.ok` 标记文件跳过校验。

#### `CONTROL_AE_TARGET_FPS_RANGE` 必须固定

不设的话暗光下相机自动降到 15fps，接收端抖动缓冲会反复调整，
表现为周期性卡顿且日志里看不出原因。

#### `tcpNoDelay` 不是可选项

Nagle 会把 2-5 KB 的 P 帧攒住，表现为「每隔几十毫秒突然到一批帧」。

#### `createCaptureSession` 尺寸不支持时不报错

传不支持的分辨率会静默失败。启动前必须 `getOutputSizes` 打印支持列表。
