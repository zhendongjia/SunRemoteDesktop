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
$targetRoot = Join-Path $projectRoot 'target'
$candidate = (Resolve-Path -LiteralPath $CandidateBinary).Path
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

$workerSource = Join-Path $PSScriptRoot 'maintenance-worker.ps1'
$workerTokens = $null
$workerErrors = $null
[Management.Automation.Language.Parser]::ParseFile($workerSource, [ref]$workerTokens, [ref]$workerErrors) | Out-Null
if ($workerErrors.Count -ne 0) { throw 'The maintenance worker did not pass the PowerShell parser check.' }
$powerShell = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
& $powerShell `
    -NoProfile `
    -File $workerSource `
    -SelfTestCandidate $candidate `
    -SelfTestBoundary $projectRoot
if ($LASTEXITCODE -ne 0) { throw 'The maintenance worker candidate-path self-test failed.' }

$serviceName = 'SunRemoteDesktop'
$service = Get-CimInstance Win32_Service -Filter "Name='$serviceName'"
if ($null -eq $service) { throw 'SunRemoteDesktop is not installed.' }
if ($service.PathName -notmatch '^"([^"]+)"\s+service$') { throw 'Unrecognized service command line.' }
$oldBinary = (Resolve-Path -LiteralPath $Matches[1]).Path
$programInstallRoot = Join-Path $env:ProgramFiles 'SunRemoteDesktop'
if (-not $oldBinary.StartsWith($projectRoot + '\', [StringComparison]::OrdinalIgnoreCase) -and
    -not $oldBinary.StartsWith($programInstallRoot + '\', [StringComparison]::OrdinalIgnoreCase)) {
    throw 'The existing service does not belong to this SunRemoteDesktop installation.'
}

$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$maintainerSid = $identity.User.Value
$maintainerName = $identity.Name
$dataRoot = Join-Path $env:ProgramData 'SunRemoteDesktop'
$maintenanceRoot = Join-Path $dataRoot 'Maintenance'
$queueRoot = Join-Path $dataRoot 'MaintenanceQueue'
$workerDestination = Join-Path $maintenanceRoot 'maintenance-worker.ps1'
$policyPath = Join-Path $maintenanceRoot 'policy.json'
$taskName = 'SunRemoteDesktop Maintenance'
$configPath = Join-Path $dataRoot 'config.toml'

if ($PreflightOnly) {
    [pscustomobject]@{
        Candidate = $candidate
        SHA256 = $actualHash
        Version = $versionOutput
        Maintainer = $maintainerName
        MaintainerSid = $maintainerSid
        InstallRoot = $programInstallRoot
        MaintenanceRoot = $maintenanceRoot
        TaskName = $taskName
        PreviousService = $service.PathName
    }
    return
}

$principal = [Security.Principal.WindowsPrincipal]::new($identity)
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'This bootstrap must be run once from an elevated PowerShell.'
}

function Set-ProtectedDirectoryAcl([string]$Path, [string]$UserSid, [bool]$AllowUserWrite) {
    $acl = New-Object Security.AccessControl.DirectorySecurity
    $userIdentity = [Security.Principal.SecurityIdentifier]::new($UserSid)
    $acl.SetOwner([Security.Principal.NTAccount]::new('NT AUTHORITY\SYSTEM'))
    $acl.SetAccessRuleProtection($true, $false)
    $inheritance = [Security.AccessControl.InheritanceFlags]'ContainerInherit, ObjectInherit'
    $propagation = [Security.AccessControl.PropagationFlags]::None
    $allow = [Security.AccessControl.AccessControlType]::Allow
    $acl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new(
        'NT AUTHORITY\SYSTEM', 'FullControl', $inheritance, $propagation, $allow
    ))
    $acl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new(
        'BUILTIN\Administrators', 'FullControl', $inheritance, $propagation, $allow
    ))
    $userRights = if ($AllowUserWrite) { 'Modify' } else { 'ReadAndExecute' }
    $acl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new(
        $userIdentity, $userRights, $inheritance, $propagation, $allow
    ))
    Set-Acl -LiteralPath $Path -AclObject $acl
}

function Set-ProtectedFileAcl([string]$Path, [string]$UserSid) {
    $acl = New-Object Security.AccessControl.FileSecurity
    $userIdentity = [Security.Principal.SecurityIdentifier]::new($UserSid)
    $acl.SetOwner([Security.Principal.NTAccount]::new('NT AUTHORITY\SYSTEM'))
    $acl.SetAccessRuleProtection($true, $false)
    $allow = [Security.AccessControl.AccessControlType]::Allow
    $acl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new(
        'NT AUTHORITY\SYSTEM', 'FullControl', $allow
    ))
    $acl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new(
        'BUILTIN\Administrators', 'FullControl', $allow
    ))
    $acl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new(
        $userIdentity, 'ReadAndExecute', $allow
    ))
    Set-Acl -LiteralPath $Path -AclObject $acl
}

