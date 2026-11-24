#Requires -Version 7.0
<#
.SYNOPSIS
    从 sidelensd / sidelenssend 的日志里提取基准指标。

.DESCRIPTION
    这个脚本只做一件事：把两份日志变成一份结构化指标。它不启动进程、不写报告，
    所以可以脱离矩阵单独用（调正则、复查某一轮日志时很方便）：

        pwsh tools/bench/parse-metrics.ps1 -ServerLog target/bench/x.srv.log -SenderLog target/bench/x.snd.out.log

    # 三个坑，都是实测踩到的

    1. 接收端的计数器分两种口径，混用会得到完全错误的数字：

       [累计值] frames / nack_seqs / expired / fec_recovered / healed / av_synced
                -> 取最大值
       [2 秒窗口] loss / rx_fps / kbps / fec_reds / avg_frags
                -> 跳过首尾两窗后取中位数
       [滑动窗口] net_ms / playable_ms / hold_ms / audio_depth_ms
                -> 取最后一行

       拿最后一行去读 frames 得到的是进程启动以来的累计值（daemon 被复用时
       还会带上上一轮）；拿最后一行去读 rx_fps 得到的是残窗——发送端已经退出，
       窗口里只剩半截帧，rx_fps 会从 30 掉到 11。所以窗口类指标必须跳过首尾。

    2. mbps 只出现在「发送中」周期日志里，「发送结束」没有这个字段。
       拿不到就用 fec_data_bytes + fec_red_bytes 除以 elapsed 自己算，
       报告里标明来源，避免两个不同口径的数字被当成同一个。

    3. 正则必须容错。任何字段抓不到都只是 $null + 记进 Missing，
       绝不让脚本崩——一组配置跑废了不该毁掉整份矩阵。
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$ServerLog,
    [Parameter(Mandatory)][string]$SenderLog
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# ─────────────────────────────────────────────────────────────────────────────
# 基础工具
# ─────────────────────────────────────────────────────────────────────────────

function Read-LogLines {
    param([string]$Path)
    if (-not (Test-Path -LiteralPath $Path)) { return @() }
    # 必须转绝对路径：PowerShell 的 Get-Location 与进程 CWD 是两个东西。
    # 用相对路径调 [System.IO.File]::ReadAllLines 会按进程 CWD 解析，
    # 读不到时抛 IOException，被下面的 catch 吞成「空日志」——
    # 表现为「所有字段都缺失」而不是「路径错了」，排查起来极费时间。
    try {
        $abs = (Resolve-Path -LiteralPath $Path).Path
        return [System.IO.File]::ReadAllLines($abs, [System.Text.Encoding]::UTF8)
    } catch {
        return @()
    }
}

<#
    tracing-subscriber 的输出形如：

        WARN sidelensd::udp_server: UDP 统计 frames=39 net_ms="0.12/0.17/0.23" ...

    值是两种写法：带引号的（含 / 或 % 的复合值）和裸的。
    这个正则两种都吃，并且只认「标识符=」开头的，不会把中文正文卷进来。
#>
$script:KvPattern = [regex]'(?<k>[A-Za-z_][A-Za-z0-9_]*)=(?:"(?<q>[^"]*)"|(?<u>[^\s]+))'

function ConvertFrom-KvLine {
    param([string]$Line)
    $h = [ordered]@{}
    foreach ($m in $script:KvPattern.Matches($Line)) {
        $k = $m.Groups['k'].Value
        $v = if ($m.Groups['q'].Success) { $m.Groups['q'].Value } else { $m.Groups['u'].Value }
        $h[$k] = $v
    }
    return $h
}

function ConvertTo-DoubleOrNull {
    param($Value)
    if ($null -eq $Value) { return $null }
    $s = [string]$Value
    if ([string]::IsNullOrWhiteSpace($s) -or $s -eq '-') { return $null }
    $d = 0.0
    if ([double]::TryParse($s, [System.Globalization.NumberStyles]::Float,
            [System.Globalization.CultureInfo]::InvariantCulture, [ref]$d)) {
        return $d
    }
    return $null
}

<#
    "20.96/22.05" -> @(20.96, 22.05)
    段数不够就补 $null，调用方按位置取，不会因为少一段就整体错位。
