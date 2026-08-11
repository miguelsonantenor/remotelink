#Requires -Version 5.1
<#
.SYNOPSIS
  List RemoteLink binaries intended for MSI/MSIX packaging.

.DESCRIPTION
  Reads deploy/packaging/binaries.toml (minimal TOML parse) and prints the
  packaging inventory. No codesign and no WiX/MakeAppx invocation - CI-safe.

.PARAMETER Json
  Emit JSON instead of a table.

.PARAMETER RepoRoot
  Repository root (default: parent of scripts/).
#>
[CmdletBinding()]
param(
    [switch]$Json,
    [string]$RepoRoot = ""
)

$ErrorActionPreference = "Stop"

if (-not $RepoRoot) {
    $RepoRoot = Split-Path -Parent $PSScriptRoot
}
$inventoryPath = Join-Path $RepoRoot "deploy\packaging\binaries.toml"
if (-not (Test-Path -LiteralPath $inventoryPath)) {
    Write-Error "Inventory not found: $inventoryPath"
}

# Minimal [[binary]] table parse - enough for our inventory (no nested tables).
$raw = Get-Content -LiteralPath $inventoryPath -Raw
$binaries = New-Object System.Collections.Generic.List[object]
$current = $null

function Flush-Current {
    param($cur, $list)
    if ($null -eq $cur) { return }
    $list.Add([pscustomobject]@{
            name      = $cur.name
            crate     = $cur.crate
            path      = $cur.path
            role      = $cur.role
            platforms = @($cur.platforms)
            packages  = @($cur.packages)
            primary   = [bool]$cur.primary
        }) | Out-Null
}

foreach ($line in ($raw -split "`r?`n")) {
    $t = $line.Trim()
    if ($t -eq "" -or $t.StartsWith("#")) { continue }
    if ($t -eq "[[binary]]") {
        Flush-Current $current $binaries
        $current = @{
            name      = ""
            crate     = ""
            path      = ""
            role      = ""
            platforms = @()
            packages  = @()
            primary   = $false
        }
        continue
    }
    if ($null -eq $current) { continue }
    if ($t -match '^\s*name\s*=\s*"(.*)"\s*$') { $current.name = $Matches[1]; continue }
    if ($t -match '^\s*crate\s*=\s*"(.*)"\s*$') { $current.crate = $Matches[1]; continue }
    if ($t -match '^\s*path\s*=\s*"(.*)"\s*$') { $current.path = $Matches[1]; continue }
    if ($t -match '^\s*role\s*=\s*"(.*)"\s*$') { $current.role = $Matches[1]; continue }
    if ($t -match '^\s*primary\s*=\s*(true|false)\s*$') {
        $current.primary = ($Matches[1] -eq "true")
        continue
    }
    if ($t -match '^\s*platforms\s*=\s*\[(.*)\]\s*$') {
        $current.platforms = @([regex]::Matches($Matches[1], '"([^"]+)"') | ForEach-Object { $_.Groups[1].Value })
        continue
    }
    if ($t -match '^\s*packages\s*=\s*\[(.*)\]\s*$') {
        $current.packages = @([regex]::Matches($Matches[1], '"([^"]+)"') | ForEach-Object { $_.Groups[1].Value })
        continue
    }
}
Flush-Current $current $binaries

if ($binaries.Count -eq 0) {
    Write-Error "No [[binary]] entries parsed from $inventoryPath"
}

if ($Json) {
    $binaries | ConvertTo-Json -Depth 4
    exit 0
}

Write-Host "RemoteLink package binaries ($($binaries.Count)) - $inventoryPath"
Write-Host ""
$binaries | Format-Table -AutoSize name, crate, path, @{
    Label = "packages"; Expression = { $_.packages -join "," }
}, @{
    Label = "platforms"; Expression = { $_.platforms -join "," }
}, primary | Out-String | Write-Host

Write-Host "Primary ship targets:"
foreach ($b in ($binaries | Where-Object { $_.primary })) {
    $pkg = $b.packages -join ","
    Write-Host ('  - {0} ({1})' -f $b.name, $pkg)
}
