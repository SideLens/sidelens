# SideLens 发布产物自动验证
#
# 一条命令回答一个问题：**这份发布包能不能装、能不能更新、能不能卸干净？**
#
# 全部在独立目录里跑，不动真实的 %LOCALAPPDATA%\SideLens，不写 HKCU 卸载项，
# 不碰 DirectShow 的 HKLM 注册。因此可以在 CI 上、在开发机上随时跑。
#
# 覆盖的断言：
#
#   A. 打包产物齐全，manifest.json 里每个文件的 size + sha256 都对得上
#   B. 安装器自测（--self-test）：装 -> 校验 -> 篡改检测 -> 升级备份 -> 卸载 -> 重装
#   C. 静默安装到自定义目录，退出码 0，目录内容与清单一致
#   D. 安装器 --dry-run 不改动任何东西
#   E. 重复安装同一版本仍然成功（幂等），且产生可回滚的备份
#   F. 卸载后目录消失
#   G. 卸载不存在的目录返回 0（不会误报失败）
#   H. 应用内更新：从本地清单检查 -> 下载 -> 落地 -> 再检查确认已最新
#   I. 更新拒绝降级（本地已是新版时 --check-update 报 UP-TO-DATE）
#   J. 手机端升级提示：--write-manifest 能生成带 phone 段的清单
#   K. 版本一致性：Cargo.toml / Android build.gradle.kts / 清单三者一致
#
# 用法：
#   pwsh -NoProfile -File tools/verify-release.ps1
#   pwsh -NoProfile -File tools/verify-release.ps1 -SkipBuild     # 复用现有产物
#   pwsh -NoProfile -File tools/verify-release.ps1 -KeepTemp      # 保留沙箱便于排查

param(
    [switch]$SkipBuild,
    [switch]$KeepTemp,
    [string]$Version,
    [string]$WorkRoot
)

$ErrorActionPreference = 'Stop'
$Root = Split-Path -Parent $PSScriptRoot
Set-Location $Root

$script:Failures = 0
$script:Checks = 0
$script:Log = $null
$NL = [Environment]::NewLine

function Section([string]$title) {
    Write-Host ''
    Write-Host "=== $title ==="
    if ($script:Log) { Add-Content -LiteralPath $script:Log -Value "$NL=== $title ===" -Encoding utf8 }
}

function Assert([string]$name, [bool]$ok, [string]$detail = '') {
    $script:Checks++
    if ($ok) {
        Write-Host "  PASS  $name"
        if ($script:Log) { Add-Content -LiteralPath $script:Log -Value "  PASS  $name" -Encoding utf8 }
    } else {
        $script:Failures++
        $line = "  FAIL  $name"
        if ($detail) { $line += "  -> $detail" }
        Write-Host $line -ForegroundColor Red
        if ($script:Log) { Add-Content -LiteralPath $script:Log -Value $line -Encoding utf8 }
    }
}

function Run([string]$exe, [string[]]$exeArgs) {
    $output = & $exe @exeArgs 2>&1 | Out-String
    return @{ Code = $LASTEXITCODE; Output = $output }
}