#>
function Split-Pair {
    param([string]$Value, [int]$Count = 2)
    $out = @()
    if ([string]::IsNullOrWhiteSpace($Value)) {
        return @($null) * $Count
    }
    $parts = $Value -split '/'
    for ($i = 0; $i -lt $Count; $i++) {
        $out += if ($i -lt $parts.Count) { ConvertTo-DoubleOrNull $parts[$i] } else { $null }
    }
    return $out
}

function Get-Median {
    param([double[]]$Values)
    $v = @($Values | Where-Object { $null -ne $_ } | Sort-Object)
    if ($v.Count -eq 0) { return $null }
    $mid = [int][Math]::Floor($v.Count / 2)
    if ($v.Count % 2 -eq 1) { return $v[$mid] }
    return ($v[$mid - 1] + $v[$mid]) / 2.0
}

function Get-MaxOrNull {
    param([double[]]$Values)
    $v = @($Values | Where-Object { $null -ne $_ })
    if ($v.Count -eq 0) { return $null }
    return ($v | Measure-Object -Maximum).Maximum
}

# ─────────────────────────────────────────────────────────────────────────────
# 接收端
# ─────────────────────────────────────────────────────────────────────────────

$serverLines = Read-LogLines -Path $ServerLog
$statsLines = @($serverLines | Where-Object { $_ -match 'UDP 统计' })

$rx = [ordered]@{
    stats_windows       = $statsLines.Count
    bound_port          = $null
    frames              = $null
    net_p50             = $null
    net_p95             = $null
    net_p99             = $null
    playable_p50        = $null
    playable_p95        = $null
    hold_p50            = $null
    hold_p95            = $null
    jitter              = $null
    # 接收端 frames_in 同时统计 Video 与 Retransmit（见 udp_server.rs 的
    # `FrameType::Video | FrameType::Retransmit =>`），所以 frames 会大于
    # 发送端 sent —— 不是重复计数 bug，是「到达帧次」的口径。原样保留，
    # 由报告点明，避免被误读成「收到了比发出去还多的帧」。
    frames_includes_retransmit = $true
    loss_med            = $null
    loss_max            = $null
    nack_sent           = $null
    nack_seqs           = $null
    healed              = $null
    expired             = $null
    dropped             = $null
    fec_recovered       = $null
    fec_reds_med        = $null
    fec_cfg_updates     = $null
    fec_redundant       = $null
    fec_group           = $null
    fec_group_usage     = $null
    fec_groups_pruned   = $null
    rx_fps_med          = $null
    avg_frags           = $null
    kbps_med            = $null
    target_kbps         = $null
    cc_state            = $null
    cc_decreases        = $null
    kf_ratio            = $null
    av_synced           = $null
    audio_frames        = $null
    audio_played        = $null
    audio_depth_p50     = $null
    audio_depth_p95     = $null
    audio_dropped_late  = $null
    replay_rejected     = $null
    decrypt_failed      = $null
}

# daemon 绑定的真实端口：端口被占用时它会自动 +1 上移（最多 20 次），
# 所以「我传了 9100」不等于「它听 9100」—— 必须从日志读回来。
foreach ($line in $serverLines) {
    if ($line -match 'SideLens UDP 已启动\s+port=(?<p>\d+)') {
        $rx.bound_port = [int]$Matches['p']
        break
    }
}

