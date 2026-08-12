#Requires -Version 5.1
<#
.SYNOPSIS
  Install RemoteLink portable package into %LOCALAPPDATA%\RemoteLink.
#>
[CmdletBinding()]
param(
    [string]$InstallDir = "",
    [switch]$NoStartMenu,
    [switch]$Startup
)

$ErrorActionPreference = "Stop"
$Here = $PSScriptRoot
$Bin = Join-Path $Here "bin"
if (-not (Test-Path (Join-Path $Bin "remotelink-host.exe")) -and -not (Test-Path (Join-Path $Bin "remotelink-app.exe"))) {
    throw "bin\remotelink-app.exe / remotelink-host.exe not found. Run this script from a package-release layout."
}

if (-not $InstallDir) {
    $InstallDir = Join-Path $env:LOCALAPPDATA "RemoteLink"
}

Write-Host "Installing RemoteLink -> $InstallDir"
New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
New-Item -ItemType Directory -Force -Path (Join-Path $InstallDir "bin") | Out-Null

Copy-Item -Force (Join-Path $Bin "*") (Join-Path $InstallDir "bin")
Get-ChildItem -Path $Here -Filter "LICENSE-*" -File | Copy-Item -Force -Destination $InstallDir
foreach ($f in @("QUICKSTART.md", "package-manifest.json", "lab-start.ps1", "uninstall-portable.ps1")) {
    $src = Join-Path $Here $f
    if (Test-Path $src) { Copy-Item -Force $src $InstallDir }
}

if (-not $NoStartMenu) {
    $programs = Join-Path $env:APPDATA "Microsoft\Windows\Start Menu\Programs\RemoteLink"
    New-Item -ItemType Directory -Force -Path $programs | Out-Null
    $ws = New-Object -ComObject WScript.Shell
    foreach ($pair in @(
            @{ Name = "RemoteLink"; Target = "remotelink-app.exe"; Args = "" },
            @{ Name = "RemoteLink Host (CLI)"; Target = "remotelink-host.exe"; Args = "--role=service --server=http://127.0.0.1:18080 --transport=live" },
            @{ Name = "RemoteLink Viewer (CLI)"; Target = "remotelink-viewer.exe"; Args = "--help" },
            @{ Name = "RemoteLink Server"; Target = "remotelink-server.exe"; Args = "" }
        )) {
        $targetPath = Join-Path $InstallDir "bin\$($pair.Target)"
        if (-not (Test-Path $targetPath)) { continue }
        $lnk = $ws.CreateShortcut((Join-Path $programs "$($pair.Name).lnk"))
        $lnk.TargetPath = $targetPath
        $lnk.Arguments = $pair.Args
        $lnk.WorkingDirectory = Join-Path $InstallDir "bin"
        $lnk.Save()
    }
    Write-Host "Start Menu: $programs"
}

if ($Startup) {
    $appExe = Join-Path $InstallDir "bin\remotelink-app.exe"
    if (Test-Path $appExe) {
        $run = "`"$appExe`" --autostart"
        reg add "HKCU\Software\Microsoft\Windows\CurrentVersion\Run" /v RemoteLink /t REG_SZ /d $run /f | Out-Null
        Write-Host "Start with Windows: $run"
    }
}

Write-Host "Done. See QUICKSTART.md in $InstallDir"
Write-Host "Uninstall: powershell -ExecutionPolicy Bypass -File `"$InstallDir\uninstall-portable.ps1`""
