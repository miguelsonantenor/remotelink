#Requires -Version 5.1
<#
.SYNOPSIS
  Start server + host for a one-machine RemoteLink lab (portable package).
#>
[CmdletBinding()]
param(
    [string]$Server = "http://127.0.0.1:18080"
)

$ErrorActionPreference = "Stop"
$Here = $PSScriptRoot
$Bin = Join-Path $Here "bin"
$appExe = Join-Path $Bin "remotelink-app.exe"
$hostExe = Join-Path $Bin "remotelink-host.exe"
$serverExe = Join-Path $Bin "remotelink-server.exe"
$viewerExe = Join-Path $Bin "remotelink-viewer.exe"

if (-not (Test-Path $serverExe)) { throw "Missing $serverExe" }
if (-not (Test-Path $appExe) -and -not (Test-Path $hostExe)) {
    throw "Missing remotelink-app.exe and remotelink-host.exe"
}

Write-Host "Starting remotelink-server in a new window..."
Start-Process -FilePath $serverExe -WorkingDirectory $Bin

Start-Sleep -Seconds 1

if (Test-Path $appExe) {
    Write-Host "Starting remotelink-app (product shell) in a new window..."
    Start-Process -FilePath $appExe -ArgumentList @("--server=$Server") -WorkingDirectory $Bin
    Write-Host ""
    Write-Host "In RemoteLink: ensure Allow remote access is on; copy Your ID + OTP."
    Write-Host "Connect from another instance with those values (Advanced → Server = $Server)."
} else {
    Write-Host "Starting remotelink-host (service + tray) in a new window..."
    Start-Process -FilePath $hostExe -ArgumentList @(
        "--role=service",
        "--server=$Server",
        "--transport=live"
    ) -WorkingDirectory $Bin
    Write-Host ""
    Write-Host "Host window will print public_id and Mode A OTP."
    if (Test-Path $viewerExe) {
        Write-Host "Then run (replace PUBLIC_ID and CODE):"
        Write-Host "  $viewerExe --ws-connect --server=$Server --host PUBLIC_ID --otp CODE --transport=live"
    }
}
