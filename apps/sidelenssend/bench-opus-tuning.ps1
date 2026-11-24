#Requires -Version 7.0
<#
.SYNOPSIS
    Opus 调优开关（--opus-frame-ms / --opus-dtx / --opus-inband-fec / --opus-loss）实测矩阵。

.DESCRIPTION
    每组配置起一对全新进程（daemon + sender），跑固定时长后解析两边日志。
    放在 apps/sidelenssend/ 下而不是 tools/ 下，是因为本任务的写入范围只有 apps/sidelenssend/**。

    三条硬约束（照抄 tools/bench/run-matrix.ps1 的结论）：

    1. 每组换一个端口。daemon 在端口被占时会**静默上移**（见
       crates/sidelens-transport/src/udp.rs 的 bind），发送端仍发给原端口，
       表现为整组零帧。
    2. 每组起**全新** daemon。接收端 audio_decoded / audio_played 是进程
       累计值，复用会把上一组算进来。
    3. 发送端退出后**再等一个统计窗口**（daemon 每 2 秒打一行）才杀，
       否则最后一个窗口不落盘。

    # 接收端为什么要设 SIDELENS_OPUS=1
    没有它 opus_dec 是 None，audio_decoded 恒为 0 —— 那不是「解码失败」，
    是「根本没开解码」。

    # 接收端为什么要按帧长调 SIDELENS_AUDIO_TARGET_MS
    AudioJitterBuffer 的容量 = ceil(target_ms / frame_ms) + 1。默认 target 40ms：
    20ms 帧容量 3、60ms 帧容量 **2** —— 第二帧入队就溢出，丢最旧的一帧，
    表现为「收到 N 帧、只解一半」。所以按 2*frame_ms + 20 抬 target，
    保证容量至少 3。这是**接收端配置**，不是发送端的锅。
#>
[CmdletBinding()]
param(
    [int]$DurationSec = 8,
    [int]$BasePort = 9400,
    [string]$RepoRoot
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

if (-not $RepoRoot) { $RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..' '..')).Path }
$RepoRoot  = (Resolve-Path $RepoRoot).Path
$OutDir    = Join-Path $RepoRoot 'target\opus-tuning'
$DaemonExe = Join-Path $RepoRoot 'target\release\sidelensd.exe'
$SenderExe = Join-Path $RepoRoot 'target\release\sidelenssend.exe'
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null

$configs = @(
    @{ id = 'f2.5'; frame = 2.5;  dtx = $false; fec = $false; loss = 0;  silence = $null }
    @{ id = 'f5';   frame = 5.0;  dtx = $false; fec = $false; loss = 0;  silence = $null }
    @{ id = 'f10';  frame = 10.0; dtx = $false; fec = $false; loss = 0;  silence = $null }
    @{ id = 'f20';  frame = 20.0; dtx = $false; fec = $false; loss = 0;  silence = $null }
    @{ id = 'f40';  frame = 40.0; dtx = $false; fec = $false; loss = 0;  silence = $null }
    @{ id = 'f60';  frame = 60.0; dtx = $false; fec = $false; loss = 0;  silence = $null }
    @{ id = 'f20-fec10'; frame = 20.0; dtx = $false; fec = $true; loss = 10; silence = $null }
    @{ id = 'f60-fec10'; frame = 60.0; dtx = $false; fec = $true; loss = 10; silence = $null }
    @{ id = 'f20-dtx-off'; frame = 20.0; dtx = $false; fec = $false; loss = 0; silence = 4 }
    @{ id = 'f20-dtx';  frame = 20.0; dtx = $true;  fec = $false; loss = 0; silence = 4 }
    @{ id = 'f60-dtx';  frame = 60.0; dtx = $true;  fec = $false; loss = 0; silence = 4 }
    @{ id = 'f20-fec0'; frame = 20.0; dtx = $false; fec = $true; loss = 0;  silence = $null }
    @{ id = 'f20-loss10'; frame = 20.0; dtx = $false; fec = $false; loss = 10; silence = $null }
)

function Get-Prop {
    param([string]$Text, [string]$Name)
    # 日志是 tracing 的 key=value / key="value" 格式
    $pat = '(?:^|\s)' + [regex]::Escape($Name) + '=(?:"([^"]*)"|(\S+))'
    $m = [regex]::Match($Text, $pat)
    if (-not $m.Success) { return $null }
    if ($m.Groups[1].Success) { return $m.Groups[1].Value }
    return $m.Groups[2].Value
}

function Invoke-Config {
    param($Cfg, [int]$Port, [int]$Index, [int]$Total)

    $id = $Cfg.id
    $srvOut = Join-Path $OutDir ($id + ".srv.log")
    $srvErr = Join-Path $OutDir ($id + ".srv.err")
    $sndOut = Join-Path $OutDir ($id + ".snd.log")
    foreach ($f in @($srvOut, $srvErr, $sndOut)) { if (Test-Path $f) { Remove-Item $f -Force } }

    Write-Host ("[{0}/{1}] {2}  frame={3}ms dtx={4} fec={5} loss={6}% silence={7}" -f `
        ($Index + 1), $Total, $id, $Cfg.frame, $Cfg.dtx, $Cfg.fec, $Cfg.loss, $Cfg.silence) -ForegroundColor Cyan

    # 接收端抖动缓冲按帧长抬，保证容量 >= 3（见脚本头注释）
    $targetMs = [Math]::Max(40.0, 2.0 * $Cfg.frame + 20.0)
    $env:SIDELENS_OPUS = '1'
    $env:SIDELENS_AUDIO_TARGET_MS = $targetMs.ToString([System.Globalization.CultureInfo]::InvariantCulture)

    $srv = Start-Process -FilePath $DaemonExe -ArgumentList "--udp", "$Port" `
        -RedirectStandardOutput $srvOut -RedirectStandardError $srvErr -PassThru -NoNewWindow
    Start-Sleep -Milliseconds 1400

    $inv = [System.Globalization.CultureInfo]::InvariantCulture
    $sndArgs = @('--udp', "127.0.0.1:$Port", '30', '8192', "$DurationSec",
                 '--audio', '--opus', '--opus-bitrate', '32000',
                 '--opus-frame-ms', $Cfg.frame.ToString($inv))
    if ($Cfg.dtx) { $sndArgs += '--opus-dtx' }
    if ($Cfg.fec) { $sndArgs += '--opus-inband-fec' }
    if ($Cfg.loss -gt 0) { $sndArgs += @('--opus-loss', "$($Cfg.loss)") }
    if ($null -ne $Cfg.silence) { $sndArgs += @('--audio-silence-after', "$($Cfg.silence)") }

    & $SenderExe @sndArgs *> $sndOut
    $sendExit = $LASTEXITCODE

    Start-Sleep -Milliseconds 2600
    Stop-Process -Id $srv.Id -Force -ErrorAction SilentlyContinue
    Start-Sleep -Milliseconds 300

    $srvText = (Get-Content $srvOut -Raw -ErrorAction SilentlyContinue)
    if (-not $srvText) { $srvText = "" }
    $sndText = (Get-Content $sndOut -Raw -ErrorAction SilentlyContinue)
    if (-not $sndText) { $sndText = "" }

    # daemon 每 2 秒一行统计：跳过首窗（启动斜坡）与末窗（残窗），取倒数第二个完整窗
    $statLines = @($srvText -split "`n" | Where-Object { $_ -match "UDP 统计" })
    $stat = if ($statLines.Count -ge 3) { $statLines[$statLines.Count - 2] }
            elseif ($statLines.Count -ge 1) { $statLines[-1] } else { "" }

    $endLine = @($sndText -split "`n" | Where-Object { $_ -match "发送结束" }) | Select-Object -Last 1
    if (-not $endLine) { $endLine = "" }
    $tuneLine = @($sndText -split "`n" | Where-Object { $_ -match "Opus 调优" }) | Select-Object -Last 1
    if (-not $tuneLine) { $tuneLine = "" }

    $rec = [ordered]@{
        id            = $id
        frame_ms      = $Cfg.frame
        dtx           = $Cfg.dtx
        fec           = $Cfg.fec
        loss          = $Cfg.loss
        silence_after = $Cfg.silence
        audio_target  = $targetMs
        send_exit     = $sendExit
        tune_line     = $tuneLine.Trim()
        rx_kbps       = Get-Prop $stat 'kbps'
        audio_decoded = Get-Prop $stat 'audio_decoded'
        audio_frames  = Get-Prop $stat 'audio_frames'
        audio_played  = Get-Prop $stat 'audio_played'
        audio_overflow= Get-Prop $stat 'audio_overflow'
        audio_late    = Get-Prop $stat 'audio_dropped_late'
        audio_concealed = Get-Prop $stat 'audio_concealed'
        audio_decode_failed = Get-Prop $stat 'audio_decode_failed'
        audio_bytes   = Get-Prop $endLine 'audio_bytes'
        audio_sent    = Get-Prop $endLine 'audio_sent'
        audio_kbps    = Get-Prop $endLine 'audio_kbps'
        avg_frame_bytes = Get-Prop $endLine 'audio_avg_frame_bytes'
        pts_span_ms   = Get-Prop $endLine 'audio_pts_span_ms'
        opus_frames   = Get-Prop $endLine 'opus_frames'
        dtx_frames    = Get-Prop $endLine 'opus_dtx_frames'
        dtx_bytes     = Get-Prop $endLine 'opus_dtx_bytes'
        speech_bytes  = Get-Prop $endLine 'opus_avg_speech_bytes'
        stat_lines    = $statLines.Count
    }
    Write-Host ("      rx_kbps={0} audio_decoded={1} audio_bytes={2} avg_frame={3} dtx={4} pts_span={5}ms" -f `
        $rec.rx_kbps, $rec.audio_decoded, $rec.audio_bytes, $rec.avg_frame_bytes,
        $rec.dtx_frames, $rec.pts_span_ms) -ForegroundColor Green
    return [pscustomobject]$rec
}

Get-Process sidelensd, sidelenssend -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 400

$results = @()
$port = $BasePort
for ($i = 0; $i -lt $configs.Count; $i++) {
    $results += Invoke-Config -Cfg $configs[$i] -Port $port -Index $i -Total $configs.Count
    $port++
}

Get-Process sidelensd, sidelenssend -ErrorAction SilentlyContinue | Stop-Process -Force

$json = Join-Path $OutDir 'matrix.json'
$results | ConvertTo-Json -Depth 5 | Set-Content $json -Encoding utf8

Write-Host ''
Write-Host '| 配置 | frame_ms | dtx | fec | loss | rx_kbps | audio_decoded | audio_bytes | 帧数 | 每帧字节 | dtx帧 | audio_kbps |' -ForegroundColor Yellow
Write-Host '| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |' -ForegroundColor Yellow
foreach ($r in $results) {
    Write-Host ("| {0} | {1} | {2} | {3} | {4} | {5} | {6} | {7} | {8} | {9} | {10} | {11} |" -f `
        $r.id, $r.frame_ms, $r.dtx, $r.fec, $r.loss, $r.rx_kbps, $r.audio_decoded,
        $r.audio_bytes, $r.audio_sent, $r.avg_frame_bytes, $r.dtx_frames, $r.audio_kbps)
}
Write-Host ''
Write-Host ("JSON: " + $json)
