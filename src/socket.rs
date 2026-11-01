//! One client's WebSocket: requests read in order and handed to the hub, replies written by a
//! task of their own so a slow client never holds the hub's lock.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt as _, StreamExt as _};
use tokio::sync::mpsc;

use crate::hub::{self, Hub, Place};
use crate::protocol::{Reason, Reply, Request, VERSION};

/// How many replies wait for a client before newer ones are dropped.
const BACKLOG: usize = 64;
/// How long a client may stay silent before the server hangs up. Sonora pings well within it.
const IDLE: Duration = Duration::from_secs(60);
/// The longest display name kept, in characters.
const NAME: usize = 64;
/// The longest provider slug accepted, in bytes.
const SLUG: usize = 32;
/// The longest picture address accepted, in bytes.
const IMAGE: usize = 2048;

/// Serves one client until it hangs up or goes quiet, then takes it out of its session.
pub async fn serve(socket: WebSocket, hub: Arc<Hub>, from: IpAddr) {
    let (mut sink, mut stream) = socket.split();
    let (outbox, mut replies) = mpsc::channel::<Arc<str>>(BACKLOG);
    let writer = tokio::spawn(async move {
        while let Some(text) = replies.recv().await {
            if sink
                .send(Message::Text(text.as_ref().into()))
                .await
                .is_err()
            {
                break;
            }
        }
        sink.close().await.ok();
    });

    let mut place: Option<Place> = None;
    while let Ok(Some(Ok(message))) = tokio::time::timeout(IDLE, stream.next()).await {
        let text = match message {
            Message::Text(text) => text,
            Message::Close(_) => break,
            Message::Binary(_) | Message::Ping(_) | Message::Pong(_) => continue,
        };
        let request = match serde_json::from_str::<Request>(&text) {
            Ok(request) => request,
            Err(error) => {
                tracing::debug!("unreadable request: {error}");
                refuse(&outbox, Reason::Invalid, None);
                continue;
            }
        };
        match request {
            Request::Create {
                version,
                name,
                image,
                provider,
            } => {
                if version != VERSION {
                    refuse(&outbox, Reason::Version, None);
                    continue;
                }
                if let Some(old) = place.take() {
                    hub.leave(&old, true);
                }
                let Some(provider) = slug(provider) else {
                    refuse(&outbox, Reason::Invalid, None);
                    continue;
                };
                match hub.create(
                    display(name),
                    picture(image),
                    provider,
                    from,
                    outbox.clone(),
                ) {
                    Ok(joined) => place = Some(joined),
                    Err(reason) => refuse(&outbox, reason, None),
                }
            }
            Request::Join {
                version,
                code,
                name,
                image,
                providers,
            } => {
                if version != VERSION {
                    refuse(&outbox, Reason::Version, None);
                    continue;
                }
                if let Some(old) = place.take() {
                    hub.leave(&old, true);
                }
                let code: String = code.chars().filter(char::is_ascii_digit).collect();
                match hub.join(
                    &code,
                    display(name),
                    picture(image),
                    &providers,
                    from,
                    outbox.clone(),
                ) {
                    Ok(joined) => place = Some(joined),
                    Err((reason, provider)) => refuse(&outbox, reason, provider),
                }
            }
            Request::State { state, seq } => match &place {
                Some(place) => hub.share(place, state, seq),
                None => refuse(&outbox, Reason::Invalid, None),
            },
            Request::Leave => {
                if let Some(old) = place.take() {
                    hub.leave(&old, true);
                }
            }
            Request::Kick { member } => match &place {
                Some(place) => hub.kick(place, &member),
                None => refuse(&outbox, Reason::Invalid, None),
            },
            Request::Ping { t } => hub::send(&outbox, &Reply::Pong { t, now: hub::now() }),
        }
    }

    if let Some(place) = place.take() {
        hub.leave(&place, false);
    }
    drop(outbox);
    writer.abort();
}

fn refuse(outbox: &hub::Outbox, reason: Reason, provider: Option<String>) {
    hub::send(outbox, &Reply::Error { reason, provider });
}

/// A display name as shown to the others: trimmed, cut short, and never empty.
fn display(name: String) -> String {
    let name: String = name.trim().chars().take(NAME).collect();
    match name.is_empty() {
        true => "Listener".to_owned(),
        false => name,
    }
}

/// A provider slug, when it reads as one.
fn slug(provider: String) -> Option<String> {
    let valid = !provider.is_empty()
        && provider.len() <= SLUG
        && provider
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
    valid.then_some(provider)
}

/// A picture address, when it is a short https one. Anything else is dropped, since every member
/// loads it.
fn picture(image: Option<String>) -> Option<String> {
    image.filter(|image| image.starts_with("https://") && image.len() <= IMAGE)
}
