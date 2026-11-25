# 发布：打包、安装、卸载

> 目标：**任何人拿到仓库就能一条命令产出可分发的东西，并且自己验证它对不对。**
> 契约（字段、判据、布局）在 [release-manifest.md](release-manifest.md)。

## 一条命令

```powershell
pwsh -NoProfile -File tools/verify-release.ps1
```

它会：构建 → 打包 → 在临时目录里静默安装 → 校验每个文件哈希 → 干跑 →
重复安装 → 卸载 → 再装一次走应用内更新 → 检查拒绝降级 → 校验手机端清单 →
核对 Cargo / Android / 清单三方版本。**49 项断言，全在沙箱里，不动你的
%LOCALAPPDATA% 和注册表。**

只想要产物：

```powershell
pwsh -NoProfile -File tools/package-release.ps1
```

## 产物

`dist/<版本>/`：

| 文件 | 大小 | 作用 |
| --- | --- | --- |
| `sidelensd.exe` | ~3.5 MB | 常驻服务（`--gui` 起界面、`--phone-to-vcam` 收手机流、`--update` 更新） |
| `sidelens-setup.exe` | ~450 KB | 安装/卸载程序 |
| `sidelens_dshow.dll` | ~300 KB | DirectShow 虚拟摄像头滤镜 |
| `sidelens_vcam_dll.dll` | ~240 KB | MFCreateVirtualCamera 媒体源（历史路线，保留） |
| `manifest.json` | — | 每个文件的 size + sha256、版本、手机端版本要求 |
| `dist/sidelens-<版本>-win64.zip` | ~1.9 MB | 上面整个目录 |
| `dist/release-<版本>.json` | — | 机器可读结果（含 zip 哈希），给 CI 与发布流水线用 |

## 为什么是 per-user 布局

装到 `%LOCALAPPDATA%\SideLens`，卸载项写 `HKCU\...\Uninstall\SideLens`。

| 维度 | per-user（本方案） | per-machine（Program Files） |
| --- | --- | --- |
| 安装权限 | 不需要管理员 | 需要 |
| 谁能卸 | 装它的那个用户 | 任何管理员 |
| DirectShow 滤镜（写 HKLM） | **装不了，需单独提权** | 能装 |
| 同名多用户冲突 | 各装各的，互不影响 | 共用一份 |

选 per-user 是因为：**装不上虚拟摄像头时用户至少还能用其余功能**，
而「必须右键以管理员运行」会让相当一部分人在第一步就放弃。
代价是 DirectShow 的 HKLM 注册需要额外一步。

### DirectShow 滤镜那一步

安装器不带管理员时会跳过注册，并在 `installed.json` 里记 `registered_dshow: false`。
需要虚拟摄像头时，用管理员终端重跑一次即可（文件已就位，不会重复安装）：

```powershell
# 管理员终端
& "$env:LOCALAPPDATA\SideLens\sidelens-setup.exe" --silent
```

安装器直接 `LoadLibraryW` + `DllRegisterServer`，不起 `regsvr32` 子进程 ——
`regsvr32` 在「DLL 内部返回失败」时退出码仍然是 0，脚本没法判断成败；
直接调用能拿到真实 HRESULT。

## 安装器命令行

```text
sidelens-setup                          交互式安装（默认 %LOCALAPPDATA%\SideLens）
sidelens-setup --silent                 静默安装，输出 SILENT-OK
sidelens-setup --uninstall              卸载
sidelens-setup --uninstall --silent     静默卸载，输出 SILENT-UNINSTALL-OK
sidelens-setup --uninstall --keep-data  卸载但保留 backups/ 与 updates/
sidelens-setup --dry-run                只打印会动哪些文件
sidelens-setup --self-test              沙箱内跑完整装/升/卸，不碰系统
sidelens-setup --install-dir <目录>     自定义安装根（测试与绿色版）
```

退出码：

| 码 | 含义 |
| --- | --- |
| 0 | 成功 |
| 2 | 参数错误 |
| 3 | 清单缺失或不合格 / 版本不允许升级 |
| 4 | 执行失败（复制、删除、注册表） |
| 5 | 自测失败 |

## 三条不变量

这三条是 `sidelens-deploy` 全部测试的对象，也是「装到一半崩溃」不会毁掉现有安装的原因：

1. **每个文件独立原子**：先写 `<name>.tmp` → `sync_all` → 改名。
   中途崩只留一个 `.tmp`，不会留半个 exe。
2. **覆盖前先备份**：旧文件进 `backups/<旧版本>/`，最多留 2 份。
   回滚就是把备份目录拷回来。
3. **失败要能说出来**：错误信息带路径与原因，CLI 据此给非 0 退出码。

### Windows 上额外要做的一件事

`rename` 会被杀毒软件的实时扫描短暂锁住，返回 `ERROR_ACCESS_DENIED` (5)。
这不是 bug 但**必须处理** —— 一次瞬时占用就让整个安装失败，用户看到的是
「装到一半报拒绝访问」。安装器退避 4 轮共约 150 ms，覆盖到扫描释放锁的典型时间窗。
（这一条是实测撞出来的：自测在并行用例下偶发失败，最后定位到两个原因 ——
沙箱目录名只用 PID 导致并发共用，以及 rename 的瞬时占用。两者都已修。）

## 卸载干净到什么程度

卸载做三件事：

1. 有管理员 → 反注册 DirectShow 滤镜；没有 → 打印需要手动执行的 `regsvr32 /u` 命令；
2. 删整个安装根（`plan_uninstall` 拒绝层级过浅的路径，防 `installed.json` 被改成 `C:\`）；
3. 删 `HKCU\...\Uninstall\SideLens`。

**不删**的东西：`%TEMP%\sidelens-*.log`（诊断日志）。这是刻意的 ——
卸载后发现「为什么当时装不上」，全靠这些日志。

## 已知边界

| 边界 | 现状 |
| --- | --- |
| 代码签名 | 未签名。清单的 sha256 是替代品，见 release-manifest.md §6 |
| MSI / NSIS 安装包 | 不做。目录 + 自研安装器已满足「装、升、卸、可验证」四件事 |
| 系统级安装（给所有用户） | 不支持 |
| 静默安装的进度条 | 没有。`--silent` 只输出关键行，靠退出码判断 |
| 用户损坏了已安装文件 | 重跑一次 `--silent` 即可修复（安装器逐文件校验后覆盖） |
