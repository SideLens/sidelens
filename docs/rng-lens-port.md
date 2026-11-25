# rng-lens 功能移植清单

参考项目：[MRRengi/rng-lens](https://github.com/MRRengi/rng-lens)。本清单按该项目公开的 README、SPEC 和 STATUS 建立；“已具备”只表示 SideLens 现状，不代表功能已经与 rng-lens 等价。

## 传输与设备

| 功能 | SideLens 当前 | 移植状态 |
|---|---|---|
| USB/Wi-Fi 摄像头视频到 Windows 虚拟摄像头 | TCP + H.264 + DirectShow；设备枚举 480p–4K、30/60fps 并在连接时与接收端协商输出模式 | 部分完成，待应用实测 |
| 断线重连 | 接收端可接受新会话并重建解码器 | 已修复 |
| Wi-Fi 自动发现 | UDP 广播探测，PC 在 27301 回复，视频走 TCP | 已接入基础发现，待同网段实机验证 |
| 手动 IP、USB 配对 | Android 可搜索、手动输入电脑 IP；USB 使用 adb reverse | 部分完成 |
| 有版本的双向控制协议、能力/状态上报 | HELLO/CONFIG/FRAME/CONTROL 已接入；STATE 现携带 versionCode/versionName/protocol，Android 周期上报电量/热状态/音频/暂停状态；DirectShow 模式协商通过本地管道同步 | 部分完成 |
| H.264 / HEVC，4K / 60fps 与输出尺寸设置 | Android 可选择设备支持的 H.264 采集尺寸/帧率至 4K/60；DirectShow 已枚举 4K/60 输出并通过命名管道协商；HEVC 未接入 | 部分完成，待实机验证 |
| 第二台手机与第二路虚拟摄像头 | 单路 | 待实现 |

视频与控制使用统一消息封装 `u8 type | u32 length (LE) | payload`，类型包括 HELLO、CONFIG、FRAME、STATE、AUDIO_CONFIG、AUDIO、CONTROL、PING；定义位于 `crates/sidelens-phone-proto`，Android 对应实现位于 `PhoneProtocol.kt`。DirectShow 通过命名管道把协商出的尺寸/帧率发给接收端；Windows 预留 4K 共享内存并按目标模式重建解码器。手机周期发送电池、热级别、麦克风和**版本**状态；Windows 接收 Opus/AAC-LC 音频并经 WASAPI 播放，并按发布清单判定手机端该不该升级（见 [update.md](update.md)）。AAC 仍待真实设备实播验证；动态模式尚未在 OBS/微信实机验证。

## Android 手机端

| 功能 | SideLens 当前 | 移植状态 |
|---|---|---|
| 硬件编码、Camera2 Surface 零拷贝 | 有 | 已有基础链路 |
| 镜头选择、分辨率/帧率、变焦、闪光灯 | 手机端按 Camera2 与 H.264 硬编能力列出 640×480 至 4K、30/60fps 组合；镜头选择、变焦、闪光灯可用 | 部分完成 |
| 自动/锁定/手动对焦、曝光、白平衡、防抖、人脸曝光 | Camera2 控件可调；人脸检测优先选择脸部 AE 测光区域，Android/Windows 两端均可开关；需设备支持 face detection 与 AE regions | 部分完成，待真机验证 |
| 锁屏后台共享、通知停止、屏幕熄灭模式 | 前台服务持有 TCP、Camera2、MediaCodec、音频采集和状态上报；Activity 可解绑/重建，通知按钮停止会话；屏幕熄灭模式已有黑屏控件 | 代码已接入；锁屏、Activity 重建和厂商后台策略待实机验证 |
| 麦克风 Opus/AAC 与 voice/raw 模式 | Opus/AAC-LC 编码；voice 模式启用设备支持的 AEC/NSG/AGC，raw 关闭处理；Windows 支持 Opus 和 Media Foundation AAC-LC 解码并路由至可选 WASAPI 输出设备 | 代码已接入，待 AAC 真机实播验证 |
| 温度/电量状态上报与自适应降档 | 热级别/电池周期上报；热时降低分辨率/码率并设置帧率上限，冷却后逐级恢复；切换时重建 Camera2/MediaCodec 并沿用 TCP 会话，接收端通过新 CONFIG 重建解码器 | 代码已接入，待真机验证 |
| **手机端升级提示** | STATE 每 2 秒上报 `version{code,name,protocol}`；电脑端按清单 `phone` 段判定并下发 `phone_upgrade`；手机端显示阻塞横幅或状态栏提示，电脑端判定已够新时显式撤销 | **完成**（11 项 Kotlin 测试 + 10 项 Rust 去重/判据测试） |

## Windows 桌面端

| 功能 | SideLens 当前 | 移植状态 |
|---|---|---|
| 虚拟摄像头供 OBS/会议软件使用 | DirectShow 摄像头 | 已有基础链路 |
| 原生 Win32 状态窗口 | `--gui` 可启动/停止接收器、配置 adb reverse 并显示实时预览 | 部分完成，待接手机实测 |
| 实时预览、旋转/镜像/裁切 | GPU 子窗口显示共享内存帧；桌面界面可镜像/垂直翻转；**旋转 0/90/180/270 与裁切已实现**（`sidelens-frame-transform`，24 项测试覆盖奇偶约束、四次旋转复原、裁切原点、镜+翻=180°）；变换结果尺寸与 DirectShow 协商模式不符时会打日志跳过而不是塞错误尺寸的帧 | 完成（代码 + 测试）；待实机确认画面方向 |
| 色彩外观、亮度/对比/饱和/色温、锐化、人像平滑、夜景 | 虚拟摄像头输出支持 8 种外观、亮度/对比度/饱和度/色温滑块、可调亮度平面 unsharp mask，以及基于亮度变化阈值的双帧夜景降噪；当前 NV12 CPU 处理，人像平滑未实现 | 部分完成，4K/60 性能待实机测量 |
| 背景虚化/替换、自动取景 | **自动取景与背景虚化已实现**（`sidelens-frame-effects`，19 项测试）：梯度显著性 + 积分图找窗口、椭圆羽化 + 可分离盒式模糊；人像平滑用双边滤波近似。**不依赖 ONNX 模型**，因此可在 CI 上完整验证；背景「替换」（换背景图）未实现，只有虚化 | 部分完成（可测部分全部完成） |
| 麦克风播放路由 | 接收 Opus/AAC-LC 并经 WASAPI 输出；桌面端可选系统默认或活动播放终端，流中切换会重建 sink；AAC 路径仍需真机实播验证 | 部分完成 |
| 托盘、自启动、快捷键、暂停画面 | Windows GUI 关闭按钮可隐藏到系统托盘，左键恢复、右键可恢复或退出；支持 Ctrl+Alt+S 共享开关、Ctrl+Alt+P 暂停、Ctrl+Alt+M 镜像；托盘菜单可启用当前用户的登录自启动 | 代码已接入，交互待实机确认 |
| **安装/卸载** | `sidelens-setup`：per-user 安装到 `%LOCALAPPDATA%\SideLens`，写 HKCU 卸载项，有管理员时直接调用 `DllRegisterServer` 注册滤镜、无管理员时降级并记录；`--silent` / `--uninstall` / `--dry-run` / `--keep-data` / `--self-test`；原子写 + 覆盖前备份 + 最多 2 份回滚点 | **完成**（21 + 9 项测试 + 49 项发布断言） |
| **应用内更新** | `sidelensd --check-update` / `--update` / `--stage-only` / `--write-manifest`；清单校验（size + sha256）、下载到 `updates/<版本>/`、逐文件校验、原子替换、拒绝降级、跨大版本拒绝原地升级；HTTP 客户端覆盖 Content-Length / chunked / 关闭定界三种分帧 | **完成**（17 + 6 e2e 项测试） |
| **手机升级提示** | 见 Android 端同名条目 | **完成** |

## 实施顺序

1. 完成能力协商和 STATE 回传（基础 STATE、Opus 音频、控制消息、版本上报已接通）。
2. 完成 Android 服务持有采集资源及 HEVC 编解码；AAC-LC 解码和热管理分辨率降档已接入，需真机验证。
3. 将手机流接入 Windows 统一渲染/预览/虚拟摄像头管线，再逐项开放 GPU 画面特效。
4. 完成 Windows 主界面、音频设备友好名称/热插拔刷新、托盘、快捷键与更新/安装体验（**安装/卸载/更新/手机升级提示已完成**）。
5. 接入双设备，并对硬件与多应用占用进行实机验证。

参考 SPEC 中的 ONNX 人像分割、特定系统虚拟摄像头和多应用并发能力需分别处理模型/驱动依赖；不能仅靠界面移植宣称完成。