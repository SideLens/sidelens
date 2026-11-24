# 验证：开启 --psk 后，NACK 重传是否被重放窗口误杀
#
# 假设：发送端重传走 cache.take_for_retransmit → send_frame(&f.bytes)，
# 原样重发缓存的密文，seq 不变。接收端先 replay.check_and_mark(seq) 再解密，
# 首传已 mark 过同一 seq → 重传被判 Duplicate → replay_rejected += 1 → continue。
# 也就是说：开加密后，重传一次都不会生效。

$root = Split-Path -Parent $PSScriptRoot
$log = Join-Path $root 'target\replay-retx.log'
Remove-Item $log -ErrorAction SilentlyContinue
function W($m) { Add-Content $log $m -Encoding utf8 }
$OutputEncoding = [System.Text.Encoding]::UTF8
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8

$d  = "$root\target\release\sidelensd.exe"
$s  = "$root\target\release\sidelenssend.exe"

Get-Process sidelensd,sidelenssend -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 500

W "=== 重放误杀重传 验证 $(Get-Date -Format HH:mm:ss) ==="
W ''

function RunCase($name, $psk) {
    W "--------- $name ---------"
    Get-Process sidelensd,sidelenssend -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
    Start-Sleep -Milliseconds 400

    $port = 27500
    $dOut = "$root\target\rr-$name.d.log"
    $sOut = "$root\target\rr-$name.s.log"

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
    Start-Sleep -Milliseconds 900

    $sargs = "--udp 127.0.0.1:$port 60 8192 8 --loss 0.03"
    if ($psk) { $sargs += " --psk 00112233445566778899aabbccddeeff" }
    $spsi = New-Object System.Diagnostics.ProcessStartInfo
    $spsi.FileName = $s
    $spsi.Arguments = $sargs
    $spsi.RedirectStandardOutput = $true
    $spsi.RedirectStandardError = $true
    $spsi.UseShellExecute = $false
    $spsi.StandardOutputEncoding = [System.Text.Encoding]::UTF8
    $spsi.StandardErrorEncoding = [System.Text.Encoding]::UTF8
    $sproc = [System.Diagnostics.Process]::Start($spsi)
    $sTask = $sproc.StandardOutput.ReadToEndAsync()
    $sErrTask = $sproc.StandardError.ReadToEndAsync()
    $sproc.WaitForExit(30000) | Out-Null
    Start-Sleep -Milliseconds 1500
    try { $dproc.Kill() } catch {}
    $dproc.WaitForExit(5000) | Out-Null

    $so = $sTask.Result
    $dout = $dTask.Result + [Environment]::NewLine + $dErrTask.Result
    Set-Content "$sOut" $so -Encoding utf8
    Set-Content "$dOut" $dout -Encoding utf8

    $lines = $so -split "\r?\n"
    $sndLine = ($lines | Where-Object { $_ -match "retransmitted" } | Select-Object -Last 1)
    W "  [sender] $sndLine"

    $dl = $dout -split "\r?\n"
    $hit = ($dl | Where-Object { $_ -match "nack_seqs|replay_rejected" })
    if ($hit) { $hit | Select-Object -Last 3 | ForEach-Object { W "  [daemon] $($_.Trim())" } }
    else { W "  [daemon] 没找到 nack_seqs/replay_rejected" }
    W ''
}

RunCase 'nopsk' $false
RunCase 'psk'   $true

W "=== 结束 ==="