New-Item -ItemType Directory -Path $programInstallRoot -Force | Out-Null
New-Item -ItemType Directory -Path $maintenanceRoot -Force | Out-Null
New-Item -ItemType Directory -Path $queueRoot -Force | Out-Null
Set-ProtectedDirectoryAcl $programInstallRoot $maintainerSid $false
Set-ProtectedDirectoryAcl $maintenanceRoot $maintainerSid $false
Set-ProtectedDirectoryAcl $queueRoot $maintainerSid $true

Copy-Item -LiteralPath $workerSource -Destination $workerDestination -Force
if ((Get-FileHash -LiteralPath $workerSource -Algorithm SHA256).Hash -ne
    (Get-FileHash -LiteralPath $workerDestination -Algorithm SHA256).Hash) {
    throw 'The protected maintenance worker copy failed verification.'
}
Set-ProtectedFileAcl $workerDestination $maintainerSid
$policy = [ordered]@{
    SchemaVersion = 1
    ProjectRoot = $projectRoot
    MaintainerName = $maintainerName
    MaintainerSid = $maintainerSid
    ServiceName = $serviceName
    FirewallRuleName = 'SunRemoteDesktop (SunRDP)'
    InstallRoot = $programInstallRoot
    ConfigPath = $configPath
}
$policy | ConvertTo-Json | Set-Content -LiteralPath $policyPath -Encoding UTF8
Set-ProtectedFileAcl $policyPath $maintainerSid

$taskAction = New-ScheduledTaskAction `
    -Execute $powerShell `
    -Argument ('-NoProfile -NonInteractive -WindowStyle Hidden -File "' + $workerDestination + '"') `
    -WorkingDirectory $maintenanceRoot
$taskPrincipal = New-ScheduledTaskPrincipal `
    -UserId $maintainerName `
    -LogonType Interactive `
    -RunLevel Highest
$taskSettings = New-ScheduledTaskSettingsSet `
    -AllowStartIfOnBatteries `
    -DontStopIfGoingOnBatteries `
    -ExecutionTimeLimit (New-TimeSpan -Minutes 5) `
    -MultipleInstances IgnoreNew
$task = New-ScheduledTask `
    -Action $taskAction `
    -Principal $taskPrincipal `
    -Settings $taskSettings `
    -Description 'Restricted maintenance entry for SunRemoteDesktop deploy, repair, and restart operations.'
Register-ScheduledTask `
    -TaskName $taskName `
    -InputObject $task `
    -Force | Out-Null

$requestId = [Guid]::NewGuid().ToString('D')
$request = [ordered]@{
    SchemaVersion = 1
    RequestId = $requestId
    Action = 'Deploy'
    RequestedBySid = $maintainerSid
    CreatedUtc = [DateTime]::UtcNow.ToString('o')
    CandidateBinary = $candidate
    ExpectedSha256 = $actualHash
}
$requestPath = Join-Path $queueRoot 'pending.json'
$temporaryRequest = Join-Path $queueRoot ("request-$requestId.tmp")
$resultPath = Join-Path $queueRoot ("result-$requestId.json")
if (Test-Path -LiteralPath $requestPath) {
    throw 'A pending SunRemoteDesktop maintenance request already exists.'
}
$request | ConvertTo-Json | Set-Content -LiteralPath $temporaryRequest -Encoding UTF8
Move-Item -LiteralPath $temporaryRequest -Destination $requestPath
Start-ScheduledTask -TaskName $taskName

$deadline = [DateTime]::UtcNow.AddSeconds(120)
do {
    if (Test-Path -LiteralPath $resultPath -PathType Leaf) { break }
    Start-Sleep -Milliseconds 250
} while ([DateTime]::UtcNow -lt $deadline)
if (-not (Test-Path -LiteralPath $resultPath -PathType Leaf)) {
    $taskInfo = Get-ScheduledTaskInfo -TaskName $taskName
    throw "The initial maintenance deployment timed out (task result $($taskInfo.LastTaskResult))."
}
$result = Get-Content -LiteralPath $resultPath -Raw | ConvertFrom-Json
if (-not $result.Success) { throw [string]$result.Error }

[pscustomobject]@{
    Success = $true
    TaskName = $taskName
    Maintainer = $maintainerName
    Binary = $result.Binary
    SHA256 = $result.SHA256
    Port = $result.Port
    ProcessId = $result.ProcessId
}
