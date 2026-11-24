# SideLens 端到端回归（统一入口）
#
# # 为什么需要这个脚本
#
# 项目有 11 个分散的 verify-*.ps1，每个验证一个特性。但它们：
#
# 1. **用 debug 构建**（延迟数字不可信，`bench_present` 明确说必须用 release）
# 2. **各自手工拼命令行**（参数配对错了会得到误导性失败，见 README 的配对表）
# 3. **没有统一入口**（每一轮验证都要人肉回忆跑哪些、怎么跑）
#
# 结果是：第 39-56 轮的 20 多次端到端实测**无法复现** —— 下次要重新手工做一遍。
#
# 这个脚本把所有关键场景收成一条命令，每个场景都有明确的断言。
#
# # 用法
#
# ```powershell
# .\tools\verify-e2e.ps1              # 跑全部
# .\tools\verify-e2e.ps1 -Only udp    # 只跑某个场景（前缀匹配）
# .\tools\verify-e2e.ps1 -Quick       # 短时长（每个场景 5 秒）
# ```
#
# # 退出码
#
# 0 = 全部通过，1 = 有失败。可直接接 CI。

param(
    [string]$Only = "",
    [switch]$Quick,
    [string]$Root = "D:\dsh-workspace\sidelens"
)

$ErrorActionPreference = "Continue"
$OutputEncoding = [System.Text.Encoding]::UTF8
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8

$daemon = Join-Path $Root "target\release\sidelensd.exe"
$sender = Join-Path $Root "target\release\sidelenssend.exe"

if (-not (Test-Path $daemon)) { Write-Host "缺少 $daemon，先跑 cargo build --release" -ForegroundColor Red; exit 1 }
if (-not (Test-Path $sender)) { Write-Host "缺少 $sender，先跑 cargo build --release" -ForegroundColor Red; exit 1 }

$dur = if ($Quick) { 7 } else { 12 }
$script:pass = 0
$script:fail = 0
$script:failures = @()

# ── 工具函数 ────────────────────────────────────────────

function KillAll {
    Get-Process sidelensd,sidelenssend -ErrorAction SilentlyContinue |
        Stop-Process -Force -ErrorAction SilentlyContinue
    Start-Sleep -Milliseconds 400
}

# 从日志行里取一个 `key=value` 的值（去掉引号）
function GetField($line, $key) {
    # # 注意：`$key` 已经带 `=`（调用方传的是 "frames="）
    #
    # 第一版模板又加了一个 `=`，拼出来是 `frames=="?...`，永远匹配不上。
    # 表现为「日志里有这一行、但取不到值」，而且不报错。
    # `[^" ]+` 而不是 `\S+`：日志里带引号的值（如 `loss="0.029"`）
    # 用 `\S+` 会把结尾的引号一起吃掉，转 double 时直接抛异常。
    $pat = [regex]::Escape($key) + '"?([^" ]+)"?'
    $m = [regex]::Match($line, $pat)
    if ($m.Success) { return $m.Groups[1].Value }
    return $null
}

function AssertTrue($cond, $msg) {
    if ($cond) {
        Write-Host "    [PASS] $msg" -ForegroundColor Green
        $script:pass++
    } else {
        Write-Host "    [FAIL] $msg" -ForegroundColor Red
        $script:fail++
        $script:failures += $msg
    }
}

function AssertGe($actual, $min, $msg) {
    $ok = $null -ne $actual -and [double]$actual -ge [double]$min
    AssertTrue $ok "$msg（实际 $actual，期望 >= $min）"
}

function AssertEq($actual, $expect, $msg) {
    AssertTrue ($actual -eq $expect) "$msg（实际 $actual，期望 $expect）"
}