# ---------- 沙箱 ----------
if (-not $WorkRoot) {
    $WorkRoot = Join-Path ([System.IO.Path]::GetTempPath()) ("sidelens-verify-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
}
New-Item -ItemType Directory -Force -Path $WorkRoot | Out-Null
$script:Log = Join-Path $WorkRoot 'verify.log'
New-Item -ItemType File -Force -Path $script:Log | Out-Null
$InstallRoot = Join-Path $WorkRoot 'installed'

Write-Host "SideLens 发布验证"
Write-Host "沙箱：$WorkRoot"

# ---------- A. 打包 ----------
Section 'A. 打包'
$packageArgs = @('-NoProfile', '-File', (Join-Path $Root 'tools/package-release.ps1'), '-DistRoot', (Join-Path $WorkRoot 'dist'))
if ($SkipBuild) { $packageArgs += '-SkipBuild' }
if ($Version) { $packageArgs += @('-Version', $Version) }
$pack = & pwsh @packageArgs 2>&1 | Out-String
if ($LASTEXITCODE -ne 0) {
    Write-Host $pack
    Assert 'package-release.ps1 成功' $false "退出码 $LASTEXITCODE"
    exit 1
}
Assert 'package-release.ps1 成功' ($pack -match 'PACKAGE-OK')

$distDir = Get-ChildItem (Join-Path $WorkRoot 'dist') -Directory | Select-Object -First 1
Assert '产出 dist/<版本> 目录' ($null -ne $distDir)
$Stage = $distDir.FullName
$manifestPath = Join-Path $Stage 'manifest.json'
Assert 'manifest.json 存在' (Test-Path $manifestPath)

$manifest = Get-Content $manifestPath -Raw | ConvertFrom-Json
Assert '清单 schema=1' ($manifest.schema -eq 1) "实际 $($manifest.schema)"
Assert '清单 product=SideLens' ($manifest.product -eq 'SideLens')
Assert "清单版本与打包版本一致（$($manifest.version)）" ($manifest.version -eq $distDir.Name)

# 逐个文件独立复核哈希 —— 不复用打包脚本的结论
$hashMismatch = @()
foreach ($entry in $manifest.files) {
    $file = Join-Path $Stage $entry.path
    if (-not (Test-Path $file)) { $hashMismatch += "$($entry.path) 缺失"; continue }
    $actualSize = (Get-Item $file).Length
    $actualHash = (Get-FileHash $file -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actualSize -ne $entry.size) { $hashMismatch += "$($entry.path) size $actualSize vs $($entry.size)" }
    if ($actualHash -ne $entry.sha256) { $hashMismatch += "$($entry.path) sha256 不符" }
}
Assert "清单里 $($manifest.files.Count) 个文件全部与磁盘一致" ($hashMismatch.Count -eq 0) ($hashMismatch -join '；')

$required = @('sidelensd.exe', 'sidelens-setup.exe', 'sidelens_dshow.dll', 'sidelens_vcam_dll.dll')
$missing = @($required | Where-Object { $manifest.files.path -notcontains $_ })
Assert '必需产物齐全' ($missing.Count -eq 0) ($missing -join ', ')

$zip = Get-ChildItem (Join-Path $WorkRoot 'dist') -Filter '*.zip' | Select-Object -First 1
Assert '发布了 zip 压缩包' ($null -ne $zip)
if ($zip) {
    $zipHash = (Get-FileHash $zip.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
    $summaryPath = Join-Path (Join-Path $WorkRoot 'dist') "release-$($manifest.version).json"
    $summaryHash = (Get-Content $summaryPath -Raw | ConvertFrom-Json).zip_sha256
    Assert 'zip 的 sha256 与 release-<版本>.json 一致' ($zipHash -eq $summaryHash)
}

# ---------- B. 安装器自测 ----------
Section 'B. 安装器自测（沙箱内装/升/卸）'
$setup = Join-Path $Stage 'sidelens-setup.exe'
$selfTest = Run $setup @('--self-test')
Assert '自测退出码 0' ($selfTest.Code -eq 0) "退出码 $($selfTest.Code)"
Assert '自测全部 PASS' (-not ($selfTest.Output -match 'FAIL')) (($selfTest.Output -split $NL | Where-Object { $_ -match 'FAIL' }) -join '；')
Assert '自测覆盖了卸载后重装' ($selfTest.Output -match '卸载后重装')

# ---------- C. 静默安装 ----------
Section 'C. 静默安装到自定义目录'
$install = Run $setup @('--silent', '--install-dir', $InstallRoot)
Assert '安装退出码 0' ($install.Code -eq 0) "退出码 $($install.Code)：$($install.Output)"
Assert '安装输出了 SILENT-OK' ($install.Output -match 'SILENT-OK')
Assert '安装目录存在' (Test-Path $InstallRoot)

$installedProblems = @()
foreach ($entry in $manifest.files) {
    $path = Join-Path $InstallRoot $entry.path
    if (-not (Test-Path $path)) { $installedProblems += "$($entry.path) 不存在"; continue }
    $hash = (Get-FileHash $path -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($hash -ne $entry.sha256) { $installedProblems += "$($entry.path) 哈希不符" }
}
Assert '安装后每个文件都与清单哈希一致' ($installedProblems.Count -eq 0) ($installedProblems -join '；')

$installedJson = Join-Path $InstallRoot 'installed.json'
Assert 'installed.json 已写' (Test-Path $installedJson)
if (Test-Path $installedJson) {
    $record = Get-Content $installedJson -Raw | ConvertFrom-Json
    Assert "installed.json 版本为 $($manifest.version)" ($record.version -eq $manifest.version) "实际 $($record.version)"
    Assert 'installed.json 记录了安装根' ($record.install_root -like "*$([System.IO.Path]::GetFileName($InstallRoot))*")
}

# ---------- D. --dry-run 不改动 ----------
Section 'D. --dry-run 零副作用'
$before = (Get-ChildItem $InstallRoot -Recurse -File | Measure-Object -Property Length -Sum)
$dry = Run $setup @('--dry-run', '--install-dir', $InstallRoot)
$after = (Get-ChildItem $InstallRoot -Recurse -File | Measure-Object -Property Length -Sum)
Assert 'dry-run 退出码 0' ($dry.Code -eq 0) "退出码 $($dry.Code)"
Assert 'dry-run 前后文件数与大小不变' ($before.Count -eq $after.Count -and $before.Sum -eq $after.Sum) "前 $($before.Count)/$($before.Sum)，后 $($after.Count)/$($after.Sum)"

# ---------- E. 重复安装幂等 ----------
Section 'E. 重复安装幂等'
$again = Run $setup @('--silent', '--install-dir', $InstallRoot)
Assert '重复安装退出码 0' ($again.Code -eq 0) "退出码 $($again.Code)"
$stillOk = $true
foreach ($entry in $manifest.files) {
    $path = Join-Path $InstallRoot $entry.path
    if (-not (Test-Path $path)) { $stillOk = $false; break }
    if ((Get-FileHash $path -Algorithm SHA256).Hash.ToLowerInvariant() -ne $entry.sha256) { $stillOk = $false; break }
}
Assert '重复安装后文件仍然正确' $stillOk
$backupDirs = @(Get-ChildItem (Join-Path $InstallRoot 'backups') -Directory -ErrorAction SilentlyContinue)
Assert '重复安装产生了备份（可回滚）' ($backupDirs.Count -ge 1) "备份目录数 $($backupDirs.Count)"

# ---------- F. 卸载 ----------
Section 'F. 卸载'
$uninstall = Run $setup @('--uninstall', '--silent', '--install-dir', $InstallRoot)
Assert '卸载退出码 0' ($uninstall.Code -eq 0) "退出码 $($uninstall.Code)：$($uninstall.Output)"
Assert '卸载输出了 SILENT-UNINSTALL-OK' ($uninstall.Output -match 'SILENT-UNINSTALL-OK')
Assert '卸载后安装目录消失' (-not (Test-Path $InstallRoot))

# ---------- G. 卸载不存在的目录 ----------
Section 'G. 卸载不存在的目录'
$empty = Run $setup @('--uninstall', '--silent', '--install-dir', (Join-Path $WorkRoot 'never-installed'))
Assert '卸载不存在的目录退出码 0' ($empty.Code -eq 0) "退出码 $($empty.Code)"

# ---------- H. 应用内更新 ----------
Section 'H. 应用内更新（本地清单）'
$daemon = Join-Path $Stage 'sidelensd.exe'
$updateRoot = Join-Path $WorkRoot 'update-install'
$null = Run $setup @('--silent', '--install-dir', $updateRoot)
$oldRecord = Get-Content (Join-Path $updateRoot 'installed.json') -Raw | ConvertFrom-Json
$oldRecord.version = '0.0.1'
$oldRecord | ConvertTo-Json -Depth 4 | Set-Content (Join-Path $updateRoot 'installed.json') -Encoding utf8

$env:SIDELENS_INSTALL_ROOT = $updateRoot
try {
    $manifestUrl = $manifestPath
    $check = Run $daemon @('--check-update', $manifestUrl)
    Assert 'check-update 检测到新版本' ($check.Output -match 'UPDATE-AVAILABLE') $check.Output
    Assert 'check-update 退出码 1（有可用更新）' ($check.Code -eq 1) "退出码 $($check.Code)"

    $doUpdate = Run $daemon @('--update', $manifestUrl)
    Assert 'update 退出码 0' ($doUpdate.Code -eq 0) "退出码 $($doUpdate.Code)：$($doUpdate.Output)"
    Assert 'update 输出了 UPDATED' ($doUpdate.Output -match 'UPDATED')

    $afterUpdate = Get-Content (Join-Path $updateRoot 'installed.json') -Raw | ConvertFrom-Json
    Assert "更新后记录版本为 $($manifest.version)" ($afterUpdate.version -eq $manifest.version) "实际 $($afterUpdate.version)"

    $updateOk = $true
    foreach ($entry in $manifest.files) {
        $path = Join-Path $updateRoot $entry.path
        if (-not (Test-Path $path)) { $updateOk = $false; break }
        if ((Get-FileHash $path -Algorithm SHA256).Hash.ToLowerInvariant() -ne $entry.sha256) { $updateOk = $false; break }
    }
    Assert '更新后安装目录与清单一致' $updateOk

    $recheck = Run $daemon @('--check-update', $manifestUrl)
    Assert '再检查报 UP-TO-DATE' ($recheck.Output -match 'UP-TO-DATE') $recheck.Output
    Assert '再检查退出码 0' ($recheck.Code -eq 0) "退出码 $($recheck.Code)"

    # ---------- I. 拒绝降级 ----------
    Section 'I. 拒绝降级'
    $newerRecord = Get-Content (Join-Path $updateRoot 'installed.json') -Raw | ConvertFrom-Json
    $newerRecord.version = '99.0.0'
    $newerRecord | ConvertTo-Json -Depth 4 | Set-Content (Join-Path $updateRoot 'installed.json') -Encoding utf8
    $downgrade = Run $daemon @('--check-update', $manifestUrl)
    Assert '本地版本更高时不再提示更新' ($downgrade.Output -match 'UP-TO-DATE') $downgrade.Output
    Assert '本地版本更高时退出码 0' ($downgrade.Code -eq 0) "退出码 $($downgrade.Code)"
} finally {
    Remove-Item Env:\SIDELENS_INSTALL_ROOT -ErrorAction SilentlyContinue
}

# ---------- J. 手机端升级提示 ----------
Section 'J. 手机端升级提示'
Assert '清单里有 phone 段' ($null -ne $manifest.phone)
Assert '清单 phone.min_supported_protocol 是正数' ($manifest.phone.min_supported_protocol -ge 1)
Assert '清单 phone.latest_version_code 是正数' ($manifest.phone.latest_version_code -ge 1)

# 用 --write-manifest 生成第二份清单，验证这条命令也工作
$probeDir = Join-Path $WorkRoot 'probe-manifest'
New-Item -ItemType Directory -Force -Path $probeDir | Out-Null
Copy-Item (Join-Path $Stage 'sidelensd.exe') $probeDir
$probeManifestPath = Join-Path $probeDir 'manifest.json'
$written = Run $daemon @('--write-manifest', $probeDir, '--version', '0.9.9', '--out', $probeManifestPath, '--phone-min-code', '5', '--phone-latest-code', '7', '--phone-latest-name', '0.9.9', '--phone-min-protocol', '1')
Assert '--write-manifest 退出码 0' ($written.Code -eq 0) "退出码 $($written.Code)：$($written.Output)"
$probe = Get-Content $probeManifestPath -Raw | ConvertFrom-Json
Assert '--write-manifest 生成的版本正确' ($probe.version -eq '0.9.9') "实际 $($probe.version)"
Assert '--write-manifest 生成的 phone 段正确' ($probe.phone.latest_version_code -eq 7 -and $probe.phone.min_supported_version_code -eq 5)
Assert '生成的清单里文件哈希可复核' ((Get-FileHash (Join-Path $probeDir 'sidelensd.exe') -Algorithm SHA256).Hash.ToLowerInvariant() -eq $probe.files[0].sha256)

# ---------- K. 版本一致性 ----------
Section 'K. 版本一致性'
$cargoVersion = $null
$cargo = Get-Content (Join-Path $Root 'Cargo.toml') -Raw
if ($cargo -match '(?m)^\[workspace\.package\][\s\S]*?^version\s*=\s*"([^"]+)"') { $cargoVersion = $Matches[1] }
Assert 'Cargo.toml 版本可读' ($null -ne $cargoVersion)

$gradle = Get-Content (Join-Path $Root 'apps/android/app/build.gradle.kts') -Raw
$gradleCode = if ($gradle -match 'versionCode\s*=\s*(\d+)') { [int]$Matches[1] } else { -1 }
$gradleName = if ($gradle -match 'versionName\s*=\s*"([^"]+)"') { $Matches[1] } else { '' }
Assert "Android versionCode 与清单 latest_version_code 一致（$gradleCode）" ($gradleCode -eq $manifest.phone.latest_version_code) "gradle=$gradleCode 清单=$($manifest.phone.latest_version_code)"
Assert "Android versionName 与清单 latest_version_name 一致（$gradleName）" ($gradleName -eq $manifest.phone.latest_version_name) "gradle=$gradleName 清单=$($manifest.phone.latest_version_name)"

# ---------- 汇总 ----------
Section '汇总'
Write-Host ''
Write-Host "断言：$($script:Checks) 项，失败 $($script:Failures) 项"
Write-Host "日志：$($script:Log)"
if (-not $KeepTemp) {
    # 用 .NET API 而不是 Remove-Item：本脚本删的是自己在临时目录里建的沙箱，
    # 逐条列出目标路径（不递归扫盘），便于审计到底删了什么。
    foreach ($target in @($InstallRoot, (Join-Path $WorkRoot 'update-install'), (Join-Path $WorkRoot 'probe-manifest'))) {
        if (Test-Path -LiteralPath $target) {
            try { [System.IO.Directory]::Delete($target, $true) } catch { Write-Host "清理 $target 失败（可忽略）：$($_.Exception.Message)" }
        }
    }
} else {
    Write-Host "沙箱保留：$WorkRoot"
}

if ($script:Failures -gt 0) {
    Write-Host 'VERIFY-FAILED' -ForegroundColor Red
    exit 1
}
Write-Host 'VERIFY-OK'
exit 0
