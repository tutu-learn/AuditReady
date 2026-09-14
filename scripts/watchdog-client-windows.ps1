<#
.SYNOPSIS
    Keep the AuditReady client-mode process alive.

.DESCRIPTION
    Watches the AuditReady-Client scheduled task and restarts it whenever it
    stops. Run it interactively for ad-hoc monitoring, or register it as its
    own scheduled task (-RegisterTask) so it starts at every user logon.
    Registration requires an elevated PowerShell window.

.PARAMETER ClientTaskName
    Name of the client scheduled task to watch (default: AuditReady-Client).

.PARAMETER CheckIntervalSeconds
    Seconds between status checks (default: 30).

.PARAMETER LogPath
    File where restarts are logged. Defaults to
    $env:LOCALAPPDATA\AuditReady\client-watchdog.log.

.PARAMETER RegisterTask
    When set, registers this script itself as a scheduled task named
    AuditReady-Client-Watchdog that starts at logon and runs forever.

.PARAMETER ScriptPath
    Path to this script when registering the watchdog task.

.EXAMPLE
    # Run interactively (keeps a PowerShell window open)
    .\watchdog-client-windows.ps1

.EXAMPLE
    # Register the watchdog so it starts at every logon
    .\watchdog-client-windows.ps1 -RegisterTask
#>
param(
    [string]$ClientTaskName = "AuditReady-Client",
    [int]$CheckIntervalSeconds = 30,
    [string]$LogPath = (Join-Path $env:LOCALAPPDATA "AuditReady\client-watchdog.log"),
    [switch]$RegisterTask,
    [string]$ScriptPath = $PSCommandPath
)

$ErrorActionPreference = "Stop"

function Test-IsAdmin {
    $identity = [System.Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = New-Object System.Security.Principal.WindowsPrincipal($identity)
    return $principal.IsInRole([System.Security.Principal.WindowsBuiltInRole]::Administrator)
}

function Write-Log {
    param([string]$Message)
    $line = "{0:yyyy-MM-dd HH:mm:ss}  {1}" -f (Get-Date), $Message
    try {
        $dir = Split-Path $LogPath -Parent
        if (-not (Test-Path $dir)) {
            New-Item -ItemType Directory -Path $dir -Force | Out-Null
        }
        Add-Content -Path $LogPath -Value $line -ErrorAction SilentlyContinue
    } catch {}
    Write-Host $line
}

# Register the watchdog as its own scheduled task so it survives reboots.
if ($RegisterTask) {
    if (-not (Test-IsAdmin)) {
        throw "Registering the watchdog task requires an elevated (Run as administrator) PowerShell window."
    }
    if (-not (Test-Path $ScriptPath)) {
        throw "Cannot find this script at $ScriptPath. Re-run with -ScriptPath <full path>."
    }

    $watchdogTaskName = "AuditReady-Client-Watchdog"
    $existing = Get-ScheduledTask -TaskName $watchdogTaskName -ErrorAction SilentlyContinue
    if ($existing) {
        Stop-ScheduledTask -TaskName $watchdogTaskName -ErrorAction SilentlyContinue
        Unregister-ScheduledTask -TaskName $watchdogTaskName -Confirm:$false -ErrorAction SilentlyContinue
        Start-Sleep -Seconds 1
    }

    $argList = "-NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File `"$ScriptPath`" -ClientTaskName `"$ClientTaskName`""
    if ($PSBoundParameters.ContainsKey('LogPath')) {
        $argList += " -LogPath `"$LogPath`""
    }
    $action = New-ScheduledTaskAction -Execute "powershell.exe" -Argument $argList
    $trigger = New-ScheduledTaskTrigger -AtLogon
    $principal = New-ScheduledTaskPrincipal -GroupId "BUILTIN\Users" -RunLevel Limited
    $settings = New-ScheduledTaskSettingsSet `
        -AllowStartIfOnBatteries `
        -DontStopIfGoingOnBatteries `
        -MultipleInstances IgnoreNew `
        -ExecutionTimeLimit ([TimeSpan]::Zero)

    Register-ScheduledTask -TaskName $watchdogTaskName `
        -Action $action -Trigger $trigger -Principal $principal -Settings $settings -Force | Out-Null

    Start-ScheduledTask -TaskName $watchdogTaskName

    Write-Host "Watchdog registered as $watchdogTaskName and started."
    if ($PSBoundParameters.ContainsKey('LogPath')) {
        Write-Host "Logs: $LogPath"
    } else {
        Write-Host "Logs: default per-user location (%LOCALAPPDATA%\AuditReady\client-watchdog.log)"
    }
    return
}

# Validate the client task exists before watching.
$task = Get-ScheduledTask -TaskName $ClientTaskName -ErrorAction SilentlyContinue
if (-not $task) {
    throw "Scheduled task $ClientTaskName not found. Run enable-client-mode-windows.ps1 or install-windows.ps1 -ClientMode first."
}

Write-Log "Watchdog started for $ClientTaskName."

$consecutiveFailures = 0
$maxRapidRestarts = 5
$rapidWindow = New-TimeSpan -Minutes 5
$lastRestarts = [System.Collections.Generic.List[DateTime]]::new()

while ($true) {
    try {
        $task = Get-ScheduledTask -TaskName $ClientTaskName -ErrorAction Stop
        $info = Get-ScheduledTaskInfo -TaskName $ClientTaskName -ErrorAction Stop

        $isRunning = $task.State -eq "Running"
        if (-not $isRunning) {
            # Prune old restart timestamps outside the rapid-restart window.
            $cutoff = (Get-Date) - $rapidWindow
            for ($i = $lastRestarts.Count - 1; $i -ge 0; $i--) {
                if ($lastRestarts[$i] -lt $cutoff) {
                    $lastRestarts.RemoveAt($i)
                }
            }

            if ($lastRestarts.Count -ge $maxRapidRestarts) {
                Write-Log "Client task $ClientTaskName has restarted $maxRapidRestarts times in the last 5 minutes; backing off for 2 minutes."
                Start-Sleep -Seconds 120
                continue
            }

            Write-Log "Client task $ClientTaskName is not running (state: $($task.State), result: $($info.LastTaskResult)). Restarting..."
            Stop-ScheduledTask -TaskName $ClientTaskName -ErrorAction SilentlyContinue
            Start-Sleep -Seconds 2
            Start-ScheduledTask -TaskName $ClientTaskName
            $lastRestarts.Add((Get-Date))
            Write-Log "Restarted $ClientTaskName."
            $consecutiveFailures = 0
        } else {
            $consecutiveFailures = 0
        }
    } catch {
        $consecutiveFailures++
        Write-Log "Watchdog error ($_); consecutive failures: $consecutiveFailures"
        if ($consecutiveFailures -ge 10) {
            Write-Log "Too many consecutive watchdog errors; exiting."
            throw
        }
    }

    Start-Sleep -Seconds $CheckIntervalSeconds
}
