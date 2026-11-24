#Requires -Version 7.0
<#
.SYNOPSIS
    清理 target/ 里的冗余产物：重复的 build 实例 + 交叉编译残留 + bench 日志。

.DESCRIPTION
    cargo 自己**不会**清理这些，它们会让 target/ 无上限膨胀（本仓库实测到过 23 GB）。
    三条来源：

    1. **build 目录的重复实例**

       target/<profile>/build/<包名>/<hash>/ 里的 hash 由「包版本 + 编译选项 +
       feature 组合 + rustc 版本」算出。任何一项变了就建新目录，旧目录不删。

       实测（仅本机两个 profile）：

           debug    总 592 MB，其中 302 MB 是旧实例（257 个）
           release  总 512 MB，其中 105 MB 是旧实例（42 个）

       典型：windows 在 debug 下同时存在 3 份（67.7 / 68.8 / 69.2 MB），
       syn 有 4 份，proc-macro2 有 6 份。

    2. **交叉编译产物**

       cargo check --target <非本机> 会在 target/<triple>/ 下建一整套依赖树，
       主机构建永远用不到。verify-cross-platform.ps1 已改为导到 target/xplat
       并跑完即删，这里再兜底清一次历史残留。

    3. **bench 日志**

       run-matrix.ps1 每组配置写 4 个日志到 target/bench/，14 组 = 56 个文件。

.PARAMETER Profile
    只处理某个 profile（debug / release）。默认两个都处理。

.PARAMETER All
    整个删除 target/（等价 cargo clean），最彻底。会强制全量重编。

.PARAMETER DryRun
    只报告会删什么、能回收多少，不真删。

.PARAMETER KeepCross
    保留交叉编译产物目录（默认会删）。

.EXAMPLE
    pwsh -File tools/clean-target.ps1 -DryRun     # 先看能回收多少
    pwsh -File tools/clean-target.ps1             # 真删
    pwsh -File tools/clean-target.ps1 -All        # 全清（cargo clean）
