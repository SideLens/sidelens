# A/B：逐个属性单独测，看哪个影响 Start 的错误码
$log = 'D:\dsh-workspace\sidelens\target\vcam-ab.log'
Remove-Item $log -ErrorAction SilentlyContinue
Add-Content $log "=== A/B 实验 $(Get-Date -Format 'HH:mm:ss') ==="
Add-Content $log ''

$modes = @('none','iscam_dword','iscam_binary','vcamclsid','customclsid')
foreach ($m in $modes) {
  Add-Content $log "########## 模式: $m ##########"
  & 'D:\dsh-workspace\sidelens\target\release\examples\registry_entry.exe' $m *>&1 | ForEach-Object {
    # 只保留关键行，避免编码问题干扰
    if ($_ -match 'Start|模式|===|✅|❌|MFCreate') { Add-Content $log "  $_" }
  }
  Add-Content $log ''
  Start-Sleep -Milliseconds 800
}
Add-Content $log '=== 结束 ==='
