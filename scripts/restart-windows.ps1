#Requires -RunAsAdministrator
<#
.SYNOPSIS
    Restart the AuditReady agent Windows service.

.PARAMETER ServiceName
    Name of the Windows service (default: AuditReady).

.EXAMPLE
    .\restart-windows.ps1
#>
param(
    [string]$ServiceName = "AuditReady"
)

$ErrorActionPreference = "Stop"

$service = Get-Service -Name $ServiceName -ErrorAction SilentlyContinue
if (-not $service) {
    throw "Service $ServiceName not found. Is AuditReady installed?"
}

Restart-Service -Name $ServiceName -Force
Write-Host "AuditReady restarted successfully."
Write-Host "  Status: Get-Service $ServiceName"
