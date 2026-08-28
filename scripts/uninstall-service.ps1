$ErrorActionPreference = "Stop"

$serviceName = "SunRemoteDesktop"
$ruleName = "SunRemoteDesktop (TCP 3389)"
$agentRunKey = "HKLM:\Software\Microsoft\Windows\CurrentVersion\Run"
$agentRunName = "SunRemoteDesktopAgent"
$projectRoot = Split-Path -Parent $PSScriptRoot
$binary = Join-Path $projectRoot "target\release\sun-remote-desktop.exe"

$existing = Get-Service -Name $serviceName -ErrorAction SilentlyContinue
if ($null -ne $existing) {
    if ($existing.Status -ne "Stopped") {
        Stop-Service -Name $serviceName -Force
    }
    & sc.exe delete $serviceName | Out-Null
}

Remove-ItemProperty `
    -Path $agentRunKey `
    -Name $agentRunName `
    -ErrorAction SilentlyContinue

Get-CimInstance Win32_Process -Filter "Name = 'sun-remote-desktop.exe'" |
    Where-Object {
        $_.ExecutablePath -eq $binary -and
        $_.CommandLine -match '(?i)(?:^|\s)agent(?:\s|$)'
    } |
    ForEach-Object {
        Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue
    }

Remove-NetFirewallRule -DisplayName $ruleName -ErrorAction SilentlyContinue
Write-Host "SunRemoteDesktop 服务、会话代理自启动项和防火墙规则已移除；配置文件和证书保留不动。"
