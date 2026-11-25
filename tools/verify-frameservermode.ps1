# 虚拟摄像头：EnableFrameServerMode A/B 验证
#
# 严格 A/B：只有 EnableFrameServerMode 不同，其余全部相同。
# 两轮顺序反转（A→B→C 与 C→B→A），排除顺序污染 —— 这是第二十七轮
# 类别 A/B 踩过的坑（Start 失败后设备记录不自动清除）。
#
# 用法（管理员）：
#   pwsh -NoProfile -File tools\verify-frameservermode.ps1
#   pwsh -NoProfile -File tools\verify-frameservermode.ps1 -Restore   # 只回滚

param(
    [string]$Root,
    [int]$HoldSeconds = 8,
    [switch]$Restore,
    [switch]$KeepDevices
)

$ErrorActionPreference = 'Continue'
if (-not $Root) { $Root = Split-Path -Parent $PSScriptRoot }

$LogFile    = Join-Path $Root 'target\vcam-frameservermode.log'
$StagingDir = Join-Path $Root 'target\vcam-dll'
$DllSrc     = Join-Path $Root 'target\release\sidelens_vcam_dll.dll'
$Exe        = Join-Path $Root 'target\release\sidelensd.exe'
$Clsid      = '{7A3E1C42-5D88-4F6B-9E20-1B4D8C3A9F57}'
$ClsidKey   = "HKLM:\SOFTWARE\Classes\CLSID\$Clsid\InprocServer32"
$FsmPaths   = @(
    'HKLM:\SOFTWARE\Microsoft\Windows Media Foundation\Platform',
    'HKLM:\SOFTWARE\WOW6432Node\Microsoft\Windows Media Foundation\Platform'
)
$DllLog = Join-Path $env:TEMP 'sidelens-vcam.log'
$NL = [Environment]::NewLine

function Log([string]$msg) {
    $line = '[{0}] {1}' -f (Get-Date -Format 'HH:mm:ss'), $msg
    Write-Host $line
    Add-Content -LiteralPath $LogFile -Value $line -Encoding utf8
}

function Set-FsmMode([string]$mode) {
    foreach ($p in $FsmPaths) {
        if ($mode -eq 'absent') {
            Remove-ItemProperty -LiteralPath $p -Name 'EnableFrameServerMode' -ErrorAction SilentlyContinue
        } else {
            New-ItemProperty -LiteralPath $p -Name 'EnableFrameServerMode' -Value ([int]$mode) -PropertyType DWord -Force | Out-Null
        }
    }
}

function Get-FsmMode {
    $vals = foreach ($p in $FsmPaths) {
        $v = (Get-Item -LiteralPath $p -ErrorAction SilentlyContinue).GetValue('EnableFrameServerMode')
        if ($null -eq $v) { 'absent' } else { [string]$v }
    }
    ($vals | Select-Object -Unique) -join '|'
}

function Stop-FrameServer {
    Get-Service FrameServer -ErrorAction SilentlyContinue | Stop-Service -Force -ErrorAction SilentlyContinue
    Start-Sleep -Milliseconds 800
}

function Clear-GhostDevices {
    $devs = @(Get-PnpDevice -ErrorAction SilentlyContinue | Where-Object { $_.InstanceId -like 'SWD\VCAMDEVAPI\*' })
    foreach ($d in $devs) { & pnputil /remove-device "$($d.InstanceId)" 2>&1 | Out-Null }
    if ($devs.Count -gt 0) { Start-Sleep -Milliseconds 600 }
    return $devs.Count
}

function Invoke-Trial([string]$name, [string]$mode) {
    Log ""
    $shown = if ($mode -eq 'absent') { '不存在' } else { $mode }
    Log "===== 组 $name ：EnableFrameServerMode = $shown ====="
    Set-FsmMode $mode
    Log "  注册表现值: $(Get-FsmMode)"
    Stop-FrameServer
    $cleared = Clear-GhostDevices
    Log "  清除幽灵设备: $cleared 个"

    Remove-Item -LiteralPath $DllLog -ErrorAction SilentlyContinue

    $sw = [Diagnostics.Stopwatch]::StartNew()
    $out = & $Exe --vcam-probe --hold $HoldSeconds --no-inproc-class *>&1
    $sw.Stop()
    $text = ($out | Out-String)

    $startOk = $text -match 'Start 成功'
    $failLine = ($text -split '\r?\n' | Where-Object { $_ -match '创建失败' } | Select-Object -First 1)
    $errCode = ''
    if ($failLine -and ($failLine -match '0x[0-9A-Fa-f]{8}')) { $errCode = $Matches[0] }

    $dllLoaded = Test-Path -LiteralPath $DllLog
    $dllLines  = if ($dllLoaded) { @(Get-Content -LiteralPath $DllLog).Count } else { 0 }
    $dllTail   = if ($dllLoaded) { (Get-Content -LiteralPath $DllLog -Tail 3) -join ' | ' } else { '(未加载)' }

    $camDevs = @(Get-PnpDevice -Class Camera -ErrorAction SilentlyContinue)
    $swdDevs = @(Get-PnpDevice -ErrorAction SilentlyContinue | Where-Object { $_.InstanceId -like 'SWD\VCAMDEVAPI\*' })
    $fsmClass = Test-Path 'HKLM:\SYSTEM\CurrentControlSet\Control\DeviceClasses\{588C8D20-C0E3-4FD3-B511-8F2F692156F8}'
    $vcClass  = Test-Path 'HKLM:\SYSTEM\CurrentControlSet\Control\DeviceClasses\{E5323777-F976-4F5B-9B55-B94699C46E44}'

    Log ('  Start 成功      : {0}' -f $startOk)
    Log ('  失败错误码      : {0}' -f $(if ($errCode) { $errCode } else { '(无)' }))
    Log ('  DLL 被加载      : {0}（日志 {1} 行）' -f $dllLoaded, $dllLines)
    Log ('  DLL 日志尾      : {0}' -f $dllTail)
    Log ('  摄像头类设备    : {0} 个' -f $camDevs.Count)
    Log ('  VCAMDEVAPI 设备 : {0} 个' -f $swdDevs.Count)
    Log ('  DeviceClasses   : VIDEO_CAMERA={0}  FSM_VIRTUAL_CAMERA={1}' -f $vcClass, $fsmClass)
    Log ('  耗时            : {0:N1} 秒' -f $sw.Elapsed.TotalSeconds)

    if ($text.Length -gt 0) {
        $keep = ($text -split '\r?\n' | Where-Object {
            $_ -match 'Start 成功|创建失败|能力检测|设备类别|CLSID 就绪|类工厂|跳过进程内|COM\(MTA\)|不支持'
        }) -join $NL
        if ($keep) { Add-Content -LiteralPath $LogFile -Value $keep -Encoding utf8 }
    }

    return [pscustomobject]@{
        Trial      = $name
        Mode       = $mode
        StartOk    = $startOk
        ErrCode    = $(if ($errCode) { $errCode } else { '' })
        DllLoaded  = $dllLoaded
        DllLines   = $dllLines
        CameraDevs = $camDevs.Count
        SwdDevs    = $swdDevs.Count
    }
}

