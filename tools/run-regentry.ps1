# 提权跑 AddRegistryEntry 实验
$log = 'D:\dsh-workspace\sidelens\target\vcam-regentry.log'
Remove-Item $log -ErrorAction SilentlyContinue
Add-Content $log "=== AddRegistryEntry 实验 $(Get-Date -Format 'HH:mm:ss') ==="
Add-Content $log "管理员: $((New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator))"
Add-Content $log ''
& 'D:\dsh-workspace\sidelens\target\release\examples\registry_entry.exe' *>&1 | Add-Content $log
Add-Content $log ''
Add-Content $log '=== 检查注册表里到底写进去了什么 ==='
$base = 'HKLM:\SYSTEM\CurrentControlSet\Control\DeviceClasses\{e5323777-f976-4f5b-9b55-b94699c46e44}'
Get-ChildItem $base -ErrorAction SilentlyContinue | ForEach-Object {
  $inst = $_.PSChildName
  Get-ChildItem $_.PSPath -Recurse -ErrorAction SilentlyContinue | ForEach-Object {
    try {
      $p = Get-ItemProperty $_.PSPath -ErrorAction Stop
      $names = ($p.PSObject.Properties | Where-Object { $_.Name -notmatch '^PS' } | ForEach-Object { "$($_.Name)=$($_.Value)" }) -join ', '
      if ($names) { Add-Content $log "  $($inst.Substring(0,30))... : $names" }
    } catch {}
  }
}
Add-Content $log ''
Add-Content $log '=== Enum 树 Properties 子键 ==='
$e = 'HKLM:\SYSTEM\CurrentControlSet\Enum\SWD\VCAMDEVAPI'
Get-ChildItem $e -ErrorAction SilentlyContinue | ForEach-Object {
  $prop = Join-Path $_.PSPath 'Properties'
  Get-ChildItem $prop -Recurse -ErrorAction SilentlyContinue | ForEach-Object {
    $v = Get-ItemProperty $_.PSPath -ErrorAction SilentlyContinue
    $names = ($v.PSObject.Properties | Where-Object { $_.Name -notmatch '^PS' } | ForEach-Object { $_.Name }) -join ','
    Add-Content $log "  $($_.PSChildName) : [$names]"
  }
}
Add-Content $log '=== 结束 ==='
