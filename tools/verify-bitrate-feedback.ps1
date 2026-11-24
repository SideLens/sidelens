# 验证码率反馈回路：接收端拥塞控制器 -> RttReply -> 发送端编码器
#
# 需要 --h264（否则没有编码器可调），并且要造出真实拥塞让控制器降码率。
# 造法：--loss 0.10 让丢包率高到控制器必须降。

$log = 'D:\dsh-workspace\sidelens\target\bitrate-feedback.log'
Remove-Item $log -ErrorAction SilentlyContinue
function W($m) { Add-Content $log $m -Encoding utf8 }
$OutputEncoding = [System.Text.Encoding]::UTF8
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8

$root = 'D:\dsh-workspace\sidelens'
$d = "$root\target\debug\sidelensd.exe"
$s = "$root\target\debug\sidelenssend.exe"

Get-Process sidelensd,sidelenssend -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 500

W "=== 码率反馈回路验证 $(Get-Date -Format HH:mm:ss) ==="
W ""

$port = 27700
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

$spsi = New-Object System.Diagnostics.ProcessStartInfo
$spsi.FileName = $s
$spsi.Arguments = "--udp 127.0.0.1:$port 60 8192 15 --h264 --loss 0.10"
$spsi.RedirectStandardOutput = $true
$spsi.RedirectStandardError = $true
$spsi.UseShellExecute = $false
$spsi.StandardOutputEncoding = [System.Text.Encoding]::UTF8
$spsi.StandardErrorEncoding = [System.Text.Encoding]::UTF8
$sproc = [System.Diagnostics.Process]::Start($spsi)
$sTask = $sproc.StandardOutput.ReadToEndAsync()
$sErrTask = $sproc.StandardError.ReadToEndAsync()
$sproc.WaitForExit(40000) | Out-Null
Start-Sleep -Milliseconds 1500
try { $dproc.Kill() } catch {}
$dproc.WaitForExit(5000) | Out-Null

$so = $sTask.Result + [Environment]::NewLine + $sErrTask.Result
$dout = $dTask.Result + [Environment]::NewLine + $dErrTask.Result
Set-Content "$root\target\bf.snd.log" $so
Set-Content "$root\target\bf.d.log" $dout

W "--- 发送端：码率调整事件 ---"
$adj = @($so -split "\r?\n" | Where-Object { $_ -match "调整编码码率" })
W ("调整次数: " + $adj.Count)
$adj | ForEach-Object { W ("  " + $_.Trim()) }
W ""
W "--- 发送端：最终统计 ---"
$fin = ($so -split "\r?\n" | Where-Object { $_ -match "bitrate_adaptations" } | Select-Object -Last 1)
W ("  " + $fin)
W ""
W "--- 接收端：拥塞控制决策 ---"
$cc = @($dout -split "\r?\n" | Where-Object { $_ -match "target_kbps" })
foreach ($h in $cc) {
    $tk = "?"; $st = "?"; $dec = "?"
    if ($h -match "target_kbps=.([0-9.]+).") { $tk = $Matches[1] }
    if ($h -match "cc_state=([A-Za-z]+)") { $st = $Matches[1] }
    if ($h -match "cc_decisions=([0-9]+)") { $dec = $Matches[1] }
    W ("  target_kbps=" + $tk.PadRight(10) + " state=" + $st.PadRight(8) + " decisions=" + $dec)
}
W ""
W "=== 结束 ==="