# ---------------------------------------------------------------- 开始

Remove-Item -LiteralPath $LogFile -ErrorAction SilentlyContinue
Log '=== EnableFrameServerMode A/B 验证 ==='
Log "身份: $([Security.Principal.WindowsIdentity]::GetCurrent().Name)"
$isAdmin = (New-Object Security.Principal.WindowsPrincipal(
    [Security.Principal.WindowsIdentity]::GetCurrent())
).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
Log "管理员: $isAdmin"
if (-not $isAdmin) { Log '需要管理员权限，退出'; exit 1 }

$build = Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion'
Log "系统: $($build.DisplayVersion) build $($build.CurrentBuild).$($build.UBR)"

if ($Restore) {
    Set-FsmMode 'absent'
    Log "已回滚 EnableFrameServerMode（现值: $(Get-FsmMode)）"
    exit 0
}

# 阶段 0：DLL 放到稳定目录并修正注册表指向
# （原注册表指向 D:\dsh-workspace\... ，仓库迁移后该路径已不存在）
if (-not (Test-Path -LiteralPath $DllSrc)) { Log "找不到 DLL: $DllSrc"; exit 1 }
New-Item -ItemType Directory -Path $StagingDir -Force | Out-Null
$DllDst = Join-Path $StagingDir 'sidelens_vcam_dll.dll'
Copy-Item -LiteralPath $DllSrc -Destination $DllDst -Force
Log "DLL 已就位: $DllDst"

$acl = Get-Acl -LiteralPath $StagingDir
$rule = New-Object System.Security.AccessControl.FileSystemAccessRule(
    'NT AUTHORITY\LOCAL SERVICE', 'ReadAndExecute', 'ContainerInherit,ObjectInherit', 'None', 'Allow')
$acl.SetAccessRule($rule)
Set-Acl -LiteralPath $StagingDir -AclObject $acl
Log 'ACL 已授权 LOCAL SERVICE 读权限'

if (-not (Test-Path -LiteralPath $ClsidKey)) { New-Item -Path $ClsidKey -Force | Out-Null }
Set-ItemProperty -LiteralPath $ClsidKey -Name '(Default)' -Value $DllDst
Set-ItemProperty -LiteralPath $ClsidKey -Name 'ThreadingModel' -Value 'Both'
Log "InprocServer32 已指向: $((Get-ItemProperty -LiteralPath $ClsidKey).'(default)')"

# 阶段 1：A → B → C
$results = @()
$results += Invoke-Trial 'A1(基线)' 'absent'
$results += Invoke-Trial 'B1(=1)'   '1'
$results += Invoke-Trial 'C1(=0)'   '0'

# 阶段 2：C → B → A（顺序反转，排除顺序污染）
$results += Invoke-Trial 'C2(=0)'   '0'
$results += Invoke-Trial 'B2(=1)'   '1'
$results += Invoke-Trial 'A2(基线)' 'absent'

Set-FsmMode 'absent'
Log ''
Log "已回滚 EnableFrameServerMode（现值: $(Get-FsmMode)）"

if (-not $KeepDevices) {
    $n = Clear-GhostDevices
    Log "清理幽灵设备: $n 个"
}

Log ''
Log '=== 结果汇总 ==='
$table = $results | Format-Table -AutoSize | Out-String -Width 200
Write-Host $table
Add-Content -LiteralPath $LogFile -Value $table -Encoding utf8

$anyOk = @($results | Where-Object { $_.StartOk }).Count
Log "Start 成功的组数: $anyOk / $($results.Count)"
Log '=== 完成 ==='
