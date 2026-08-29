param(
    [Parameter(Mandatory = $true)]
    [ValidateSet('Deploy', 'Restart', 'Repair')]
    [string]$Action,
    [string]$CandidateBinary,
    [ValidatePattern('^[A-Fa-f0-9]{64}$')]
    [string]$ExpectedSha256,
    [ValidateRange(10, 300)]
    [int]$TimeoutSeconds = 90
)

$ErrorActionPreference = 'Stop'
$taskName = 'SunRemoteDesktop Maintenance'
$dataRoot = Join-Path $env:ProgramData 'SunRemoteDesktop'
$maintenanceRoot = Join-Path $dataRoot 'Maintenance'
$queueRoot = Join-Path $dataRoot 'MaintenanceQueue'
$policyPath = Join-Path $maintenanceRoot 'policy.json'
$requestPath = Join-Path $queueRoot 'pending.json'

$task = Get-ScheduledTask -TaskName $taskName -ErrorAction Stop
if ($task.State -eq 'Running') { throw 'SunRemoteDesktop maintenance is already running.' }
$policy = Get-Content -LiteralPath $policyPath -Raw | ConvertFrom-Json
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
if ($identity.User.Value -ne [string]$policy.MaintainerSid) {
    throw 'The current account is not the configured SunRemoteDesktop maintainer.'
}

$requestId = [Guid]::NewGuid().ToString('D')
$request = [ordered]@{
    SchemaVersion = 1
    RequestId = $requestId
    Action = $Action
    RequestedBySid = $identity.User.Value
    CreatedUtc = [DateTime]::UtcNow.ToString('o')
    CandidateBinary = $null
    ExpectedSha256 = $null
}
if ($Action -eq 'Deploy') {
    if ([string]::IsNullOrWhiteSpace($CandidateBinary)) {
        throw 'Deploy requires -CandidateBinary.'
    }
    $candidate = (Resolve-Path -LiteralPath $CandidateBinary).Path
    if ([string]::IsNullOrWhiteSpace($ExpectedSha256)) {
        $ExpectedSha256 = (Get-FileHash -LiteralPath $candidate -Algorithm SHA256).Hash
    }
    $request.CandidateBinary = $candidate
    $request.ExpectedSha256 = $ExpectedSha256.ToUpperInvariant()
}

$temporaryRequest = Join-Path $queueRoot ("request-$requestId.tmp")
$resultPath = Join-Path $queueRoot ("result-$requestId.json")
if (Test-Path -LiteralPath $requestPath) {
    throw 'A pending SunRemoteDesktop maintenance request already exists.'
}
$request | ConvertTo-Json | Set-Content -LiteralPath $temporaryRequest -Encoding UTF8
Move-Item -LiteralPath $temporaryRequest -Destination $requestPath
Start-ScheduledTask -TaskName $taskName

$deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
do {
    if (Test-Path -LiteralPath $resultPath -PathType Leaf) { break }
    Start-Sleep -Milliseconds 250
} while ([DateTime]::UtcNow -lt $deadline)
if (-not (Test-Path -LiteralPath $resultPath -PathType Leaf)) {
    $taskInfo = Get-ScheduledTaskInfo -TaskName $taskName
    throw "SunRemoteDesktop maintenance timed out (task result $($taskInfo.LastTaskResult))."
}

$result = Get-Content -LiteralPath $resultPath -Raw | ConvertFrom-Json
if (-not $result.Success) { throw [string]$result.Error }
$result
