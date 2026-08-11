#Requires -Version 5.1
# Build RemoteLink MSI from staged package layout (WiX v3 candle/light).
# If WiX is missing, exits 0 after pointing at the portable zip (core product).

param(
    [switch]$SkipStage
)

$ErrorActionPreference = "Stop"
$Root = Split-Path -Parent $PSScriptRoot
Set-Location $Root

$versionLine = Select-String -Path (Join-Path $Root "Cargo.toml") -Pattern '^\s*version\s*=\s*"([^"]+)"' |
    Select-Object -First 1
$Version = $versionLine.Matches[0].Groups[1].Value
$StageDir = Join-Path $Root "dist\remotelink-$Version"

if (-not $SkipStage) {
    & (Join-Path $Root "scripts\package-release.ps1")
}

if (-not (Test-Path (Join-Path $StageDir "bin\remotelink-host.exe"))) {
    throw "Stage layout missing: $StageDir (run package-release.ps1 first)"
}

function Find-WiX {
    $candleCmd = Get-Command candle -ErrorAction SilentlyContinue
    $lightCmd = Get-Command light -ErrorAction SilentlyContinue
    if ($candleCmd -and $lightCmd) {
        return @{ Candle = $candleCmd.Source; Light = $lightCmd.Source }
    }
    $roots = @(
        "${env:ProgramFiles(x86)}\WiX Toolset v3.14\bin",
        "${env:ProgramFiles(x86)}\WiX Toolset v3.11\bin",
        "${env:ProgramFiles}\WiX Toolset v3.14\bin",
        "${env:ProgramFiles}\WiX Toolset v3.11\bin"
    )
    foreach ($r in $roots) {
        $c = Join-Path $r "candle.exe"
        $l = Join-Path $r "light.exe"
        if ((Test-Path $c) -and (Test-Path $l)) {
            return @{ Candle = $c; Light = $l }
        }
    }
    return $null
}

$wix = Find-WiX
if (-not $wix) {
    Write-Host ""
    Write-Host "WiX Toolset (candle/light) not found - MSI skipped."
    Write-Host "Portable package is ready at:"
    Write-Host "  $StageDir"
    $zip = Join-Path $Root "dist\RemoteLink-$Version-portable.zip"
    if (Test-Path $zip) {
        Write-Host "  $zip"
    }
    Write-Host ""
    Write-Host "Install WiX v3, then re-run: .\scripts\build-msi.ps1 -SkipStage"
    Write-Host "Core product is shippable as the portable zip without MSI."
    exit 0
}

$StageAbs = (Resolve-Path $StageDir).Path
$Wxs = Join-Path $Root "deploy\packaging\Product.wxs"
$ObjDir = Join-Path $Root "dist\wix-obj"
New-Item -ItemType Directory -Force -Path $ObjDir | Out-Null
$Wixobj = Join-Path $ObjDir "Product.wixobj"
$Msi = Join-Path $Root "dist\RemoteLink-$Version.msi"

Write-Host "Building MSI with $($wix.Candle)"
& $wix.Candle "-dProductVersion=$Version" "-dStageDir=$StageAbs" -o $Wixobj $Wxs
if ($LASTEXITCODE -ne 0) {
    throw "candle failed ($LASTEXITCODE)"
}

& $wix.Light -o $Msi $Wixobj
if ($LASTEXITCODE -ne 0) {
    throw "light failed ($LASTEXITCODE)"
}

Write-Host ""
Write-Host "MSI (unsigned): $Msi"
Write-Host "Sign in release pipeline with signtool before distribution."
