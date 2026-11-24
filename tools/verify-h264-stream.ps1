# H.264 码流回归：用独立解析器验证 dump，而不是只看发送端自报的 encode_frames
#
# # 为什么需要这个脚本
#
# `--h264-dump` 写出的文件此前只用发送端日志的 `encode_frames` 数字确认过，
# 那是**自报**。如果编码器把一帧切成 7 个 slice、或 SPS 写错分辨率、
# 或关键帧标志与实际不符，发送端日志完全看不出来。
#
# `tools/h264-inspect.mjs` 是完全独立的实现（Node，自己写 bit reader），
# 两边对同一份 dump 得出相同结论才算真验证过。
#
# 用法: pwsh -File tools/verify-h264-stream.ps1

$ErrorActionPreference = "Continue"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

$env:RUSTUP_HOME = "D:\software\rust\rustup"
$env:CARGO_HOME = "D:\software\rust\cargo"
$env:PATH = "$env:CARGO_HOME\bin;" + $env:PATH

function Log($m) { Write-Host $m }

Get-Process | Where-Object { $_.ProcessName -like "*sidelens*" } | ForEach-Object {
    taskkill /F /PID $_.Id 2>&1 | Out-Null
}
Start-Sleep -Milliseconds 500

Log "=== H.264 码流回归 ==="
Log ""

# 分辨率矩阵：每档都验证 SPS 里的分辨率与请求的一致
$cases = @(
    @{ name = "720p";  size = "1280x720";  bytes = 8192 }
    @{ name = "1080p"; size = "1920x1080"; bytes = 32768 }
    @{ name = "360p";  size = "640x360";   bytes = 4096 }
)

$fail = 0
foreach ($c in $cases) {
    $dump = "target\verify-$($c.name).h264"
    Remove-Item $dump -ErrorAction SilentlyContinue
    $secs = 5

    Log "--- $($c.name) (--size $($c.size), frame_bytes=$($c.bytes)) ---"
    $out = & "target\release\sidelenssend.exe" --udp 127.0.0.1:29999 60 $($c.bytes) $secs --h264 --size $($c.size) --h264-dump $dump *>&1
    $snd = ($out | Where-Object { $_ -match "发送结束" } | Select-Object -Last 1)

    $encodeFrames = "?"
    if ($snd -match "encode_frames=(\d+)") { $encodeFrames = $Matches[1] }
    Log "  发送端自报 encode_frames=$encodeFrames"

    if (-not (Test-Path $dump)) {
        Log "  FAIL: dump 文件不存在"
        $fail++
        continue
    }

    # 独立解析
    $json = node tools\h264-inspect.mjs $dump --json 2>&1 | Out-String
    try { $r = $json | ConvertFrom-Json } catch {
        Log "  FAIL: 解析器输出不是合法 JSON"
        Log $json
        $fail++
        continue
    }

    # 断言 1：帧数与发送端自报一致（容差 1，允许 flush 延迟）
    $diff = [math]::Abs([int]$r.frames - [int]$encodeFrames)
    if ($diff -le 1) {
        Log "  OK   帧数 $($r.frames) 与自报 $encodeFrames 一致（差 $diff）"
    } else {
        Log "  FAIL 帧数 $($r.frames) 与自报 $encodeFrames 差 $diff（>1）"
        $fail++
    }

    # 断言 2：SPS 分辨率
    $want = $c.size -replace "x", "x"
    $got = if ($r.sps) { "$($r.sps.width)x$($r.sps.height)" } else { "无 SPS" }
    if ($got -eq $want) {
        Log "  OK   SPS 分辨率 $got"
    } else {
        Log "  FAIL SPS 分辨率期望 $want，实际 $got"
        $fail++
    }

    # 断言 3：必须有 SPS / PPS / IDR（否则无法解码）
    if ($r.pps -ge 1 -and $r.idrSlices -ge 1) {
        Log "  OK   PPS=$($r.pps) IDR=$($r.idrSlices)"
    } else {
        Log "  FAIL 缺 PPS 或 IDR（PPS=$($r.pps) IDR=$($r.idrSlices)）"
        $fail++
    }

    # 断言 3.5：GOP 必须恒定且等于 --gop 配置（默认 60）
    #
    # 这一项能抓到「关键帧周期被编码器改写」这类问题 ——
    # 它不会让码流非法，但会让 FEC / NACK / 抖动缓冲的假设全部偏移。
    if ($r.gopUnique.Count -eq 1 -and $r.gopUnique[0] -eq 60) {
        Log "  OK   GOP 恒为 $($r.gopUnique[0]) 帧（关键帧 $($r.keyframes) 个）"
    } elseif ($r.keyframes -le 1) {
        Log "  SKIP 只有 $($r.keyframes) 个关键帧，无法验证 GOP"
    } else {
        Log "  FAIL GOP 不恒定或不为 60：取值 $($r.gopUnique -join '/')"
        $fail++
    }

    # 断言 4：首个 SPS 必须在文件很靠前的位置（否则解码器起播会失败）
    if ($r.firstSpsAtByte -lt 64) {
        Log "  OK   首个 SPS 在第 $($r.firstSpsAtByte) 字节"
    } else {
        Log "  FAIL 首个 SPS 在第 $($r.firstSpsAtByte) 字节（应 <64）"
        $fail++
    }

    Log "  每帧字节: avg=$($r.frameBytes.avg) p50=$($r.frameBytes.p50) p95=$($r.frameBytes.p95) max=$($r.frameBytes.max)"
    Log "  切帧依据: $($r.frameBoundary)"
    Log ""
}


