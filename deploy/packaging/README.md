# RemoteLink packaging (MSI / MSIX)

Beta packaging outline for Windows host and viewer. **No codesign is required in CI**; production signing is an offline / release-pipeline step.

## Binaries to package

Inventory lives in [`binaries.toml`](binaries.toml). List them from a checkout:

```powershell
# From repository root
.\scripts\list-package-binaries.ps1
# JSON:
.\scripts\list-package-binaries.ps1 -Json
```

| Binary | Crate | Role | Windows packages |
|--------|-------|------|------------------|
| `remotelink-app` | `apps/desktop` | Product shell (This PC + Connect) | MSI, MSIX |
| `remotelink-host` | `apps/host` | Service + session agent | MSI, MSIX |
| `remotelink-viewer` | `apps/viewer` | Viewer client (CLI) | MSI, MSIX |
| `remotelink-server` | `apps/server` | Signaling / registry | Optional MSI or container only |

Server is primarily distributed via container (`deploy/docker-compose.yml`); an MSI is optional for air-gapped operators.

## Build release (core product)

```powershell
$env:Path = "C:\msys64\mingw64\bin;$env:USERPROFILE\.cargo\bin;" + $env:Path
$env:RUSTUP_TOOLCHAIN = "stable-x86_64-pc-windows-gnu"

# Release build + portable layout + zip (primary shippable artifact)
.\scripts\package-release.ps1

# Optional MSI if WiX v3 is installed
.\scripts\build-msi.ps1 -SkipStage
```

Outputs:

```text
dist\remotelink-<version>\
  bin\*.exe
  QUICKSTART.md
  install-portable.ps1
  uninstall-portable.ps1
  lab-start.ps1
  package-manifest.json   # version, SHA-256, core_product=true, unsigned=true

dist\RemoteLink-<version>-portable.zip   # ship this without WiX
dist\RemoteLink-<version>.msi            # only if WiX present
```

The package is **unsigned**. Authenticode is release-pipeline only.

### Portable install (no WiX)

```powershell
Expand-Archive dist\RemoteLink-0.1.0-portable.zip -DestinationPath $env:TEMP\rl
powershell -ExecutionPolicy Bypass -File $env:TEMP\rl\install-portable.ps1
```

### MSI (WiX v3)

[`Product.wxs`](Product.wxs) + [`scripts/build-msi.ps1`](../../scripts/build-msi.ps1):

```powershell
.\scripts\package-release.ps1
.\scripts\build-msi.ps1 -SkipStage
# → dist\RemoteLink-<ver>.msi (unsigned)

# Release pipeline only:
# signtool sign /tr http://timestamp.digicert.com /td sha256 /fd sha256 dist\*.msi dist\remotelink-*\bin\*.exe
```

## MSIX outline

Target: Store-adjacent sideload / enterprise deploy for viewer (and later host if service model allows).

1. `AppxManifest.xml` with `Identity Name="RemoteLink.Viewer"` (and Host package when ready).
2. Capabilities: `runFullTrust` for host service; internetClient for both.
3. Package with `MakeAppx pack` from a layout directory of release EXEs + assets.
4. Sign with a code-signing cert or free test cert for local sideload (`SignTool`).
5. CI may produce an **unsigned** `.msix` layout tarball for size/layout checks only.

## Update channel pin

Clients poll a **signed update manifest** (not over the remote session media path):

| Channel | Purpose |
|---------|---------|
| `beta` | Open beta; may force-update more aggressively |
| `stable` | GA candidates |

Schema and ed25519 verify helpers live in `remotelink-common` (`update_manifest` module). Host/viewer pin a channel at install time (MSI property / MSIX resource) and refuse payloads whose signature does not verify against the embedded release public key.

```text
GET /v1/updates/manifest?channel=beta
→ JSON SignedUpdateManifest (body + signature_b64)
```

Manifest signing keys are **release keys**, separate from device enrollment keys. Offline sign step in release:

```text
# Sign tool (future): cargo run -p remotelink-host -- --sign-manifest path.json
# Verify: unit tests in packages/common cover sign/verify round-trip
```

## Force-disconnect (ops)

Admin endpoint (server):

```http
POST /v1/admin/sessions/{session_id}/force-disconnect
Authorization: Bearer $ADMIN_TOKEN
```

Broadcasts `session_end` with `reason=security` to both peers. Set `ADMIN_TOKEN` in the server environment (required for the route; empty/unset rejects all admin calls).

Local host CLI (no server) — **mock demo only**: attaches an in-process
`AgentSession::new_mock`, fires a policy kill-switch, and exits. It does **not**
talk to a live service pipe or the signaling server. Use the admin HTTP endpoint
for real operator force-disconnect.

```powershell
cargo run -p remotelink-host -- --force-disconnect-local
cargo run -p remotelink-host -- --force-disconnect-local=sess-demo
```

Clients that poll manifests must call `verify_manifest_for_channel` with the
install-time pin (`beta` / `stable`). `verify_manifest` alone is crypto-only.

## CI policy

| Step | CI | Release pipeline |
|------|----|------------------|
| `cargo build --release` (Windows) | optional / nightly | yes |
| List package binaries script | yes (`ci.yml` → packaging inventory smoke via `pwsh`) | yes |
| Produce unsigned MSI/MSIX layout | optional / local | yes |
| Authenticode / MSIX signing | **no** | yes |
| Manifest ed25519 unit tests | yes (`cargo test --workspace`) | yes |

## Related crates

- `remotelink-common::update_manifest` — schema, sign, `verify_manifest_for_channel`
- `remotelink-server` — admin force-disconnect route (`ADMIN_TOKEN`, rate limit + audit)
- `remotelink-host` — `--force-disconnect-local` mock demo
