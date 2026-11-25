# SideLens 发布打包
#
# 产出 dist/<版本>/ 下一个可直接分发的目录：
#
#   sidelensd.exe            常驻服务
#   sidelens-setup.exe       安装/卸载程序
#   sidelens_dshow.dll       DirectShow 虚拟摄像头滤镜
#   sidelens_vcam_dll.dll    MFCreateVirtualCamera 媒体源（历史路线，保留）
#   manifest.json            机器可读清单（每个文件的 size + sha256）
#   sidelens-<版本>-win64.zip  整个目录的压缩包
#
# # 为什么是一个目录而不是一个安装包
#
# 安装器要能"把发布包目录里的文件装到安装根"，这样：
#   - 解压即用的绿色版直接就是那个目录；
#   - 安装器自测（--self-test）可以拿任何一份目录当夹具；
#   - 应用内更新（--update）下载的也是同样的文件集合。
# 引入 NSIS/MSI 会让"可自动验证"变成"需要装一个打包工具才能验证"。
#
# 用法：
#   pwsh -NoProfile -File tools/package-release.ps1
#   pwsh -NoProfile -File tools/package-release.ps1 -SkipBuild      # 复用已有产物
#   pwsh -NoProfile -File tools/package-release.ps1 -Version 0.2.0

param(
    [string]$Version,
    [string]$Channel = 'stable',
    [string]$DistRoot,
    [switch]$SkipBuild,
    [switch]$Debug,
    [int]$PhoneMinCode = 2,
    [int]$PhoneLatestCode = 2,
    [string]$PhoneLatestName = '0.2.0',
    [int]$PhoneMinProtocol = 1,
    [string]$Notes = ''
)

$ErrorActionPreference = 'Stop'
# 仓库根由脚本自身位置推导（tools/ 的父目录），挪目录不用改脚本
$Root = Split-Path -Parent $PSScriptRoot
Set-Location $Root

if (-not $Version) {
    # 版本单一来源：workspace.package.version
    $cargo = Get-Content (Join-Path $Root 'Cargo.toml') -Raw
    if ($cargo -match '(?m)^\[workspace\.package\][\s\S]*?^version\s*=\s*"([^"]+)"') {
        $Version = $Matches[1]
    } else {
        throw '无法从 Cargo.toml 读取 workspace.package.version'
    }
}
if (-not $DistRoot) { $DistRoot = Join-Path $Root 'dist' }

$Profile = if ($Debug) { 'debug' } else { 'release' }
$Stage = Join-Path $DistRoot $Version
$Log = Join-Path $DistRoot "package-$Version.log"

New-Item -ItemType Directory -Force -Path $DistRoot | Out-Null
if (Test-Path $Stage) { Remove-Item $Stage -Recurse -Force }
New-Item -ItemType Directory -Force -Path $Stage | Out-Null

function Log([string]$msg) {
    Write-Host $msg
    Add-Content -LiteralPath $Log -Value $msg -Encoding utf8
}

Log "SideLens 发布打包 v$Version（通道 $Channel，profile $Profile）"
Log "输出目录：$Stage"

# ---------- 1. 构建 ----------
if (-not $SkipBuild) {
    $buildArgs = @('build', '--workspace', '--bins')
    if (-not $Debug) { $buildArgs += '--release' }
    Log "构建：cargo $($buildArgs -join ' ')"
    & cargo @buildArgs
    if ($LASTEXITCODE -ne 0) { throw "cargo build 失败（退出码 $LASTEXITCODE）" }
} else {
    Log '跳过构建（-SkipBuild）'
}

# ---------- 2. 收集产物 ----------
# 每个条目都必须在，缺任何一个都直接失败 —— 发布包缺 DLL 的后果是
# 用户装了之后虚拟摄像头里没有 SideLens Camera，而安装本身"成功"了。
$Artifacts = @(
    @{ Name = 'sidelensd.exe';    Crate = 'sidelensd' },
    @{ Name = 'sidelens-setup.exe'; Crate = 'sidelens-setup' },
    @{ Name = 'sidelens_dshow.dll'; Crate = 'sidelens-dshow' },
    @{ Name = 'sidelens_vcam_dll.dll'; Crate = 'sidelens-vcam-dll' }
)

$BinDir = Join-Path $Root "target\$Profile"
$Manifest = [ordered]@{
    schema = 1
    product = 'SideLens'
    channel = $Channel
    version = $Version
    released = (Get-Date).ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ')
    min_upgradable_version = '0.0.0'
    files = @()
    phone = [ordered]@{
        min_supported_version_code = $PhoneMinCode
        latest_version_code = $PhoneLatestCode
        latest_version_name = $PhoneLatestName
        min_supported_protocol = $PhoneMinProtocol
    }
    notes = $Notes
}

foreach ($item in $Artifacts) {
    $source = Join-Path $BinDir $item.Name
    if (-not (Test-Path $source)) {
        throw "缺少产物 $source。先跑 cargo build --release（或去掉 -SkipBuild）"
    }
    $destination = Join-Path $Stage $item.Name
    Copy-Item $source $destination -Force
    $hash = (Get-FileHash -LiteralPath $destination -Algorithm SHA256).Hash.ToLowerInvariant()
    $size = (Get-Item -LiteralPath $destination).Length
    $Manifest.files += [ordered]@{ path = $item.Name; size = $size; sha256 = $hash }
    Log ("  {0,-26} {1,10} 字节  sha256={2}" -f $item.Name, $size, $hash.Substring(0, 16) + '…')
}

# ---------- 3. 写清单 ----------
$manifestPath = Join-Path $Stage 'manifest.json'
$Manifest | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath $manifestPath -Encoding utf8
Log "清单已写：$manifestPath"

# ---------- 4. 压缩包 ----------
# 用 .NET 自带 ZipFile，避免依赖 7z / Compress-Archive 的目录层级差异
$zipPath = Join-Path $DistRoot "sidelens-$Version-win64.zip"
if (Test-Path $zipPath) { Remove-Item $zipPath -Force }
Add-Type -AssemblyName System.IO.Compression.FileSystem
[System.IO.Compression.ZipFile]::CreateFromDirectory($Stage, $zipPath)
$zipHash = (Get-FileHash -LiteralPath $zipPath -Algorithm SHA256).Hash.ToLowerInvariant()
Log "压缩包：$zipPath（$([math]::Round((Get-Item $zipPath).Length / 1MB, 2)) MB）"
Log "压缩包 sha256：$zipHash"

# ---------- 5. 机器可读的结果 ----------
$summary = [ordered]@{
    version = $Version
    channel = $Channel
    profile = $Profile
    stage = $Stage
    zip = $zipPath
    zip_sha256 = $zipHash
    manifest = $manifestPath
    files = $Manifest.files
}
$summaryPath = Join-Path $DistRoot "release-$Version.json"
$summary | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath $summaryPath -Encoding utf8
Log "结果：$summaryPath"
Log 'PACKAGE-OK'
