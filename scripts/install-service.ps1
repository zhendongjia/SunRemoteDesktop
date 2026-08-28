$ErrorActionPreference = "Stop"

$projectRoot = Split-Path -Parent $PSScriptRoot
$binary = Join-Path $projectRoot "target\release\sun-remote-desktop.exe"
$serviceName = "SunRemoteDesktop"
$agentRunKey = "HKLM:\Software\Microsoft\Windows\CurrentVersion\Run"
$agentRunName = "SunRemoteDesktopAgent"

if (-not (Test-Path -LiteralPath $binary)) {
    throw "未找到 $binary，请先执行 cargo build --release。"
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
    -Description "Share the current desktop through the RDP transport" `
    -StartupType Automatic | Out-Null

$agentCommand = '"' + $binary + '" agent'
New-ItemProperty `
    -Path $agentRunKey `
    -Name $agentRunName `
    -PropertyType String `
    -Value $agentCommand `
    -Force | Out-Null

New-NetFirewallRule `
    -DisplayName "SunRemoteDesktop (TCP 3389)" `
    -Direction Inbound `
    -Action Allow `
    -Protocol TCP `
    -LocalPort 3389 `
    -Profile Domain,Private | Out-Null

Start-Service -Name $serviceName
Start-Process -FilePath $binary -ArgumentList @("agent") -WindowStyle Hidden
Write-Host "SunRemoteDesktop 服务已安装并启动；会话代理已启动，并会在以后每次登录时自动运行。"
