#Requires -RunAsAdministrator
<#
.SYNOPSIS
    Restart the AuditReady agent scheduled task.

.PARAMETER TaskName
    Name of the scheduled task (default: AuditReady).

.EXAMPLE
    .\restart-windows.ps1
#>
param(
    [string]$TaskName = "AuditReady"
)

$ErrorActionPreference = "Stop"

$task = Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
if (-not $task) {
    throw "Scheduled task $TaskName not found. Is AuditReady installed?"
}

Stop-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
Start-Sleep -Seconds 2
Start-ScheduledTask -TaskName $TaskName

Write-Host "AuditReady restarted successfully."
Write-Host "  Status: Get-ScheduledTaskInfo $TaskName"
