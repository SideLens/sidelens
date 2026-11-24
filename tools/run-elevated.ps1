# 提权验证：设官方 SDK 设备接口属性后重跑 vcam probe
#
# 用法：由非管理员进程用 Start-Process -Verb RunAs 启动
$root = Split-Path -Parent $PSScriptRoot

$log = Join-Path $root 'target\vcam-elevated.log'
Remove-Item $log -ErrorAction SilentlyContinue
Add-Content $log "=== 提权验证开始 $(Get-Date -Format 'HH:mm:ss') ==="
Add-Content $log "当前身份: $([System.Security.Principal.WindowsIdentity]::GetCurrent().Name)"
Add-Content $log "是否管理员: $((New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator))"
Add-Content $log ''

# 清掉 DLL 文件日志，便于区分本轮
Remove-Item "$env:TEMP\sidelens-vcam.log" -ErrorAction SilentlyContinue

Add-Content $log '=== 跑 probe（新增 IsVirtualCamera 等官方属性）==='
& (Join-Path $root 'target\release\sidelensd.exe') --vcam-probe --hold 8 --no-inproc-class *>&1 | Add-Content $log

Add-Content $log ''
Add-Content $log '=== 检查设备上的 IsVirtualCamera 属性 ==='
$inst = Get-PnpDevice -ErrorAction SilentlyContinue | Where-Object { $_.InstanceId -like 'SWD\\VCAMDEVAPI\\*' } | Select-Object -First 1
if ($inst) {
  Add-Content $log "设备: $($inst.InstanceId)"
  Add-Content $log "FriendlyName: $($inst.FriendlyName)"
  foreach ($pid in @(3,4,5)) {
    $key = "{6EDC630D-C2E3-43B7-B2D1-20525A1AF120} $pid"
    try {
      $v = Get-PnpDeviceProperty -InstanceId $inst.InstanceId -KeyName $key -ErrorAction Stop
      Add-Content $log "  pid=$pid : 值=$($v.Data) 类型=$($v.Type)"
    } catch { Add-Content $log "  pid=$pid : (读不到)" }
  }
} else {
  Add-Content $log '(没有找到 VCAMDEVAPI 设备)'
}

Add-Content $log ''
Add-Content $log '=== DLL 日志（FrameServer 的 13 步轨迹）==='
$dl = "$env:TEMP\sidelens-vcam.log"
if (Test-Path $dl) { Get-Content $dl | Add-Content $log } else { Add-Content $log '(DLL 未被加载)' }

Add-Content $log ''
Add-Content $log '=== 结束 ==='
