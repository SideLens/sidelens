# SideLens 虚拟摄像头验证脚本
#
# 必须在**管理员**终端运行 —— 注册虚拟摄像头需要写系统设备树。
#
# 用法：
#   Windows 终端(管理员)
#   cd <仓库根>   # 脚本会自动 Set-Location，这里只是示意
#   .\tools\verify-vcam.ps1

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

# 提权运行时输出窗口会一闪而过，所以全部写日志
$LogFile = Join-Path $root 'target\vcam-verify.log'
Remove-Item $LogFile -ErrorAction SilentlyContinue
function Log($msg) {
    Write-Host $msg
    Add-Content -Path $LogFile -Value $msg -Encoding UTF8
}
"SideLens 虚拟摄像头验证 - $(Get-Date -Format 'yyyy-MM-dd HH:mm:ss')" | Set-Content -Path $LogFile -Encoding UTF8

# 检查管理员
$id = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = New-Object Security.Principal.WindowsPrincipal($id)
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    Log ""
    Log "❌ 需要管理员权限"
    Log ""
    Log "注册虚拟摄像头要写系统设备接口属性，普通用户会被拒绝 (0x80070005)。"
    Log "请以管理员身份打开 Windows 终端，再运行本脚本。"
    Log ""
    exit 1
}

Log "✅ 管理员权限确认"

# 环境
$env:RUSTUP_HOME = 'D:\software\rust\rustup'
$env:CARGO_HOME = 'D:\software\rust\cargo'
$env:Path = "$env:CARGO_HOME\bin;" + $env:Path

# 构建
Log ""
Log "构建中..."
cargo build --release 2>&1 | Select-Object -Last 3

# 注册 DLL
$clsid = '{7A3E1C42-5D88-4F6B-9E20-1B4D8C3A9F57}'
$dll = Join-Path $root 'target\release\sidelens_vcam_dll.dll'
if (-not (Test-Path $dll)) {
    Log "❌ DLL 不存在：$dll"
    exit 1
}

$key = "HKCU:\Software\Classes\CLSID\$clsid\InprocServer32"
New-Item -Path $key -Force | Out-Null
Set-ItemProperty -Path $key -Name '(default)' -Value $dll
Set-ItemProperty -Path $key -Name 'ThreadingModel' -Value 'Both'
Log "✅ DLL 已注册：$dll"

# 清理旧日志
Remove-Item "$env:TEMP\sidelens-vcam.log" -ErrorAction SilentlyContinue

# 跑验证
Log ""
Log "启动虚拟摄像头（保持 20 秒）..."
Log "请在 20 秒内打开「相机」应用，检查设备列表里是否出现 SideLens Camera"
Log ""
& "$root\target\release\sidelensd.exe" --vcam-probe --hold 20 --remove

# 检查结果
Log ""
Log "=== 设备列表 ==="
$cams = Get-PnpDevice -Class Camera -ErrorAction SilentlyContinue
if ($cams) {
    $cams | ForEach-Object { "$($_.Status)  $($_.FriendlyName)" }
} else {
    Log "（Camera 类为空）"
}

Log ""
Log "=== DLL 内部日志 ==="
if (Test-Path "$env:TEMP\sidelens-vcam.log") {
    Get-Content "$env:TEMP\sidelens-vcam.log"
} else {
    Log "（无日志）"
}

# 清理注册
Log ""
Remove-Item -Path "HKCU:\Software\Classes\CLSID\$clsid" -Recurse -Force -ErrorAction SilentlyContinue
Log "✅ 注册表已清理"