#>
[CmdletBinding()]
param(
    [ValidateSet('debug', 'release')]
    [string[]]$Profile = @('debug', 'release'),
    [switch]$All,
    [switch]$DryRun,
    [switch]$KeepCross
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$targetDir = Join-Path $root 'target'

function Get-SizeMB($Path) {
    if (-not (Test-Path -LiteralPath $Path)) { return 0 }
    $s = (Get-ChildItem $Path -Recurse -File -Force -ErrorAction SilentlyContinue |
          Measure-Object Length -Sum).Sum
    if (-not $s) { return 0 }
    return $s / 1MB
}

if (-not (Test-Path -LiteralPath $targetDir)) {
    Write-Host 'target/ 不存在，无需清理'
    exit 0
}

$beforeMB = Get-SizeMB $targetDir
Write-Host ('=== target 清理 ===  当前 {0:N0} MB' -f $beforeMB)
if ($DryRun) { Write-Host '（DryRun：只报告，不删除）' -ForegroundColor DarkYellow }
Write-Host ''

# -- -All：直接清空 target/ ----------------------------------------------
if ($All) {
    if ($DryRun) {
        Write-Host ('将删除整个 target/，回收 {0:N0} MB' -f $beforeMB)
        exit 0
    }
    Write-Host '删除整个 target/ ...' -ForegroundColor Yellow
    Remove-Item -LiteralPath $targetDir -Recurse -Force
    Write-Host '完成。下次构建会全量重编。' -ForegroundColor Green
    exit 0
}

$reclaimedMB = 0
$removedCount = 0

# -- 1. build 目录里的重复实例 ------------------------------------------
#
# 判定规则：同一个包名下如果有多份 hash 目录，只保留**最近写入**的那份。
#
# 为什么保留最新的而不是最大的：cargo 用 fingerprint 判断复用，最新那份
# 才对应当前的 feature 组合与 rustc 版本；旧的那份即使更大也不会再被命中。
# 删错最多导致一次重编，不会让构建失败。

foreach ($prof in $Profile) {
    $buildDir = Join-Path $targetDir "$prof\build"
    if (-not (Test-Path -LiteralPath $buildDir)) { continue }

    Write-Host ("--- {0}/build ---" -f $prof)

    foreach ($pkg in Get-ChildItem $buildDir -Directory -Force) {
        $subs = @(Get-ChildItem $pkg.FullName -Directory -Force -ErrorAction SilentlyContinue)
        if ($subs.Count -le 1) { continue }

        $entries = $subs | ForEach-Object {
            [pscustomobject]@{
                Path   = $_.FullName
                Name   = $_.Name
                SizeMB = Get-SizeMB $_.FullName
                Time   = $_.LastWriteTime
            }
        } | Sort-Object Time -Descending

        $keep = $entries[0]
        $drop = @($entries | Select-Object -Skip 1)
        $dropMB = ($drop | Measure-Object SizeMB -Sum).Sum
        $reclaimedMB += $dropMB
        $removedCount += $drop.Count

        Write-Host ('  {0,-26} {1} 份 -> 保留 {2}，删 {3} 份，回收 {4:N1} MB' -f `
            $pkg.Name, $subs.Count, $keep.Name, $drop.Count, $dropMB)

        if (-not $DryRun) {
            foreach ($d in $drop) {
                Remove-Item -LiteralPath $d.Path -Recurse -Force -ErrorAction SilentlyContinue
            }
        }
    }
}

# -- 2. 交叉编译产物 ----------------------------------------------------

if (-not $KeepCross) {
    Write-Host ''
    Write-Host '--- 交叉编译产物 ---'
    $crossFound = $false

    $xplat = Join-Path $targetDir 'xplat'
    if (Test-Path -LiteralPath $xplat) {
        $mb = Get-SizeMB $xplat
        $reclaimedMB += $mb
        $crossFound = $true
        Write-Host ('  target/xplat  {0:N0} MB' -f $mb)
        if (-not $DryRun) { Remove-Item -LiteralPath $xplat -Recurse -Force -ErrorAction SilentlyContinue }
    }

    # 历史残留：target/<triple>/ 形式（脚本改隔离目录之前产生的）
    $hostTriple = 'x86_64-pc-windows-msvc'
    foreach ($d in Get-ChildItem $targetDir -Directory -Force) {
        if ($d.Name -match '^(x86_64|aarch64|i686|armv7|wasm32)' -and $d.Name -ne $hostTriple) {
            $mb = Get-SizeMB $d.FullName
            $reclaimedMB += $mb
            $crossFound = $true
            Write-Host ('  {0}  {1:N0} MB（历史残留）' -f $d.Name, $mb)
            if (-not $DryRun) { Remove-Item -LiteralPath $d.FullName -Recurse -Force -ErrorAction SilentlyContinue }
        }
    }

    if (-not $crossFound) { Write-Host '  无交叉产物' }
}

# -- 3. bench 日志 ------------------------------------------------------

$bench = Join-Path $targetDir 'bench'
if (Test-Path -LiteralPath $bench) {
    Write-Host ''
    Write-Host '--- bench 日志 ---'
    $mb = Get-SizeMB $bench
    $n = @(Get-ChildItem $bench -File -Force -ErrorAction SilentlyContinue).Count
    $reclaimedMB += $mb
    Write-Host ('  target/bench  {0} 个文件, {1:N1} MB' -f $n, $mb)
    if (-not $DryRun) { Remove-Item -LiteralPath $bench -Recurse -Force -ErrorAction SilentlyContinue }
}

# -- 汇总 ---------------------------------------------------------------

$afterMB = if ($DryRun) { $beforeMB } else { Get-SizeMB $targetDir }
$verb = if ($DryRun) { '可回收' } else { '完成' }
Write-Host ''
Write-Host ('=== {0}：回收 {1:N0} MB（{2} 个旧实例）  {3:N0} MB -> {4:N0} MB ===' -f `
    $verb, $reclaimedMB, $removedCount, $beforeMB, $afterMB) -ForegroundColor Green
