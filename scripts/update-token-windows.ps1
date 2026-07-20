#Requires -RunAsAdministrator
<#
.SYNOPSIS
    Update the agent token of an existing AuditReady installation.

.DESCRIPTION
    Rewrites server.token in appsettings.json and restarts the scheduled task
    so the new token is used immediately.

.PARAMETER Token
    New agent token. If omitted, you will be prompted interactively.

.PARAMETER ConfigDir
    Directory containing appsettings.json (default: C:\ProgramData\AuditReady).

.PARAMETER TaskName
    Name of the scheduled task (default: AuditReady).

.EXAMPLE
    .\update-token-windows.ps1 abc123

.EXAMPLE
    .\update-token-windows.ps1 -Token abc123
#>
param(
    [Parameter(Mandatory = $false, Position = 0)]
    [string]$Token,

    [string]$ConfigDir = "C:\ProgramData\AuditReady",
    [string]$TaskName = "AuditReady"
)

$ErrorActionPreference = "Stop"

$ConfigPath = Join-Path $ConfigDir "appsettings.json"

if (-not (Test-Path $ConfigPath)) {
    throw "Config not found at $ConfigPath. Is the agent installed?"
}

# Token from argument or interactive prompt.
if (-not $Token) {
    $secure = Read-Host "New agent token" -AsSecureString
    $Token = [System.Runtime.InteropServices.Marshal]::PtrToStringAuto(
        [System.Runtime.InteropServices.Marshal]::SecureStringToBSTR($secure))
    if (-not $Token) {
        throw "A token is required."
    }
}

# Backup existing config.
Copy-Item -Path $ConfigPath -Destination "${ConfigPath}.bak" -Force

# Rewrite server.token, preserving all other settings.
$config = Get-Content -Path $ConfigPath -Raw | ConvertFrom-Json
if (-not $config.server) {
    $config | Add-Member -NotePropertyName server -NotePropertyValue @{}
}
$config.server.token = $Token
$config | ConvertTo-Json -Depth 10 | Out-File -FilePath $ConfigPath -Encoding utf8

Write-Host "Updated token in $ConfigPath (backup at ${ConfigPath}.bak)"

# Restart the agent so it picks up the new token.
$task = Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
if ($task) {
    Stop-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
    Start-Sleep -Seconds 2
    Start-ScheduledTask -TaskName $TaskName
    Write-Host "Restarted $TaskName."
    Write-Host "  Status: Get-ScheduledTaskInfo $TaskName"
} else {
    Write-Host "Restart the agent manually to apply the new token."
}
