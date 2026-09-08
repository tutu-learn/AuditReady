#Requires -RunAsAdministrator
<#
.SYNOPSIS
    Enable client mode on an existing AuditReady installation.

.DESCRIPTION
    Registers a per-user AtLogon scheduled task (AuditReady-Client) that runs
    the already-installed agent in client mode (file-change and clipboard
    monitoring, tray icon). Does not modify the binary, configuration, or the
    main AuditReady task.

.PARAMETER InstallDir
    Directory containing auditready.exe (default: C:\Program Files\AuditReady).

.PARAMETER ConfigDir
    Directory containing appsettings.json (default: C:\ProgramData\AuditReady).

.PARAMETER ClientTaskName
    Name of the per-user scheduled task (default: AuditReady-Client).

.EXAMPLE
    .\enable-client-mode-windows.ps1
#>
param(
    [string]$InstallDir = "C:\Program Files\AuditReady",
    [string]$ConfigDir = "C:\ProgramData\AuditReady",
    [string]$ClientTaskName = "AuditReady-Client"
)

$ErrorActionPreference = "Stop"

$BinaryPath = Join-Path $InstallDir "auditready.exe"
if (-not (Test-Path $BinaryPath)) {
    throw "No existing installation at $BinaryPath. Use install-windows.ps1 for a fresh install."
}

$ConfigPath = Join-Path $ConfigDir "appsettings.json"
if (-not (Test-Path $ConfigPath)) {
    throw "No configuration at $ConfigPath. Use install-windows.ps1 for a fresh install."
}

# Recreate the task so re-runs apply updated paths/settings.
$existingTask = Get-ScheduledTask -TaskName $ClientTaskName -ErrorAction SilentlyContinue
if ($existingTask) {
    Stop-ScheduledTask -TaskName $ClientTaskName -ErrorAction SilentlyContinue
    Unregister-ScheduledTask -TaskName $ClientTaskName -Confirm:$false -ErrorAction SilentlyContinue
    Start-Sleep -Seconds 1
}

$clientAction = New-ScheduledTaskAction -Execute $BinaryPath `
    -Argument "--config `"$ConfigPath`" --mode client"
$clientTrigger = New-ScheduledTaskTrigger -AtLogon
# Any user's logon starts the task as that user.
$clientPrincipal = New-ScheduledTaskPrincipal -GroupId "BUILTIN\Users" -RunLevel Limited
# ExecutionTimeLimit must be PT0S (unlimited): the Task Scheduler default is
# 72 hours, after which it force-stops the task and nothing restarts it.
$clientSettings = New-ScheduledTaskSettingsSet `
    -AllowStartIfOnBatteries `
    -DontStopIfGoingOnBatteries `
    -MultipleInstances IgnoreNew `
    -ExecutionTimeLimit ([TimeSpan]::Zero)

Register-ScheduledTask -TaskName $ClientTaskName `
    -Action $clientAction -Trigger $clientTrigger -Principal $clientPrincipal -Settings $clientSettings -Force | Out-Null

# Start it immediately for the currently logged-in user instead of waiting
# for the next logon.
Start-ScheduledTask -TaskName $ClientTaskName

Write-Host ""
Write-Host "Client mode enabled: $ClientTaskName registered (starts at each user logon)."
Write-Host "A tray icon should appear in the notification area within a few seconds;"
Write-Host "click it to open the stats dashboard."
