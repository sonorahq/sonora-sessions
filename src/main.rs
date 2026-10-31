//! The Sonora Session server. Sonora clients open a session here, get a six-digit code to share,
//! and every member's playback follows whatever anyone in the session plays.

mod hub;
mod protocol;
mod socket;

use std::io::{Read as _, Write as _};
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use axum::Router;
use axum::extract::{ConnectInfo, State, WebSocketUpgrade};
use axum::http::HeaderMap;
use axum::response::Response;
use axum::routing::get;
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;

use crate::hub::Hub;

/// The port the server listens on when `PORT` names none.
const PORT: u16 = 8080;
/// The largest frame a client may send. A state with a full queue of tracks stays well under it.
const MAX_MESSAGE: usize = 1 << 20;
/// How often sessions nobody came back to are closed.
const SWEEP: Duration = Duration::from_secs(30);

#[tokio::main]
async fn main() -> Result<()> {
    let port = match std::env::var("PORT") {
        Ok(port) => port.parse().context("cannot read PORT")?,
        Err(_) => PORT,
    };
    if std::env::args().nth(1).as_deref() == Some("health") {
        return health(port);
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| "sonora_sessions=info".into()),
        )
        .init();

    let hub = Arc::new(Hub::default());
    let sweeper = hub.clone();
    tokio::spawn(async move {
        let mut every = tokio::time::interval(SWEEP);
        loop {
            every.tick().await;
            sweeper.sweep();
        }
    });

    let app = Router::new()
        .route("/", get(|| async { "sonora-sessions" }))
        .route("/health", get(|| async { "ok" }))
        .route("/ws", get(upgrade))
        .with_state(hub);
    let listener = TcpListener::bind(("0.0.0.0", port))
        .await
        .with_context(|| format!("cannot listen on port {port}"))?;
    tracing::info!("listening on port {port}");
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown())
    .await
    .context("cannot serve")
}

async fn upgrade(
    upgrade: WebSocketUpgrade,
    State(hub): State<Arc<Hub>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    let from = client_ip(&headers).unwrap_or(peer.ip());
    upgrade
        .max_message_size(MAX_MESSAGE)
        .on_upgrade(move |socket| socket::serve(socket, hub, from))
}

/// The address a request came from when a proxy stands in front: Cloudflare's header first, then
/// the address the last proxy saw.
fn client_ip(headers: &HeaderMap) -> Option<IpAddr> {
    let header = |name: &str| headers.get(name).and_then(|value| value.to_str().ok());
    header("cf-connecting-ip")
        .and_then(|ip| ip.trim().parse().ok())
        .or_else(|| {
            header("x-forwarded-for")?
                .rsplit(',')
                .next()?
                .trim()
                .parse()
                .ok()
        })
}

/// Resolves on Ctrl+C or SIGTERM, which is how a container is stopped.
async fn shutdown() {
    let interrupt = async {
        tokio::signal::ctrl_c().await.ok();
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = interrupt => {}
        () = terminate => {}
    }
    tracing::info!("shutting down");
}

/// Asks the server on this machine whether it answers, for the container's health check, which
/// has no curl to do it with.
fn health(port: u16) -> Result<()> {
    let mut stream = TcpStream::connect_timeout(
        &SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_secs(2),
    )
    .context("cannot reach the server")?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.write_all(b"GET /health HTTP/1.0\r\nHost: localhost\r\n\r\n")?;
    let mut answer = String::new();
    stream.read_to_string(&mut answer)?;
    anyhow::ensure!(
        answer.starts_with("HTTP/1.0 200") || answer.starts_with("HTTP/1.1 200"),
        "unhealthy"
    );
    Ok(())
}
