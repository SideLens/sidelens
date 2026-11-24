# A/B：把虚拟摄像头登记到不同 KS 类别下，看 Start 的行为是否变化
#
# 背景（见 docs/vcam-summary.md §1.5）：
#   mfsensorgroup.dll 里有一个 SDK 头文件查不到的类别
#   KSCATEGORY_FSM_VIRTUAL_CAMERA = {588C8D20-C0E3-4FD3-B511-8F2F692156F8}
#
# # 为什么要「每组前清幽灵设备」+「顺序反转各跑一遍」
#
# 第一版直接连跑 vc → fsm → both，结果是：
#   vc   : DLL 被加载 15 行
#   fsm  : DLL 未被加载
#   both : DLL 未被加载
# 看起来像「fsm 类别让 FrameServer 看不见设备」，但有个致命混淆：
# Start 失败后设备记录**不会自动清除**（本 crate 默认不 Remove，
# 见 register.rs 的 remove_on_drop 注释）。于是第一组留下的
# SWD\VCAMDEVAPI\<hash> 幽灵设备会让后两组复用旧节点，
# 而旧节点的类别是第一组写死的 VIDEO_CAMERA。
# 这样一来「后两组 DLL 未加载」既可能是类别导致，也可能是顺序导致。
#
# 所以本脚本做两件事：
#   1. 每组之前用 pnputil /remove-device 删掉所有 VCAMDEVAPI 幽灵设备
#   2. 跑两轮，第二轮顺序反转（both → fsm → vc），看结果是否随顺序变
#
# 只有「同一类别在两轮里行为一致」才算有效观测。
$root = Split-Path -Parent $PSScriptRoot

$log = Join-Path $root 'target\vcam-category-ab.log'
Remove-Item $log -ErrorAction SilentlyContinue
function W($m) { Add-Content $log $m -Encoding utf8 }

$OutputEncoding = [System.Text.Encoding]::UTF8
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8

W "=== 类别 A/B 开始 $(Get-Date -Format 'HH:mm:ss') ==="
W "身份: $([System.Security.Principal.WindowsIdentity]::GetCurrent().Name)"
W "管理员: $((New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator))"
W ''

$exe = (Join-Path $root 'target\debug\sidelensd.exe')
$vc  = '{e5323777-f976-4f5b-9b55-b94699c46e44}'
$fsm = '{588c8d20-c0e3-4fd3-b511-8f2f692156f8}'

function GhostCount { return @(Get-PnpDevice -ErrorAction SilentlyContinue | Where-Object { $_.InstanceId -like 'SWD\VCAMDEVAPI\*' }).Count }

function PurgeGhosts {
    $ghosts = @(Get-PnpDevice -ErrorAction SilentlyContinue | Where-Object { $_.InstanceId -like 'SWD\VCAMDEVAPI\*' })
    foreach ($g in $ghosts) {
        $null = & pnputil.exe /remove-device "$($g.InstanceId)" 2>&1
    }
    Start-Sleep -Milliseconds 600
    return $ghosts.Count
}

function Snapshot($tag) {
    $vcN = 0; $fsmN = 0
    $p1 = "HKLM:\SYSTEM\CurrentControlSet\Control\DeviceClasses\$vc"
    $p2 = "HKLM:\SYSTEM\CurrentControlSet\Control\DeviceClasses\$fsm"
    if (Test-Path $p1) { $vcN  = @(Get-ChildItem $p1 -ErrorAction SilentlyContinue).Count }
    if (Test-Path $p2) { $fsmN = @(Get-ChildItem $p2 -ErrorAction SilentlyContinue).Count }
    W ("  [$tag] 接口节点: VIDEO_CAMERA=$vcN  FSM=$fsmN   幽灵设备=$(GhostCount)")
}

function RunOne($mode) {
    W ''
    W "───────── 组: $mode ─────────"
    $killed = PurgeGhosts
    W ("  清理幽灵设备: $killed 个")
    Snapshot '运行前'

    Remove-Item "$env:TEMP\sidelens-vcam.log" -ErrorAction SilentlyContinue

    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $exe
    $psi.Arguments = "--vcam-probe --hold 3 --no-inproc-class --category $mode"
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.UseShellExecute = $false
    $psi.StandardOutputEncoding = [System.Text.Encoding]::UTF8
    $psi.StandardErrorEncoding = [System.Text.Encoding]::UTF8
    $pr = [System.Diagnostics.Process]::Start($psi)
    $so = $pr.StandardOutput.ReadToEnd()
    $se = $pr.StandardError.ReadToEnd()
    $pr.WaitForExit()

    (($so + "`n" + $se) -split "`r?`n") | Where-Object { $_ -match '类别|Start|创建失败|ActivateObject|GetMediaSource 失败|IsVirtualCamera' } | ForEach-Object { W ("  " + $_.TrimEnd()) }

    $dll = "$env:TEMP\sidelens-vcam.log"
    if (Test-Path $dll) {
        $lines = @(Get-Content $dll -Encoding utf8)
        W ("  >>> DLL 被加载，$($lines.Count) 行；最后一步: " + ($lines | Select-Object -Last 1))
    } else {
        W '  >>> DLL 未被加载'
    }

    Start-Sleep -Milliseconds 400
    Snapshot '运行后'
}

W ''
W '╔══════════ 第一轮：vc → fsm → both ══════════'
foreach ($m in @('vc','fsm','both')) { RunOne $m }

W ''
W '╔══════════ 第二轮：both → fsm → vc（顺序反转）══════════'
foreach ($m in @('both','fsm','vc')) { RunOne $m }

W ''
W "=== 结束 $(Get-Date -Format 'HH:mm:ss') ==="