# 跑一个场景：起 daemon + sender，返回 (daemon 日志行, sender 日志行)
function RunScenario {
    param(
        [string]$Tag,
        [int]$Port,
        [string]$SenderArgs,
        [hashtable]$DaemonEnv = @{}
    )
    KillAll
    $dlog = Join-Path $Root "target\e2e-$Tag.d.log"
    $slog = Join-Path $Root "target\e2e-$Tag.s.log"
    Remove-Item $dlog,$slog -ErrorAction SilentlyContinue

    # 每个场景独立的 daemon 环境
    $saved = @{}
    foreach ($k in $DaemonEnv.Keys) {
        $saved[$k] = [Environment]::GetEnvironmentVariable($k)
        [Environment]::SetEnvironmentVariable($k, $DaemonEnv[$k])
    }

    # 显式设日志级别：不设的话某些环境下 INFO 会被吞掉，统计行就没了
    $savedLog = [Environment]::GetEnvironmentVariable("RUST_LOG")
    [Environment]::SetEnvironmentVariable("RUST_LOG", "info")
    $d = Start-Process -FilePath $daemon -ArgumentList @("--udp", "$Port") -RedirectStandardError $dlog -RedirectStandardOutput "$dlog.out" -PassThru -WindowStyle Hidden
    Start-Sleep -Milliseconds 900

    $s = Start-Process -FilePath $sender -ArgumentList $SenderArgs -RedirectStandardError $slog -RedirectStandardOutput "$slog.out" -PassThru -WindowStyle Hidden
    Start-Sleep -Seconds $dur

    if ($s -and -not $s.HasExited) { $s.Kill() }
    if ($d -and -not $d.HasExited) { $d.Kill() }
    Start-Sleep -Milliseconds 500

    # 恢复环境
    foreach ($k in $saved.Keys) { [Environment]::SetEnvironmentVariable($k, $saved[$k]) }
    [Environment]::SetEnvironmentVariable("RUST_LOG", $savedLog)

    $dtext = if (Test-Path "$dlog.out") { Get-Content "$dlog.out" -Raw } else { "" }
    $stext = if (Test-Path "$slog.out") { Get-Content "$slog.out" -Raw } else { "" }
        # # 为什么用 `frames=` 而不是「UDP 统计」做匹配
    #
    # PowerShell 读源文件的编码与日志文件的编码可能不一致（脚本 UTF-8、
    # 日志也可能是 UTF-8），中文字面量比较在跨编码时会静默失败 ——
    # 表现为「文件存在、行数对、但筛选结果为空」。
    #
    # 用纯 ASCII 的特征串（`frames=` 只在统计行出现）避开这个问题。
    $dstat = ($dtext -split [string][char]10 | Where-Object { $_ -match "frames=" } | Select-Object -Last 1)
        # 发送端有两种汇总行：
    # - 「发送中」每 5 秒一条，含 fec_reds / audio_kbps 等运行时统计
    # - 「发送结束」只在正常结束时输出，含 elapsed / 完整分布
    #
    # 被 Kill 掉时只有「发送中」，所以两级回退。
    $sstat = ($stext -split [string][char]10 | Where-Object { $_ -match "elapsed=" } | Select-Object -First 1)
    if (-not $sstat) {
        $sstat = ($stext -split [string][char]10 | Where-Object { $_ -match "sent=" } | Select-Object -Last 1)
    }
    # # 为什么用全局变量而不是 return
    #
    # PowerShell 函数的返回值会把**函数内所有未被消费的输出**都带上：
    # `Remove-Item`、`Start-Sleep`、`$s.Kill()` 的返回值都会混进结果，
    # 于是 `$r.d` 拿到的是一个数组而不是那一行日志。
    #
    # 第一版就是 return 一个 hashtable，结果断言里全是空值。
    $script:E2E_D = $dstat
    $script:E2E_SLOG = "$slog.out"
    if (-not $dstat) {
        Write-Host "    [warn] 未找到统计行，日志尾部：" -ForegroundColor Yellow
        $tail = ($dtext -split [string][char]10 | Select-Object -Last 3)
        foreach ($x in $tail) { Write-Host ("      " + $x.Substring(0, [Math]::Min(120, $x.Length))) }
    }
    $script:E2E_S = $sstat
}

function ShouldRun {
    param([string]$Name)
    if ([string]::IsNullOrEmpty($Only)) { return $true }
    return $Name.StartsWith($Only, [System.StringComparison]::OrdinalIgnoreCase)
}

# ── 场景 1：基础 UDP（无丢包） ─────────────────────────

if (ShouldRun "basic") {
    Write-Host ""
    Write-Host "=== [1/8] 基础 UDP（无丢包） ===" -ForegroundColor Cyan
    RunScenario -Tag "basic" -Port 28100 -SenderArgs @("--udp","127.0.0.1:28100","60","8192","$dur")
    AssertGe (GetField $script:E2E_D "frames=") 100 "收到足够帧"
    AssertGe (GetField $script:E2E_D "nack_sent=") 0 "nack_sent 字段存在"
    AssertTrue ($null -ne (GetField $script:E2E_D "playable_ms=")) "有可播放延迟统计"
}

