$ErrorActionPreference = "Stop"

$projectRoot = Split-Path -Parent $PSScriptRoot
$binary = Join-Path $projectRoot "target\release\sun-remote-desktop.exe"
$serviceName = "SunRemoteDesktop"
$ruleName = "SunRemoteDesktop (SunRDP)"
$agentRunKey = "HKLM:\Software\Microsoft\Windows\CurrentVersion\Run"
$agentRunName = "SunRemoteDesktopAgent"

if (-not (Test-Path -LiteralPath $binary)) {
    throw "Binary not found: $binary. Run cargo build --release first."
}

# Stop agents from older installations before replacing the service. They all
# use the same protected named pipe and an old agent can otherwise win the
# first connection after the service is updated.
Get-CimInstance Win32_Process -Filter "Name = 'sun-remote-desktop.exe'" |
    Where-Object {
        $_.CommandLine -match '(?i)(?:^|\s)agent(?:\s|$)'
    } |
    ForEach-Object {
        Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue
    }

$existing = Get-Service -Name $serviceName -ErrorAction SilentlyContinue
if ($null -ne $existing) {
    if ($existing.Status -ne "Stopped") {
        Stop-Service -Name $serviceName -Force
    }
    & sc.exe delete $serviceName | Out-Null
    Start-Sleep -Milliseconds 500
}

New-Service `
    -Name $serviceName `
    -BinaryPathName "`"$binary`" service" `
    -DisplayName "SunRemoteDesktop" `
    -Description "Share the current desktop through SunRDP" `
    -StartupType Automatic | Out-Null

$agentCommand = '"' + $binary + '" agent'
New-ItemProperty `
    -Path $agentRunKey `
    -Name $agentRunName `
    -PropertyType String `
    -Value $agentCommand `
    -Force | Out-Null

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
New-NetFirewallRule `
    -DisplayName $ruleName `
    -Direction Inbound `
    -Action Allow `
    -Protocol TCP `
    -LocalPort $port `
    -Profile Domain,Private | Out-Null

Start-Service -Name $serviceName
Start-Process -FilePath $binary -ArgumentList @("agent") -WindowStyle Hidden
$binaryHash = (Get-FileHash -LiteralPath $binary -Algorithm SHA256).Hash
& (Join-Path $PSScriptRoot 'install-maintenance.ps1') `
    -CandidateBinary $binary `
    -ExpectedSha256 $binaryHash | Out-Null
Write-Host "SunRemoteDesktop and its restricted maintenance entry are installed. SunRDP and the session agent are running on port $port."
