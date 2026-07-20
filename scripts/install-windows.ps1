#Requires -RunAsAdministrator
<#
.SYNOPSIS
    Install AuditReady agent as a Windows service.

.DESCRIPTION
    Downloads the latest (or a specific) AuditReady release for Windows x64,
    installs the binary, writes appsettings.json, and creates/starts a Windows
    service.

.PARAMETER Domain
    Backend domain or URL, e.g. api.example.com or https://api.example.com.
    Accepts the alias "Url".

.PARAMETER Token
    Agent token used to authenticate with the backend.

.PARAMETER Version
    Release tag to install (default: latest).

.PARAMETER InstallDir
    Directory for the binary and helper scripts (default: C:\Program Files\AuditReady).

.PARAMETER ConfigDir
    Directory for appsettings.json (default: C:\ProgramData\AuditReady).

.PARAMETER ServiceName
    Name of the Windows service (default: AuditReady).

.EXAMPLE
    .\install-windows.ps1 -Domain api.example.com -Token abc123

.EXAMPLE
    .\install-windows.ps1 -Url https://api.example.com -Token abc123 -Version v1.2.3
#>
param(
    [Alias("Url")]
    [string]$Domain,

    [string]$Token,

    [string]$Version = "latest",
    [string]$InstallDir = "C:\Program Files\AuditReady",
    [string]$ConfigDir = "C:\ProgramData\AuditReady",
    [string]$ServiceName = "AuditReady"
)

$ErrorActionPreference = "Stop"

$Repo = "tutu-learn/AuditReady"
$Target = "x86_64-pc-windows-msvc"

# Prompt interactively if values were not passed as parameters.
if (-not $Domain) {
    $Domain = Read-Host "Backend domain or URL (e.g. api.example.com or localhost:8000)"
    if (-not $Domain) {
        throw "A backend domain is required."
    }
}
if (-not $Token) {
    $secure = Read-Host "Agent token" -AsSecureString
    $Token = [System.Runtime.InteropServices.Marshal]::PtrToStringAuto(
        [System.Runtime.InteropServices.Marshal]::SecureStringToBSTR($secure))
    if (-not $Token) {
        throw "An agent token is required."
    }
}

# Normalize a pasted URL down to a bare domain.
$Domain = $Domain -replace '^https?://', '' -replace '/$', ''

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

Write-Host "Installing AuditReady $Version for $Target..."

$TmpDir = Join-Path $env:TEMP "auditready-install-$([System.Guid]::NewGuid())"
New-Item -ItemType Directory -Path $TmpDir -Force | Out-Null

try {
    $ZipPath = Join-Path $TmpDir $Asset
    Invoke-WebRequest -Uri $DownloadUrl -OutFile $ZipPath -UseBasicParsing

    Expand-Archive -Path $ZipPath -DestinationPath $TmpDir -Force
    $ExtractedDir = Join-Path $TmpDir "auditready"

    # Install binary.
    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
    Copy-Item -Path (Join-Path $ExtractedDir "auditready.exe") `
        -Destination (Join-Path $InstallDir "auditready.exe") -Force
    Write-Host "Installed auditready.exe to $InstallDir"

    # Install helper scripts if present in the release archive.
    $RestartScript = Join-Path $ExtractedDir "restart-windows.ps1"
    if (Test-Path $RestartScript) {
        Copy-Item -Path $RestartScript `
            -Destination (Join-Path $InstallDir "restart-windows.ps1") -Force
        Write-Host "Installed restart-windows.ps1 to $InstallDir"
    }
    $UpdateTokenScript = Join-Path $ExtractedDir "update-token-windows.ps1"
    if (Test-Path $UpdateTokenScript) {
        Copy-Item -Path $UpdateTokenScript `
            -Destination (Join-Path $InstallDir "update-token-windows.ps1") -Force
        Write-Host "Installed update-token-windows.ps1 to $InstallDir"
    }

    # Prepare config directory.
    New-Item -ItemType Directory -Path $ConfigDir -Force | Out-Null

    # Write configuration.
    $ConfigPath = Join-Path $ConfigDir "appsettings.json"
    @{
        server = @{
            domain           = $Domain
            token            = $Token
            interval_seconds = 10
            tunnel_enabled   = $true
            tunnel_shell     = $null
            tunnel_cwd       = $ConfigDir
        }
    } | ConvertTo-Json -Depth 10 | Out-File -FilePath $ConfigPath -Encoding utf8
    Write-Host "Wrote configuration to $ConfigPath"

    # Create or recreate a scheduled task so re-runs apply an updated binary/config.
    # A scheduled task is used instead of a Windows service because auditready.exe
    # is a regular console application.
    $existingTask = Get-ScheduledTask -TaskName $ServiceName -ErrorAction SilentlyContinue
    if ($existingTask) {
        Stop-ScheduledTask -TaskName $ServiceName -ErrorAction SilentlyContinue
        Unregister-ScheduledTask -TaskName $ServiceName -Confirm:$false -ErrorAction SilentlyContinue
        Start-Sleep -Seconds 1
    }

    # Clean up any stale Windows service from an earlier install attempt.
    $existingService = Get-Service -Name $ServiceName -ErrorAction SilentlyContinue
    if ($existingService) {
        Stop-Service -Name $ServiceName -Force -ErrorAction SilentlyContinue
        & sc.exe delete $ServiceName | Out-Null
        Start-Sleep -Seconds 1
    }

    $action = New-ScheduledTaskAction -Execute (Join-Path $InstallDir "auditready.exe") `
        -Argument "--config `"$ConfigPath`""
    $trigger = New-ScheduledTaskTrigger -AtStartup
    $principal = New-ScheduledTaskPrincipal -UserId "SYSTEM" `
        -LogonType ServiceAccount -RunLevel Highest
    $settings = New-ScheduledTaskSettingsSet `
        -AllowStartIfOnBatteries `
        -DontStopIfGoingOnBatteries `
        -StartWhenAvailable `
        -MultipleInstances IgnoreNew `
        -RestartCount 3 `
        -RestartInterval (New-TimeSpan -Minutes 1)

    Register-ScheduledTask -TaskName $ServiceName `
        -Action $action -Trigger $trigger -Principal $principal -Settings $settings -Force | Out-Null

    Start-ScheduledTask -TaskName $ServiceName
    Write-Host ""
    Write-Host "AuditReady is installed and running."
    Write-Host "  Status:       Get-ScheduledTaskInfo $ServiceName"
    Write-Host "  Restart:      & `"$InstallDir\restart-windows.ps1`""
    Write-Host "  Update token: & `"$InstallDir\update-token-windows.ps1`" <token>"
} finally {
    Remove-Item -Path $TmpDir -Recurse -Force -ErrorAction SilentlyContinue
}
