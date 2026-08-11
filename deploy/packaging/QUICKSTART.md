# RemoteLink — Quick start (portable package)

This folder is a **complete core product** layout: host, viewer, and optional server.

## Contents

| Path | Role |
|------|------|
| `bin/remotelink-app.exe` | **Product shell** — This PC (ID + OTP) + Connect |
| `bin/remotelink-host.exe` | Host service + session agent (CLI / advanced) |
| `bin/remotelink-viewer.exe` | Viewer client (CLI / advanced) |
| `bin/remotelink-server.exe` | Signaling server (lab / self-host) |
| `package-manifest.json` | Version + SHA-256 of binaries |
| `install-portable.ps1` | Copy to `%LOCALAPPDATA%\RemoteLink` + Start Menu |
| `uninstall-portable.ps1` | Remove portable install |
| `lab-start.ps1` | One-machine demo (server + host) |

## Product shell (recommended)

```powershell
# Terminal 1 — signaling (in-memory if DATABASE_URL unset)
.\bin\remotelink-server.exe

# Terminal 2 — single app (Allow remote access shows Your ID + OTP)
.\bin\remotelink-app.exe
# Advanced → Server defaults to http://127.0.0.1:8080
# Connect to another PC: enter their ID + OTP on the same home screen
```

## One-machine lab (CLI binaries)

```powershell
# Terminal 1 — signaling (in-memory if DATABASE_URL unset)
.\bin\remotelink-server.exe

# Terminal 2 — host (prints public_id + OTP; tray icon on Windows)
.\bin\remotelink-host.exe --role=service --server=http://127.0.0.1:8080 --transport=live

# Terminal 3 — viewer (use OTP and public_id from host)
.\bin\remotelink-viewer.exe --ws-connect --server=http://127.0.0.1:8080 --host PUBLIC_ID --otp CODE --transport=live
```

Or from this folder:

```powershell
.\lab-start.ps1
# follow printed viewer command
```

## Split host (KD5: service + agent)

```powershell
# Agent (media plane)
.\bin\remotelink-host.exe --role=agent --control-listen=pipe --boot-secret=SECRET --transport=live

# Service (WSS + enrollment)
.\bin\remotelink-host.exe --role=service --server=http://127.0.0.1:8080 `
  --transport=live --agent-control=pipe --boot-secret=SECRET
```

## Portable install

```powershell
# From this package directory (may prompt for execution policy)
powershell -ExecutionPolicy Bypass -File .\install-portable.ps1
```

Installs under `%LOCALAPPDATA%\RemoteLink` and adds Start Menu shortcuts.

## Signing

This package is **unsigned** by default (`unsigned: true` in the manifest).  
Production releases should Authenticode-sign EXEs (and MSI if built) in a private release pipeline.

## Known v1 limits

- Secure desktop / UAC is not capturable or injectable remotely  
- Media Foundation H.264 is preferred; falls back to software mock if MF is missing  
- WASAPI PreferNative falls back to stub if no render endpoint  
