$ErrorActionPreference = "Stop"

$projectRoot = Split-Path -Parent $PSScriptRoot
$binary = Join-Path $projectRoot "target\release\rdp-desktop-host.exe"
$serviceName = "RdpDesktopHost"

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
    -DisplayName "RdpDesktopHost" `
    -Description "Share the current desktop through the RDP transport" `
    -StartupType Automatic | Out-Null

New-NetFirewallRule `
    -DisplayName "RdpDesktopHost (TCP 3389)" `
    -Direction Inbound `
    -Action Allow `
    -Protocol TCP `
    -LocalPort 3389 `
    -Profile Domain,Private | Out-Null

Start-Service -Name $serviceName
Write-Host "RdpDesktopHost 服务已安装并启动。"
