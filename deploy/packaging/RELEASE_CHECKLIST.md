# RemoteLink release checklist

## Core product (done without codesign)

- [x] Host + viewer + server release binaries
- [x] Portable zip via `scripts/package-release.ps1`
- [x] `install-portable.ps1` / `uninstall-portable.ps1` / `lab-start.ps1`
- [x] `package-manifest.json` with SHA-256
- [x] CI Linux + Windows tests
- [x] CI package artifact on `main`

## Optional: MSI

1. Install [WiX Toolset v3](https://wixtoolset.org/) (admin):  
   `winget install WiXToolset.WiXToolset`
2. From repo root:  
   `.\scripts\package-release.ps1`  
   `.\scripts\build-msi.ps1 -SkipStage`
3. Output: `dist\RemoteLink-<ver>.msi` (unsigned)

## Optional: Authenticode (private release pipeline)

```text
signtool sign /tr http://timestamp.digicert.com /td sha256 /fd sha256 ^
  dist\remotelink-*\bin\*.exe dist\RemoteLink-*.msi
```

Requires a code-signing certificate (not stored in this repo).

## Smoke after package

1. Expand portable zip on a clean user profile  
2. `install-portable.ps1`  
3. `lab-start.ps1` → connect viewer with OTP  
4. Tray: Copy OTP / End session  
5. Optional: split agent/service with `--boot-secret` and named pipe  

## Version bump

1. Bump `version` in root `Cargo.toml` `[workspace.package]`  
2. `.\scripts\package-release.ps1`  
3. Tag + GitHub release attaching `RemoteLink-*-portable.zip`  
