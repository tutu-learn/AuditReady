#Requires -RunAsAdministrator
<#
.SYNOPSIS
    Update an existing AuditReady installation to the latest (or a given) release.

.DESCRIPTION
    Keeps the existing configuration and scheduled task; only replaces the
    binary and helper scripts.

.PARAMETER Version
    Release tag to install (default: latest).

.PARAMETER InstallDir
    Directory containing auditready.exe (default: C:\Program Files\AuditReady).

.PARAMETER ConfigDir
    Directory containing appsettings.json (default: C:\ProgramData\AuditReady).

.PARAMETER TaskName
    Name of the scheduled task (default: AuditReady).

.EXAMPLE
    .\update-windows.ps1

.EXAMPLE
    .\update-windows.ps1 -Version v1.2.3
#>
param(
    [string]$Version = "latest",
    [string]$InstallDir = "C:\Program Files\AuditReady",
    [string]$ConfigDir = "C:\ProgramData\AuditReady",
    [string]$TaskName = "AuditReady"
)

$ErrorActionPreference = "Stop"

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

Write-Host "Updating AuditReady to $Version for $Target..."

$TmpDir = Join-Path $env:TEMP "auditready-update-$([System.Guid]::NewGuid())"
New-Item -ItemType Directory -Path $TmpDir -Force | Out-Null

try {
    $ZipPath = Join-Path $TmpDir $Asset
    Invoke-WebRequest -Uri $DownloadUrl -OutFile $ZipPath -UseBasicParsing

    Expand-Archive -Path $ZipPath -DestinationPath $TmpDir -Force
    $ExtractedDir = Join-Path $TmpDir "auditready"

    # Stop the scheduled task before replacing the running executable.
    $task = Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
    if ($task) {
        Stop-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
    }

    # Update binary.
    Copy-Item -Path (Join-Path $ExtractedDir "auditready.exe") `
        -Destination $BinaryPath -Force
    Write-Host "Updated $BinaryPath"

    # Update helper scripts if present in the release archive.
    $scripts = @("restart-windows.ps1", "update-token-windows.ps1", "update-windows.ps1")
    foreach ($script in $scripts) {
        $source = Join-Path $ExtractedDir $script
        if (Test-Path $source) {
            Copy-Item -Path $source -Destination (Join-Path $InstallDir $script) -Force
            Write-Host "Updated ${InstallDir}\${script}"
        }
    }

    # Ensure config directory exists.
    New-Item -ItemType Directory -Path $ConfigDir -Force | Out-Null

    # Start the scheduled task again.
    if ($task) {
        Start-ScheduledTask -TaskName $TaskName
        Write-Host ""
        Write-Host "AuditReady $Version is installed and running."
        Write-Host "  Status: Get-ScheduledTaskInfo $TaskName"
    } else {
        Write-Host "No $TaskName task found; binary updated. Start the agent manually."
    }
} finally {
    Remove-Item -Path $TmpDir -Recurse -Force -ErrorAction SilentlyContinue
}
