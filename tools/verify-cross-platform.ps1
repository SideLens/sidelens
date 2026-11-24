# 跨平台编译回归（第四十轮新增）
#
# # 为什么需要它
#
# 项目此前**从未**跑过 `cargo check --target <非本机>`。
# 第四十轮第一次跑就暴露了两个累积多轮的缺陷：
#   1. sidelensd 在非 Windows 上 5 个 E0425（cfg 配对错误）
#   2. sidelens-vcam/examples/registry_entry.rs 缺 cfg 门控（3 个 E0433）
#
# 这条检查比 `cargo test` 便宜得多（不跑测试、不链接），
# 却是唯一能发现「只在某个平台编译失败」的手段。
#
# # 为什么不直接用默认 target/
#
# cargo 的产物目录是 `target/<triple>/<profile>/`，**每个 triple 一份
# 完整、互不共享的依赖树**。机器上装了 5 个 target，全跑一遍就是 5 套
# 依赖编译结果堆在 target/ 里。
#
# 实测：单个 triple 的 check 产物约 0.3 GB；加上 `windows` crate 单个
# rlib 就有 105 MB、rmeta 100 MB（feature 开得多），累积到几十 GB 很轻松。
#
# 更糟的是这些产物**永远不会被主构建复用** —— 主构建只用主机 triple，
# 交叉产物纯属一次性垃圾，却和主产物挤在同一个 target/ 里，没法区分
# 「哪些能删」。
#
# 所以这里把交叉 check 全部导到 `target/xplat`（独立 target-dir），
# 跑完就删。check 不链接，单个 triple 只要 10 秒上下，重编成本远低于
# 长期占几十 GB。
#
# 用法:
#   pwsh -File tools/verify-cross-platform.ps1           # 跑完自动清理
#   pwsh -File tools/verify-cross-platform.ps1 -Keep     # 保留产物（调试用）

[CmdletBinding()]
param(
    # 保留 target/xplat 下的交叉产物，便于事后翻看 .rmeta / 复现错误
    [switch]$Keep,
    # 交叉产物目录，默认 target/xplat（被 .gitignore 的 /target/ 覆盖）
    [string]$TargetDir
)

$ErrorActionPreference = "Continue"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

if (-not $TargetDir) { $TargetDir = Join-Path $root 'target\xplat' }

# 工具链路径：与仓库位置无关，仓库搬到哪台机器都可能不同，故允许环境变量覆盖
$env:RUSTUP_HOME = if ($env:RUSTUP_HOME) { $env:RUSTUP_HOME } else { "D:\software\rust\rustup" }
$env:CARGO_HOME  = if ($env:CARGO_HOME)  { $env:CARGO_HOME }  else { "D:\software\rust\cargo" }
$env:PATH = "$env:CARGO_HOME\bin;" + $env:PATH

# 所有已安装的 target（自动发现，不硬编码）
$installed = rustup target list --installed 2>&1
if ($LASTEXITCODE -ne 0) {
    Write-Host "rustup 不可用，跳过跨平台检查"
    exit 0
}

Write-Host "=== 跨平台编译回归 ==="
Write-Host ""
Write-Host "已安装 target："
$installed | ForEach-Object { Write-Host "  $_" }
Write-Host ""
Write-Host "隔离产物目录：$TargetDir"
Write-Host ""

function Get-DirSizeMB($Path) {
    if (-not (Test-Path -LiteralPath $Path)) { return 0 }
    $s = (Get-ChildItem $Path -Recurse -File -Force -ErrorAction SilentlyContinue |
          Measure-Object Length -Sum).Sum
    if (-not $s) { return 0 }
    return [math]::Round($s / 1MB, 1)
}

# 开跑前先清掉上次残留，避免旧产物掩盖「这次真的编过了」
if (Test-Path -LiteralPath $TargetDir) {
    $stale = Get-DirSizeMB $TargetDir
    Write-Host ("清理上次残留：{0:N1} MB" -f $stale)
    Remove-Item -LiteralPath $TargetDir -Recurse -Force -ErrorAction SilentlyContinue
    Write-Host ""
}

$fail = 0
$ok = 0
$sw = [System.Diagnostics.Stopwatch]::StartNew()

foreach ($t in $installed) {
    if ([string]::IsNullOrWhiteSpace($t)) { continue }
    $t = $t.Trim()

    # --target-dir 把产物导到隔离目录；cargo 会在其下按 triple 自动分子目录
    $out = cargo check --workspace --all-targets --target $t --target-dir $TargetDir 2>&1
    $errs = @($out | Select-String -Pattern "^error").Count

    if ($errs -eq 0) {
        Write-Host "  OK   $t"
        $ok++
    } else {
        Write-Host "  FAIL $t  ($errs errors)"
        # 只打前 5 条错误，避免刷屏
        $out | Select-String -Pattern "^error" | Select-Object -First 5 | ForEach-Object {
            Write-Host "         $($_.Line)"
        }
        $fail++
    }
}

$sw.Stop()
$sizeMB = Get-DirSizeMB $TargetDir

Write-Host ""
Write-Host ("=== 结果: {0} 通过 / {1} 失败   耗时 {2:N1}s   产物 {3:N0} MB ===" -f $ok, $fail, $sw.Elapsed.TotalSeconds, $sizeMB)

# 默认清理：交叉产物对主构建毫无价值，留着只会让 target/ 无限膨胀
if ($Keep) {
    Write-Host ("-Keep 生效：保留 {0}" -f $TargetDir) -ForegroundColor DarkYellow
    Write-Host "  看完记得删：Remove-Item target\xplat -Recurse -Force" -ForegroundColor DarkYellow
} else {
    Remove-Item -LiteralPath $TargetDir -Recurse -Force -ErrorAction SilentlyContinue
    if (Test-Path -LiteralPath $TargetDir) {
        Write-Host ("清理失败，请手动删除 " + $TargetDir) -ForegroundColor Red
    } else {
        Write-Host ("已清理 {0:N0} MB 交叉产物" -f $sizeMB) -ForegroundColor Green
    }
}

if ($fail -gt 0) { exit 1 }
