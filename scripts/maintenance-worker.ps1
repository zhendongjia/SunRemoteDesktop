param(
    [string]$SelfTestCandidate,
    [string]$SelfTestBoundary
)

$ErrorActionPreference = 'Stop'

$dataRoot = Join-Path $env:ProgramData 'SunRemoteDesktop'
$maintenanceRoot = Join-Path $dataRoot 'Maintenance'
$queueRoot = Join-Path $dataRoot 'MaintenanceQueue'
$policyPath = Join-Path $maintenanceRoot 'policy.json'
$requestPath = Join-Path $queueRoot 'pending.json'
$lastErrorPath = Join-Path $queueRoot 'last-error.json'
$processingPath = $null
$resultPath = $null
$requestId = $null
$result = [ordered]@{
    SchemaVersion = 1
    RequestId = $null
    Action = $null
    Success = $false
    StartedUtc = [DateTime]::UtcNow.ToString('o')
    FinishedUtc = $null
    Error = $null
    RolledBack = $false
}

function Write-JsonAtomic([object]$Value, [string]$Path) {
    $temporaryPath = $Path + '.tmp'
    $Value | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath $temporaryPath -Encoding UTF8
    Move-Item -LiteralPath $temporaryPath -Destination $Path -Force
}

function Set-ProtectedDirectoryAcl([string]$Path, [string]$UserSid) {
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
    $acl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new(
        $userIdentity, 'ReadAndExecute', $inheritance, $propagation, $allow
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

function Test-IsBelow([string]$Path, [string]$Root) {
    $fullPath = [IO.Path]::GetFullPath($Path)
    $fullRoot = [IO.Path]::GetFullPath($Root).TrimEnd('\') + '\'
    return $fullPath.StartsWith($fullRoot, [StringComparison]::OrdinalIgnoreCase)
}

function Assert-NoReparsePoint([string]$Path, [string]$Boundary) {
    $current = Get-Item -LiteralPath $Path -Force
    $boundaryPath = [IO.Path]::GetFullPath($Boundary).TrimEnd('\')
    while ($null -ne $current) {
        if (($current.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
            throw "Reparse points are not allowed in a candidate path: $($current.FullName)"
        }
        if ($current.FullName.TrimEnd('\').Equals($boundaryPath, [StringComparison]::OrdinalIgnoreCase)) {
            return
        }
        if ($current -is [IO.FileInfo]) {
            $current = $current.Directory
        } else {
            $current = $current.Parent
        }
    }
    throw 'Candidate path escaped the configured project boundary.'
}

if (-not [string]::IsNullOrWhiteSpace($SelfTestCandidate) -or
    -not [string]::IsNullOrWhiteSpace($SelfTestBoundary)) {
    if ([string]::IsNullOrWhiteSpace($SelfTestCandidate) -or
        [string]::IsNullOrWhiteSpace($SelfTestBoundary)) {
        throw 'Both maintenance worker self-test paths are required.'
    }
    Assert-NoReparsePoint `
        (Resolve-Path -LiteralPath $SelfTestCandidate).Path `
        (Resolve-Path -LiteralPath $SelfTestBoundary).Path
    return
}

function Get-ServiceInfo([string]$ServiceName) {
    $service = Get-CimInstance Win32_Service -Filter "Name='$ServiceName'"
    if ($null -eq $service) { throw "Windows service '$ServiceName' is not installed." }
    return $service
}

function Set-ServiceBinary([string]$ServiceName, [string]$CommandLine) {
    $service = Get-ServiceInfo $ServiceName
    $change = Invoke-CimMethod -InputObject $service -MethodName Change -Arguments @{ PathName = $CommandLine }
    if ($change.ReturnValue -ne 0) {
        throw "Unable to configure the service binary (CIM return $($change.ReturnValue))."
    }
}

function Stop-HostService([string]$ServiceName) {
    $service = Get-Service -Name $ServiceName
    if ($service.Status -ne 'Stopped') {
        Stop-Service -Name $ServiceName
        $service.WaitForStatus('Stopped', [TimeSpan]::FromSeconds(20))
    }
}

function Start-HostService([string]$ServiceName) {
    $service = Get-Service -Name $ServiceName
    if ($service.Status -ne 'Running') {
        Start-Service -Name $ServiceName
        $service.WaitForStatus('Running', [TimeSpan]::FromSeconds(20))
    }
}

function Get-ConfiguredPort([string]$ConfigPath) {
    $portSetting = Select-String -LiteralPath $ConfigPath -Pattern '^\s*port\s*=\s*(\d+)\s*$' |
        Select-Object -First 1
    if ($null -eq $portSetting) { throw 'Unable to determine the configured SunRDP TCP port.' }
    $port = [int]$portSetting.Matches[0].Groups[1].Value
    if ($port -lt 1 -or $port -gt 65535) { throw 'The configured SunRDP TCP port is invalid.' }
    return $port
}

function Set-HostFirewall([string]$RuleName, [int]$Port) {
    Remove-NetFirewallRule -DisplayName $RuleName -ErrorAction SilentlyContinue
    New-NetFirewallRule `
        -DisplayName $RuleName `
        -Direction Inbound `
        -Action Allow `
        -Protocol TCP `
        -LocalPort $Port `
        -Profile Domain,Private | Out-Null
}

function Wait-HostReady([string]$ServiceName, [int]$Port) {
    $deadline = [DateTime]::UtcNow.AddSeconds(25)
    do {
        $service = Get-ServiceInfo $ServiceName
        $listener = Get-NetTCPConnection -LocalPort $Port -State Listen -ErrorAction SilentlyContinue |
            Where-Object { $_.OwningProcess -eq $service.ProcessId }
        if ($service.State -eq 'Running' -and $null -ne $listener) {
            return $service
        }
        Start-Sleep -Milliseconds 250
    } while ([DateTime]::UtcNow -lt $deadline)
    throw "SunRDP did not become ready on TCP $Port."
}

function Get-InstalledBinary([string]$ServiceName, [string]$InstallRoot) {
    $service = Get-ServiceInfo $ServiceName
    if ($service.PathName -notmatch '^"([^"]+)"\s+service$') {
        throw 'The SunRemoteDesktop service command line is not recognized.'
    }
    $binary = [IO.Path]::GetFullPath($Matches[1])
    if (-not (Test-IsBelow $binary $InstallRoot)) {
        throw 'The service executable is outside the protected SunRemoteDesktop installation directory.'
    }
    if (-not (Test-Path -LiteralPath $binary -PathType Leaf)) {
        throw 'The installed SunRemoteDesktop executable is missing.'
    }
    return $binary
}

try {
    if (-not (Test-Path -LiteralPath $policyPath -PathType Leaf)) {
        throw 'The protected maintenance policy is missing.'
    }
    if (-not (Test-Path -LiteralPath $requestPath -PathType Leaf)) {
        throw 'No pending SunRemoteDesktop maintenance request exists.'
    }

    $policy = Get-Content -LiteralPath $policyPath -Raw | ConvertFrom-Json
    if ($policy.SchemaVersion -ne 1) { throw 'Unsupported maintenance policy version.' }
    $request = Get-Content -LiteralPath $requestPath -Raw | ConvertFrom-Json
    $requestGuid = [Guid]::Parse([string]$request.RequestId)
    $requestId = $requestGuid.ToString('D')
    $result.RequestId = $requestId
    $result.Action = [string]$request.Action
    $resultPath = Join-Path $queueRoot ("result-$requestId.json")
    $processingPath = Join-Path $queueRoot ("processing-$requestId.json")
    Move-Item -LiteralPath $requestPath -Destination $processingPath

    if ([string]$request.RequestedBySid -ne [string]$policy.MaintainerSid) {
        throw 'The request owner is not the configured SunRemoteDesktop maintainer.'
    }
    $createdUtc = [DateTime]::Parse(
        [string]$request.CreatedUtc,
        [Globalization.CultureInfo]::InvariantCulture,
        [Globalization.DateTimeStyles]::RoundtripKind
    ).ToUniversalTime()
    $requestAge = [DateTime]::UtcNow - $createdUtc
    if ($requestAge.TotalMinutes -gt 10 -or $requestAge.TotalMinutes -lt -2) {
        throw 'The maintenance request is stale or has an invalid timestamp.'
    }

    $serviceName = [string]$policy.ServiceName
    $ruleName = [string]$policy.FirewallRuleName
    $installRoot = [IO.Path]::GetFullPath([string]$policy.InstallRoot)
    $projectRoot = [IO.Path]::GetFullPath([string]$policy.ProjectRoot)
    $targetRoot = Join-Path $projectRoot 'target'
    $configPath = [IO.Path]::GetFullPath([string]$policy.ConfigPath)
    $maintainerSid = [string]$policy.MaintainerSid
    $runKey = 'HKLM:\Software\Microsoft\Windows\CurrentVersion\Run'
    $runName = 'SunRemoteDesktopAgent'
    $port = Get-ConfiguredPort $configPath

    switch ([string]$request.Action) {
        'Deploy' {
            if ([string]$request.ExpectedSha256 -notmatch '^[A-Fa-f0-9]{64}$') {
                throw 'The deployment request has an invalid SHA256 value.'
            }
            $candidate = (Resolve-Path -LiteralPath ([string]$request.CandidateBinary)).Path
            if (-not (Test-IsBelow $candidate $targetRoot)) {
                throw 'The candidate must be below this project target directory.'
            }
            if ([IO.Path]::GetFileName($candidate) -ne 'sun-remote-desktop.exe') {
                throw 'The candidate executable name is invalid.'
            }
            Assert-NoReparsePoint $candidate $projectRoot

            $expectedHash = ([string]$request.ExpectedSha256).ToUpperInvariant()
            $stagingRoot = Join-Path $installRoot 'staging'
            $stagedBinary = Join-Path $stagingRoot ("$requestId.exe")
            New-Item -ItemType Directory -Path $stagingRoot -Force | Out-Null
            Set-ProtectedDirectoryAcl $stagingRoot $maintainerSid
            Copy-Item -LiteralPath $candidate -Destination $stagedBinary
            Set-ProtectedFileAcl $stagedBinary $maintainerSid
            try {
                $actualHash = (Get-FileHash -LiteralPath $stagedBinary -Algorithm SHA256).Hash
                if ($actualHash -ne $expectedHash) { throw 'The staged candidate SHA256 does not match the request.' }
                $versionOutput = (& $stagedBinary --version 2>&1 | Out-String).Trim()
                if ($LASTEXITCODE -ne 0 -or $versionOutput -notmatch '^sun-remote-desktop\s+') {
                    throw 'The staged candidate startup check failed.'
                }

                $versionsRoot = Join-Path $installRoot 'versions'
                New-Item -ItemType Directory -Path $versionsRoot -Force | Out-Null
                Set-ProtectedDirectoryAcl $versionsRoot $maintainerSid
                $versionRoot = Join-Path $versionsRoot $actualHash.Substring(0, 16).ToLowerInvariant()
                $installedBinary = Join-Path $versionRoot 'sun-remote-desktop.exe'
                New-Item -ItemType Directory -Path $versionRoot -Force | Out-Null
                Set-ProtectedDirectoryAcl $versionRoot $maintainerSid
                if (Test-Path -LiteralPath $installedBinary) {
                    if ((Get-FileHash -LiteralPath $installedBinary -Algorithm SHA256).Hash -ne $actualHash) {
                        throw 'The protected version directory contains a binary with an unexpected hash.'
                    }
                } else {
                    Copy-Item -LiteralPath $stagedBinary -Destination $installedBinary
                }
                Set-ProtectedFileAcl $installedBinary $maintainerSid

                $oldService = Get-ServiceInfo $serviceName
                $oldServicePath = [string]$oldService.PathName
                $oldAgentCommand = (Get-ItemProperty -LiteralPath $runKey -Name $runName -ErrorAction SilentlyContinue).$runName
                $serviceChanged = $false
                try {
                    Set-HostFirewall $ruleName $port
                    Stop-HostService $serviceName
                    Set-ServiceBinary $serviceName ('"' + $installedBinary + '" service')
                    $serviceChanged = $true
                    Set-Service -Name $serviceName -StartupType Automatic
                    Set-ItemProperty -LiteralPath $runKey -Name $runName -Value ('"' + $installedBinary + '" agent')
                    Start-HostService $serviceName
                    $running = Wait-HostReady $serviceName $port
                } catch {
                    $deploymentError = $_.Exception.Message
                    if ($serviceChanged -or (Get-Service -Name $serviceName).Status -ne 'Running') {
                        try {
                            Stop-HostService $serviceName
                            Set-ServiceBinary $serviceName $oldServicePath
                            if ($null -ne $oldAgentCommand) {
                                Set-ItemProperty -LiteralPath $runKey -Name $runName -Value $oldAgentCommand
                            } else {
                                Remove-ItemProperty -LiteralPath $runKey -Name $runName -ErrorAction SilentlyContinue
                            }
                            Start-HostService $serviceName
                            $result.RolledBack = $true
                        } catch {
                            $deploymentError += '; rollback failed: ' + $_.Exception.Message
                        }
                    }
                    throw $deploymentError
                }

                $result.SHA256 = $actualHash
                $result.Binary = $installedBinary
                $result.Version = $versionOutput
                $result.Port = $port
                $result.ProcessId = $running.ProcessId
            } finally {
                Remove-Item -LiteralPath $stagedBinary -Force -ErrorAction SilentlyContinue
            }
        }
        'Restart' {
            $installedBinary = Get-InstalledBinary $serviceName $installRoot
            Stop-HostService $serviceName
            Start-HostService $serviceName
            $running = Wait-HostReady $serviceName $port
            $result.Binary = $installedBinary
            $result.Port = $port
            $result.ProcessId = $running.ProcessId
        }
        'Repair' {
            $installedBinary = Get-InstalledBinary $serviceName $installRoot
            Set-Service -Name $serviceName -StartupType Automatic
            Set-ItemProperty -LiteralPath $runKey -Name $runName -Value ('"' + $installedBinary + '" agent')
            Set-HostFirewall $ruleName $port
            Start-HostService $serviceName
            $running = Wait-HostReady $serviceName $port
            $result.Binary = $installedBinary
            $result.Port = $port
            $result.ProcessId = $running.ProcessId
        }
        default {
            throw "Unsupported maintenance action: $($request.Action)"
        }
    }

    $result.Success = $true
} catch {
    $result.Error = $_.Exception.Message
} finally {
    $result.FinishedUtc = [DateTime]::UtcNow.ToString('o')
    if ($null -ne $resultPath) {
        Write-JsonAtomic $result $resultPath
    } else {
        Write-JsonAtomic $result $lastErrorPath
    }
    if ($null -ne $processingPath) {
        Remove-Item -LiteralPath $processingPath -Force -ErrorAction SilentlyContinue
    }
}

if (-not $result.Success) { exit 1 }
