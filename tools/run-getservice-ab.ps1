# 实验：GetService(2032C7EF) 返回 E_NOINTERFACE 而不是 MF_E_UNSUPPORTED_SERVICE
$log = 'D:\dsh-workspace\sidelens\target\vcam-getservice-ab.log'
Remove-Item $log -ErrorAction SilentlyContinue
Add-Content $log "=== GetService A/B $(Get-Date -Format 'HH:mm:ss') ==="
Remove-Item "$env:TEMP\sidelens-vcam.log" -ErrorAction SilentlyContinue
& 'D:\dsh-workspace\sidelens\target\release\sidelensd.exe' --vcam-probe --hold 5 --no-inproc-class *>&1 | ForEach-Object { Add-Content $log "  $_" }
Add-Content $log ''
Add-Content $log '=== DLL 日志 ==='
$dl = "$env:TEMP\sidelens-vcam.log"
if (Test-Path $dl) { Get-Content $dl | ForEach-Object { Add-Content $log "  $_" } }
Add-Content $log '=== 结束 ==='
