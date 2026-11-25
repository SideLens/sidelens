# SideLensSpace 功能移植进度

更新日期：2026-11-26

## 总目标

以 [MRRengi/rng-lens](https://github.com/MRRengi/rng-lens) 的公开功能为对照，在 SideLensSpace 中逐步实现完整可用的 Windows 桌面端和 Android 手机端，覆盖连接协议、相机控制、音频、热管理、画面效果、设备管理和发布体验。功能盘点和逐项状态详见 [rng-lens 移植清单](rng-lens-port.md)。

## 当前进度

- 已有 Android → Windows 虚拟摄像头基本链路：Camera2、硬件 H.264、USB/局域网 TCP、DirectShow。
- 已接入基础能力/状态/控制协议、断线重连、Wi-Fi 发现和手动 IP；仍需多设备及实机兼容性验证。
- Android 已有镜头/画质选择、变焦、闪光灯、对焦、曝光、白平衡、防抖、人脸优先曝光、暂停、麦克风和热状态上报。
- Android 已有热策略的分辨率/帧率/码率降档，并按冷却滞回逐步恢复；高温切档代码已完成，仍待真机验证。
- Android 前台服务现持有完整共享会话；Activity 重建/离开前台不会主动停止采集，仍需锁屏和厂商策略实测。
- Windows 已有原生 Win32 控制窗口、预览、DirectShow 虚拟摄像头、基本图像调节和 WASAPI 音频路由。
- **Windows 发布体验已闭环**：`sidelens-setup` 安装/卸载、`sidelensd --check-update`/`--update` 应用内更新、手机端升级提示三项全部完成并有自动化验证。
- Windows/Android 两端已积累若干尚未提交的功能改动；目前构建通过的部分和真机验证限制记录在 `docs/rng-lens-port.md`。

## 尚缺的主要功能

1. 热管理：热降分辨率，并在降温后按滞回策略平滑恢复；真机验证温度状态和相机重建。
2. 编解码：HEVC 视频；Windows AAC-LC 解码已接入，待 Android 旧系统设备与 Windows 实播验证；验证 4K/60 的设备能力与性能。
3. 画面效果：背景虚化/替换、自动取景、人像平滑，以及高分辨率 GPU 处理管线；锐化控制已接入，性能待测。
4. 设备和连接：第二台手机/第二路摄像头，能力协商和不同硬件组合的完整验证。
5. Android 生命周期：采集资源已迁入前台服务；锁屏、Activity 重建和进程恢复仍需真机及厂商后台策略验证。
6. ~~Windows 发布体验~~：**已完成**。安装/卸载、应用内更新、手机端升级提示均有代码、测试与可自动验证的发布产物，见 [release.md](release.md) 与 [update.md](update.md)。
7. 系统验证：OBS、微信及其他摄像头客户端的启动/停止/重连、音视频路由和不同 DirectShow 格式实测。

## 工作记录

### 2026-11-26：画面效果（自动取景、背景虚化、人像平滑）

清单里「背景虚化/替换、自动取景」是剩下最大的一块不依赖真机的功能。这一轮的做法与 rng-lens 不同，理由写在文档里。

- 新建 `crates/sidelens-frame-effects`：19 项测试，跨平台。
- **不引 ONNX**：模型进不了仓库、CI 没 GPU，这两条会让这块永远测不了。换成无需模型的近似 —— 自动取景用「梯度显著性 + 积分图」，背景虚化用「椭圆羽化 + 可分离盒式模糊」，人像平滑用「双边滤波的盒式近似」。
- 管线顺序是「效果 → 几何变换」：虚化和平滑都不改尺寸，放在前面让自动取景看到完整画面。
- 自动取景算出的窗口写进 `geometry.crop` 由几何变换执行；窗口尺寸正好等于 DirectShow 协商模式时不裁，避免尺寸不符。
- 三个效果默认全关。控制消息新增 `autoFrame` / `autoFrameScale` / `blur` / `smooth` 四个字段。
- 关闭自动取景时同时清 `focus_window` 与 `geometry.crop` —— 只清一半会留下「窗口没了但裁切还在」的状态。

关键断言：

| 断言 | 防的是什么 |
| --- | --- |
| `自动取景把窗口放在主体上` | 显著性算法退化成「追光源」 |
| `自动取景窗口是偶数对齐的` | 奇数裁切导致色度错位 |
| `平滑让窗口不逐帧跳动` | 窗口抖动比不裁更难看 |
| `背景虚化不改动清晰区中心` | 羽化半径算错，把人也糊了 |
| `人像平滑保住强边缘` | 退化成普通高斯模糊，五官糊掉 |
| `小尺寸画面直接跳过而不是越界` | 4K/1080p 之外的小分辨率崩溃 |

验证：`cargo test -p sidelens-frame-effects` 19 项；全仓 1256 项；clippy / fmt 通过。文档见 [frame-effects.md](frame-effects.md)。

未做：背景**替换**（换成自定义图片）只有虚化没有抠图，需要分割模型；GPU 侧实现。
### 2026-11-26：旋转与裁切（完成，含一个真实 bug）

rng-lens 清单里 Windows 端的「旋转/裁切」是另一项不依赖真机的缺失功能，这一轮补上。

- 新建 `crates/sidelens-frame-transform`：NV12 的裁切、旋转（0/90/180/270）、镜像、翻转。
- 用类型钉住三条 NV12 硬约束：宽高必须偶数、裁切偏移必须偶数、90°/270° 交换宽高。
- 手机链路的 CONTROL 新增 `rotate`（角度）与 `crop`（`x,y,w,h`）两个字段，非法值会打日志而不是静默忽略。
- 与 DirectShow 协商模式冲突时（旋转后尺寸变了）跳过变换并打一次日志，不往共享内存塞尺寸不符的帧。

**抓到的一个真 bug**：第一版 `fill_cropped_y` 的 stride 用了 `output.geometry.width()`，
在 90°/270° 旋转时那是**旋转后**的宽度，导致行写错位、小图上直接越界 panic。
测试 `裁切后旋转的尺寸是裁切尺寸的旋转` 抓到它；修法是显式用裁切后的宽度。

另一处值得记：`输出缓冲区尺寸不符时拒绝` 最初用 8×8 正方形做夹具，而正方形旋转 90° 后
几何不变，这条断言永远通过。改成 8×4 才真正有效 —— **夹具选错会让测试变成摆设**。

验证：`cargo test -p sidelens-frame-transform` 24 项通过；`cargo test -p sidelensd` 73 项通过；
clippy / fmt 全通过。文档见 [frame-transform.md](frame-transform.md)。

未做：GPU 侧变换（当前是 CPU 逐像素，默认配置下因 `is_identity` 整帧跳过而零开销）；
旋转后的画面方向仍需真机确认。
### 2026-11-26：Windows 安装/卸载、应用内更新、手机端升级提示（完成，已自动化验证）

这一轮把 rng-lens 移植清单里最后一条“待实现”的 Windows 发布体验做完了。三项功能、三层测试、一条命令验证。

#### 新增/改动的 crate

| crate | 职责 | 测试 |
| --- | --- | --- |
| `crates/sidelens-update` | 版本、清单、哈希、布局、升级决策（纯逻辑，跨平台） | 51 单元 + 5 集成 |
| `crates/sidelens-deploy` | 安装计划、原子写、备份、注册表（安装器与更新器共用） | 21 |
| `apps/sidelens-setup` | 安装/卸载 CLI + 沙箱自测 | 9 |
| `apps/sidelensd/src/update` | 清单获取、下载校验、落地、升级提示下发 | 17 + 6 e2e |
| `apps/android/.../PhoneUpgradePrompt.kt` | 手机端提示解析与显示 | 11 |

`sidelensd` 因此多了一个 lib 目标：更新链路是唯一既跨平台、又不依赖真实设备的分支，放进 lib 才能在 CI 上对它做真实的 HTTP 集成测试。采集/解码/GUI 仍留在 bin 里。

#### 安装/卸载

- per-user 装到 `%LOCALAPPDATA%\SideLens`，卸载项写 `HKCU\...\Uninstall\SideLens`（`DisplayName` / `DisplayVersion` / `InstallLocation` / `UninstallString` / `QuietUninstallString` / `NoModify` / `NoRepair` / `Publisher`）。
- 有管理员时直接 `LoadLibraryW` + `DllRegisterServer` 注册 DirectShow 滤镜，拿真实 HRESULT；不起 `regsvr32` 子进程（它在 DLL 内部失败时退出码仍是 0，脚本无法判断成败）。
- 无管理员时降级：只装文件、跳过滤镜注册、在 `installed.json` 里记 `registered_dshow: false`，并打印需要管理员执行的命令。
- 三条不变量：每文件独立原子（`.tmp` → `sync_all` → 改名）、覆盖前备份到 `backups/<旧版本>/`（最多 2 份）、失败带路径与原因。
- 卸载会拒绝层级过浅的安装根（防 `installed.json` 被改成 `C:\` 导致整盘删除）；故意不删 `%TEMP%\sidelens-*.log`。

#### 应用内更新

- `--check-update` / `--update` / `--stage-only` / `--write-manifest` 四个子命令，退出码区分“有更新”(1) 与“失败”(4)。
- 清单校验先比 size 再算 sha256；任一文件不符即清空本次暂存目录（不留半套文件）。
- 拒绝降级：清单版本 ≤ 当前版本时判 `UpToDate`，回滚分发不会把用户降级。
- 跨大版本：本地版本低于 `min_upgradable_version` 时判 `ReinstallRequired`，要求卸载重装（防残留旧 DLL）。
- HTTP 客户端手写：`http://` 走纯 std TcpStream，`https://` 走系统 WinHTTP（证书校验交给 Windows，仓库不引入 rustls/ring）。三种分帧方式（Content-Length / chunked / 关闭定界）都有测试。

#### 手机端升级提示

- Android 每 2 秒的 STATE 里新增 `version{code,name,protocol}`，顶层同时放一份 `protocol` 供只认顶层字段的老电脑端使用。
- 电脑端按清单 `phone` 段的两个数字判定，四种结论：`unsupported`（协议过老，优先级最高）/ `must` / `recommended` / `none`。
- 只在结论**变化**时下发 CONTROL：手机每 2 秒一条 STATE，不去重会反复弹横幅。十个单元测试覆盖首次下发、重复不重发、升级后显式撤销、级别收紧、协议优先、无清单不发、老手机没上报不发、垃圾 JSON 不崩、重置后重发。
- 手机端 `PhoneUpgradePrompt.fromControl` 对结构错误一律返回 null 而不抛：它在控制消息回调里，抛异常会连带丢掉同一条消息里的其它指令。
- UI：`must`/`unsupported` 显示阻塞横幅，`recommended` 只在状态栏追加一句；无提示时显式清空横幅（否则旧提示会一直挂着）。

#### 可自动验证的发布产物

```powershell
pwsh -NoProfile -File tools/verify-release.ps1   # 49 项断言，全在临时沙箱内
```

覆盖：打包产物齐全与逐文件哈希复核、安装器自测、静默安装、`--dry-run` 零副作用、重复安装幂等且产生备份、卸载干净、卸载不存在的目录返回 0、应用内更新全流程、拒绝降级、`--write-manifest` 生成带 phone 段的清单、Cargo / Android / 清单三方版本一致。

#### 这一轮撞到的两个真问题

1. **并行测试共用沙箱目录**：`cargo test` 同进程并发跑用例，目录名只用 PID 会让两个用例互相删文件，症状是「断言失败但单独跑通过」。改为 PID + 原子递增序号。
2. **Windows `rename` 被瞬时占用**：杀毒软件实时扫描会短暂锁住刚写的文件，`rename` 返回 `ERROR_ACCESS_DENIED` (5)。一次瞬时占用就让整个安装失败，用户看到「装到一半报拒绝访问」。加了 4 轮退避（共约 150 ms）。

#### 验证结果（本轮实测）

```text
cargo test --workspace                 1256 项通过，0 失败
cargo clippy --workspace -D warnings   通过
cargo fmt --all -- --check             通过
cargo check --workspace --all-targets  通过
gradlew :app:testDebugUnitTest         11 项通过（Kotlin）
gradlew :app:assembleDebug             成功
tools/verify-release.ps1               49 项断言全通过
```

#### 未做的部分

- 代码签名：未签名。清单的 sha256 是替代品（见 release-manifest.md §6）。
- MSI / NSIS：不做。目录 + 自研安装器已满足装/升/卸/可验证四件事。
- DirectShow 滤镜的 HKLM 注册在无管理员时仍需用户手动提权执行一次。

### 2026-11-25：Windows 登录自启动与断线提示（代码完成，交互待实机确认）

- Windows 托盘菜单新增“随 Windows 登录启动”勾选项。启动时读取当前用户的 `HKCU\Software\Microsoft\Windows\CurrentVersion\Run\SideLens`，托盘菜单显示真实状态；切换时写入或删除当前 exe 路径，失败会回滚勾选并在状态栏显示错误。
- 自启动命令为 `"<当前 exe>" --gui --background`。登录后静默启动接收器并驻留托盘，不主动弹窗；用户手动双击程序仍使用普通 `--gui`，窗口照常显示。
- 修正后台启动路径：先收起视频占位区，首帧到达后再显示预览，避免托盘启动时闪出空白视频框。
- 接收器 worker 输出“手机断开连接”或“等待重新连接”时，桌面状态栏现在明确显示“连接中断，等待手机重新共享…”，不再停留在上一次连接状态。
- 验证：`cargo check --workspace --all-targets` 成功；`cargo test -p sidelens-ui -p sidelensd` 通过（UI 47、sidelensd 39、doc 2）；新增自启动命令行与托盘事件的单元测试。尚未在真实 Windows 登录会话验证注册表启动项、Explorer 托盘菜单和多用户切换。

### 2026-11-25：热降分辨率（代码完成，待真机验证）

- 已确认现状：`ThermalPolicy` 仅返回帧率和码率上限；`MainActivity` 用 Camera2 和 MediaCodec 的固定尺寸启动会话。
- 已确认接收端支持新 `CONFIG` 到来时重建 H.264 解码器，因此协议层无需断开 TCP。
- 已完成：热级别 2/3/4 分别把上限设为 1080p/720p/480p，并按当前设备已协商的尺寸/帧率组合选择不超过用户所选档位的模式。
- 已完成：编码输出线程与热重建使用同一把锁；停止旧相机、创建新编码器/相机会话，不关闭 TCP；新编码器格式变化时发送新的 CONFIG。失败时尝试恢复旧编码器对应的采集会话。
- 已完成：冷却恢复仍使用现有每级至少等待一分钟的滞回。
- 验证：`apps/android/gradlew.bat :app:assembleDebug` 成功；尚未用 Android 真机验证高热降档、解码器切换、温度回落恢复和不同相机的重新打开行为。
- 限制：热降档帧率目前最低使用设备能力清单中的 30fps 档；需实机确认相机支持的 AE 范围和 4K/60 设备上的切换稳定性。

### 2026-11-25：Windows AAC-LC 音频解码（代码完成，待实播验证）

- 已新增 `sidelens-codec-mf::AacDecoder`，通过同步 Media Foundation AAC 解码 MFT 将 raw AAC-LC access unit 解为 PCM16 或 Float32，再转换为 WASAPI 使用的交错 PCM16。
- Android `AudioConfig` 中的 AudioSpecificConfig 被包装进 Media Foundation AAC 输入媒体类型；AAC 音频帧按原始 `raw_data_block` 输入，解码器输出接入现有设备路由/音频队列。
- Opus 路径保持原样，AAC 和 Opus 共用 Windows 音频设备切换逻辑。
- 验证：`cargo check -p sidelens-codec-mf -p sidelensd` 成功；尚未用 Android AAC 发送端和真实 Windows 输出设备验证声音、延迟及重连。
- 实现依据：[Microsoft AAC Decoder](https://learn.microsoft.com/en-us/windows/win32/medfound/aac-decoder) 与 [AAC Media Types](https://learn.microsoft.com/en-us/windows/win32/medfound/aac-media-types)。当前实现限定 AAC-LC、单声道/立体声以及 Media Foundation 同步解码 MFT。

### 2026-11-25：Windows 画面锐化（代码完成，性能待测）

- Windows 原生界面新增锐化滑块（0–1.5），启动状态和实时调整都会将值发送到接收端。
- 接收端在 NV12 亮度平面执行可调强度的 4 邻域 unsharp mask；锐化作用于共享内存输出帧，因此虚拟摄像头和本地预览使用同一结果。
- 同时修复自然外观下亮度/对比度/饱和度/色温调整被早退条件跳过的问题；这些参数现在在自然外观下也会应用。
- 验证：`cargo check -p sidelensd -p sidelens-codec-mf` 成功；尚未测量 1080p/4K CPU 开销和画面质量。锐化默认为关闭，用户启用后会增加每像素处理成本。

### 2026-11-25：Windows 全局快捷键（代码完成，交互待实机确认）

- 原生窗口新增 `RegisterHotKey` 全局快捷键，并在窗口销毁时注销：`Ctrl+Alt+S` 共享开关、`Ctrl+Alt+P` 暂停、`Ctrl+Alt+M` 镜像。
- 快捷键通过按钮的原生 `BM_CLICK` 链路触发，与鼠标点击共用同一业务逻辑；注册冲突会报告到标准错误。
- 验证：`cargo check -p sidelens-ui -p sidelensd` 成功；仍需确认快捷键在不同桌面应用前台时能否正常触发、并检查系统组合键冲突。

### 2026-11-25：Android 前台服务接管共享会话（代码完成，待真机生命周期验证）

- 已将 TCP 连接、Camera2、MediaCodec、麦克风采集、电脑控制消息和周期状态上报迁入 `SharingForegroundService`；Activity 只负责配置、权限和 UI 控制。
- Activity 进入后台或重建时只解绑服务，不再停止采集；绑定回来后读取共享状态，服务广播同步音频、暂停及麦克风模式开关。
- 通知栏“停止共享”由服务直接执行；启动/停止使用会话代次保护，避免旧启动任务污染刚重启的新会话；热切档继续在服务内完成。
- 验证：`apps/android/gradlew.bat :app:assembleDebug` 成功。尚未用真机验证锁屏、息屏、旋转重建、通知停止，以及各厂商后台限制下的持续推流。
- 限制：Android 进程被系统强杀后不会自动恢复 TCP/相机会话；恢复体验和 Android 版本差异仍需后续处理。

### 2026-11-25：Windows 最小化到系统托盘（代码完成，待交互实机确认）

- Windows 原生 UI 增加可选托盘行为；SideLens GUI 启用后，点窗口关闭按钮会隐藏窗口并保留接收器进程。
- 托盘左键恢复窗口，右键菜单提供“打开 SideLens”和“退出”；退出会正常销毁窗口并触发现有清理流程。
- 托盘图标创建失败时回退到正常退出，避免窗口隐藏后无法找回；窗口销毁与 UI Drop 都会移除图标。
- 验证：`cargo check -p sidelens-ui -p sidelensd` 成功，`git diff --check` 通过；尚未在 Windows 桌面实际确认任务栏图标、Explorer 重启和右键菜单交互。

## 更新约定

每完成一个可独立检查的功能块，记录日期、改动范围、验证结果和仍需实机验证的事项，并同步更新 `docs/rng-lens-port.md` 的对应状态。未验证的功能不标记为完整移植。