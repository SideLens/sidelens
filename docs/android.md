# Android 端（手机当摄像头）

> 第二十一轮。目标：一加 13（Android 17 / API 37）把摄像头画面低延迟传给 Windows。

## 架构决策

### 为什么采集用 Kotlin 而不是 Rust

两条路：

| 方案 | 做法 | 代价 |
| --- | --- | --- |
| A | Rust 通过 NDK 直接调 Camera2（C++ API） | 要写大量 JNI 绑定，且 Camera2 的 C++ 接口在 NDK 里是**不公开**的 |
| **B** | Kotlin 用 Camera2 + MediaCodec，Rust 只做协议与传输 | 符合 Android 习惯，硬件编码器由系统管理 |

选 B。Android 的相机与编解码是**系统服务**，Kotlin 层调它们最直接；
Rust 的价值在协议、时钟、抖动缓冲这些**纯逻辑**，那部分已经在 `sidelens-core` 里。

### 零拷贝链路

```text
Camera2 HAL
   ↓ 直接写入
MediaCodec.createInputSurface()   ← 不经过 CPU 内存
   ↓ 硬件 H.264 编码
ByteBuffer
   ↓ TCP
Windows 端
```

摄像头帧默认路径是 `ImageReader` → CPU 内存 → 编码器，1280x720 NV12 约 1.3 MB/帧、
30fps = 40 MB/s 的拷贝。这里直接把编码器的 input Surface 当作 `createCaptureSession`
的输出目标 —— 帧从 HAL 直接进硬件编码器。这是 Android 相机录视频的标准做法。

代价：拿不到 YUV（编码前无法做图像处理）。当前不需要。

### 为什么第一步用裸 TCP 而不是现有 UDP 协议

`sidelens-transport` 的 UDP 协议带分片、NACK、FEC —— 那是**为丢包设计的**。
但手机端第一步只需要验证「摄像头 → 编码 → 网络 → 电脑」这条链能不能通。
裸 TCP 半天能跑通，丢包由内核重传兜住。

等链路验证完，再把这层换成 UDP 分片 —— 那时才有必要处理弱网。

线格式：

```text
[4B 长度 BE][8B ptsUs BE][1B flags][N B H.264 数据]
```

flags 的 bit0 表示关键帧。

## 环境

全部装在 `D:\software`：

| 组件 | 版本 | 路径 |
| --- | --- | --- |
| JDK | Temurin 21.0.13.1 | `D:\software\Java\jdk-21.0.13.1+2` |
| Android SDK | cmdline-tools 19.0 | `D:\software\Android\Sdk` |
| platform-tools | adb 37.0.3 | `D:\software\Android\Sdk\platform-tools` |
| build-tools | 35.0.0 | `D:\software\Android\Sdk\build-tools` |
| platform | android-35 | `D:\software\Android\Sdk\platforms` |
| NDK | 29.0.14206865 | `D:\software\Android\Sdk\ndk` |
| Gradle | 8.11.1 | `D:\software\Gradle\gradle-8.11.1` |
| Gradle 缓存 | — | `D:\software\Gradle\cache` |

环境变量（User 级）：`JAVA_HOME` / `ANDROID_HOME` / `ANDROID_SDK_ROOT` /
`GRADLE_HOME` / `GRADLE_USER_HOME`，Path 里加了 `jdk\bin`、`platform-tools`、
`cmdline-tools\latest\bin`、`gradle\bin`。

Rust target：`aarch64-linux-android`（一加 13 是 arm64-v8a）。

## 代码结构

```text
apps/android/
  app/src/main/java/com/sidelens/app/
    MainActivity.kt     权限、开关、状态显示
    CameraCapture.kt    Camera2 → Surface
    VideoEncoder.kt     MediaCodec 硬件编码
    FrameSender.kt      TCP 发送
  app/src/main/res/     布局与主题
  build.gradle.kts      AGP 8.7.3 / Kotlin 2.0.21 / minSdk 26
```

## 实测结果（一加 13 / Android 17）

```powershell
# 1. 编译安装
cd apps\android
.\gradlew.bat assembleDebug
adb install -r app\build\outputs\apk\debug\app-debug.apk

# 2. 端口转发（手机访问电脑的 27300）
adb reverse tcp:27300 tcp:27300

# 3. 电脑端开始接收
target\release\sidelensd.exe --phone-recv 27300

# 4. 手机上打开 SideLens → 开始共享
```

收到码流后用现有工具验证：

```powershell
node tools/h264-inspect.mjs target\phone-capture.h264 --summary
```

## 关键参数

| 参数 | 值 | 理由 |
| --- | --- | --- |
| 分辨率 | 1280x720 | 与 DirectShow 滤镜、现有管线一致 |
| 帧率 | 30 fps | 固定 `CONTROL_AE_TARGET_FPS_RANGE`，避免暗光降到 15fps |
| 码率 | 4 Mbps | 视频会议高画质档；再低快速运动会有块效应 |
| 关键帧间隔 | 1 秒 | 与现有 FEC 关键帧保护配合 |
| `KEY_LATENCY` | 1 | 不让编码器攒帧 |
| `KEY_PRIORITY` | 0 | realtime |
| `tcpNoDelay` | true | 否则 Nagle 会攒 40ms |

## 踩坑记录

### 1. `CONTROL_AE_TARGET_FPS_RANGE` 必须固定

