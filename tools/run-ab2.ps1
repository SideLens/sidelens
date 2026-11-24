# 正确的 A/B：走完整 start() 流程 ± AddRegistryEntry
$root = Split-Path -Parent $PSScriptRoot

$log = Join-Path $root 'target\vcam-ab2.log'
Remove-Item $log -ErrorAction SilentlyContinue
Add-Content $log "=== A/B v2 $(Get-Date -Format 'HH:mm:ss') ==="
Add-Content $log "管理员: $((New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator))"
Add-Content $log ''

$exe = (Join-Path $root 'target\release\sidelensd.exe')

foreach ($mode in @('baseline','regentry')) {
  Add-Content $log "########## $mode ##########"
  Remove-Item "$env:TEMP\sidelens-vcam.log" -ErrorAction SilentlyContinue
  $args = @('--vcam-probe','--hold','5','--no-inproc-class')
  if ($mode -eq 'regentry') { $args += '--regentry' }
  & $exe @args *>&1 | ForEach-Object { Add-Content $log "  $_" }
  Add-Content $log '  --- DLL 日志（13 步）---'
  $dl = "$env:TEMP\sidelens-vcam.log"
  if (Test-Path $dl) { Get-Content $dl | ForEach-Object { Add-Content $log "    $_" } } else { Add-Content $log '    (未加载)' }
  Add-Content $log ''
  Start-Sleep -Milliseconds 1500
}
Add-Content $log '=== 结束 ==='