# ── 参数交互回归（第三十四轮补）────────────────────────────
#
# `--size` 加进 VALUED_FLAGS 后必须确认它不污染位置参数解析。
# 三件事要同时成立：
#   1. 位置参数（fps / frame_bytes / duration）仍被正确识别
#   2. --size 放在位置参数**前面**也能用（解析顺序无关）
#   3. 非法 --size 告警并回退，不硬失败

Log "--- 参数交互 ---"

$interact = @(
    @{ name = "normal";       argv = @("--udp","127.0.0.1:29998","30","4096","1","--h264","--size","640x360"); w = 640; h = 360; fps = 30 }
    @{ name = "size-first";   argv = @("--size","640x360","--udp","127.0.0.1:29998","30","4096","1","--h264"); w = 640; h = 360; fps = 30 }
    @{ name = "bad-size";     argv = @("--udp","127.0.0.1:29998","30","4096","1","--h264","--size","1920"); w = 1280; h = 720; fps = 30 }
)

foreach ($t in $interact) {
    $dump = "target\interact-$($t.name).h264"
    Remove-Item $dump -ErrorAction SilentlyContinue
    $argv = $t.argv + @("--h264-dump", $dump)
    $out = & "target\release\sidelenssend.exe" @argv *>&1
    $modeLine = ($out | Where-Object { $_ -match "UDP 模式" } | Select-Object -Last 1)

    # 断言 1：位置参数没被污染
    $fpsOk = $modeLine -match "fps=$($t.fps)\b"
    $bytesOk = $modeLine -match "frame_bytes=4096\b"
    if ($fpsOk -and $bytesOk) {
        Log "  OK   [$($t.name)] 位置参数未被污染"
    } else {
        Log "  FAIL [$($t.name)] 位置参数被污染: $modeLine"
        $fail++
    }

    # 断言 2：分辨率符合预期
    if (Test-Path $dump) {
        $j = (node tools\h264-inspect.mjs $dump --json 2>&1 | Out-String) | ConvertFrom-Json
        $got = "$($j.sps.width)x$($j.sps.height)"
        $want = "$($t.w)x$($t.h)"
        if ($got -eq $want) {
            Log "  OK   [$($t.name)] 分辨率 $got"
        } else {
            Log "  FAIL [$($t.name)] 分辨率期望 $want，实际 $got"
            $fail++
        }
    } else {
        Log "  FAIL [$($t.name)] dump 未生成"
        $fail++
    }
}
Log ""
Log "=== 结果: $fail 项失败 ==="
if ($fail -gt 0) { exit 1 }
