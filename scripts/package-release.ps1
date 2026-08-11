#Requires -Version 5.1
# Build release binaries and stage a complete portable core product layout.
# Does not require WiX or codesign. MSI is optional via build-msi.ps1.
#
# Usage (repo root):
#   .\scripts\package-release.ps1
#   .\scripts\package-release.ps1 -SkipBuild
#   .\scripts\package-release.ps1 -SkipZip

param(
    [string]$OutDir = "",
    [switch]$SkipBuild,
    [switch]$SkipZip,
    [switch]$Json
)

$ErrorActionPreference = "Stop"
$Root = Split-Path -Parent $PSScriptRoot
Set-Location $Root

$versionLine = Select-String -Path (Join-Path $Root "Cargo.toml") -Pattern '^\s*version\s*=\s*"([^"]+)"' |
    Select-Object -First 1
if (-not $versionLine) {
    throw "Could not read workspace version from Cargo.toml"
}
$Version = $versionLine.Matches[0].Groups[1].Value

if (-not $OutDir) {
    $OutDir = Join-Path $Root "dist\remotelink-$Version"
}

Write-Host "RemoteLink package stage version=$Version out=$OutDir"

if (-not $SkipBuild) {
    if (-not $env:RUSTUP_TOOLCHAIN) {
        $env:RUSTUP_TOOLCHAIN = "stable-x86_64-pc-windows-gnu"
    }
    $mingwCandidates = @(
        "C:\msys64\mingw64\bin",
        "C:\Users\Linked\tools\mingw64\bin",
        (Join-Path $env:USERPROFILE "tools\mingw64\bin")
    )
    $mingw = $mingwCandidates | Where-Object { Test-Path $_ } | Select-Object -First 1
    if ($mingw) {
        $env:Path = $mingw + ";" + $env:USERPROFILE + "\.cargo\bin;" + $env:Path
    } else {
        $env:Path = $env:USERPROFILE + "\.cargo\bin;" + $env:Path
    }
    if (-not (Get-Command gcc -ErrorAction SilentlyContinue)) {
        Write-Warning "gcc not on PATH - windows-gnu release build may fail."
    }
    Write-Host "cargo build --release -p remotelink-host -p remotelink-viewer -p remotelink-server"
    cargo build --release -p remotelink-host -p remotelink-viewer -p remotelink-server
    if ($LASTEXITCODE -ne 0) {
        throw "cargo build failed ($LASTEXITCODE)"
    }
}

$bins = @(
    "remotelink-host.exe",
    "remotelink-viewer.exe",
    "remotelink-server.exe"
)

if (Test-Path $OutDir) {
    Remove-Item -Recurse -Force $OutDir
}
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$binDir = Join-Path $OutDir "bin"
New-Item -ItemType Directory -Force -Path $binDir | Out-Null

$releaseDir = Join-Path $Root "target\release"
foreach ($b in $bins) {
    $src = Join-Path $releaseDir $b
    if (-not (Test-Path $src)) {
        throw "Missing release binary: $src (run without -SkipBuild)"
    }
    Copy-Item -Force $src (Join-Path $binDir $b)
}

Copy-Item -Force (Join-Path $Root "LICENSE-MIT") $OutDir
Copy-Item -Force (Join-Path $Root "LICENSE-APACHE") $OutDir
Copy-Item -Force (Join-Path $Root "deploy\packaging\binaries.toml") $OutDir
Copy-Item -Force (Join-Path $Root "deploy\packaging\README.md") (Join-Path $OutDir "PACKAGING.md")
Copy-Item -Force (Join-Path $Root "deploy\packaging\QUICKSTART.md") $OutDir
Copy-Item -Force (Join-Path $Root "deploy\packaging\install-portable.ps1") $OutDir
Copy-Item -Force (Join-Path $Root "deploy\packaging\uninstall-portable.ps1") $OutDir
Copy-Item -Force (Join-Path $Root "deploy\packaging\lab-start.ps1") $OutDir

$binList = @()
foreach ($b in $bins) {
    $p = Join-Path $binDir $b
    $hash = (Get-FileHash -Algorithm SHA256 $p).Hash.ToLowerInvariant()
    $binList += @{ name = $b; sha256 = $hash; path = "bin/$b" }
}

$manifest = @{
    product      = "RemoteLink"
    version      = $Version
    built_at     = (Get-Date).ToUniversalTime().ToString("o")
    toolchain    = $env:RUSTUP_TOOLCHAIN
    unsigned     = $true
    core_product = $true
    shippable_as = @("portable-zip", "portable-install", "msi-when-wix")
    binaries     = $binList
    notes        = @(
        "Core product complete: host + viewer + server.",
        "Portable zip is the primary shippable artifact without WiX.",
        "MSI: scripts/build-msi.ps1 (requires WiX v3 candle/light).",
        "Sign EXEs + MSI in the release pipeline only (Authenticode).",
        "Host tray: right-click NotifyIcon for Copy OTP / End session / Exit."
    )
}

$manifestPath = Join-Path $OutDir "package-manifest.json"
$manifest | ConvertTo-Json -Depth 6 | Set-Content -Encoding UTF8 $manifestPath

$zipPath = Join-Path $Root "dist\RemoteLink-$Version-portable.zip"
if (-not $SkipZip) {
    New-Item -ItemType Directory -Force -Path (Join-Path $Root "dist") | Out-Null
    if (Test-Path $zipPath) {
        Remove-Item -Force $zipPath
    }
    Compress-Archive -Path (Join-Path $OutDir "*") -DestinationPath $zipPath -Force
    Write-Host "Portable zip: $zipPath"
}

if ($Json) {
    Get-Content $manifestPath -Raw
} else {
    Write-Host ""
    Write-Host "=== Core product package staged ==="
    Write-Host "  Layout: $OutDir"
    if (-not $SkipZip) {
        Write-Host "  Zip:    $zipPath"
    }
    Get-ChildItem -Recurse $OutDir | ForEach-Object {
        if (-not $_.PSIsContainer) {
            $rel = $_.FullName.Substring($OutDir.Length).TrimStart('\')
            Write-Host ("  " + $rel + "  size=" + $_.Length)
        }
    }
    Write-Host ""
    Write-Host ("Install portable:  " + $OutDir + "\install-portable.ps1")
    Write-Host ("Lab demo:          " + $OutDir + "\lab-start.ps1")
    Write-Host "Optional MSI:      .\scripts\build-msi.ps1 -SkipStage"
}
