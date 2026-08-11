# Build release binaries and stage an unsigned package layout for MSI/MSIX work.
# Does not require WiX or codesign — production signing is a separate release step.
#
# Usage (repo root):
#   .\scripts\package-release.ps1
#   .\scripts\package-release.ps1 -OutDir dist\my-layout
#   .\scripts\package-release.ps1 -SkipBuild

param(
    [string]$OutDir = "",
    [switch]$SkipBuild,
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
    $mingw = "C:\msys64\mingw64\bin"
    if (Test-Path $mingw) {
        $env:Path = "$mingw;$env:USERPROFILE\.cargo\bin;" + $env:Path
    } else {
        $env:Path = "$env:USERPROFILE\.cargo\bin;" + $env:Path
    }
    Write-Host "cargo build --release -p remotelink-host -p remotelink-viewer -p remotelink-server"
    cargo build --release -p remotelink-host -p remotelink-viewer -p remotelink-server
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed ($LASTEXITCODE)" }
}

$bins = @(
    "remotelink-host.exe",
    "remotelink-viewer.exe",
    "remotelink-server.exe"
)

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

$manifest = @{
    product = "RemoteLink"
    version = $Version
    built_at = (Get-Date).ToUniversalTime().ToString("o")
    toolchain = $env:RUSTUP_TOOLCHAIN
    unsigned = $true
    binaries = @($bins | ForEach-Object {
        $p = Join-Path $binDir $_
        $hash = (Get-FileHash -Algorithm SHA256 $p).Hash.ToLowerInvariant()
        @{ name = $_; sha256 = $hash; path = "bin/$_" }
    })
    notes = @(
        "Unsigned layout for WiX/MSIX packaging.",
        "Sign EXEs + MSI in the release pipeline only.",
        "Host tray: right-click NotifyIcon for Copy OTP / End session / Exit."
    )
}

$manifestPath = Join-Path $OutDir "package-manifest.json"
$manifest | ConvertTo-Json -Depth 6 | Set-Content -Encoding UTF8 $manifestPath

if ($Json) {
    Get-Content $manifestPath -Raw
} else {
    Write-Host ""
    Write-Host "Staged package layout:"
    Write-Host "  $OutDir"
    Get-ChildItem -Recurse $OutDir | ForEach-Object {
        $rel = $_.FullName.Substring($OutDir.Length).TrimStart('\')
        if (-not $_.PSIsContainer) {
            Write-Host ("  {0,-40} {1,12:N0} bytes" -f $rel, $_.Length)
        }
    }
    Write-Host ""
    Write-Host "Next: harvest with WiX/cargo-wix or MakeAppx (see deploy/packaging/README.md)"
}