if ($statsLines.Count -gt 0) {
    $rows = @()
    foreach ($line in $statsLines) { $rows += , (ConvertFrom-KvLine -Line $line) }

    # ── 累计口径：取最大值 ────────────────────────────────────────────────
    # 这几个字段只会单调增长，末行即全程总量；取 max 是为了防止日志被截断。
    $cumulative = @{
        frames             = 'frames'
        nack_sent          = 'nack_sent'
        nack_seqs          = 'nack_seqs'
        healed             = 'healed'
        expired            = 'expired'
        dropped            = 'dropped'
        fec_recovered      = 'fec_recovered'
        av_synced          = 'av_synced'
        audio_frames       = 'audio_frames'
        audio_played       = 'audio_played'
        audio_dropped_late = 'audio_dropped_late'
        replay_rejected    = 'replay_rejected'
        decrypt_failed     = 'decrypt_failed'
        fec_cfg_updates    = 'fec_cfg_updates'
        fec_groups_pruned  = 'fec_groups_pruned'
    }
    foreach ($target in $cumulative.Keys) {
        $col = @()
        foreach ($r in $rows) {
            if ($r.Contains($cumulative[$target])) {
                $d = ConvertTo-DoubleOrNull $r[$cumulative[$target]]
                if ($null -ne $d) { $col += $d }
            }
        }
        $rx[$target] = Get-MaxOrNull $col
    }

    # ── 百分位口径：取最后一行 ────────────────────────────────────────────
    # 这三个是 4000 样本的滑动窗口，不像 rx_fps 那样每 2 秒清零，
    # 所以最后一行最接近全程分布。
    $last = $rows[$rows.Count - 1]
    if ($last.Contains('net_ms')) {
        $p = Split-Pair $last['net_ms'] 3
        $rx.net_p50 = $p[0]; $rx.net_p95 = $p[1]; $rx.net_p99 = $p[2]
    }
    if ($last.Contains('playable_ms')) {
        $p = Split-Pair $last['playable_ms'] 2
        $rx.playable_p50 = $p[0]; $rx.playable_p95 = $p[1]
    }
    if ($last.Contains('hold_ms')) {
        $p = Split-Pair $last['hold_ms'] 2
        $rx.hold_p50 = $p[0]; $rx.hold_p95 = $p[1]
    }
    if ($last.Contains('audio_depth_ms')) {
        $p = Split-Pair $last['audio_depth_ms'] 2
        $rx.audio_depth_p50 = $p[0]; $rx.audio_depth_p95 = $p[1]
    }
    if ($last.Contains('jitter')) { $rx.jitter = ConvertTo-DoubleOrNull $last['jitter'] }
    if ($last.Contains('cc_state')) { $rx.cc_state = $last['cc_state'] }
    if ($last.Contains('cc_decreases')) { $rx.cc_decreases = ConvertTo-DoubleOrNull $last['cc_decreases'] }
    if ($last.Contains('target_kbps')) { $rx.target_kbps = ConvertTo-DoubleOrNull $last['target_kbps'] }
    if ($last.Contains('kf_ratio')) { $rx.kf_ratio = ConvertTo-DoubleOrNull $last['kf_ratio'] }
    if ($last.Contains('fec_redundant')) { $rx.fec_redundant = ConvertTo-DoubleOrNull $last['fec_redundant'] }
    if ($last.Contains('fec_group')) { $rx.fec_group = ConvertTo-DoubleOrNull $last['fec_group'] }
    if ($last.Contains('fec_group_usage')) { $rx.fec_group_usage = $last['fec_group_usage'] }

    # ── 窗口口径：跳过首尾，取中位数 ──────────────────────────────────────
    # 首窗是启动斜坡（FEC 配置还没协商完），末窗是残窗（发送端已退出）。
    # 这两个窗口的 rx_fps 都会明显偏低，混进中位数会污染结果。
    $interior = @()
    if ($rows.Count -ge 3) {
        $interior = $rows[1..($rows.Count - 2)]
    } elseif ($rows.Count -gt 0) {
        $interior = $rows
    }

    $windowKeys = @{
        loss_med     = 'loss'
        rx_fps_med   = 'rx_fps'
        kbps_med     = 'kbps'
        fec_reds_med = 'fec_reds'
        avg_frags    = 'avg_frags'
    }
    foreach ($target in $windowKeys.Keys) {
        $col = @()
        foreach ($r in $interior) {
            if ($r.Contains($windowKeys[$target])) {
                $d = ConvertTo-DoubleOrNull $r[$windowKeys[$target]]
                if ($null -ne $d) { $col += $d }
            }
        }
        $rx[$target] = Get-Median $col
    }

    # 最差窗口的丢包率：中位数会掩盖「某一窗炸了」，这里单独留一份
    $lossCol = @()
    foreach ($r in $interior) {
        if ($r.Contains('loss')) {
            $d = ConvertTo-DoubleOrNull $r['loss']
            if ($null -ne $d) { $lossCol += $d }
        }
    }
    $rx.loss_max = Get-MaxOrNull $lossCol
}

