//! The messages Sonora and the server trade over the WebSocket, one JSON object per text frame.
//! Every object carries its kind in `type`. Sonora keeps its own copy of these types, so a field
//! added here has to be optional or defaulted until every client knows it.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The protocol a client speaks. A client sending another one is turned away with
/// `Reason::Version`.
pub const VERSION: u32 = 1;

/// What a client asks of the server.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    /// Opens a session for the provider `provider` and makes the sender its host.
    Create {
        version: u32,
        name: String,
        /// The address of the sender's picture, when the provider gave one.
        #[serde(default)]
        image: Option<String>,
        provider: String,
    },
    /// Joins the session `code` names. `providers` are the slugs the client has an account for,
    /// and one of them has to be the session's.
    Join {
        version: u32,
        code: String,
        name: String,
        #[serde(default)]
        image: Option<String>,
        providers: Vec<String>,
    },
    /// What plays on the sender now. `seq` counts the sender's own states, and every member,
    /// the sender included, hears the state back in the order the server took it.
    State {
        state: Playback,
        #[serde(default)]
        seq: u64,
    },
    /// Leaves the session. The host leaving ends it for everyone.
    Leave,
    /// Removes the member `member` from the session. Only the host may.
    Kick { member: String },
    /// Asks for the server's clock, so the client can tell how far behind a state it hears is.
    Ping { t: u64 },
}

/// What the server tells a client.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Reply {
    /// The sender is in the session now. `state` is what plays there, if anything does.
    Joined {
        code: String,
        provider: String,
        you: String,
        members: Vec<Member>,
        state: Option<Playback>,
        now: u64,
    },
    /// Who is in the session after someone came, went or took over as host.
    Members { members: Vec<Member> },
    /// What plays in the session now, as the member `by` set it in their state `seq`. The
    /// sender hears its own states back too, which tells it everything before them is older.
    State {
        state: Playback,
        by: String,
        seq: u64,
    },
    /// The answer to `Request::Ping`, with the client's own clock reading echoed back.
    Pong { t: u64, now: u64 },
    /// The host ended the session.
    Ended,
    /// The host removed the receiver from the session.
    Kicked,
    /// What the server could not do.
    Error {
        reason: Reason,
        #[serde(skip_serializing_if = "Option::is_none")]
        provider: Option<String>,
    },
}

/// Why a request failed.
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    /// No live session has that code.
    NotFound,
    /// The session plays from a provider the client has no account for.
    Provider,
    /// The session has as many members as it takes.
    Full,
    /// Too many codes were tried from one address.
    RateLimited,
    /// The server holds as many sessions as it takes.
    Busy,
    /// The sender's address already started as many live sessions as one may.
    TooMany,
    /// The client speaks another protocol version.
    Version,
    /// The request made no sense in this state, as a state sent outside a session.
    Invalid,
}

/// A person in a session.
#[derive(Clone, Debug, Serialize)]
pub struct Member {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    pub host: bool,
}

/// Playback as one member shares it. Tracks are the provider's own models, which the server
/// passes on without reading beyond their ids.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Playback {
    /// The track playing, or `None` while the member plays nothing that can be shared.
    pub track: Option<Value>,
    /// What plays after it, in order.
    #[serde(default)]
    pub upcoming: Vec<Value>,
    pub playing: bool,
    /// Where the track was, in milliseconds, at `at`.
    pub position: u64,
    /// The server's clock in milliseconds when the state reached it. Whatever the client sends
    /// is overwritten.
    #[serde(default)]
    pub at: u64,
}
