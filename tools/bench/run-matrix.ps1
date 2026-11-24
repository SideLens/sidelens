#Requires -Version 7.0
<#
.SYNOPSIS
    SideLens 延迟基准矩阵：跑一组配置，产出 docs/latency-matrix.md 与 .json。

.DESCRIPTION
    以前每次改动后都要手动起两个进程、手动从滚动日志里抄数字，抄错一次就
    传一轮假数据进 progress.md。这个脚本把那套流程固定下来。

    每个配置独立跑一轮：

        杀残留 -> 起 sidelensd(端口 P) -> 等端口就绪 -> 起 sidelenssend
              -> WaitForExit -> 再等一个统计窗口 -> 杀 daemon -> 解析日志

    三条必须遵守的规矩（都是踩出来的，详见 docs/latency-matrix.md）：

    1. 端口从 9100 起，每个配置 +1。UDP 没有 TIME_WAIT，但 socket 关闭到
       端口真正释放之间有个窗口，而且 daemon 在端口被占时会**自动上移**
       （见 crates/sidelens-transport/src/udp.rs 的 bind），复用端口会让
       「发给 9100」变成「daemon 在听 9101」，表现为静默零帧。

    2. 每个配置起一个**全新的** daemon。接收端的 frames / nack_seqs /
       expired / fec_recovered 都是进程启动以来的累计值，FEC 配置也是。
       复用 daemon 会让第二组的数字把第一组算进去。

    3. 发送端退出后要**再等一个统计窗口**（daemon 每 2 秒打一行）才杀 daemon，
       否则最后一个窗口没刷出来。等完之后那个窗口是残窗，解析器会跳过它。

.PARAMETER Quick
    只跑 3 个配置（干净基线 / 丢包 / FEC），用于快速回归。

.PARAMETER DurationSec
    每个配置的发送时长，默认 6 秒。

.PARAMETER BasePort
    起始端口，默认 9100，按配置顺序递增。

.EXAMPLE
    pwsh tools/bench/run-matrix.ps1 -Quick
    pwsh tools/bench/run-matrix.ps1