# ── 场景 2：3% 丢包 + NACK ─────────────────────────────

if (ShouldRun "loss") {
    Write-Host ""
    Write-Host "=== [2/8] 3% 丢包 + NACK 重传 ===" -ForegroundColor Cyan
    RunScenario -Tag "loss" -Port 28110 -SenderArgs @("--udp","127.0.0.1:28110","60","8192","$dur","--loss","0.03")
    AssertGe (GetField $script:E2E_D "nack_sent=") 1 "发出了 NACK"
    AssertGe (GetField $script:E2E_D "healed=") 1 "有帧被重传救回"
    # 丢包率应该接近 3%（±2%）
    $loss = [double](GetField $script:E2E_D "loss=")
    AssertTrue ($loss -gt 0.005 -and $loss -lt 0.06) "反解出的丢包率在合理区间（$loss）"
}

# ── 场景 3：H.264 编解码闭环 ───────────────────────────

if (ShouldRun "h264") {
    Write-Host ""
    Write-Host "=== [3/8] H.264 编解码闭环 ===" -ForegroundColor Cyan
    RunScenario -Tag "h264" -Port 28120 -SenderArgs @("--udp","127.0.0.1:28120","60","8192","$dur","--h264","--size","1280x720","--gop","30") -DaemonEnv @{ SIDELENS_H264_DECODE = "1" }
    AssertGe (GetField $script:E2E_D "video_decoded=") 50 "解码出足够帧"
    AssertEq (GetField $script:E2E_D "video_decode_failed=") "0" "解码零失败"
    AssertEq (GetField $script:E2E_D "decrypt_failed=") "0" "无解密失败"
}

# ── 场景 4：音频 Opus 闭环 ─────────────────────────────

if (ShouldRun "audio") {
    Write-Host ""
    Write-Host "=== [4/8] Opus 音频闭环 ===" -ForegroundColor Cyan
    RunScenario -Tag "audio" -Port 28130 -SenderArgs @("--udp","127.0.0.1:28130","60","8192","$dur","--audio","--opus") -DaemonEnv @{ SIDELENS_OPUS = "1"; SIDELENS_AUDIO_OUT = "headless" }
    AssertGe (GetField $script:E2E_D "audio_decoded=") 50 "解码出足够音频帧"
    AssertEq (GetField $script:E2E_D "audio_decode_failed=") "0" "音频解码零失败"
    AssertGe (GetField $script:E2E_S "audio_kbps=") 1 "音频码率 > 1kbps（从发送端汇总行取）"
}

# ── 场景 5：加密 + 重放防护 ────────────────────────────

if (ShouldRun "crypto") {
    Write-Host ""
    Write-Host "=== [5/8] 帧加密 + 重放防护 ===" -ForegroundColor Cyan
    $psk = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f"
    RunScenario -Tag "crypto" -Port 28140 -SenderArgs @("--udp","127.0.0.1:28140","60","8192","$dur","--h264","--size","640x360","--psk",$psk) -DaemonEnv @{ SIDELENS_H264_DECODE = "1"; SIDELENS_PSK = $psk }
    AssertEq (GetField $script:E2E_D "decrypt_failed=") "0" "解密零失败"
    AssertGe (GetField $script:E2E_D "video_decoded=") 30 "加密链路能解码出帧"
}

# ── 场景 6：FEC + 关键帧保护 ───────────────────────────

if (ShouldRun "fec") {
    Write-Host ""
    Write-Host "=== [6/8] FEC 自适应 + 关键帧保护 ===" -ForegroundColor Cyan
    RunScenario -Tag "fec" -Port 28150 -SenderArgs @("--udp","127.0.0.1:28150","60","60000","$dur","--loss","0.08","--fec","2")
    AssertGe (GetField $script:E2E_S "fec_reds=") 10 "发出了 FEC 冗余包"
    AssertGe (GetField $script:E2E_D "fec_recovered=") 1 "有帧被 FEC 恢复"
    # # 关键帧保护的证据：`fec_redundant` 应高于初始值 2
    #
    # 第一版断言 `fec_redundant_usage` 非空 —— 但那个字段只在「发送结束」
    # 行里（被 Kill 时没有）。改断言运行时字段，它每 5 秒都打。
    #
    # 关键帧保护的效果是：高丢包下 `fec_redundant` 从初始 2 涨到 10+，
    # 而不是停在 2（第五十二轮修的 bug 就是它恒为 1）。
    $redundant = GetField $script:E2E_S "fec_redundant="
    AssertGe $redundant 3 "关键帧保护把冗余度从初始 2 提上去了"
}

