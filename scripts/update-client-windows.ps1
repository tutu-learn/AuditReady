<#
.SYNOPSIS
    Update / repair the AuditReady client-mode installation on this machine.

.DESCRIPTION
    Downloads the latest (or a specific) AuditReady release, replaces the
    binary and helper scripts, repairs the per-user AuditReady-Client scheduled
    task, and restarts it. Does not touch the main AuditReady agent task.

.PARAMETER Version
    Release tag to install (default: latest).

.PARAMETER InstallDir
    Directory containing auditready.exe (default: C:\Program Files\AuditReady).

.PARAMETER ConfigDir
    Directory containing appsettings.json (default: C:\ProgramData\AuditReady).

.PARAMETER ClientTaskName
    Name of the per-user client scheduled task (default: AuditReady-Client).

.PARAMETER RegisterWatchdog
    Also register the watchdog-client-windows.ps1 watchdog task so the client
    stays running after crashes.

.EXAMPLE
    # Update the client-mode binary/task and repair settings
    .\update-client-windows.ps1

.EXAMPLE
    # Update to a specific release and register the crash watchdog
    .\update-client-windows.ps1 -Version v1.2.3 -RegisterWatchdog
#>
#Requires -RunAsAdministrator
param(
    [string]$Version = "latest",
    [string]$InstallDir = "C:\Program Files\AuditReady",
    [string]$ConfigDir = "C:\ProgramData\AuditReady",
    [string]$ClientTaskName = "AuditReady-Client",
    [switch]$RegisterWatchdog
)

$ErrorActionPreference = "Stop"

# If launched by the running AuditReady agent, re-run detached so the update
# survives the scheduled task/process being stopped while the binary is replaced.
$parentPid = (Get-CimInstance Win32_Process -Filter "ProcessId=$PID").ParentProcessId
$parentName = (Get-Process -Id $parentPid -ErrorAction SilentlyContinue).ProcessName
if (-not $env:AUDITREADY_UPDATE_DETACHED -and ($parentName -eq "auditready" -or -not [Environment]::UserInteractive)) {
    $env:AUDITREADY_UPDATE_DETACHED = "1"
    $logFile = Join-Path $env:ProgramData "AuditReady\update-client.log"
    New-Item -ItemType Directory -Path (Split-Path $logFile) -Force | Out-Null
    Start-Process -FilePath "powershell.exe" `
        -ArgumentList "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File", "`"$PSCommandPath`"" `
        -WindowStyle Hidden `
        -RedirectStandardOutput $logFile `
        -RedirectStandardError $logFile
    Write-Host "AuditReady client update detached; log: $logFile"
    exit 0
}

$Repo = "tutu-learn/AuditReady"
$Target = "x86_64-pc-windows-msvc"

$BinaryPath = Join-Path $InstallDir "auditready.exe"
if (-not (Test-Path $BinaryPath)) {
    throw "No existing installation at $BinaryPath. Use install-windows.ps1 for a fresh install."
}

# Resolve version.
if ($Version -eq "latest") {
    $release = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/latest" -UseBasicParsing
    $Version = $release.tag_name
    if (-not $Version) {
        throw "Failed to determine latest version"
    }
}

$Asset = "auditready-${Target}.zip"
$DownloadUrl = "https://github.com/$Repo/releases/download/$Version/$Asset"

Write-Host "Updating AuditReady client mode to $Version for $Target..."

$TmpDir = Join-Path $env:TEMP "auditready-client-update-$([System.Guid]::NewGuid())"
New-Item -ItemType Directory -Path $TmpDir -Force | Out-Null

