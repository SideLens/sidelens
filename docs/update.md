# 应用内更新与手机端升级提示

> 判据与契约见 [release-manifest.md](release-manifest.md)；打包与安装见 [release.md](release.md)。

## 桌面端：`--check-update` / `--update`

```text
sidelensd --check-update <URL|本地路径>     只检查，不改动
sidelensd --update <URL|本地路径>           检查 + 下载 + 校验 + 落地
sidelensd --update <URL> --stage-only       只下载到 updates/，下次启动落地
sidelensd --write-manifest <目录> --version 0.2.0 --out manifest.json
```

退出码（`tools/verify-release.ps1` 唯一的判据来源）：

| 码 | `--check-update` | `--update` |
| --- | --- | --- |
| 0 | 已是最新 | 更新已落地 |
| 1 | **有可用更新**（只检查） | 已下载待重启落地（`--stage-only`） |
| 2 | 参数错误 | 参数错误 |
| 3 | 清单无效 / 拒绝执行 | 清单无效 / 拒绝执行 |
| 4 | — | 下载或落地失败 |

把「有更新」和「失败」分成两个非 0 码很重要：CI 里前者是预期结果，后者是事故。
混在一起就没法用退出码做断言。

## 分两段：下载与落地

```text
   --check-update  ──▶  decision（纯逻辑，不碰磁盘）
   --update        ──▶  updates/<版本>/  （下载 + 逐文件 sha256 校验）
                    ──▶  安装根          （原子替换 + 备份旧版本）
```

### 为什么不就地覆盖正在运行的 exe

Windows 允许**改写**正在运行的 exe（它是「共享删除」打开的），但不允许**删除**它。
而原子替换是「写 `.tmp` → 删旧文件 → 改名」。因此 `--stage-only` 是给
「服务正在跑、用户正在用摄像头」这个场景准备的：下载好、校验完、提示重启。

托盘后台的自动检查走这条路径。

### 失败即丢弃

任何一个文件校验不过，整个 `updates/<版本>/` 被清空。留着一个「部分正确」的
暂存目录会诱导后续步骤把半套文件装上去 —— 这是本模块唯一的破坏性动作，
因此它只作用于 `updates/` 之下、且只作用于本次新建的版本子目录。

## HTTP 客户端

三选一之后选了手写：

| 方案 | 代价 |
| --- | --- |
| `reqwest` | 拉 tokio-tls 全家桶，release 体积与编译时间都翻倍 |
| `ureq` | 依赖 rustls + ring，离线环境装不上 |
| **手写 + WinHTTP** | 只需 GET、Content-Length/chunked、最多 5 次重定向 |

更新器下载的是固定的几 MB 二进制，不需要连接池、压缩、cookie、HTTP/2。
`http://` 走纯 std `TcpStream`，`https://` 走 Windows 自带的 WinHTTP ——
证书校验交给系统，本仓库不放任何证书或加密库。

### 三种分帧方式都测了

| 方式 | 谁会用 | 测试 |
| --- | --- | --- |
| `Content-Length` | nginx 静态文件 | `三种分帧方式都能取回并解析清单` |
| `chunked` | 反代、S3 直传、动态生成 | 同上（用 7 字节不规则块长） |
| 关闭连接定界 | 老旧 CGI、部分 CDN 边缘 | 同上 |

`apps/sidelensd/tests/update_http.rs` 起一个真 HTTP 服务，六个用例覆盖：
三种分帧、完整更新流程、404 可读错误、传输中内容被改坏、
清单里写了服务器上不存在的文件、服务器不可达不挂死。

## 手机端升级提示

### 谁判断

**电脑端**。手机端不知道自己该多新 —— 只有电脑端知道：它从发布清单的
`phone` 段读到两个数字，按手机在 STATE 里上报的 `versionCode` 判断。

把判断放在电脑端还有一个现实好处：手机可能没联网、可能是内测包、
可能来自第三方应用市场，指望它自己检查更新是不可靠的。

### 数据流

```text
  手机 STATE（每 2 秒）
    { "version": { "code": 2, "name": "0.2.0", "protocol": 1 }, "protocol": 1, ... }
        │
        ▼
  sidelensd::update::hint::HintTracker
    判据（sidelens_update::phone_hint）  +  去重
        │  只在结论变化时下发
        ▼
  CONTROL { "phone_upgrade": { "level": "must", "min_code": 3, ... } }
        │
        ▼
  手机 PhoneUpgradePrompt.fromControl  →  横幅 + 状态栏
```

### 四种级别

| level | 触发条件 | 手机端表现 |
| --- | --- | --- |
| `unsupported` | 手机协议 < `min_supported_protocol` | 阻塞横幅 |
| `must` | `versionCode` < `min_supported_version_code` | 阻塞横幅 |
| `recommended` | `versionCode` < `latest_version_code` | 状态栏追加一句 |
| `none` | — | 撤销上一次的横幅 |

协议下限优先级最高：协议不兼容时「版本够不够新」已经没有意义，会话本身建不起来。

### 为什么必须显式撤销

手机端那条横幅是被 CONTROL 写上去的，没有任何东西会去清它。
如果只在「有提示」时下发、没提示时什么都不发，用户升级手机后横幅会一直挂着。
所以「从有到无」会发一条 `level: none`。

### 去重

手机每 2 秒一条 STATE。每 2 秒重发同一条 CONTROL 会让手机端反复弹横幅、
日志被刷满。`HintTracker` 维护「上一次发过的提示」，只在结论变化时下发。
十个单元测试覆盖：首次下发、重复不重发、升级后撤销、级别收紧、协议优先、
无清单不发、老手机没上报不发、垃圾 JSON 不崩、重置后重发。

### 手机端三种历史形态都要能读

`sidelens-update/src/phone.rs` 接受：

| 形态 | 来源 | 解析结果 |
| --- | --- | --- |
| 没有 `version` 字段 | 0.1.0 的手机端 | 未上报（不下发任何提示） |
| `version` 是整数 | 简短形态 | 只有 code |
| `version` 是对象 | 本次新增的规范形态 | code / name / protocol |

用户手上一定有没升级的手机端，电脑端不能因为读不懂 STATE 就断开会话。

## 清单从哪来

优先级：

1. `SIDELENS_MANIFEST` 环境变量（测试与内网部署）
2. 安装根下的 `manifest.json`

都没有 → 不下发任何提示，行为与「没有这个功能」一致。
从开发机直接跑 `sidelensd` 时安装目录里没有清单，这时不该报错，也不该瞎提示。

## 测试矩阵

| 层 | 位置 | 数量 |
| --- | --- | --- |
| 版本 / 清单 / 哈希 / 布局 / 决策 | `crates/sidelens-update` | 51 单元 + 5 集成 |
| 安装计划 / 原子写 / 备份 | `crates/sidelens-deploy` | 21 |
| 安装器 CLI 与自测 | `apps/sidelens-setup` | 9（含一次完整装/升/卸） |
| 更新模块 | `apps/sidelensd/src/update` | 17 |
| 更新 HTTP 端到端 | `apps/sidelensd/tests/update_http.rs` | 6 |
| Android 提示与 STATE | `apps/android/app/src/test` | 11 |
| NV12 几何变换 | `crates/sidelens-frame-transform` | 24 |
| 画面效果 | `crates/sidelens-frame-effects` | 19 |
| 发布全流程 | `tools/verify-release.ps1` | 49 断言 |