# ─────────────────────────────────────────────────────────────────────────────
# 发送端
# ─────────────────────────────────────────────────────────────────────────────

$senderLines = Read-LogLines -Path $SenderLog

$tx = [ordered]@{
    sent               = $null
    keyframes          = $null
    avg_fps            = $null
    mbps               = $null
    mbps_source        = $null
    fec_overhead       = $null
    retransmitted      = $null
    simulated_drops    = $null
    cache_served       = $null
    fec_reds           = $null
    fec_group          = $null
    fec_redundant      = $null
    fec_skipped_frames = $null
    fec_skip_overhead  = $null
    fec_redundant_usage = $null
    fec_initial_redundant = $null
    fec_adaptive       = $null
    fec_hyst_switches  = $null
    fec_loss_est       = $null
    fec_loss_last      = $null
    fec_loss_frame_last = $null
    fec_loss_avg_frags = $null
    fec_loss_windows   = $null
    fec_nack_frames    = $null
    fec_config_frames  = $null
    fec_group_usage    = $null
    elapsed_s          = $null
    audio_sent         = $null
    audio_pts_span_ms  = $null
}

$endLine = $null
$lastProgress = $null
foreach ($line in $senderLines) {
    if ($line -match '发送结束') { $endLine = $line }
    elseif ($line -match '发送中') { $lastProgress = $line }
}

if ($null -ne $endLine) {
    $kv = ConvertFrom-KvLine -Line $endLine
    if ($kv.Contains('sent')) { $tx.sent = ConvertTo-DoubleOrNull $kv['sent'] }
    if ($kv.Contains('keyframes')) { $tx.keyframes = ConvertTo-DoubleOrNull $kv['keyframes'] }
    if ($kv.Contains('avg_fps')) { $tx.avg_fps = ConvertTo-DoubleOrNull $kv['avg_fps'] }
    if ($kv.Contains('retransmitted')) { $tx.retransmitted = ConvertTo-DoubleOrNull $kv['retransmitted'] }
    if ($kv.Contains('simulated_drops')) { $tx.simulated_drops = ConvertTo-DoubleOrNull $kv['simulated_drops'] }
    if ($kv.Contains('cache_served')) { $tx.cache_served = ConvertTo-DoubleOrNull $kv['cache_served'] }
    if ($kv.Contains('fec_reds')) { $tx.fec_reds = ConvertTo-DoubleOrNull $kv['fec_reds'] }
    if ($kv.Contains('fec_group')) { $tx.fec_group = ConvertTo-DoubleOrNull $kv['fec_group'] }
    if ($kv.Contains('fec_redundant')) { $tx.fec_redundant = ConvertTo-DoubleOrNull $kv['fec_redundant'] }
    if ($kv.Contains('fec_skipped_frames')) { $tx.fec_skipped_frames = ConvertTo-DoubleOrNull $kv['fec_skipped_frames'] }
    if ($kv.Contains('fec_skip_overhead')) { $tx.fec_skip_overhead = ConvertTo-DoubleOrNull $kv['fec_skip_overhead'] }
    if ($kv.Contains('fec_redundant_usage')) { $tx.fec_redundant_usage = $kv['fec_redundant_usage'] }
    if ($kv.Contains('fec_initial_redundant')) { $tx.fec_initial_redundant = ConvertTo-DoubleOrNull $kv['fec_initial_redundant'] }
    if ($kv.Contains('fec_adaptive')) { $tx.fec_adaptive = $kv['fec_adaptive'] }
    if ($kv.Contains('fec_hyst_switches')) { $tx.fec_hyst_switches = ConvertTo-DoubleOrNull $kv['fec_hyst_switches'] }
    if ($kv.Contains('fec_loss_est')) { $tx.fec_loss_est = ConvertTo-DoubleOrNull $kv['fec_loss_est'] }
    if ($kv.Contains('fec_loss_last')) { $tx.fec_loss_last = ConvertTo-DoubleOrNull $kv['fec_loss_last'] }
    if ($kv.Contains('fec_loss_frame_last')) { $tx.fec_loss_frame_last = ConvertTo-DoubleOrNull $kv['fec_loss_frame_last'] }
    if ($kv.Contains('fec_loss_avg_frags')) { $tx.fec_loss_avg_frags = ConvertTo-DoubleOrNull $kv['fec_loss_avg_frags'] }
    if ($kv.Contains('fec_loss_windows')) { $tx.fec_loss_windows = ConvertTo-DoubleOrNull $kv['fec_loss_windows'] }
    if ($kv.Contains('fec_nack_frames')) { $tx.fec_nack_frames = ConvertTo-DoubleOrNull $kv['fec_nack_frames'] }
    if ($kv.Contains('fec_config_frames')) { $tx.fec_config_frames = ConvertTo-DoubleOrNull $kv['fec_config_frames'] }
    if ($kv.Contains('fec_group_usage')) { $tx.fec_group_usage = $kv['fec_group_usage'] }
    if ($kv.Contains('audio_sent')) { $tx.audio_sent = ConvertTo-DoubleOrNull $kv['audio_sent'] }
    if ($kv.Contains('audio_pts_span_ms')) { $tx.audio_pts_span_ms = ConvertTo-DoubleOrNull $kv['audio_pts_span_ms'] }

    # fec_overhead 是 "27.6%" 这种带百分号的，先剥掉再转数字
    if ($kv.Contains('fec_overhead')) {
        $raw = $kv['fec_overhead'] -replace '%', ''
        $tx.fec_overhead = ConvertTo-DoubleOrNull $raw
    }
    # elapsed 是 "6.00s"
    if ($kv.Contains('elapsed')) {
        $raw = $kv['elapsed'] -replace 's$', ''
        $tx.elapsed_s = ConvertTo-DoubleOrNull $raw
    }
}

