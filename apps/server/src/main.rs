//! RemoteLink registry/signaling server binary.

use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use remotelink_server::{router, AppState, ClientIpConfig, MemoryDeviceRepo, PostgresDeviceRepo};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let listen = env::var("LISTEN_ADDR").unwrap_or_else(|_| "0.0.0.0:18080".into());
    let addr: SocketAddr = listen.parse()?;
    let client_ip = ClientIpConfig::from_env();

    let ice = remotelink_server::IceConfig::from_env();
    if ice.has_servers() {
        tracing::info!(
            stun = ice.stun_urls.len(),
            turn = ice.turn_urls.len(),
            "advertising ICE servers on hello_ok"
        );
    }

    let state = match env::var("DATABASE_URL") {
        Ok(url) if !url.is_empty() => {
            tracing::info!("using Postgres repository");
            let repo = PostgresDeviceRepo::connect(&url).await?;
            AppState::new(Arc::new(repo))
                .with_client_ip(client_ip)
                .with_admin_token_from_env()
                .with_ice(ice)
        }
        _ => {
            let path = registry_path();
            tracing::info!(path = %path.display(), "using durable JSON registry (set DATABASE_URL for Postgres)");
            let repo = MemoryDeviceRepo::open_or_create(&path)?;
            AppState::new(Arc::new(repo))
                .with_client_ip(client_ip)
                .with_admin_token_from_env()
                .with_ice(ice)
        }
    };

    let admin_enabled = state.admin_token.is_some();
    let app = router(state);
    tracing::info!(
        %addr,
        version = remotelink_server::VERSION,
        trust_proxy = client_ip.trust_proxy,
        admin_enabled,
        "remotelink-server listening (set TRUST_PROXY=1 only behind a reverse proxy that overwrites X-Forwarded-For; set ADMIN_TOKEN for force-disconnect)"
    );

    let listener = tokio::net::TcpListener::bind(addr).await?;
    // ConnectInfo so handlers key rate limits / lockout on the real peer IP.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}

fn registry_path() -> PathBuf {
    if let Ok(p) = env::var("REGISTRY_PATH") {
        if !p.trim().is_empty() {
            return PathBuf::from(p);
        }
    }
    if let Ok(d) = env::var("REMOTELINK_DATA_DIR") {
        if !d.trim().is_empty() {
            return PathBuf::from(d).join("registry.json");
        }
    }
    PathBuf::from("data").join("registry.json")
}
