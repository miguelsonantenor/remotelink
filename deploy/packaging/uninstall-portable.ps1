#Requires -Version 5.1
<#
.SYNOPSIS
  Remove a portable RemoteLink install from %LOCALAPPDATA%\RemoteLink.
#>
[CmdletBinding()]
param(
    [string]$InstallDir = ""
)

$ErrorActionPreference = "Stop"
if (-not $InstallDir) {
    $InstallDir = Join-Path $env:LOCALAPPDATA "RemoteLink"
}

reg delete "HKCU\Software\Microsoft\Windows\CurrentVersion\Run" /v RemoteLink /f 2>$null | Out-Null

$programs = Join-Path $env:APPDATA "Microsoft\Windows\Start Menu\Programs\RemoteLink"
if (Test-Path $programs) {
    Remove-Item -Recurse -Force $programs
    Write-Host "Removed Start Menu shortcuts"
}

if (Test-Path $InstallDir) {
    Remove-Item -Recurse -Force $InstallDir
    Write-Host "Removed $InstallDir"
} else {
    Write-Host "Nothing to remove at $InstallDir"
}