# mbps 只在「发送中」有；拿不到就用字节数自己算，并标明来源
if ($null -ne $lastProgress) {
    $kv = ConvertFrom-KvLine -Line $lastProgress
    if ($kv.Contains('mbps')) {
        $tx.mbps = ConvertTo-DoubleOrNull $kv['mbps']
        if ($null -ne $tx.mbps) { $tx.mbps_source = 'log:发送中' }
    }
}
if ($null -eq $tx.mbps -and $null -ne $endLine) {
    $kv = ConvertFrom-KvLine -Line $endLine
    $dataB = ConvertTo-DoubleOrNull $kv['fec_data_bytes']
    $redB = ConvertTo-DoubleOrNull $kv['fec_red_bytes']
    if ($null -ne $dataB -and $null -ne $redB -and $null -ne $tx.elapsed_s -and $tx.elapsed_s -gt 0) {
        $tx.mbps = [Math]::Round((($dataB + $redB) / 1e6) / $tx.elapsed_s, 3)
        $tx.mbps_source = 'computed:(data+red)/elapsed'
    }
}

# ─────────────────────────────────────────────────────────────────────────────
# 缺失字段清单
# ─────────────────────────────────────────────────────────────────────────────

$missing = @()
foreach ($k in $rx.Keys) {
    if ($null -eq $rx[$k] -and $k -ne 'cc_state') { $missing += "rx.$k" }
}
foreach ($k in $tx.Keys) {
    if ($null -eq $tx[$k] -and $k -ne 'mbps_source') { $missing += "tx.$k" }
}
if ($statsLines.Count -eq 0) { $missing = @('rx.*') + $missing }

# 日志文件本身不存在 / 读不出来时，要把这件事单独报出来。
# 否则「路径写错」会伪装成「49 个字段全缺失」，让人去查正则。
$ioErrors = @()
if (-not (Test-Path -LiteralPath $ServerLog)) { $ioErrors += ("接收端日志不存在: " + $ServerLog) }
if (-not (Test-Path -LiteralPath $SenderLog)) { $ioErrors += ("发送端日志不存在: " + $SenderLog) }

[pscustomobject]@{
    ServerLog = $ServerLog
    SenderLog = $SenderLog
    Receiver  = [pscustomobject]$rx
    Sender    = [pscustomobject]$tx
    Missing   = @($missing | Select-Object -Unique)
    IOErrors  = @($ioErrors)
}
