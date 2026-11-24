#Requires -Version 7.0
<#
.SYNOPSIS
    自适应冗余度的 A/B 实测：同一场景跑两遍（--fec-fixed 2 vs 自适应）。
.DESCRIPTION
    每组配置起全新 daemon（接收端计数器是进程累计值），跑 DurationSec 秒，
    发送端退出后再等 2.4 秒让最后一个统计窗口落盘，然后解析两份日志。
#>
[CmdletBinding()]
param(
    [int]$DurationSec = 6,
    [int]$BasePort = 9400,
    [string]$OutDir
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$RepoRoot = 'D:\dsh-workspace\sidelens'
if (-not $OutDir) { $OutDir = Join-Path $RepoRoot 'target\fecab' }
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null

$DaemonExe = Join-Path $RepoRoot 'target\release\sidelensd.exe'
$SenderExe = Join-Path $RepoRoot 'target\release\sidelenssend.exe'

$scenarios = @(
    # A: H.264 小帧（2 分片）—— 看自适应是否仍然合理跳过
    @{ id='A-h264-fixed';   h264=$true;  frame=8192;  loss=0.03; fecArgs=@('--fec','2','--fec-fixed') }
    @{ id='A-h264-adapt';   h264=$true;  frame=8192;  loss=0.03; fecArgs=@('--fec','2') }
    # B: 合成 8KB（8 分片）—— 甜区，对比固定 2
    @{ id='B-8k-fixed';     h264=$false; frame=8192;  loss=0.05; fecArgs=@('--fec','2','--fec-fixed') }
    @{ id='B-8k-adapt';     h264=$false; frame=8192;  loss=0.05; fecArgs=@('--fec','2') }
    # C: 64KB 大帧（55 分片）—— 主战场
    @{ id='C-64k-fixed';    h264=$false; frame=65536; loss=0.03; fecArgs=@('--fec','2','--fec-fixed') }
    @{ id='C-64k-adapt';    h264=$false; frame=65536; loss=0.03; fecArgs=@('--fec','2') }
    # D: 64KB 大帧 + 更高丢包，验证自适应能涨到 5+
    @{ id='D-64k-loss8-fixed'; h264=$false; frame=65536; loss=0.08; fecArgs=@('--fec','2','--fec-fixed') }
    @{ id='D-64k-loss8-adapt'; h264=$false; frame=65536; loss=0.08; fecArgs=@('--fec','2') }
    # E: 对照：完全不开 FEC
    @{ id='E-64k-off';      h264=$false; frame=65536; loss=0.03; fecArgs=@() }
    @{ id='F-8k-off';       h264=$false; frame=8192;  loss=0.05; fecArgs=@() }
)

function Get-UdpPortOwner {
    param([int]$Port)
    try {
        $ep = @(Get-NetUDPEndpoint -LocalPort $Port -ErrorAction SilentlyContinue)
        if ($ep.Count -eq 0) { return 0 }
        return [int]($ep | Select-Object -First 1).OwningProcess
    } catch { return 0 }
}

$results = @()
$port = $BasePort
foreach ($sc in $scenarios) {
    $id = $sc.id
    $srvOut = Join-Path $OutDir ($id + '.srv.log')
    $sndOut = Join-Path $OutDir ($id + '.snd.log')
    foreach ($f in @($srvOut, $sndOut)) { if (Test-Path $f) { Remove-Item $f -Force } }

    $stale = Get-UdpPortOwner -Port $port
    if ($stale -ne 0) { try { Stop-Process -Id $stale -Force } catch {}; Start-Sleep -Milliseconds 400 }

    $psi = [System.Diagnostics.ProcessStartInfo]::new()
    $psi.FileName = $DaemonExe
    foreach ($a in @('--udp', "$port")) { $psi.ArgumentList.Add($a) }
    $psi.UseShellExecute = $false; $psi.CreateNoWindow = $true
    $psi.RedirectStandardOutput = $true; $psi.RedirectStandardError = $true
    $psi.StandardOutputEncoding = [System.Text.Encoding]::UTF8
    $psi.StandardErrorEncoding = [System.Text.Encoding]::UTF8
    $srv = [System.Diagnostics.Process]::new(); $srv.StartInfo = $psi; $null = $srv.Start()
    $srvOutTask = $srv.StandardOutput.ReadToEndAsync()
    $srvErrTask = $srv.StandardError.ReadToEndAsync()

    # 等端口就绪
    $deadline = (Get-Date).AddSeconds(10); $ready = $false
    while ((Get-Date) -lt $deadline) {
        if (Get-UdpPortOwner -Port $port -eq $srv.Id) { $ready = $true; break }
        Start-Sleep -Milliseconds 100
    }
    if (-not $ready) { Write-Host ("[{0}] daemon 未就绪" -f $id) -ForegroundColor Red; try { $srv.Kill($true) } catch {}; continue }

    $sndArgs = @('--udp', ("127.0.0.1:" + $port), '30', "$($sc.frame)", "$DurationSec", '--loss', "$($sc.loss)")
    if ($sc.h264) { $sndArgs += '--h264' }
    $sndArgs += $sc.fecArgs

    $psi2 = [System.Diagnostics.ProcessStartInfo]::new()
    $psi2.FileName = $SenderExe
    foreach ($a in $sndArgs) { $psi2.ArgumentList.Add($a) }
    $psi2.UseShellExecute = $false; $psi2.CreateNoWindow = $true
    $psi2.RedirectStandardOutput = $true; $psi2.RedirectStandardError = $true
    $psi2.StandardOutputEncoding = [System.Text.Encoding]::UTF8
    $psi2.StandardErrorEncoding = [System.Text.Encoding]::UTF8
    $snd = [System.Diagnostics.Process]::new(); $snd.StartInfo = $psi2; $null = $snd.Start()
    $sndOutTask = $snd.StandardOutput.ReadToEndAsync()
    $sndErrTask = $snd.StandardError.ReadToEndAsync()
    $null = $snd.WaitForExit(60000)
    try { $snd.WaitForExit() } catch {}
    $sndText = ''; try { $sndText = $sndOutTask.GetAwaiter().GetResult() } catch {}
    $sndErr = ''; try { $sndErr = $sndErrTask.GetAwaiter().GetResult() } catch {}
    [System.IO.File]::WriteAllText($sndOut, $sndText, [System.Text.UTF8Encoding]::new($false))
    if ($sndErr.Trim()) { Write-Host ("[{0}] stderr: {1}" -f $id, $sndErr.Substring(0,[Math]::Min(200,$sndErr.Length))) -ForegroundColor Yellow }

    Start-Sleep -Milliseconds 2400
    try { $srv.Kill($true) } catch {}
    try { $srv.WaitForExit(5000) | Out-Null } catch {}
    $srvText = ''; try { $srvText = $srvOutTask.GetAwaiter().GetResult() } catch {}
    $srvErr = ''; try { $srvErr = $srvErrTask.GetAwaiter().GetResult() } catch {}
    [System.IO.File]::WriteAllText($srvOut, $srvText, [System.Text.UTF8Encoding]::new($false))

    $parsed = & (Join-Path $RepoRoot 'tools\bench\parse-metrics.ps1') -ServerLog $srvOut -SenderLog $sndOut
    $results += [pscustomobject]@{ id = $id; scenario = $sc; metrics = $parsed }
    $r = $parsed.Receiver; $s = $parsed.Sender
    Write-Host ("[{0}] playable={1}/{2} fec_recov={3} nack={4} ovh={5}% red={6} cfg_upd={7} usage={8}" -f `
        $id, $r.playable_p50, $r.playable_p95, $r.fec_recovered, $r.nack_seqs, $s.fec_overhead, $s.fec_redundant, $r.fec_cfg_updates, $s.fec_group_usage) -ForegroundColor Green
    $port++
}

$results | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath (Join-Path $OutDir 'results.json') -Encoding UTF8
Write-Host ("完成，结果在 " + $OutDir) -ForegroundColor Cyan
