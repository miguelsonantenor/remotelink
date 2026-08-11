#Requires -Version 5.1
<#
.SYNOPSIS
  Start server + host for a one-machine RemoteLink lab (portable package).
#>
[CmdletBinding()]
param(
    [string]$Server = "http://127.0.0.1:8080"
)

$ErrorActionPreference = "Stop"
$Here = $PSScriptRoot
$Bin = Join-Path $Here "bin"
$hostExe = Join-Path $Bin "remotelink-host.exe"
$serverExe = Join-Path $Bin "remotelink-server.exe"
$viewerExe = Join-Path $Bin "remotelink-viewer.exe"

foreach ($p in @($hostExe, $serverExe, $viewerExe)) {
    if (-not (Test-Path $p)) { throw "Missing $p" }
}

Write-Host "Starting remotelink-server in a new window..."
Start-Process -FilePath $serverExe -WorkingDirectory $Bin

Start-Sleep -Seconds 1

Write-Host "Starting remotelink-host (service + tray) in a new window..."
Start-Process -FilePath $hostExe -ArgumentList @(
    "--role=service",
    "--server=$Server",
    "--transport=live"
) -WorkingDirectory $Bin

Write-Host ""
Write-Host "Host window will print public_id and Mode A OTP."
Write-Host "Then run (replace PUBLIC_ID and CODE):"
Write-Host "  $viewerExe --ws-connect --server=$Server --host PUBLIC_ID --otp CODE --transport=live"
Write-Host ""
Write-Host "Or use lab-start after reading OTP from host console / tray balloon."
