param(
    [Parameter(Mandatory = $true)]
    [string]$CandidateBinary,
    [Parameter(Mandatory = $true)]
    [ValidatePattern('^[A-Fa-f0-9]{64}$')]
    [string]$ExpectedSha256,
    [switch]$PreflightOnly
)

$ErrorActionPreference = 'Stop'
$projectRoot = (Resolve-Path -LiteralPath (Split-Path -Parent $PSScriptRoot)).Path
$candidate = (Resolve-Path -LiteralPath $CandidateBinary).Path
$targetRoot = Join-Path $projectRoot 'target'
if (-not $candidate.StartsWith($targetRoot + '\', [StringComparison]::OrdinalIgnoreCase)) {
    throw 'Candidate must be below this project target directory.'
}
if ([IO.Path]::GetFileName($candidate) -ne 'sun-remote-desktop.exe') {
    throw 'Unexpected candidate executable name.'
}
$actualHash = (Get-FileHash -LiteralPath $candidate -Algorithm SHA256).Hash
if ($actualHash -ne $ExpectedSha256) { throw 'Candidate SHA256 does not match the verified build.' }
$versionOutput = (& $candidate --version 2>&1 | Out-String).Trim()
if ($LASTEXITCODE -ne 0 -or $versionOutput -notmatch '^sun-remote-desktop\s+') {
    throw 'Candidate startup check failed.'
}

$taskName = 'SunRemoteDesktop Maintenance'
$task = Get-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue
if ($PreflightOnly) {
    [pscustomobject]@{
        Candidate = $candidate
        SHA256 = $actualHash
        Version = $versionOutput
        MaintenanceInstalled = ($null -ne $task)
        MaintenanceTask = $taskName
    }
    return
}
if ($null -eq $task) {
    throw 'SunRemoteDesktop maintenance is not installed; run install-maintenance.ps1 once with administrator approval.'
}

& (Join-Path $PSScriptRoot 'invoke-maintenance.ps1') `
    -Action Deploy `
    -CandidateBinary $candidate `
    -ExpectedSha256 $actualHash
