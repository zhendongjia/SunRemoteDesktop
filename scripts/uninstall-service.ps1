$ErrorActionPreference = "Stop"

$serviceName = "RdpDesktopHost"
$ruleName = "RdpDesktopHost (TCP 3389)"

$existing = Get-Service -Name $serviceName -ErrorAction SilentlyContinue
if ($null -ne $existing) {
    if ($existing.Status -ne "Stopped") {
        Stop-Service -Name $serviceName -Force
    }
    & sc.exe delete $serviceName | Out-Null
}

Remove-NetFirewallRule -DisplayName $ruleName -ErrorAction SilentlyContinue
Write-Host "RdpDesktopHost 服务和防火墙规则已移除；配置文件和证书保留不动。"