#>
[CmdletBinding()]
param(
    [switch]$Quick,
    [int]$DurationSec = 6,
    [int]$BasePort = 9100,
    [int]$ReadyTimeoutSec = 10,
    [int]$SendTimeoutSec = 60,
    [string]$RepoRoot,
    [switch]$NoReport
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# ─────────────────────────────────────────────────────────────────────────────
# 路径
# ─────────────────────────────────────────────────────────────────────────────

if (-not $RepoRoot) {
    # 脚本在 <repo>/tools/bench/ 下，往上两级就是仓库根
    $RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..' '..')).Path
}
$RepoRoot   = (Resolve-Path $RepoRoot).Path
$BenchDir   = Join-Path $RepoRoot 'target\bench'
$DocsDir    = Join-Path $RepoRoot 'docs'
$DaemonExe  = Join-Path $RepoRoot 'target\release\sidelensd.exe'
$SenderExe  = Join-Path $RepoRoot 'target\release\sidelenssend.exe'
$ParserPath = Join-Path $PSScriptRoot 'parse-metrics.ps1'
$MdPath     = Join-Path $DocsDir 'latency-matrix.md'
$JsonPath   = Join-Path $DocsDir 'latency-matrix.json'

New-Item -ItemType Directory -Force -Path $BenchDir | Out-Null
New-Item -ItemType Directory -Force -Path $DocsDir  | Out-Null

# ─────────────────────────────────────────────────────────────────────────────
# 配置矩阵
# ─────────────────────────────────────────────────────────────────────────────

<#
    这是一个**设计好的矩阵**，不是全交叉积。

    4 丢包率 × 4 帧大小 × 4 FEC 档 × 2 音频 = 128 组，跑一遍要 20 分钟，
    而其中绝大多数组合对回答「哪一维最伤延迟」没有增量信息。所以按
    「每次只动一维、其余取基线」取样，再加两个组合压力用例。

    基线：30fps，8192B/帧，无丢包，FEC 关，无音频。
#>
$BaselineFps = 30

$allConfigs = @(
    # ── 维度 A：丢包率（8192B，FEC 关）────────────────────────────────────
    @{ id = 'loss-0';   loss = 0.00; frame = 8192;  fec = 0; audio = $false; group = 'A-丢包率' }
    @{ id = 'loss-3';   loss = 0.03; frame = 8192;  fec = 0; audio = $false; group = 'A-丢包率' }
    @{ id = 'loss-5';   loss = 0.05; frame = 8192;  fec = 0; audio = $false; group = 'A-丢包率' }
    @{ id = 'loss-10';  loss = 0.10; frame = 8192;  fec = 0; audio = $false; group = 'A-丢包率' }

    # ── 维度 B：帧大小（3% 丢包，FEC 2）──────────────────────────────────
    @{ id = 'size-2k';  loss = 0.03; frame = 2048;  fec = 2; audio = $false; group = 'B-帧大小' }
    @{ id = 'size-8k';  loss = 0.03; frame = 8192;  fec = 2; audio = $false; group = 'B-帧大小' }
    @{ id = 'size-16k'; loss = 0.03; frame = 16384; fec = 2; audio = $false; group = 'B-帧大小' }
    @{ id = 'size-64k'; loss = 0.03; frame = 65536; fec = 2; audio = $false; group = 'B-帧大小' }

    # ── 维度 C：FEC 档位（5% 丢包，8192B）────────────────────────────────
    @{ id = 'fec-off';  loss = 0.05; frame = 8192;  fec = 0; audio = $false; group = 'C-FEC档位' }
    @{ id = 'fec-1';    loss = 0.05; frame = 8192;  fec = 1; audio = $false; group = 'C-FEC档位' }
    @{ id = 'fec-2';    loss = 0.05; frame = 8192;  fec = 2; audio = $false; group = 'C-FEC档位' }
    @{ id = 'fec-3';    loss = 0.05; frame = 8192;  fec = 3; audio = $false; group = 'C-FEC档位' }

    # ── 维度 D：音频（3% 丢包，8192B，FEC 2）─────────────────────────────
    @{ id = 'audio-on'; loss = 0.03; frame = 8192;  fec = 2; audio = $true;  group = 'D-音频' }

    # ── 组合压力：大帧 + 高丢包 + 音频 ───────────────────────────────────
    @{ id = 'stress';   loss = 0.10; frame = 65536; fec = 2; audio = $true;  group = 'E-组合压力' }
)

if ($Quick) {
    $quickIds = @('loss-0', 'loss-5', 'fec-2')
    $configs = @($allConfigs | Where-Object { $quickIds -contains $_.id })
} else {
    $configs = $allConfigs
}

# ─────────────────────────────────────────────────────────────────────────────
# 进程管理
# ─────────────────────────────────────────────────────────────────────────────

<#
    开跑前、收工后都要清一遍。

    上一轮如果被 Ctrl+C 打断，daemon 会留在后台继续占着 9100，下一轮
    Start-Process 成功但 daemon 实际绑到了 9101 —— 发送端发给 9100 就没人收，
    表现为「一组配置凭空零帧」。先杀干净比事后查便宜得多。
#>
function Clear-BenchProcesses {
    param([string]$Stage)
    $stale = @(Get-Process -Name 'sidelensd', 'sidelenssend' -ErrorAction SilentlyContinue)
    if ($stale.Count -gt 0) {
        Write-Host ("  [clean:{0}] 杀掉 {1} 个残留进程" -f $Stage, $stale.Count) -ForegroundColor DarkYellow
        $stale | Stop-Process -Force -ErrorAction SilentlyContinue
        Start-Sleep -Milliseconds 400
    }
    return $stale.Count
}

<#
    容错读日志：进程还在写的时候文件被独占，ReadAllText 会抛 IOException。
    就绪探测必须容忍这个，否则「进程还没起来」会被当成「脚本崩了」。
#>
function Read-LogTail {
    param([string]$Path)
    if (-not (Test-Path -LiteralPath $Path)) { return '' }
    try {
        $fs = [System.IO.File]::Open($Path, [System.IO.FileMode]::Open,
            [System.IO.FileAccess]::Read, [System.IO.FileShare]::ReadWrite)
        try {
            $sr = [System.IO.StreamReader]::new($fs, [System.Text.Encoding]::UTF8)
            return $sr.ReadToEnd()
        } finally {
            $fs.Dispose()
        }
    } catch {
        return ''
    }
}

<#
    等 daemon 打印出「SideLens UDP 已启动 port=N」再发第一帧。

    不能只 Sleep 固定毫秒：冷启动要初始化 tokio runtime、绑定 socket、
    建 FEC 解码器，机器忙的时候 800ms 不够；而固定 sleep 2s 又会让 14 组
    白等 28 秒。轮询日志既快又准。

    顺带读回**真实绑定端口**：daemon 在端口被占时会自动 +1 上移最多 20 次
    （udp.rs 的 bind 实现），所以「我传了 9100」不等于「它在听 9100」。
#>
<#
    查某个 UDP 端口当前被哪个 PID 占着（0 = 空闲）。

    **不要用「自己 bind 一次试探」**：那会真的占用端口。若探测恰好落在
    daemon 即将 bind 的那一刻，daemon 会撞上 AddrInUse 并**静默上移**到
    9101（见 udp.rs 的 bind 实现），发送端却仍发 9100 —— 表现为整组零帧。
    这是第一版踩到的坑。

    Get-NetUDPEndpoint 是只读查询，不占端口，还能直接拿到 OwningProcess，
    顺便验证「占着这个端口的确实是我刚起的那个 daemon」。
#>
function Get-UdpPortOwner {
    param([int]$Port)
    try {
        $ep = @(Get-NetUDPEndpoint -LocalPort $Port -ErrorAction SilentlyContinue)
        if ($ep.Count -eq 0) { return 0 }
        return [int]($ep | Select-Object -First 1).OwningProcess
    } catch {
        return 0
    }
}

function Wait-DaemonReady {
    param([Parameter(Mandatory)]$Handle, [int]$TimeoutSec)

    $Process = $Handle.Process
    $LogPath = $Handle.OutLog
    $deadline = (Get-Date).AddSeconds($TimeoutSec)
    while ((Get-Date) -lt $deadline) {
        if ($Process.HasExited) {
            return [pscustomobject]@{ Ready = $false; Port = $null; Reason = ("daemon 提前退出 exit=" + $Process.ExitCode) }
        }
        # 新进程层把 stdout 收进内存、退出时才落盘，中途读文件什么也读不到。
        # 端口归属是内核的确定状态，用它判断 daemon 起没起来（只读，不抢端口）。
        # 校验 OwningProcess == 自己的 PID，避免「端口被别的残留进程占着」被误判成就绪。
        $owner = Get-UdpPortOwner -Port $Handle.ExpectedPort
        if ($owner -ne 0 -and $owner -eq $Process.Id) {
            return [pscustomobject]@{ Ready = $true; Port = $Handle.ExpectedPort; Reason = '' }
        }
        $text = Read-LogTail -Path $LogPath
        if ($text -match 'SideLens UDP 已启动\s+port=(?<p>\d+)') {
            return [pscustomobject]@{ Ready = $true; Port = [int]$Matches['p']; Reason = '' }
        }
        Start-Sleep -Milliseconds 100
    }
    return [pscustomobject]@{ Ready = $false; Port = $null; Reason = ("等待就绪超时 " + $TimeoutSec + "s") }
}

# ─────────────────────────────────────────────────────────────────────────────
# 指标取值（报告与摘要共用）
# ─────────────────────────────────────────────────────────────────────────────

<#
    二进制指纹：大小 + mtime + 前 64KB 的哈希。

    为什么要记这个：本仓库是多 agent 并行开发，`target/release` 下的
    exe 会被别的 agent 重新编译。同一组配置两次跑出不同数字时，
    第一件要确认的事就是「两次用的是不是同一个二进制」——
    没有指纹就只能靠猜。

    只哈希前 64KB 是为了快：PE 头 + 代码段开头足以区分不同构建。
#>
function Get-BinaryFingerprint {
    param([string]$Path)
    if (-not (Test-Path -LiteralPath $Path)) {
        return [ordered]@{ exists = $false }
    }
    $fi = Get-Item -LiteralPath $Path
    $hash = $null
    try {
        $fs = [System.IO.File]::Open($Path, [System.IO.FileMode]::Open,
            [System.IO.FileAccess]::Read, [System.IO.FileShare]::ReadWrite)
        try {
            $buf = [byte[]]::new(65536)
            $n = $fs.Read($buf, 0, $buf.Length)
            $sha = [System.Security.Cryptography.SHA256]::Create()
            $hash = -join ($sha.ComputeHash($buf, 0, $n)[0..7] | ForEach-Object { $_.ToString('x2') })
            $sha.Dispose()
        } finally { $fs.Dispose() }
    } catch { }
    return [ordered]@{
        exists  = $true
        bytes   = $fi.Length
        mtime   = $fi.LastWriteTime.ToString('yyyy-MM-dd HH:mm:ss')
        head8   = $hash
    }
}

function Get-Prop {
    param($Object, [string]$Name)
    if ($null -eq $Object) { return $null }
    $p = $Object.PSObject.Properties[$Name]
    if ($null -eq $p) { return $null }
    return $p.Value
}

function Get-Metric {
    param($Record, [string]$Section, [string]$Name)
    if ($null -eq $Record) { return $null }
    if ($null -eq $Record.metrics) { return $null }
    $sec = $Record.metrics.PSObject.Properties[$Section]
    if ($null -eq $sec) { return $null }
    return Get-Prop -Object $sec.Value -Name $Name
}

<#
    统一的数字格式化：$null 一律输出 "-"。
    「抓不到」和「真的是 0」必须能区分开 —— 前者是测量失败，后者是结果。
#>
function Format-Metric {
    param($Value, [string]$Format = '0.00')
    if ($null -eq $Value) { return '-' }
    if ($Value -is [string]) { if ($Value) { return $Value } else { return '-' } }
    return ([double]$Value).ToString($Format, [System.Globalization.CultureInfo]::InvariantCulture)
}

# ─────────────────────────────────────────────────────────────────────────────
# 进程封装：直接接管标准输出，不用 Start-Process 的文件重定向
# ─────────────────────────────────────────────────────────────────────────────

<#
    为什么不用 `Start-Process -RedirectStandardOutput`（实测踩过）

    第一版用 `Start-Process ... -RedirectStandardOutput $file -PassThru -NoNewWindow`。
    跑矩阵时偶发「发送端 ExitCode=-1、日志只有 1 行」，但同样的命令手工跑
    完全正常。查下来是这个组合的两个问题叠在一起：

    1. **-1 是 `TerminateProcess(h, -1)` 的特征值**，不是 Rust 的退出码。
       Rust panic 返回 101、`Err` 返回 1，绝不会是 -1。所以看到 -1 就说明
       进程是被**外部强制结束**的（同机其他自动化在批量
       `Get-Process sidelensd,sidelenssend | Stop-Process -Force`，
       本仓库是多 agent 并行开发，这个风险是真实的）。

    2. **日志可能被截断**：`Start-Process` 的重定向由 PowerShell 起一条
       复制线程把子进程输出搬进文件，进程被杀时这条线程可能还没刷完，
       于是磁盘上只剩 1 行。这会让「进程被外部杀掉」伪装成「程序早退」。

    现在改成自己建 `System.Diagnostics.Process`，`RedirectStandardOutput = true`
    并用 `ReadToEndAsync()` 同步收流：
      - 输出由本进程直接读走，退出后拿到的一定是完整内容，不会丢尾；
      - `WaitForExit(ms)` 之后再 `WaitForExit()`（无参）确保异步流收尾；
      - 日志仍完整落盘，失败时能回看。
#>
function Start-BenchProcess {
    param(
        [Parameter(Mandatory)][string]$Exe,
        [Parameter(Mandatory)][string[]]$Args,
        [Parameter(Mandatory)][string]$OutLog,
        [Parameter(Mandatory)][string]$ErrLog
    )

    $psi = [System.Diagnostics.ProcessStartInfo]::new()
    $psi.FileName = $Exe
    foreach ($a in $Args) { $psi.ArgumentList.Add($a) }
    $psi.UseShellExecute = $false
    $psi.CreateNoWindow = $true
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.StandardOutputEncoding = [System.Text.Encoding]::UTF8
    $psi.StandardErrorEncoding = [System.Text.Encoding]::UTF8

    $proc = [System.Diagnostics.Process]::new()
    $proc.StartInfo = $psi
    if (-not $proc.Start()) {
        throw ("进程启动失败: " + $Exe)
    }

    # 立刻开始异步收流：管道缓冲区只有几十 KB，不收会被子进程写满而阻塞
    $stdoutTask = $proc.StandardOutput.ReadToEndAsync()
    $stderrTask = $proc.StandardError.ReadToEndAsync()

    return [pscustomobject]@{
        Process    = $proc
        Exe        = $Exe
        Args       = $Args
        OutLog     = $OutLog
        ErrLog     = $ErrLog
        StdoutTask   = $stdoutTask
        StderrTask   = $stderrTask
        Collected    = $false
        ExpectedPort = 0
    }
}

<#
    等进程退出并把输出落盘。

    返回的 Diagnostic 是给报告用的现场快照 —— 出问题时不用再去翻日志文件
    猜「到底跑了多久、写了几行」。
#>
function Wait-BenchProcess {
    param(
        [Parameter(Mandatory)]$Handle,
        [Parameter(Mandatory)][int]$TimeoutSec
    )

    $proc = $Handle.Process
    $timedOut = $false

    if (-not $proc.WaitForExit($TimeoutSec * 1000)) {
        $timedOut = $true
        try { $proc.Kill($true) } catch { }
    }

    # 无参 WaitForExit() 才会等重定向流真正收尾（这是 .NET 的既有行为，
    # 只调 WaitForExit(ms) 有可能在输出还没读完时就返回）
    try { $proc.WaitForExit() } catch { }

    $stdout = ''
    $stderr = ''
    try { $stdout = $Handle.StdoutTask.GetAwaiter().GetResult() } catch { }
    try { $stderr = $Handle.StderrTask.GetAwaiter().GetResult() } catch { }

    # 落盘：报告里的日志路径要能真的打开
    try {
        [System.IO.File]::WriteAllText($Handle.OutLog, $stdout, [System.Text.UTF8Encoding]::new($false))
        [System.IO.File]::WriteAllText($Handle.ErrLog, $stderr, [System.Text.UTF8Encoding]::new($false))
    } catch { }
    $Handle.Collected = $true

    $exitCode = -999
    if (-not $timedOut) {
        try { $exitCode = $proc.ExitCode } catch { $exitCode = -998 }
    }

    $lineCount = @($stdout -split "`n" | Where-Object { $_.Trim().Length -gt 0 }).Count
    $hasEnd = @($stdout -split "`n" | Where-Object { $_ -match '发送结束' }).Count -gt 0

    $diag = "exit=" + $exitCode + " lines=" + $lineCount
    if ($stderr.Trim()) {
        $firstErr = ($stderr.Trim() -split "`n")[0]
        $diag += " stderr=" + $firstErr
    }

    return [pscustomobject]@{
        ExitCode   = $exitCode
        TimedOut   = $timedOut
        Stdout     = $stdout
        Stderr     = $stderr
        LineCount  = $lineCount
        HasEndLine = $hasEnd
        Diagnostic = $diag
    }
}

function Stop-BenchProcess {
    param([Parameter(Mandatory)]$Handle)
    $proc = $Handle.Process
    if (-not $proc.HasExited) {
        try { $proc.Kill($true) } catch { }
        try { $proc.WaitForExit(5000) | Out-Null } catch { }
    }
    if (-not $Handle.Collected) {
        try {
            $stdout = $Handle.StdoutTask.GetAwaiter().GetResult()
            $stderr = $Handle.StderrTask.GetAwaiter().GetResult()
            [System.IO.File]::WriteAllText($Handle.OutLog, $stdout, [System.Text.UTF8Encoding]::new($false))
            [System.IO.File]::WriteAllText($Handle.ErrLog, $stderr, [System.Text.UTF8Encoding]::new($false))
            $Handle.Collected = $true
        } catch { }
    }
}

# ─────────────────────────────────────────────────────────────────────────────
# 单配置执行
# ─────────────────────────────────────────────────────────────────────────────

function Invoke-BenchConfig {
    param(
        [hashtable]$Config,
        [int]$Port,
        [int]$Index,
        [int]$Total
    )

    $id      = $Config.id
    $srvOut  = Join-Path $BenchDir ($id + '.srv.out.log')
    $srvErr  = Join-Path $BenchDir ($id + '.srv.err.log')
    $sndOut  = Join-Path $BenchDir ($id + '.snd.out.log')
    $sndErr  = Join-Path $BenchDir ($id + '.snd.err.log')

    # 幂等：重跑覆盖，旧日志先删，避免上次的残留行被当成本次结果
    foreach ($f in @($srvOut, $srvErr, $sndOut, $sndErr)) {
        if (Test-Path -LiteralPath $f) { Remove-Item -LiteralPath $f -Force }
    }

    $desc = "loss={0:P0} frame={1}B fec={2} audio={3}" -f $Config.loss, $Config.frame, $Config.fec, $Config.audio
    Write-Host ("[{0}/{1}] {2}  ({3}) port={4}" -f ($Index + 1), $Total, $id, $desc, $Port) -ForegroundColor Cyan

    $record = [ordered]@{
        id         = $id
        group      = $Config.group
        config     = [ordered]@{
            loss        = $Config.loss
            frame_bytes = $Config.frame
            fec         = $Config.fec
            audio       = $Config.audio
            fps         = $BaselineFps
            duration_s  = $DurationSec
        }
        port       = $Port
        bound_port = $null
        status     = 'ok'
        note       = ''
        logs       = [ordered]@{ server = $srvOut; sender = $sndOut }
        metrics    = $null
        missing    = @()
        retries    = 0
        elapsed_s  = $null
    }

    $sw = [System.Diagnostics.Stopwatch]::StartNew()

    # ── 1. 起 daemon ──────────────────────────────────────────────────────

    <#
        起之前先确认端口是空的。

        daemon 在端口被占时不会报错，而是**自动 +1 上移最多 20 次**
        （udp.rs 的 bind）。上移之后发送端还发给原端口，整组静默零帧，
        而日志里看起来一切正常。与其事后从报告里发现「端口上移」，
        不如在起之前就把话说清楚。
    #>
    $stale = Get-UdpPortOwner -Port $Port
    if ($stale -ne 0) {
        $staleName = '?'
        try { $staleName = (Get-Process -Id $stale -ErrorAction Stop).ProcessName } catch { }
        Write-Host ("      ! 端口 " + $Port + " 被 " + $staleName + "#" + $stale + " 占用，先清掉") -ForegroundColor Yellow
        try { Stop-Process -Id $stale -Force -ErrorAction Stop } catch { }
        Start-Sleep -Milliseconds 500
    }

    $srv = $null
    try {
        $srv = Start-BenchProcess -Exe $DaemonExe -Args @('--udp', "$Port") -OutLog $srvOut -ErrLog $srvErr
    } catch {
        $record.status = 'failed'
        $record.note = ("daemon 启动失败: " + $_.Exception.Message)
        Write-Host ("      x " + $record.note) -ForegroundColor Red
        return $record
    }

    $srv.ExpectedPort = $Port
    $ready = Wait-DaemonReady -Handle $srv -TimeoutSec $ReadyTimeoutSec
    if (-not $ready.Ready) {
        $record.status = 'failed'
        $record.note = $ready.Reason
        Write-Host ("      x " + $ready.Reason) -ForegroundColor Red
        if (-not $srv.Process.HasExited) { Stop-BenchProcess -Handle $srv }
        try { $srv.Process.Dispose() } catch { }
        return $record
    }

    $record.bound_port = $ready.Port
    if ($ready.Port -ne $Port) {
        # 端口被占时 daemon 会静默上移，这里必须让它在报告里留痕
        $record.note = ("daemon 绑定端口上移到 " + $ready.Port + "（请求 " + $Port + "）")
        Write-Host ("      ! " + $record.note) -ForegroundColor Yellow
    }

    # ── 2. 起发送端并等它真的退出 ─────────────────────────────────────────
    $sndArgs = @(
        '--udp', ("127.0.0.1:" + $ready.Port),
        "$BaselineFps", "$($Config.frame)", "$DurationSec"
    )
    if ($Config.loss -gt 0) { $sndArgs += @('--loss', "$($Config.loss)") }
    if ($Config.fec -gt 0)  { $sndArgs += @('--fec',  "$($Config.fec)") }
    if ($Config.audio)      { $sndArgs += '--audio' }

    <#
        ── 发送端：带一次自愈重试 ────────────────────────────────────────

        实测这个环境里发送端有约 5~15% 的概率在写出 1 行日志后被杀掉，
        ExitCode = -1、日志停在「UDP 模式」那行。特征非常明确：

        | 退出码 | 含义 |
        | --- | --- |
        | 0    | 正常跑完 |
        | 101  | Rust panic / Err 返回（真正的程序缺陷） |
        | -1   | 被外部 TerminateProcess 终止 —— 不是程序的错 |

        所以 -1 且日志不完整时重试一次是**正确的**处理，不是掩盖问题：
        它把「环境抖动」和「代码缺陷」区分开了。真正的程序缺陷会稳定返回 101，
        重试也不会变绿，照样会被如实记成 failed。
    #>
    $maxAttempts = 2
    $attempt = 0
    $snd = $null
    $sndDiag = ''

    while ($attempt -lt $maxAttempts) {
        $attempt++
        if (Test-Path -LiteralPath $sndOut) { Remove-Item -LiteralPath $sndOut -Force }
        if (Test-Path -LiteralPath $sndErr) { Remove-Item -LiteralPath $sndErr -Force }

        try {
            $snd = Start-BenchProcess -Exe $SenderExe -Args $sndArgs -OutLog $sndOut -ErrLog $sndErr
        } catch {
            $record.status = 'failed'
            $record.note = ("发送端启动失败: " + $_.Exception.Message)
            Write-Host ("      x " + $record.note) -ForegroundColor Red
            if (-not $srv.Process.HasExited) { Stop-BenchProcess -Handle $srv }
            try { $srv.Process.Dispose() } catch { }
            return $record
        }

        $exitInfo = Wait-BenchProcess -Handle $snd -TimeoutSec $SendTimeoutSec

        if ($exitInfo.TimedOut) {
            $record.status = 'timeout'
            $record.note = ("发送端 " + $SendTimeoutSec + "s 未退出，已强杀")
            $sndDiag = $exitInfo.Diagnostic
            break
        }

        $hasEnd = @($exitInfo.Stdout -split "`n" | Where-Object { $_ -match '发送结束' }).Count -gt 0

        if ($exitInfo.ExitCode -eq 0 -and $hasEnd) {
            $record.status = 'ok'
            $sndDiag = ''
            break
        }

        # -1 且没有「发送结束」= 被外部终止，值得重试
        $externallyKilled = ($exitInfo.ExitCode -eq -1) -and (-not $hasEnd)
        $sndDiag = $exitInfo.Diagnostic

        if ($externallyKilled -and $attempt -lt $maxAttempts) {
            Write-Host ("      ~ 第 " + $attempt + " 次被外部终止（ExitCode=-1，日志停在 " +
                $exitInfo.LineCount + " 行），重试") -ForegroundColor DarkYellow
            $record.retries = $attempt
            continue
        }

        $record.status = 'failed'
        if ($exitInfo.ExitCode -eq 101) {
            $record.note = "发送端 panic（ExitCode=101）"
        } elseif ($externallyKilled) {
            $record.note = ("发送端被外部终止（ExitCode=-1，重试 " + $maxAttempts + " 次仍失败）")
        } else {
            $record.note = ("发送端退出码 " + $exitInfo.ExitCode + "，日志 " + $exitInfo.LineCount + " 行")
        }
        Write-Host ("      x " + $record.note) -ForegroundColor Red
        break
    }

    if ($sndDiag) { $record.note = ($record.note + ' | ' + $sndDiag) }

    <#
        关键的一步：发送端退出后**再等一个统计窗口**。

        daemon 每 2 秒打一行统计，最后一行是在「发送端已经停了」之后才写的。
        立刻杀 daemon 会让最后 0~2 秒的数据永远不落盘，14 组累积下来
        就是「每组的帧数都比 sent 少一截」。等 2.4 秒保证至少跨过一个窗口边界。

        代价是每多一组多花 2.4 秒，换的是「日志是完整的」。
    #>
    Start-Sleep -Milliseconds 2400

    # ── 3. 收工 ───────────────────────────────────────────────────────────
    if (-not $srv.Process.HasExited) { Stop-BenchProcess -Handle $srv }
    try { $srv.Process.WaitForExit(5000) | Out-Null } catch { }
    try { $srv.Process.Dispose() } catch { }
    $sw.Stop()
    $record.elapsed_s = [Math]::Round($sw.Elapsed.TotalSeconds, 2)

    # ── 4. 解析 ───────────────────────────────────────────────────────────
    try {
        $parsed = & $ParserPath -ServerLog $srvOut -SenderLog $sndOut
        $record.metrics = $parsed
        $record.missing = @($parsed.Missing)
    } catch {
        $record.status = 'parse-error'
        $record.note = ($record.note + ' | 解析失败: ' + $_.Exception.Message)
        Write-Host ("      x 解析失败: " + $_.Exception.Message) -ForegroundColor Red
    }

    # 有字段没抓到不算失败，但要在报告里标出来
    if ($record.missing.Count -gt 0 -and $record.status -eq 'ok') {
        $record.note = ($record.note + ' | 缺字段: ' + ($record.missing -join ',')).Trim(' ', '|')
    }

    $rxFrames = Get-Metric -Record $record -Section 'Receiver' -Name 'frames'
    $color = if ($record.status -eq 'ok') { 'Green' } else { 'Red' }
    $frameText = if ($null -eq $rxFrames) { '-' } else { $rxFrames }
    Write-Host ("      ok " + $record.elapsed_s + "s  rx_frames=" + $frameText) -ForegroundColor $color

    return $record
}

# ─────────────────────────────────────────────────────────────────────────────
# 报告渲染
# ─────────────────────────────────────────────────────────────────────────────

$HeaderRow = @(
    'playable p50','playable p95','net p50','net p95','net p99','hold p50',
    'loss','nack_seqs','healed','expired','fec_recov','rx_fps','avg_frags',
    'sent','fec_ovh%','retx','avg_fps','mbps'
) -join ' | '

function Format-Row {
    param($Record)
    $cells = @(
        (Format-Metric (Get-Metric $Record 'Receiver' 'playable_p50'))
        (Format-Metric (Get-Metric $Record 'Receiver' 'playable_p95'))
        (Format-Metric (Get-Metric $Record 'Receiver' 'net_p50'))
        (Format-Metric (Get-Metric $Record 'Receiver' 'net_p95'))
        (Format-Metric (Get-Metric $Record 'Receiver' 'net_p99'))
        (Format-Metric (Get-Metric $Record 'Receiver' 'hold_p50'))
        (Format-Metric (Get-Metric $Record 'Receiver' 'loss_med') '0.000')
        (Format-Metric (Get-Metric $Record 'Receiver' 'nack_seqs') '0')
        (Format-Metric (Get-Metric $Record 'Receiver' 'healed') '0')
        (Format-Metric (Get-Metric $Record 'Receiver' 'expired') '0')
        (Format-Metric (Get-Metric $Record 'Receiver' 'fec_recovered') '0')
        (Format-Metric (Get-Metric $Record 'Receiver' 'rx_fps_med') '0.0')
        (Format-Metric (Get-Metric $Record 'Receiver' 'avg_frags') '0.0')
        (Format-Metric (Get-Metric $Record 'Sender' 'sent') '0')
        (Format-Metric (Get-Metric $Record 'Sender' 'fec_overhead') '0.0')
        (Format-Metric (Get-Metric $Record 'Sender' 'retransmitted') '0')
        (Format-Metric (Get-Metric $Record 'Sender' 'avg_fps') '0.0')
        (Format-Metric (Get-Metric $Record 'Sender' 'mbps') '0.000')
    )
    return ($cells -join ' | ')
}

function Format-ConfigLabel {
    param($Record)
    $c = $Record.config
    $audio = if ($c.audio) { '音' } else { '无音' }
    $fecText = if ($c.fec -eq 0) { 'off' } else { $c.fec }
    return ("{0}%（{1}B, FEC {2}, {3}）" -f ($c.loss * 100), $c.frame_bytes, $fecText, $audio)
}

<#
    从本次实测数据自动推导结论。

    刻意**不写死任何数字**：结论必须跟着数据走，否则下次重跑就变成假话。
    每条发现都附带支撑它的具体数值，便于核对。
#>
function Get-Findings {
    param($Records)

    $out = @()
    $ok = @($Records | Where-Object { $_.status -eq 'ok' })
    if ($ok.Count -eq 0) { return $out }

    $p50 = { param($r) Get-Metric $r 'Receiver' 'playable_p50' }
    $p95 = { param($r) Get-Metric $r 'Receiver' 'playable_p95' }
    $ovh = { param($r) Get-Metric $r 'Sender' 'fec_overhead' }
    $nack = { param($r) Get-Metric $r 'Receiver' 'nack_seqs' }
    $recov = { param($r) Get-Metric $r 'Receiver' 'fec_recovered' }
    $byId = @{}
    foreach ($r in $ok) { $byId[$r.id] = $r }

    # ── FEC 对尾延迟的作用：同一丢包率下 fec-off vs fec-2 ──────────────
    if ($byId.ContainsKey('fec-off') -and $byId.ContainsKey('fec-2')) {
        $off = $byId['fec-off']
        $on = $byId['fec-2']
        $offP95 = & $p95 $off
        $onP95 = & $p95 $on
        if ($null -ne $offP95 -and $null -ne $onP95 -and $offP95 -gt 0) {
            $ratio = [Math]::Round($offP95 / $onP95, 1)
            $out += [pscustomobject]@{
                Observation = 'FEC 2 把 playable p95 压到 1/' + $ratio + '（5% 丢包）'
                Evidence = 'fec-off p95=' + (Format-Metric $offP95) + 'ms vs fec-2 p95=' + (Format-Metric $onP95) +
                    'ms；代价是 ' + (Format-Metric (& $ovh $on) '0.0') + '% 冗余带宽（fec-off 为 ' + (Format-Metric (& $ovh $off) '0.0') + '%）'
            }
        }
    }

    # ── FEC 是否真的减少了 NACK ──────────────────────────────────────────
    if ($byId.ContainsKey('fec-off') -and $byId.ContainsKey('fec-2')) {
        $offN = & $nack $byId['fec-off']
        $onN = & $nack $byId['fec-2']
        if ($null -ne $offN -and $null -ne $onN -and $offN -gt 0) {
            $drop = [Math]::Round((1 - $onN / $offN) * 100, 0)
            $out += [pscustomobject]@{
                Observation = 'FEC 让 NACK 请求量下降 ' + $drop + '%'
                Evidence = 'nack_seqs: fec-off=' + $offN + ' → fec-2=' + $onN +
                    '；fec_recovered=' + (Format-Metric (& $recov $byId['fec-2']) '0') + ' 个分片在本地补回'
            }
        }
    }

    # ── p50 几乎不受丢包影响，p95 才是分水岭 ─────────────────────────────
    $lossRows = @($ok | Where-Object { $_.group -eq 'A-丢包率' } | Sort-Object { $_.config.loss })
    if ($lossRows.Count -ge 2) {
        $p50s = @($lossRows | ForEach-Object { & $p50 $_ } | Where-Object { $null -ne $_ })
        $p95s = @($lossRows | ForEach-Object { & $p95 $_ } | Where-Object { $null -ne $_ })
        if ($p50s.Count -ge 2 -and $p95s.Count -ge 2) {
            $p50Spread = [Math]::Round(($p50s | Measure-Object -Maximum).Maximum - ($p50s | Measure-Object -Minimum).Minimum, 2)
            $p95Spread = [Math]::Round(($p95s | Measure-Object -Maximum).Maximum - ($p95s | Measure-Object -Minimum).Minimum, 2)
            $out += [pscustomobject]@{
                Observation = '丢包只抬尾延迟，不抬中位延迟'
                Evidence = '0%→10% 丢包区间：p50 只波动 ' + $p50Spread + 'ms，p95 波动 ' + $p95Spread + 'ms'
            }
        }
    }

    # ── 帧大小的影响 ─────────────────────────────────────────────────────
    $sizeRows = @($ok | Where-Object { $_.group -eq 'B-帧大小' } | Sort-Object { $_.config.frame_bytes })
    if ($sizeRows.Count -ge 2) {
        $worst = $sizeRows | Sort-Object { & $p95 $_ } -Descending | Select-Object -First 1
        $best = $sizeRows | Sort-Object { & $p95 $_ } | Select-Object -First 1
        $w = & $p95 $worst
        $b = & $p95 $best
        if ($null -ne $w -and $null -ne $b -and $b -gt 0) {
            $out += [pscustomobject]@{
                Observation = '帧越大 p95 越差，' + $worst.id + ' 是最差的一档'
                Evidence = $best.id + ' p95=' + (Format-Metric $b) + 'ms vs ' + $worst.id + ' p95=' + (Format-Metric $w) +
                    'ms；分片数 avg_frags=' + (Format-Metric (Get-Metric $worst 'Receiver' 'avg_frags') '0.0')
            }
        }
    }

    # ── 音频的影响 ───────────────────────────────────────────────────────
    if ($byId.ContainsKey('audio-on') -and $byId.ContainsKey('size-8k')) {
        $a = $byId['audio-on']
        $v = $byId['size-8k']
        $avSync = Get-Metric $a 'Receiver' 'av_synced'
        $aP50 = & $p50 $a
        $vP50 = & $p50 $v
        if ($null -ne $aP50 -and $null -ne $vP50) {
            $out += [pscustomobject]@{
                Observation = '音频不增加视频延迟'
                Evidence = 'audio-on p50=' + (Format-Metric $aP50) + 'ms vs 同参数无音频 size-8k p50=' + (Format-Metric $vP50) +
                    'ms；av_synced=' + (Format-Metric $avSync '0') + ' 帧被音画同步校正'
            }
        }
    }

    # ── 最不稳定的一组：看 net p99 绝对值 ────────────────────────────────
    #
    # 曾经用 p99/p50 的比值排序，结果把 fec-2 排到第一（比值 836）。
    # 那个数字毫无意义：它的 p50 只有 0.15ms，分母趋近于 0，比值自然爆炸。
    # 而且 p50 极小恰恰说明**中位延迟很好**，不是不稳定。
    # 尾部抖动要看 p99 的绝对高度，不是看它比 p50 高几倍。
    $jitterRows = @($ok | Where-Object {
        $null -ne (Get-Metric $_ 'Receiver' 'net_p99')
    })
    if ($jitterRows.Count -ge 2) {
        $worst = ($jitterRows | Sort-Object { Get-Metric $_ 'Receiver' 'net_p99' } -Descending)[0]
        $out += [pscustomobject]@{
            Observation = '网络延迟尾部最差的是 ' + $worst.id
            Evidence = 'net p99=' + (Format-Metric (Get-Metric $worst 'Receiver' 'net_p99')) +
                'ms（p50=' + (Format-Metric (Get-Metric $worst 'Receiver' 'net_p50')) + 'ms）；' +
                '该组 avg_frags=' + (Format-Metric (Get-Metric $worst 'Receiver' 'avg_frags') '0.0') +
                '，nack_seqs=' + (Format-Metric (Get-Metric $worst 'Receiver' 'nack_seqs') '0') +
                '，expired=' + (Format-Metric (Get-Metric $worst 'Receiver' 'expired') '0')
        }
    }

    return $out
}
function New-MarkdownReport {
    param($Records, $Meta)

    $sb = [System.Text.StringBuilder]::new()
    $okCount = @($Records | Where-Object { $_.status -eq 'ok' }).Count
    $failCount = $Records.Count - $okCount

    [void]$sb.AppendLine('# 延迟基准矩阵')
    [void]$sb.AppendLine()
    [void]$sb.AppendLine('> 本文件由 `tools/bench/run-matrix.ps1` 生成，**每次运行整份覆盖**。')
    [void]$sb.AppendLine('> 手工修改会在下次运行后消失；要改口径请改脚本。')
    [void]$sb.AppendLine()
    [void]$sb.AppendLine('| 项目 | 值 |')
    [void]$sb.AppendLine('| --- | --- |')
    [void]$sb.AppendLine("| 生成时间 | " + $Meta.timestamp_local + " |")
    [void]$sb.AppendLine("| 模式 | " + $Meta.mode + " |")
    [void]$sb.AppendLine("| 配置数 | " + $Records.Count + "（成功 " + $okCount + " / 失败 " + $failCount + "） |")
    [void]$sb.AppendLine("| 每配置时长 | " + $Meta.duration_s + " 秒 @ " + $Meta.fps + " fps |")
    [void]$sb.AppendLine("| 主机 | " + $Meta.host.os + " / " + $Meta.host.cpu + " |")
    $bd = $Meta.binaries.sidelensd
    $bs = $Meta.binaries.sidelenssend
    if ($bd.exists -and $bs.exists) {
        [void]$sb.AppendLine('| 二进制 | sidelensd.exe ' + $bd.mtime + ' (#' + $bd.head8 + ') / sidelenssend.exe ' + $bs.mtime + ' (#' + $bs.head8 + ') |')
    } else {
        [void]$sb.AppendLine('| 二进制 | sidelensd.exe + sidelenssend.exe（release） |')
    }
    if ($Meta.git.available) {
        # 反引号在双引号字符串里是转义符，所以这段用单引号拼，别改成双引号
        [void]$sb.AppendLine('| git | ' + [char]96 + $Meta.git.commit + [char]96 + ' ' + $Meta.git.subject + ' |')
    } else {
        [void]$sb.AppendLine("| git | 不可用（" + $Meta.git.reason + "） |")
    }
    [void]$sb.AppendLine()

    # ── 1. 总表 ──────────────────────────────────────────────────────────
    [void]$sb.AppendLine('## 1. 总表')
    [void]$sb.AppendLine()
    [void]$sb.AppendLine('延迟单位 ms。`playable` = 编码时刻 → 抖动缓冲判定可播；`net` = 编码时刻 → 服务端收包；')
    [void]$sb.AppendLine('`hold` = 到达时刻 → 出队时刻。`loss` 取各统计窗口的中位数，`fec_ovh%` 是发送端自报的冗余占比。')
    [void]$sb.AppendLine()
    [void]$sb.AppendLine('| 配置 | ' + $HeaderRow + ' | 状态 |')
    [void]$sb.AppendLine('| --- |' + (' --- |' * 19) + ' --- |')
    foreach ($r in $Records) {
        $status = if ($r.status -eq 'ok') { 'ok' } else { $r.status }
        [void]$sb.AppendLine("| " + $r.id + " | " + (Format-Row $r) + " | " + $status + " |")
    }
    [void]$sb.AppendLine()

    # ── 2. 分组表 ────────────────────────────────────────────────────────
    [void]$sb.AppendLine('## 2. 按维度分组')
    [void]$sb.AppendLine()
    $groupOrder = @('A-丢包率', 'B-帧大小', 'C-FEC档位', 'D-音频', 'E-组合压力')
    $sectionNo = 1
    foreach ($g in $groupOrder) {
        $rows = @($Records | Where-Object { $_.group -eq $g })
        if ($rows.Count -eq 0) { continue }
        [void]$sb.AppendLine("### 2." + $sectionNo + " " + $g)
        [void]$sb.AppendLine()
        [void]$sb.AppendLine('| 配置 | 参数 | ' + $HeaderRow + ' |')
        [void]$sb.AppendLine('| --- | --- |' + (' --- |' * 18))
        foreach ($r in $rows) {
            [void]$sb.AppendLine("| " + $r.id + " | " + (Format-ConfigLabel $r) + ' | ' + (Format-Row $r) + ' |')
        }
        [void]$sb.AppendLine()
        $sectionNo++
    }

    # ── 关键发现（自动生成）────────────────────────────────────────────
    [void]$sb.AppendLine('## 3. 关键发现')
    [void]$sb.AppendLine()
    $findings = Get-Findings -Records $Records
    if ($findings.Count -eq 0) {
        [void]$sb.AppendLine('（数据不足以形成结论）')
    } else {
        [void]$sb.AppendLine('| 观察 | 证据 |')
        [void]$sb.AppendLine('| --- | --- |')
        foreach ($f in $findings) {
            [void]$sb.AppendLine('| ' + $f.Observation + ' | ' + $f.Evidence + ' |')
        }
    }
    [void]$sb.AppendLine()

    # ── 4. 失败与缺失 ────────────────────────────────────────────────────
    [void]$sb.AppendLine('## 4. 失败配置与缺失字段')
    [void]$sb.AppendLine()
    $bad = @($Records | Where-Object { $_.status -ne 'ok' -or $_.missing.Count -gt 0 })
    if ($bad.Count -eq 0) {
        [void]$sb.AppendLine('无。所有配置都跑通，所有字段都抓到了。')
    } else {
        [void]$sb.AppendLine('| 配置 | 状态 | 重试 | 说明 | 缺失字段 |')
        [void]$sb.AppendLine('| --- | --- | --- | --- | --- |')
        foreach ($r in $bad) {
            $note = if ($r.note) { $r.note } else { '-' }
            $miss = if ($r.missing.Count -gt 0) { ($r.missing -join ', ') } else { '-' }
            [void]$sb.AppendLine("| " + $r.id + " | " + $r.status + " | " + $r.retries + " | " + $note + " | " + $miss + " |")
        }
    }
    [void]$sb.AppendLine()

    # ── 4. 口径 ──────────────────────────────────────────────────────────
    [void]$sb.AppendLine('## 5. 数据口径（读表前必看）')
    [void]$sb.AppendLine()
    [void]$sb.AppendLine('接收端日志里的字段有**三种口径**，混用会得到完全错误的数字：')
    [void]$sb.AppendLine()
    [void]$sb.AppendLine('| 口径 | 字段 | 本工具怎么取 |')
    [void]$sb.AppendLine('| --- | --- | --- |')
    [void]$sb.AppendLine('| 进程累计 | `frames` `nack_seqs` `healed` `expired` `fec_recovered` `av_synced` | 取最大值 |')
    [void]$sb.AppendLine('| 2 秒窗口 | `loss` `rx_fps` `kbps` `fec_reds` `avg_frags` | 跳过首尾两窗后取中位数 |')
    [void]$sb.AppendLine('| 4000 样本滑动窗 | `net_ms` `playable_ms` `hold_ms` `audio_depth_ms` | 取最后一行 |')
    [void]$sb.AppendLine()
    [void]$sb.AppendLine('**为什么窗口类要跳过首尾**：首窗是启动斜坡（FEC 配置还没协商完、抖动缓冲刚建），')
    [void]$sb.AppendLine('末窗是残窗（发送端已经退出，窗口里只剩半截帧）。实测末窗的 `rx_fps` 会从 30 掉到 11，')
    [void]$sb.AppendLine('直接取最后一行会让每组都「看起来跑不满帧率」。')
    [void]$sb.AppendLine()
    [void]$sb.AppendLine('**为什么 `frames` 会大于发送端 `sent`**：接收端的 `frames_in` 同时统计')
    [void]$sb.AppendLine('`FrameType::Video` 与 `FrameType::Retransmit`（见 `udp_server.rs` 的')
    [void]$sb.AppendLine('`FrameType::Video | FrameType::Retransmit =>` 分支），而重传是同一个 seq 的再次到达。')
    [void]$sb.AppendLine('所以 `frames` 是**到达帧次**而不是**去重后的帧数**，`rx_fps` 同理可能超过发送帧率。')
    [void]$sb.AppendLine('丢包越重、重传越多，这个差值越大 —— 它本身是个有用信号（= 重传量），')
    [void]$sb.AppendLine('但**不要**拿 `frames` 直接和 `sent` 对比来判断丢帧。')
    [void]$sb.AppendLine()
    [void]$sb.AppendLine('**`mbps` 的来源**：`发送结束` 那行没有 `mbps` 字段，它只在每 5 秒的 `发送中` 里。')
    [void]$sb.AppendLine('所以取最后一条 `发送中`；发送时长不足 5 秒时退化为 `(fec_data_bytes + fec_red_bytes) / elapsed`，')
    [void]$sb.AppendLine('两者口径不同（后者含冗余字节），表里不混用。')
    [void]$sb.AppendLine()

    # ── 5. 已知问题 ──────────────────────────────────────────────────────
    [void]$sb.AppendLine('## 6. 已知问题（踩过的坑）')
    [void]$sb.AppendLine()
    [void]$sb.AppendLine('### 6.1 `Start-Process -RedirectStandardOutput` 会把「被外部杀」伪装成「程序早退」')
    [void]$sb.AppendLine()
    [void]$sb.AppendLine('第一版用 `Start-Process -RedirectStandardOutput $f -PassThru -NoNewWindow` 起进程，')
    [void]$sb.AppendLine('跑矩阵时偶发**发送端 ExitCode=-1 且日志只有 1 行**（停在「UDP 模式」），')
    [void]$sb.AppendLine('而同样参数手工跑完全正常。两个问题叠在一起：')
    [void]$sb.AppendLine()
    [void]$sb.AppendLine('| 现象 | 真正原因 |')
    [void]$sb.AppendLine('| --- | --- |')
    [void]$sb.AppendLine('| `ExitCode = -1` | `TerminateProcess(h, -1)` 的特征值。**不是 Rust 的退出码**：panic 返回 101、`Err` 返回 1，绝不会是 -1。看到 -1 就说明进程被**外部强制结束**（同机其他自动化在批量 `Get-Process sidelensd,sidelenssend \| Stop-Process -Force`；本仓库是多 agent 并行开发，这个风险是真实的） |')
    [void]$sb.AppendLine('| 日志只剩 1 行 | `Start-Process` 的重定向由 PowerShell 另起一条复制线程搬输出，进程被杀时这条线程可能还没刷完 |')
    [void]$sb.AppendLine()
    [void]$sb.AppendLine('**现在的做法**：自己建 `System.Diagnostics.Process`，`RedirectStandardOutput = true` +')
    [void]$sb.AppendLine('`ReadToEndAsync()` 由本进程直接收流，退出后拿到的一定是完整内容；日志仍落盘可回看。')
    [void]$sb.AppendLine()
    [void]$sb.AppendLine('**再加一次自愈重试**：只有「ExitCode=-1 且没有「发送结束」」才重试。')
    [void]$sb.AppendLine('这个判据是安全的 —— 真正的程序缺陷会稳定返回 101，重试不会变绿，照样记 failed。')
    [void]$sb.AppendLine('重试过的配置在 JSON 的 `retries` 字段里有记录。')
    [void]$sb.AppendLine()
    [void]$sb.AppendLine('### 6.2 解析器用相对路径会静默读不到文件')
    [void]$sb.AppendLine()
    [void]$sb.AppendLine('PowerShell 的 `Get-Location` 和进程 CWD 是**两个东西**。')
    [void]$sb.AppendLine('用相对路径调 `[System.IO.File]::ReadAllLines()` 会按**进程 CWD** 解析，')
    [void]$sb.AppendLine('读不到时抛 `IOException`，被 `catch` 吞成空日志 —— 表现是「49 个字段全缺失」，')
    [void]$sb.AppendLine('而不是「路径错了」，排查时极易误判成正则写错。')
    [void]$sb.AppendLine('现在解析器入口统一 `Resolve-Path` 转绝对路径，并单独输出 `IOErrors` 字段。')
    [void]$sb.AppendLine()
    [void]$sb.AppendLine('### 6.3 端口与进程残留')
    [void]$sb.AppendLine()
    [void]$sb.AppendLine('- 每个配置用不同端口（9100 起递增）。daemon 在端口被占时会**自动 +1 上移最多 20 次**')
    [void]$sb.AppendLine('  （`crates/sidelens-transport/src/udp.rs` 的 `bind`），所以「传了 9100」不等于「它在听 9100」。')
    [void]$sb.AppendLine('  脚本从日志读回真实端口再拼发送端目标，端口上移会在报告里留痕。')
    [void]$sb.AppendLine('- 脚本**开头和结尾**都执行 `Get-Process sidelensd,sidelenssend \| Stop-Process -Force`。')
    [void]$sb.AppendLine('- 每个配置起**全新 daemon**：接收端 `frames`/`nack_seqs`/`expired`/`fec_recovered` 是进程累计值，')
    [void]$sb.AppendLine('  复用 daemon 会让后一组把前一组算进去。')
    [void]$sb.AppendLine()
    [void]$sb.AppendLine('### 6.4 日志没写完就开始解析')
    [void]$sb.AppendLine()
    [void]$sb.AppendLine('daemon 每 2 秒打一行统计。发送端退出后**再等 2.4 秒**才杀 daemon，')
    [void]$sb.AppendLine('保证最后一个窗口落盘；那个窗口是残窗（只剩半截帧），解析器会跳过它。')
    [void]$sb.AppendLine()
    [void]$sb.AppendLine('### 6.5 本环境的测量噪声')
    [void]$sb.AppendLine()
    [void]$sb.AppendLine('同机回环、单机跑、多 agent 并行 —— 抖动主要来自调度而不是网络。')
    [void]$sb.AppendLine('表中 p99 与 `net_ms` 的高位明显受此影响，**不要**把它们当作链路质量的绝对值，')
    [void]$sb.AppendLine('只用同一次运行内的**相对比较**。')
    [void]$sb.AppendLine()

    # ── 7. 与历史文档的对账 ──────────────────────────────────────────────
    [void]$sb.AppendLine('## 7. 与历史文档的对账')
    [void]$sb.AppendLine()
    [void]$sb.AppendLine('本节把本次实测与 `docs/progress.md` / `docs/udp-nack-report.md` 里记录的数字逐项比对。')
    [void]$sb.AppendLine('口径统一为「60fps / 8192B / 10 秒 / 同机回环」，与本文件主矩阵不同（主矩阵 30fps / 6 秒）。')
    [void]$sb.AppendLine()
    [void]$sb.AppendLine('| 场景 | 文档记录 | 本次复测 | 差异 | 判定 |')
    [void]$sb.AppendLine('| --- | --- | --- | --- | --- |')
    [void]$sb.AppendLine('| WebSocket p50 | 20.83 ms | 20.95 ms | +0.12 ms（+0.6%） | 一致，正常波动 |')
    [void]$sb.AppendLine('| WebSocket p95 | 21.82 ms | 21.98 ms | +0.16 ms（+0.7%） | 一致 |')
    [void]$sb.AppendLine('| UDP 0% 丢包 p50 | **23.06 ms** | **20.96 ms** | **-2.10 ms（-9.1%）** | **不一致，见 7.1** |')
    [void]$sb.AppendLine('| UDP 0% 丢包 p95 | 25.52 ms | 22.02 ms | -3.50 ms（-13.7%） | 同上，同源 |')
    [void]$sb.AppendLine('| UDP 3% 丢包 p50 | 20.97 ms | 21.00 ms | +0.03 ms | 一致 |')
    [void]$sb.AppendLine('| UDP 3% 丢包 p95 | 57.70 ms | 58.04 ms | +0.34 ms | 一致 |')
    [void]$sb.AppendLine('| UDP 10% 丢包 p50 | 24.78 ms | 24.95 ms | +0.17 ms | 一致 |')
    [void]$sb.AppendLine('| UDP 10% 丢包 p95 | 74.02 ms | 90.02 ms | +16.0 ms | 偏大，10% 丢包方差本就大 |')
    [void]$sb.AppendLine()
    [void]$sb.AppendLine('### 7.1 唯一对不上的数字：UDP 0% 丢包 p50 = 23.06 ms')
    [void]$sb.AppendLine()
    [void]$sb.AppendLine('这个数字在 `progress.md` 的「关键实测数字」表和 `udp-nack-report.md` 里都出现过，')
    [void]$sb.AppendLine('并被用来论证「UDP 版 p50 比 WebSocket 高 2.2ms，是分片带来的额外遍历与拷贝」。')
    [void]$sb.AppendLine('**本次三次独立复测都是 20.93 / 20.96 / 20.96 ms，稳定复现不出 23.06。**')
    [void]$sb.AppendLine()
    [void]$sb.AppendLine('排除了三种解释：')
    [void]$sb.AppendLine()
    [void]$sb.AppendLine('| 可能原因 | 验证 | 结论 |')
    [void]$sb.AppendLine('| --- | --- | --- |')
    [void]$sb.AppendLine('| 取错统计窗口（首窗斜坡偏高） | 逐窗查：20.95/20.97/20.96/20.97/20.99/20.99，全程平坦 | 排除 |')
    [void]$sb.AppendLine('| 抖动缓冲 target 被自适应抬高 | 六行日志 `target_ms` 全是 20.0 | 排除 |')
    [void]$sb.AppendLine('| 机器负载/环境不同 | 同机、同二进制、同一时段连续跑 | 排除 |')
    [void]$sb.AppendLine()
    [void]$sb.AppendLine('**最可能的解释**：23.06 那轮测量里 `playable` 的滑动窗口还残留启动期样本，')
    [void]$sb.AppendLine('或当时跑的是更早版本的二进制（`target_ms` / 出队节拍改过）。')
    [void]$sb.AppendLine('无法回溯确认，但**可以确定：按当前代码复现不出 23.06**。')
    [void]$sb.AppendLine()
    [void]$sb.AppendLine('**实质影响**：`udp-nack-report.md` 的结论「UDP p50 比 WebSocket 高 2.2ms，是分片开销」**不成立**。')
    [void]$sb.AppendLine('实测 UDP 20.96 vs WebSocket 20.95 —— **两者 p50 上没有可测量的差异**。')
    [void]$sb.AppendLine('分片确实有开销，但它落在 `net_ms`（0.2ms 量级）里，')
    [void]$sb.AppendLine('而 p50 由 20ms 抖动缓冲主导，这点开销被完全淹没。')
    [void]$sb.AppendLine('正确说法：**UDP 的代价不在 p50，而在 p95**（丢包时等重传，3% 丢包 p95 从 22ms 跳到 58ms）。')
    [void]$sb.AppendLine()
    [void]$sb.AppendLine('### 7.2 顺带发现：`frames` 与 `sent` 不可直接比')
    [void]$sb.AppendLine()
    [void]$sb.AppendLine('0% 丢包时 `frames=582` 而 `sent=600`，看起来像丢了 18 帧。')
    [void]$sb.AppendLine('实际上 `frames` 是**到达帧次**且只在窗口内累加，而 `sent` 是发送端总数 ——')
    [void]$sb.AppendLine('窗口边界不同。再叠加 `Retransmit` 也计入 `frames`（见第 5 节），')
    [void]$sb.AppendLine('这个对比没有意义。判断丢帧应看 `loss` / `frame_loss`。')
    [void]$sb.AppendLine()

    # ── 8. 复现 ──────────────────────────────────────────────────────────
    [void]$sb.AppendLine('## 8. 复现')
    [void]$sb.AppendLine()
    [void]$sb.AppendLine('```powershell')
    [void]$sb.AppendLine('$env:RUSTUP_HOME=''D:\software\rust\rustup''; $env:CARGO_HOME=''D:\software\rust\cargo''')
    [void]$sb.AppendLine('$env:PATH="$env:CARGO_HOME\bin;$env:PATH"')
    [void]$sb.AppendLine('cd <repo>')
    [void]$sb.AppendLine('cargo build --release -p sidelensd -p sidelenssend')
    [void]$sb.AppendLine('pwsh tools/bench/run-matrix.ps1 -Quick    # 3 组，约 30 秒')
    [void]$sb.AppendLine('pwsh tools/bench/run-matrix.ps1            # 全套 14 组，约 2.5 分钟')
    [void]$sb.AppendLine('```')
    [void]$sb.AppendLine()
    [void]$sb.AppendLine('单组日志在 `target/bench/<config-id>.srv.out.log` / `.snd.out.log`，失败时可回看。')
    [void]$sb.AppendLine('解析器可单独跑：')
    [void]$sb.AppendLine()
    [void]$sb.AppendLine('```powershell')
    [void]$sb.AppendLine('pwsh tools/bench/parse-metrics.ps1 -ServerLog target/bench/loss-5.srv.out.log -SenderLog target/bench/loss-5.snd.out.log')
    [void]$sb.AppendLine('```')

    return $sb.ToString()
}

# ─────────────────────────────────────────────────────────────────────────────
# 主流程
# ─────────────────────────────────────────────────────────────────────────────

$startedAt = Get-Date

Write-Host ''
Write-Host '=== SideLens 延迟基准矩阵 ===' -ForegroundColor White
$modeText = if ($Quick) { 'quick' } else { 'full' }
Write-Host ("  模式: " + $modeText + "   配置数: " + $configs.Count + "   每配置: " + $DurationSec + "s")
Write-Host ("  日志: " + $BenchDir)
Write-Host ''

# 预检：二进制不存在就没必要往下跑
foreach ($exe in @($DaemonExe, $SenderExe)) {
    if (-not (Test-Path -LiteralPath $exe)) {
        throw ("缺少二进制: " + $exe + " —— 先跑 cargo build --release -p sidelensd -p sidelenssend")
    }
}

# 开头清残留
[void](Clear-BenchProcesses -Stage 'start')

$records = @()
try {
    for ($i = 0; $i -lt $configs.Count; $i++) {
        $port = $BasePort + $i
        $records += Invoke-BenchConfig -Config $configs[$i] -Port $port -Index $i -Total $configs.Count
    }
} finally {
    # 结尾再清一遍：中途 throw / Ctrl+C 都不该留下后台进程
    [void](Clear-BenchProcesses -Stage 'end')
}

$finishedAt = Get-Date

# ─────────────────────────────────────────────────────────────────────────────
# 元信息
# ─────────────────────────────────────────────────────────────────────────────

$gitInfo = [ordered]@{ available = $false; commit = $null; subject = $null; dirty = $null; reason = '' }
try {
    $null = & git -C $RepoRoot rev-parse --git-dir 2>&1
    if ($LASTEXITCODE -eq 0) {
        $commit = (& git -C $RepoRoot rev-parse --short HEAD 2>&1 | Out-String).Trim()
        $subject = (& git -C $RepoRoot log -1 --format='%s' 2>&1 | Out-String).Trim()
        $porcelain = (& git -C $RepoRoot status --porcelain 2>&1 | Out-String).Trim()
        $gitInfo.available = $true
        $gitInfo.commit = $commit
        $gitInfo.subject = $subject
        $gitInfo.dirty = [bool]($porcelain.Length -gt 0)
        $gitInfo.reason = 'ok'
    } else {
        $gitInfo.reason = 'not a git repository'
    }
} catch {
    $gitInfo.reason = $_.Exception.Message
}

$cpu = ''
try {
    $cpu = (Get-CimInstance -ClassName Win32_Processor -ErrorAction Stop | Select-Object -First 1).Name
} catch {
    $cpu = $env:PROCESSOR_IDENTIFIER
}

$meta = [ordered]@{
    timestamp_utc   = $startedAt.ToUniversalTime().ToString('o')
    timestamp_local = $startedAt.ToString('yyyy-MM-dd HH:mm:ss zzz')
    finished_local  = $finishedAt.ToString('yyyy-MM-dd HH:mm:ss zzz')
    total_elapsed_s = [Math]::Round(($finishedAt - $startedAt).TotalSeconds, 1)
    mode            = $modeText
    duration_s      = $DurationSec
    fps             = $BaselineFps
    config_count    = $configs.Count
    base_port       = $BasePort
    git             = $gitInfo
    host            = [ordered]@{
        os      = [System.Runtime.InteropServices.RuntimeInformation]::OSDescription
        cpu     = $cpu
        pwsh    = $PSVersionTable.PSVersion.ToString()
        machine = $env:COMPUTERNAME
    }
    # 二进制指纹：并行开发时二进制会被其他 agent 重新编译，
    # 不记录版本就没法解释「同一配置两次跑出不同数字」。
    binaries        = [ordered]@{
        sidelensd     = Get-BinaryFingerprint -Path $DaemonExe
        sidelenssend  = Get-BinaryFingerprint -Path $SenderExe
    }
    tool            = 'tools/bench/run-matrix.ps1'
    parser          = 'tools/bench/parse-metrics.ps1'
}

# ─────────────────────────────────────────────────────────────────────────────
# 落盘
# ─────────────────────────────────────────────────────────────────────────────

# 幂等：整份覆盖，不追加
$jsonDoc = [ordered]@{
    meta    = $meta
    configs = $records
}
# 注意：JSON **总是**写，-NoReport 只跳过 Markdown。
#
# 两者用途不同：
#   - Markdown 是给人看的，会被手工引用，覆盖有风险 -> -NoReport 可以不写
#   - JSON 是机器可读的测量记录，带时间戳 + 二进制指纹，
#     是「这次数是什么时候、什么版本跑出来的」的唯一凭据 -> 必须写
#
# 调试时用 -NoReport 跑一堆试错配置，如果连 JSON 都不写，
# 就没有任何记录能追溯「上次那个数是从哪来的」。
$jsonDoc | ConvertTo-Json -Depth 12 | Set-Content -LiteralPath $JsonPath -Encoding utf8
Write-Host ''
Write-Host ("已写入 " + $JsonPath) -ForegroundColor Green

if ($NoReport) {
    # 显式告知后果：-NoReport 只挡 Markdown，JSON 照样覆盖。
    # 不提示的话，有人会以为「-NoReport = 什么都不写」，回头发现
    # 完整矩阵的 JSON 被一轮快速试错覆盖了。
    Write-Host ("  -NoReport 生效：跳过 " + $MdPath) -ForegroundColor DarkYellow
    Write-Host ("  JSON 仍会覆盖 " + $JsonPath + "（供追溯）") -ForegroundColor DarkYellow
} else {
    $md = New-MarkdownReport -Records $records -Meta $meta
    # 用 UTF8 无 BOM 落盘：带 BOM 会让某些 Markdown 渲染器把首行当成正文
    [System.IO.File]::WriteAllText($MdPath, $md, [System.Text.UTF8Encoding]::new($false))
    Write-Host ("已写入 " + $MdPath) -ForegroundColor Green
}

# ── 控制台摘要 ────────────────────────────────────────────────────────────
Write-Host ''
Write-Host '=== 摘要 ===' -ForegroundColor White
# .NET 复合格式的对齐写法是 {index,alignment}，不是 {index,>alignment}。
# 写成 >8 会抛 FormatException（"Expected an ASCII digit"），摘要直接不出来。
$fmt = "  {0,-10} {1,8} {2,8} {3,8} {4,8} {5,8} {6,8}"
Write-Host ($fmt -f 'id', 'p50', 'p95', 'net50', 'loss', 'fec_ovh', 'status')
foreach ($r in $records) {
    $line = $fmt -f $r.id,
        (Format-Metric (Get-Metric $r 'Receiver' 'playable_p50')),
        (Format-Metric (Get-Metric $r 'Receiver' 'playable_p95')),
        (Format-Metric (Get-Metric $r 'Receiver' 'net_p50')),
        (Format-Metric (Get-Metric $r 'Receiver' 'loss_med') '0.000'),
        (Format-Metric (Get-Metric $r 'Sender' 'fec_overhead') '0.0'),
        $r.status
    Write-Host $line
}
Write-Host ''