# ── 场景 7：真实屏幕采集（DXGI） ──────────────────────

if (ShouldRun "screen") {
    Write-Host ""
    Write-Host "=== [7/8] 真实屏幕采集（DXGI → 缩放 → 编码） ===" -ForegroundColor Cyan
    RunScenario -Tag "screen" -Port 28160 `
        -SenderArgs @("--udp","127.0.0.1:28160","60","8192","$dur",
                      "--screen","dxgi","--size","1280x720",
                      "--h264","--gop","30") `
        -DaemonEnv @{ SIDELENS_H264_DECODE = "1" }
    # # 为什么断言「接收端解出画面」而不是「capture=true」
    #
    # `capture=` / `capture_process_us=` 这些字段只在「发送结束」汇总行里，
    # 而被 Kill 掉时只有「发送中」行 —— 取不到是**测试脚本的局限**，
    # 不是功能问题。
    #
    # 更本质的判据：如果采集没工作，发送端会退回合成图案，接收端仍能解码。
    # 所以「能解码」不能区分「采集成功」与「退回合成」。
    #
    # 真正能区分的是**日志里的采集初始化行**：
    # `屏幕采集已启用 backend=Dxgi ...`。这条只在采集真的建起来时才有。
    $sraw = if (Test-Path "$script:E2E_SLOG") { Get-Content "$script:E2E_SLOG" -Raw } else { "" }
    AssertTrue ($sraw -match "backend=Dxgi") "DXGI 采集已初始化（日志有 backend=Dxgi）"
    AssertTrue ($sraw -match "scale=Some") "缩放已生效（日志有 scale=Some）"
    # 接收端能解出画面
    AssertGe (GetField $script:E2E_D "video_decoded=") 30 "解码出真实画面"
    AssertEq (GetField $script:E2E_D "video_decode_failed=") "0" "解码零失败"
}

# ── 场景 8：真实音频采集（WASAPI loopback） ────────────

if (ShouldRun "loopback") {
    Write-Host ""
    Write-Host "=== [8/8] 真实音频采集（loopback → Opus） ===" -ForegroundColor Cyan
    RunScenario -Tag "loopback" -Port 28170 `
        -SenderArgs @("--udp","127.0.0.1:28170","60","4096","$dur",
                      "--audio","--opus","--audio-in","loopback") `
        -DaemonEnv @{ SIDELENS_OPUS = "1"; SIDELENS_AUDIO_OUT = "headless" }
    # # 断言的是「采集没崩」而不是「有声音」
    #
    # loopback 在系统静音时 `read()` 返回 0，此时发送端补零继续发 ——
    # 这是**正常状态**（见 docs/audio-in.md）。CI 机器上通常没有声音在播，
    # 所以不能断言「RMS > 0」。
    #
    # 能断言的是：链路跑通、解码零失败、时间线等距。
    AssertGe (GetField $script:E2E_D "audio_decoded=") 30 "解码出音频帧"
    AssertEq (GetField $script:E2E_D "audio_decode_failed=") "0" "音频解码零失败"
    AssertGe (GetField $script:E2E_S "audio_sent=") 30 "发送端持续产出音频帧"
    # 采集器真的建起来了（日志里初始化时会打 mode=wasapi-loopback）
    $sraw2 = if (Test-Path "$script:E2E_SLOG") { Get-Content "$script:E2E_SLOG" -Raw } else { "" }
    AssertTrue ($sraw2 -match "wasapi-loopback") "loopback 采集已初始化"
}

# ── 汇总 ───────────────────────────────────────────────

KillAll
Write-Host ""
Write-Host ("=" * 56)
if ($script:fail -eq 0) {
    Write-Host "全部通过：$script:pass 项断言" -ForegroundColor Green
    exit 0
} else {
    Write-Host "通过 $script:pass 项，失败 $script:fail 项：" -ForegroundColor Red
    foreach ($f in $script:failures) { Write-Host "  - $f" -ForegroundColor Red }
    exit 1
}
