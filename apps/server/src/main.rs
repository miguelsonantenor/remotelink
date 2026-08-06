//! RemoteLink registry/signaling server binary.

use std::env;
use std::net::SocketAddr;
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

    let listen = env::var("LISTEN_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".into());
    let addr: SocketAddr = listen.parse()?;
    let client_ip = ClientIpConfig::from_env();

    let state = match env::var("DATABASE_URL") {
        Ok(url) if !url.is_empty() => {
            tracing::info!("using Postgres repository");
            let repo = PostgresDeviceRepo::connect(&url).await?;
            AppState::new(Arc::new(repo)).with_client_ip(client_ip)
        }
        _ => {
            tracing::warn!("DATABASE_URL unset; using in-memory repository (not durable)");
            AppState::new(Arc::new(MemoryDeviceRepo::new())).with_client_ip(client_ip)
        }
    };

    let app = router(state);
    tracing::info!(
        %addr,
        version = remotelink_server::VERSION,
        trust_proxy = client_ip.trust_proxy,
        "remotelink-server listening (set TRUST_PROXY=1 only behind a reverse proxy that overwrites X-Forwarded-For)"
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
