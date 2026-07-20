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
    [Parameter(Mandatory = $true)]
    [Alias("Url")]
    [string]$Domain,

    [Parameter(Mandatory = $true)]
    [string]$Token,

    [string]$Version = "latest",
    [string]$InstallDir = "C:\Program Files\AuditReady",
    [string]$ConfigDir = "C:\ProgramData\AuditReady",
    [string]$ServiceName = "AuditReady"
)

$ErrorActionPreference = "Stop"

$Repo = "tutu-learn/AuditReady"
$Target = "x86_64-pc-windows-msvc"

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

    # Create or recreate the service so re-runs apply an updated binary/config.
    $existing = Get-Service -Name $ServiceName -ErrorAction SilentlyContinue
    if ($existing) {
        Stop-Service -Name $ServiceName -Force -ErrorAction SilentlyContinue
        & sc.exe delete $ServiceName | Out-Null
        Start-Sleep -Seconds 1
    }

    $BinaryPath = "`"$InstallDir\auditready.exe`" --config `"$ConfigPath`""
    New-Service -Name $ServiceName `
        -BinaryPathName $BinaryPath `
        -DisplayName "AuditReady Agent" `
        -StartupType Automatic `
        -Description "AuditReady endpoint agent" | Out-Null

    Start-Service -Name $ServiceName
    Write-Host ""
    Write-Host "AuditReady is installed and running."
    Write-Host "  Status:       Get-Service $ServiceName"
    Write-Host "  Restart:      & `"$InstallDir\restart-windows.ps1`""
    Write-Host "  Update token: & `"$InstallDir\update-token-windows.ps1`" <token>"
} finally {
    Remove-Item -Path $TmpDir -Recurse -Force -ErrorAction SilentlyContinue
}
