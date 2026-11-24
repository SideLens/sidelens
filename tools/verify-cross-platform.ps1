# 跨平台编译回归（第四十轮新增）
#
# # 为什么需要它
#
# 项目此前**从未**跑过 `cargo check --target <非本机>`。
# 第四十轮第一次跑就暴露了两个累积多轮的缺陷：
#   1. sidelensd 在非 Windows 上 5 个 E0425（cfg 配对错误）
#   2. sidelens-vcam/examples/registry_entry.rs 缺 cfg 门控（3 个 E0433）
#
# 这条检查比 `cargo test` 便宜得多（不跑测试、不链接），
# 却是唯一能发现「只在某个平台编译失败」的手段。
#
# 用法: pwsh -File tools/verify-cross-platform.ps1

$ErrorActionPreference = "Continue"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

$env:RUSTUP_HOME = "D:\software\rust\rustup"
$env:CARGO_HOME = "D:\software\rust\cargo"
$env:PATH = "$env:CARGO_HOME\bin;" + $env:PATH

# 所有已安装的 target（自动发现，不硬编码）
$installed = rustup target list --installed 2>&1
if ($LASTEXITCODE -ne 0) {
    Write-Host "rustup 不可用，跳过跨平台检查"
    exit 0
}

Write-Host "=== 跨平台编译回归 ==="
Write-Host ""
Write-Host "已安装 target："
$installed | ForEach-Object { Write-Host "  $_" }
Write-Host ""

$fail = 0
$ok = 0
foreach ($t in $installed) {
    if ([string]::IsNullOrWhiteSpace($t)) { continue }
    $t = $t.Trim()

    $out = cargo check --workspace --all-targets --target $t 2>&1
    $errs = @($out | Select-String -Pattern "^error").Count

    if ($errs -eq 0) {
        Write-Host "  OK   $t"
        $ok++
    } else {
        Write-Host "  FAIL $t  ($errs errors)"
        # 只打前 5 条错误，避免刷屏
        $out | Select-String -Pattern "^error" | Select-Object -First 5 | ForEach-Object {
            Write-Host "         $($_.Line)"
        }
        $fail++
    }
}

Write-Host ""
Write-Host "=== 结果: $ok 通过 / $fail 失败 ==="
if ($fail -gt 0) { exit 1 }