try {
    $ZipPath = Join-Path $TmpDir $Asset
    Invoke-WebRequest -Uri $DownloadUrl -OutFile $ZipPath -UseBasicParsing

    Expand-Archive -Path $ZipPath -DestinationPath $TmpDir -Force
    $ExtractedDir = Join-Path $TmpDir "auditready"

    # Stop the client scheduled task before replacing the running executable.
    $clientTask = Get-ScheduledTask -TaskName $ClientTaskName -ErrorAction SilentlyContinue
    if ($clientTask) {
        Stop-ScheduledTask -TaskName $ClientTaskName -ErrorAction SilentlyContinue
    }

    # Stopping the task is asynchronous and best-effort: kill any lingering
    # auditready processes, then wait for the file lock on the executable to be
    # released before overwriting it.
    Get-Process -Name "auditready" -ErrorAction SilentlyContinue |
        Stop-Process -Force -ErrorAction SilentlyContinue
    $unlocked = $false
    for ($i = 0; $i -lt 15; $i++) {
        try {
            $stream = [System.IO.File]::Open($BinaryPath, 'Open', 'ReadWrite', 'None')
            $stream.Close()
            $unlocked = $true
            break
        } catch {
            Start-Sleep -Seconds 1
        }
    }
    if (-not $unlocked) {
        throw "$BinaryPath is still locked by another process after 15s."
    }

    # Update binary.
    Copy-Item -Path (Join-Path $ExtractedDir "auditready.exe") `
        -Destination $BinaryPath -Force
    Write-Host "Updated $BinaryPath"

    # Update helper scripts if present in the release archive.
    $scripts = @(
        "restart-windows.ps1",
        "update-token-windows.ps1",
        "update-windows.ps1",
        "update-client-windows.ps1",
        "enable-client-mode-windows.ps1",
        "watchdog-client-windows.ps1"
    )
    foreach ($script in $scripts) {
        $source = Join-Path $ExtractedDir $script
        if (Test-Path $source) {
            Copy-Item -Path $source -Destination (Join-Path $InstallDir $script) -Force
            Write-Host "Updated ${InstallDir}\${script}"
        }
    }

    # Ensure config directory exists.
    New-Item -ItemType Directory -Path $ConfigDir -Force | Out-Null

    # Recreate or repair the client task so it has the current binary path and
    # the auto-restart / unlimited-runtime settings.
    $ConfigPath = Join-Path $ConfigDir "appsettings.json"
    if (-not (Test-Path $ConfigPath)) {
        throw "No configuration at $ConfigPath. Cannot refresh client task."
    }

    if ($clientTask) {
        Stop-ScheduledTask -TaskName $ClientTaskName -ErrorAction SilentlyContinue
        Unregister-ScheduledTask -TaskName $ClientTaskName -Confirm:$false -ErrorAction SilentlyContinue
        Start-Sleep -Seconds 1
    }

    $clientAction = New-ScheduledTaskAction -Execute $BinaryPath `
        -Argument "--config `"$ConfigPath`" --mode client"
    $clientTrigger = New-ScheduledTaskTrigger -AtLogon
    $clientPrincipal = New-ScheduledTaskPrincipal -GroupId "BUILTIN\Users" -RunLevel Limited
    $clientSettings = New-ScheduledTaskSettingsSet `
        -AllowStartIfOnBatteries `
        -DontStopIfGoingOnBatteries `
        -MultipleInstances IgnoreNew `
        -RestartCount 3 `
        -RestartInterval (New-TimeSpan -Minutes 1) `
        -ExecutionTimeLimit ([TimeSpan]::Zero)

    Register-ScheduledTask -TaskName $ClientTaskName `
        -Action $clientAction -Trigger $clientTrigger -Principal $clientPrincipal -Settings $clientSettings -Force | Out-Null

    # Start it immediately for the currently logged-in user.
    Start-ScheduledTask -TaskName $ClientTaskName

    Write-Host ""
    Write-Host "AuditReady client mode $Version is installed and running."
    Write-Host "  Status: Get-ScheduledTaskInfo $ClientTaskName"

    if ($RegisterWatchdog) {
        $watchdog = Join-Path $InstallDir "watchdog-client-windows.ps1"
        if (Test-Path $watchdog) {
            & $watchdog -RegisterTask -ClientTaskName $ClientTaskName
        } else {
            Write-Warning "watchdog-client-windows.ps1 not found in $InstallDir; skipping watchdog registration."
        }
    }
} finally {
    Remove-Item -Path $TmpDir -Recurse -Force -ErrorAction SilentlyContinue
}
