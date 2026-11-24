# 验证抖动缓冲自适应是否真的降低 playable_ms
#
# 修复前：shrink/grow 从未被调用，target_ms 恒为 20.0，playable p50 约 20.95ms
# 修复后：adapt 按实测抖动调整，回环抖动 <1ms 应把 target 降到 min_ms=10

$log = 'D:\dsh-workspace\sidelens\target\jitter-adapt.log'
Remove-Item $log -ErrorAction SilentlyContinue
function W($m) { Add-Content $log $m -Encoding utf8 }
$OutputEncoding = [System.Text.Encoding]::UTF8
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8

$root = 'D:\dsh-workspace\sidelens'
$d = "$root\target\debug\sidelensd.exe"
$s = "$root\target\debug\sidelenssend.exe"

Get-Process sidelensd,sidelenssend -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 500

W "=== 抖动自适应验证 $(Get-Date -Format HH:mm:ss) ==="
W ""

$port = 27600
$dpsi = New-Object System.Diagnostics.ProcessStartInfo
$dpsi.FileName = $d
$dpsi.Arguments = "--udp $port --no-tui"
$dpsi.RedirectStandardOutput = $true
$dpsi.RedirectStandardError = $true
$dpsi.UseShellExecute = $false
$dpsi.StandardOutputEncoding = [System.Text.Encoding]::UTF8
$dpsi.StandardErrorEncoding = [System.Text.Encoding]::UTF8
$dproc = [System.Diagnostics.Process]::Start($dpsi)
$dTask = $dproc.StandardOutput.ReadToEndAsync()
$dErrTask = $dproc.StandardError.ReadToEndAsync()
Start-Sleep -Milliseconds 1000

# 20 秒，让 adapt 有足够时间（2s 一步，从 20 降到 10 需 5 步）
$spsi = New-Object System.Diagnostics.ProcessStartInfo
$spsi.FileName = $s
$spsi.Arguments = "--udp 127.0.0.1:$port 60 8192 20"
$spsi.RedirectStandardOutput = $true
$spsi.RedirectStandardError = $true
$spsi.UseShellExecute = $false
$spsi.StandardOutputEncoding = [System.Text.Encoding]::UTF8
$spsi.StandardErrorEncoding = [System.Text.Encoding]::UTF8
$sproc = [System.Diagnostics.Process]::Start($spsi)
$sTask = $sproc.StandardOutput.ReadToEndAsync()
$sproc.WaitForExit(40000) | Out-Null
Start-Sleep -Milliseconds 1500
try { $dproc.Kill() } catch {}
$dproc.WaitForExit(5000) | Out-Null

$dout = $dTask.Result + [Environment]::NewLine + $dErrTask.Result
Set-Content "$root\target\jitter-adapt.d.log" $dout

$dl = $dout -split "\r?\n"
$hits = @($dl | Where-Object { $_ -match "playable_ms" })
W "共 $($hits.Count) 条统计行："
foreach ($h in $hits) {
    $t  = "?"
    $p  = "?"
    $j  = "?"
    $a  = "?"
    $dn = "?"
    if ($h -match "target_ms=.([0-9.]+).") { $t = $Matches[1] }
    if ($h -match "playable_ms=.([0-9.]+)/([0-9.]+).") { $p = $Matches[1] + "/" + $Matches[2] }
    if ($h -match "jitter_ewma_ms=.([0-9.-]+).") { $j = $Matches[1] }
    if ($h -match "jitter_adapt=([0-9]+)") { $a = $Matches[1] }
    if ($h -match "jitter_down=([0-9]+)") { $dn = $Matches[1] }
    W ("  target=" + $t.PadRight(6) + " playable=" + $p.PadRight(16) + " jitter_ewma=" + $j.PadRight(8) + " adapt=" + $a + " down=" + $dn)
}
W ""
W "=== 结束 ==="