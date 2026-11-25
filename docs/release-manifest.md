# Release manifest contract (rng-lens 移植：安装/更新/升级提示)

这一份文件定义 **SideLens 发布产物** 里机器可读的那一层。三个消费方都是本仓库的代码，
因此它同时是接口契约、测试夹具格式、以及 `tools/verify-release.ps1` 的断言依据。

## 1. 两个文件

| 文件 | 谁写 | 谁读 |
| --- | --- | --- |
| `manifest.json` | `tools/package-release.ps1`（或 `sidelensd --write-manifest`） | `sidelens-setup`、`sidelensd --check-update`、`verify-release.ps1` |
| `installed.json` | `sidelens-setup` 安装成功后写入 | 卸载程序、`--check-update`、GUI 的"当前版本" |

两个文件都是 UTF-8 JSON，字段只增不减（读方必须容忍未知字段，写方不得省略已定义的字段）。

## 2. `manifest.json`

```json
{
  "schema": 1,
  "product": "SideLens",
  "channel": "stable",
  "version": "0.2.0",
  "released": "2026-11-26T08:00:00Z",
  "min_upgradable_version": "0.1.0",
  "files": [
    { "path": "sidelensd.exe", "size": 3182080, "sha256": "ab12…" },
    { "path": "sidelens_dshow.dll", "size": 302080, "sha256": "cd34…" }
  ],
  "phone": {
    "min_supported_version_code": 1,
    "latest_version_code": 2,
    "latest_version_name": "0.2.0",
    "min_supported_protocol": 1
  },
  "notes": "自由文本，安装器与更新器只透传、不解析"
}
```

### 逐字段判据

| 字段 | 规则 |
| --- | --- |
| `schema` | 固定 `1`。读方遇到更大值按"清单来自更新的版本"处理（见 §5） |
| `version` | 严格 `MAJOR.MINOR.PATCH`，与 `Cargo.toml` 的 `workspace.package.version` 一致 |
| `min_upgradable_version` | 低于它的安装不能原地升级，只能卸载重装（防止跨大版本残留 DLL） |
| `files` | **相对路径**，正斜杠，不允许 `..`、不允许绝对路径、不允许重复。`sha256` 小写十六进制 |
| `phone.min_supported_version_code` | 电脑端声明"低于它的手机端必须升级" |
| `phone.latest_version_code` | 电脑端声明的最新手机端 versionCode（提示用） |
| `phone.min_supported_protocol` | 手机端协议版本下限；手机端 `PhoneProtocol.VERSION` 低于它时电脑端拒绝建立会话 |

### 为什么 `files` 里必须带 size

校验时先比 size 再算 sha256：损坏的下载里 size 就差的情况占绝大多数，
一次 `stat` 就能挡掉，不必读 3 MB 算哈希。sha256 仍然是最终判据。

## 3. `installed.json`

```json
{
  "schema": 1,
  "product": "SideLens",
  "version": "0.2.0",
  "install_root": "C:\\Users\\me\\AppData\\Local\\SideLens",
  "installed_at": "2026-11-26T09:12:31Z",
  "source_manifest": "manifest.json",
  "registered_dshow": true
}
```

`registered_dshow` 记录安装时是否成功写入了 DirectShow 的 HKLM 注册：
安装器在没有管理员权限时降级为"只装文件、不注册滤镜"，值记 `false`，
卸载时据此决定要不要提示用户清理 HKLM 残留。

## 4. 安装布局（per-user，无需管理员）

```text
%LOCALAPPDATA%\SideLens\
  sidelensd.exe
  sidelens_dshow.dll
  sidelens_vcam_dll.dll
  manifest.json
  installed.json
  updates\          # --update 的下载与暂存目录
  backups\<version>\ # 覆盖前留存的上一个版本，最多保留 2 份
```

注册表：`HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\SideLens`
（`DisplayName` / `DisplayVersion` / `InstallLocation` / `UninstallString` /
`QuietUninstallString` / `NoModify` / `NoRepair` / `Publisher`）。

## 5. 版本与决策

升级决策是纯函数 `decide(current, manifest) -> Decision`：

| 条件 | Decision |
| --- | --- |
| 清单 `schema` > 本程序支持的 | `UnsupportedManifest` |
| 无法解析任何一侧版本 | `InvalidVersion` |
| 当前版本 < `min_upgradable_version` | `ReinstallRequired` |
| 清单 `version` ≤ 当前版本 | `UpToDate` |
| 否则 | `UpdateAvailable` |

"≤" 而不是 "="：从 0.3.0 滚回 0.2.0 分发时，0.2.0 的清单**不**应把 0.3.0 用户降级。

手机端同理，判据只有 versionCode：

| 条件 | PhoneHint |
| --- | --- |
| `reported < min_supported_version_code` | `MustUpgrade { ... }` |
| `reported < latest_version_code` | `Recommended { ... }` |
| 否则 | `None` |
| 手机端未上报任何版本 | `Unknown` |

协议下限单独判：`protocol < min_supported_protocol` 时直接
`PhoneHint::Unsupported { .. }`，优先级高于上面四行。

## 6. 为什么不签名

可自动验证的发布产物要求"任何人拿到产物就能自己跑一遍校验"。
代码签名需要证书，证书不在仓库里，会把可验证性变成"信我"。
这里的替代是：**清单里每个文件的 sha256 由打包脚本生成、由安装器与更新器校验、
由 `verify-release.ps1` 独立再算一遍**。三者不共享代码路径，
任何一环被改坏都会在另外两环暴露。

## 7. 落地位置

| 契约 | 实现 | 测试 |
| --- | --- | --- |
| 清单解析/生成/版本比较/决策 | `crates/sidelens-update` | 同 crate `tests/` |
| 安装/卸载/注册表 | `apps/sidelens-setup` | `--self-test` 沙箱 + `tools/verify-release.ps1` |
| 下载/校验/替换 | `apps/sidelensd --check-update` / `--update` | `tests/update_e2e.rs`（本地 HTTP 夹具） |
