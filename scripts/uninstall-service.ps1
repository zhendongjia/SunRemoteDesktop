$ErrorActionPreference = "Stop"

$serviceName = "SunRemoteDesktop"
$ruleName = "SunRemoteDesktop (SunRDP)"
$publicRuleName = "$ruleName (Public local subnet)"
$tailscaleRuleName = "$ruleName (Tailscale)"
$agentRunKey = "HKLM:\Software\Microsoft\Windows\CurrentVersion\Run"
$agentRunName = "SunRemoteDesktopAgent"
$maintenanceTaskName = "SunRemoteDesktop Maintenance"
$projectRoot = Split-Path -Parent $PSScriptRoot
$binary = Join-Path $projectRoot "target\release\sun-remote-desktop.exe"
$installedRoot = Join-Path $env:ProgramFiles "SunRemoteDesktop"
$maintenanceRoot = Join-Path $env:ProgramData "SunRemoteDesktop\Maintenance"
$maintenanceQueue = Join-Path $env:ProgramData "SunRemoteDesktop\MaintenanceQueue"

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
        -not [string]::IsNullOrWhiteSpace($_.ExecutablePath) -and
        ($_.ExecutablePath -eq $binary -or $_.ExecutablePath.StartsWith(
            $installedRoot + '\', [StringComparison]::OrdinalIgnoreCase
        )) -and
        $_.CommandLine -match '(?i)(?:^|\s)(?:console-agent|agent)(?:\s|$)'
    } |
    ForEach-Object {
        Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue
    }

Remove-NetFirewallRule -DisplayName $ruleName -ErrorAction SilentlyContinue
Remove-NetFirewallRule -DisplayName $publicRuleName -ErrorAction SilentlyContinue
Remove-NetFirewallRule -DisplayName $tailscaleRuleName -ErrorAction SilentlyContinue
Remove-NetFirewallRule -DisplayName "SunRemoteDesktop (RDP Transport)" -ErrorAction SilentlyContinue
Remove-NetFirewallRule -DisplayName "SunRemoteDesktop (TCP 3389)" -ErrorAction SilentlyContinue
Unregister-ScheduledTask -TaskName $maintenanceTaskName -Confirm:$false -ErrorAction SilentlyContinue
Remove-Item -LiteralPath $maintenanceRoot -Recurse -Force -ErrorAction SilentlyContinue
Remove-Item -LiteralPath $maintenanceQueue -Recurse -Force -ErrorAction SilentlyContinue
Remove-Item -LiteralPath $installedRoot -Recurse -Force -ErrorAction SilentlyContinue
Write-Host "SunRemoteDesktop service, maintenance task, installed binaries, legacy agent autorun, and firewall rule were removed. Configuration and certificates were preserved."
