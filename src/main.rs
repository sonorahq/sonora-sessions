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
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse as _, Response};
use axum::routing::get;
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;

use crate::hub::{Hub, Turned};

/// The port the server listens on when `PORT` names none.
const PORT: u16 = 8080;
/// The largest frame a client may send. A state with a full queue of tracks stays well under it.
const MAX_MESSAGE: usize = 512 << 10;
/// How much a connection reads from its socket at a time. Messages are small, and the default of
/// 128 KiB held for every open connection was most of the memory the server used.
const READ_BUFFER: usize = 4 << 10;
/// How far writes to one slow connection may pile up before they fail. It has room for the
/// largest message.
const MAX_WRITE_BUFFER: usize = 1 << 20;
/// How many connections the server holds at once when `MAX_CONNECTIONS` names no other number.
/// An idle connection takes about 9 KiB, so this stays far inside a small container.
const MAX_CONNECTIONS: usize = 20_000;
/// Files the process keeps for itself beyond its connections: the listener, logs and the like.
const SPARE_FILES: u64 = 64;
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

    // every connection holds a file, and the soft limit a container starts with is far below
    // what the hard limit allows
    let files = match rlimit::increase_nofile_limit(u64::MAX) {
        Ok(limit) => limit,
        Err(error) => {
            tracing::warn!("cannot raise the open files limit: {error}");
            1024
        }
    };
    let wanted = match std::env::var("MAX_CONNECTIONS") {
        Ok(max) => max.parse().context("cannot read MAX_CONNECTIONS")?,
        Err(_) => MAX_CONNECTIONS,
    };
    // a connection past the open files limit would fail anyway, so the server never takes it
    let capacity = wanted.min(files.saturating_sub(SPARE_FILES) as usize);
    tracing::info!("taking up to {capacity} connections, with {files} open files allowed");

    let hub = Arc::new(Hub::new(capacity));
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
    // one address holding too many sockets is turned away before it gets another
    let admission = match hub.admit(from) {
        Ok(admission) => admission,
        Err(Turned::Full) => {
            return (StatusCode::SERVICE_UNAVAILABLE, "the server is full").into_response();
        }
        Err(Turned::Address) => {
            return (StatusCode::TOO_MANY_REQUESTS, "too many connections").into_response();
        }
    };
    upgrade
        .max_message_size(MAX_MESSAGE)
        .read_buffer_size(READ_BUFFER)
        // every reply is flushed on its own, so nothing waits in a buffer to be batched
        .write_buffer_size(0)
        .max_write_buffer_size(MAX_WRITE_BUFFER)
        .on_upgrade(move |socket| async move {
            socket::serve(socket, hub, from).await;
            drop(admission);
        })
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