不设的话，暗光下相机会自动降到 15fps 甚至更低。接收端的抖动缓冲会跟着
反复调整 `target_ms`，表现为画面周期性卡顿 —— 而且日志里看不出是帧率变了。

### 2. `tcpNoDelay` 不是可选项

Nagle 算法会把小包攒到 MSS 再发。一帧 720p 关键帧约 30 KB 能一次发完，
但 P 帧只有 2-5 KB，会被攒住 —— 表现为「每隔几十毫秒突然到一批帧」。

### 3. `createCaptureSession` 的尺寸必须真的被支持

传一个不支持的分辨率，`createCaptureSession` **不报错**，只是 `onConfigureFailed`
或者干脆静默。所以启动前先 `getOutputSizes` 打印一遍支持的列表。

## 实测结果（一加 13 / Android 17 / API 37）

### 采集与编码

```text
SideLens: 连接中…
SideLens.Sender: 已连接 127.0.0.1:27300（第 1 次尝试）
SideLens.Encoder: 编码器已启动 1280x720@30 4000kbps
SideLens.Camera: 采集已启动 1280x720@30
SideLens: 共享中 · 1280x720@30fps
SideLens.Encoder: 输出格式: {mime=video/avc, profile=8, level=512,
                                num-input-slots=14, num-output-slots=8, latency=1}
```

摄像头支持 39 种尺寸（4096x3072 到 176x144），1280x720 在其中。

### 传输

```text
帧 600  关键帧 20  12352.1 KB  29.5 fps  4.98 Mbps
```

关键帧严格 1 秒一个（600 帧 / 20 个），与 `KEY_I_FRAME_INTERVAL=1` 一致。

### 码流验证

```text
node tools/h264-inspect.mjs target/phone-real.h264 --summary
→ 619帧  1280x720  13086233B

--json 明细：
  types: { "1 (non-IDR)": 598, "5 (IDR)": 21, "7 (SPS)": 1, "8 (PPS)": 1 }
  sps:   { profile: 100, level: 31, width: 1280, height: 720 }
  frames: 619, idrSlices: 21
```

**注意 `keyframes: 0` 不是 bug**：解析器按「SPS 是否紧跟 IDR」判关键帧，
而 MediaCodec 只在流开头输出一次 SPS/PPS（`csd-0` / `csd-1`），
没有在每个 IDR 前重复。解码器会缓存参数集所以播放正常；
但要把码流交给外部播放器，需要设 `KEY_PREPEND_SPS_PPS_TO_IDR_FRAMES`。

### 完整链路（手机 → 虚拟摄像头）

```text
=== 手机摄像头 → 虚拟摄像头 ===
共享内存已建: sidelens-shm
管道已建: sidelens-frame
✅ 手机已连接: 127.0.0.1:12409
解码器: H.264 Decoder MFT
  解码 210 帧  28.8 fps  4.6 Mbps
```

手机 Camera2 → MediaCodec → USB(TCP) → MF 解码 → 共享内存，**28.8 fps 稳定**。

### 滤镜侧

```text
=== 从 SideLens Camera 拉帧 ===
✅ 滤镜已创建
✅ 找到输出引脚
✅ 滤镜已 Run
   状态: FILTER_STATE(2)（2 = Running）
```

滤镜能被创建、能 Run、引脚就绪。真正的 `FillBuffer` 由消费端驱动 ——
需要 Zoom / OBS 这类真实应用拉起它。

### UWP 应用的限制

Windows 自带相机 App 是 UWP，**不走 DirectShow**，所以它看不到这个滤镜
（启动后 FrameServer 仍是 Stopped）。UWP 只认 Media Foundation 设备。
这是 DirectShow 路线的固有边界，不是 bug。

## 两个真 bug

### 1. `NetworkOnMainThreadException`

第一版把 `FrameSender.connect()` 直接放在点击回调里，一点就崩：

```text
E SideLens: android.os.NetworkOnMainThreadException
    at com.sidelens.app.FrameSender.connect(FrameSender.kt:38)
    at com.sidelens.app.MainActivity.startSharing(MainActivity.kt:81)
```

Android 从 3.0 起禁止主线程网络操作。改成两个线程：
`worker` 做连接与采集初始化，`drainer` 轮询编码输出。

### 2. 连接时序

手机点按钮时电脑端接收器可能还没起来。`FrameSender.connect` 加了
15 次重试（每秒一次），第一次失败不再直接崩。

## 环境变量清单

```powershell
JAVA_HOME        = D:\software\Java\jdk-21.0.13.1+2
ANDROID_HOME     = D:\software\Android\Sdk
ANDROID_SDK_ROOT = D:\software\Android\Sdk
GRADLE_HOME      = D:\software\Gradle\gradle-8.11.1
GRADLE_USER_HOME = D:\software\Gradle\cache
```

## 下一步

| 任务 | 说明 |
| --- | --- |
| 用 OBS 验证真实画面 | OBS 的「视频捕获设备」就是 DirectShow，能直接确认滤镜出图 |
| 换 UDP 传输 | 现在是裸 TCP；换 `sidelens-proto` 才有 NACK / FEC |
| 加音频 | `RECORD_AUDIO` 权限已申请，`AudioRecord` + Opus 还没写 |
| 动态分辨率 | 现在固定 1280x720，可接 `IAMStreamConfig` 协商 |
