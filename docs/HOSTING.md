# Host RemoteLink so two PCs can connect over the internet

This is the missing AnyDesk-shaped piece: a **public signaling server** plus **STUN/TURN** so WebRTC can leave the LAN.

A 1 CPU / 4 GB VPS is enough for tens of concurrent sessions (signaling is light; TURN uses bandwidth only when peers cannot talk directly).

## What to run on the VPS

From the repo root, with Docker:

```bash
# Set a public hostname or IP first
export RL_PUBLIC=your.server.example
export TURN_SHARED_SECRET=$(openssl rand -hex 24)

docker compose -f deploy/docker-compose.yml --profile turn up -d --build
```

On Linux, `coturn` uses host networking (`--profile turn`). On Windows/Docker Desktop use the default `coturn-bridge` service (UDP 3478).

Compose already starts:

| Service | Port | Role |
|---------|------|------|
| `server` | 8080 | Signaling + device registry (Postgres) |
| `postgres` | 5432 | Durable IDs |
| `coturn` / `coturn-bridge` | 3478 | STUN/TURN |

Point the app **Advanced → Server** at `http://YOUR_VPS:8080` (or `https://` behind a reverse proxy).

Set on the **server** so `hello_ok` tells clients how to ICE:

```env
STUN_URLS=stun:YOUR_VPS:3478
TURN_URLS=turn:YOUR_VPS:3478
TURN_SHARED_SECRET=same-as-coturn
```

Leave **STUN / TURN** empty in the app when the server advertises ICE — the host and viewer pick it up from `hello_ok`.

## Single-binary server (no Docker)

```powershell
$env:LISTEN_ADDR = "0.0.0.0:18080"
$env:REGISTRY_PATH = "C:\data\remotelink\registry.json"
$env:STUN_URLS = "stun:YOUR_VPS:3478"
.\remotelink-server.exe
```

Without `DATABASE_URL`, IDs persist in `REGISTRY_PATH` (or `data/registry.json`). Restarting the server no longer forgets devices.

With `DATABASE_URL=postgres://…` the Postgres repo is used (recommended for the VPS).

## TLS

Put Caddy or nginx in front of `:8080` and terminate HTTPS/WSS. Set `TRUST_PROXY=1` only if that proxy overwrites `X-Forwarded-For`.

## What this still is not

- **Authenticode** (Windows SmartScreen) needs a paid code-signing certificate.
- **Real H.264 decode** on the viewer is still the mock reconstruction path on windows-gnu.
- **UAC / login screen** remote control is out of v1.
