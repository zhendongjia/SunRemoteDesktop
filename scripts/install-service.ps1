$ErrorActionPreference = "Stop"

$projectRoot = Split-Path -Parent $PSScriptRoot
$binary = Join-Path $projectRoot "target\release\sun-remote-desktop.exe"
$serviceName = "SunRemoteDesktop"
$ruleName = "SunRemoteDesktop (SunRDP)"
$publicRuleName = "$ruleName (Public local subnet)"
$tailscaleRuleName = "$ruleName (Tailscale)"
$legacyAgentRunKey = "HKLM:\Software\Microsoft\Windows\CurrentVersion\Run"
$legacyAgentRunName = "SunRemoteDesktopAgent"

if (-not (Test-Path -LiteralPath $binary)) {
    throw "Binary not found: $binary. Run cargo build --release first."
}

$existing = Get-Service -Name $serviceName -ErrorAction SilentlyContinue
if ($null -ne $existing -and $existing.Status -ne "Stopped") {
    Stop-Service -Name $serviceName -Force
}

# Stop helpers from older installations after stopping their supervisor. The
# installed service creates a LocalSystem helper directly in the active
# physical console session.
Get-CimInstance Win32_Process -Filter "Name = 'sun-remote-desktop.exe'" |
    Where-Object {
        $_.CommandLine -match '(?i)(?:^|\s)(?:console-agent|agent)(?:\s|$)'
    } |
    ForEach-Object {
        Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue
    }

if ($null -ne $existing) {
    & sc.exe delete $serviceName | Out-Null
    Start-Sleep -Milliseconds 500
}

New-Service `
    -Name $serviceName `
    -BinaryPathName "`"$binary`" service" `
    -DisplayName "SunRemoteDesktop" `
    -Description "Share the current desktop through SunRDP" `
    -StartupType Automatic | Out-Null

Remove-ItemProperty `
    -Path $legacyAgentRunKey `
    -Name $legacyAgentRunName `
    -ErrorAction SilentlyContinue

$configPath = & $binary config-path
$port = 3390
if (Test-Path -LiteralPath $configPath) {
    $portSetting = Select-String `
        -LiteralPath $configPath `
        -Pattern '^\s*port\s*=\s*(\d+)\s*$' |
        Select-Object -First 1
    if ($null -ne $portSetting) {
        $port = [int]$portSetting.Matches[0].Groups[1].Value
    }
}

Remove-NetFirewallRule -DisplayName $ruleName -ErrorAction SilentlyContinue
Remove-NetFirewallRule -DisplayName $publicRuleName -ErrorAction SilentlyContinue
Remove-NetFirewallRule -DisplayName $tailscaleRuleName -ErrorAction SilentlyContinue
New-NetFirewallRule `
    -DisplayName $ruleName `
    -Direction Inbound `
    -Action Allow `
    -Protocol TCP `
    -LocalPort $port `
    -Profile Domain,Private | Out-Null
New-NetFirewallRule `
    -DisplayName $publicRuleName `
    -Direction Inbound `
    -Action Allow `
    -Protocol TCP `
    -LocalPort $port `
    -RemoteAddress LocalSubnet `
    -Profile Public | Out-Null
$tailscaleAliases = @(
    Get-NetAdapter -ErrorAction SilentlyContinue |
        Where-Object {
            $_.Name -eq 'Tailscale' -or
            $_.InterfaceDescription -like '*Tailscale*'
        } |
        Select-Object -ExpandProperty Name -Unique
)
if ($tailscaleAliases.Count -gt 0) {
    New-NetFirewallRule `
        -DisplayName $tailscaleRuleName `
        -Direction Inbound `
        -Action Allow `
        -Protocol TCP `
        -LocalPort $port `
        -RemoteAddress '100.64.0.0/10','fd7a:115c:a1e0::/48' `
        -InterfaceAlias $tailscaleAliases `
        -Profile Any | Out-Null
}

Start-Service -Name $serviceName
$binaryHash = (Get-FileHash -LiteralPath $binary -Algorithm SHA256).Hash
& (Join-Path $PSScriptRoot 'install-maintenance.ps1') `
    -CandidateBinary $binary `
    -ExpectedSha256 $binaryHash | Out-Null
Write-Host "SunRemoteDesktop and its restricted maintenance entry are installed. SunRDP and its service-managed physical console agent are running on port $port."
