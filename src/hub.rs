//! Every live session, keyed by its code. The hub is the only place a session changes, and it
//! holds one lock that is never kept across an await, so a request is applied and announced to
//! the members as one step.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rand::Rng as _;
use serde_json::Value;
use tokio::sync::mpsc;

use crate::protocol::{Member, Playback, Reason, Reply};

/// Where a connection's replies go, already serialized.
pub type Outbox = mpsc::Sender<Arc<str>>;

/// How many people one session takes.
const MAX_MEMBERS: usize = 16;
/// How many sessions the server holds at once.
const MAX_SESSIONS: usize = 10_000;
/// How many upcoming tracks a shared state keeps. The rest are dropped.
const MAX_UPCOMING: usize = 100;
/// How long a session outlives its last member, so a client whose connection dropped can come
/// back to it with the same code.
const LINGER: Duration = Duration::from_secs(120);
/// How many codes one address may get wrong within `MISS_WINDOW` before it is turned away.
const MISSES: u32 = 20;
const MISS_WINDOW: Duration = Duration::from_secs(600);
/// How many live sessions one address may have started. An address is a whole IPv6 `/64`, which
/// one household or one attacker holds all of.
const HOSTED: usize = 3;
/// How many connections one address may hold open at once.
const CONNECTIONS: usize = 32;
/// What Sonora puts before the id of a file on the listener's own disk. Such a track means
/// nothing to anyone else, so it is never shared.
const LOCAL_PREFIX: &str = "local:";

/// The sessions, behind one lock.
pub struct Hub {
    inner: Mutex<Inner>,
    /// How many connections the whole server holds open at once. It is what bounds the memory
    /// the server takes, since every connection keeps buffers and a queue of its own.
    capacity: usize,
}

#[derive(Default)]
struct Inner {
    sessions: HashMap<String, Session>,
    /// The number the next member id is made from.
    next: u64,
    /// Wrong codes per address, for rate limiting.
    misses: HashMap<IpAddr, Misses>,
    /// Open connections per address.
    connections: HashMap<IpAddr, usize>,
    /// Open connections in all.
    open: usize,
}

struct Misses {
    since: Instant,
    count: u32,
}

struct Session {
    provider: String,
    /// The members in the order they came. The host is always among them unless it is empty.
    members: Vec<Seat>,
    state: Option<Playback>,
    /// When the last member left, while nobody is in the session.
    emptied: Option<Instant>,
    /// The address that started the session, which counts against its `HOSTED`.
    origin: IpAddr,
}

struct Seat {
    id: String,
    name: String,
    image: Option<String>,
    host: bool,
    outbox: Outbox,
}

/// The session and member a connection stands for, which it hands back on every later request.
#[derive(Clone, Debug)]
pub struct Place {
    pub code: String,
    pub member: String,
}

/// Why a connection was turned away before it opened.
pub enum Turned {
    /// The server holds as many connections as it takes.
    Full,
    /// The address holds as many as one address may.
    Address,
}

/// Why a join failed, with the session's provider when the client lacks it.
pub type Refusal = (Reason, Option<String>);

impl Hub {
    /// A hub that holds at most `capacity` connections open at once.
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Mutex::default(),
            capacity,
        }
    }

    /// Opens a session with the sender as its host and tells it the code.
    pub fn create(
        &self,
        name: String,
        image: Option<String>,
        provider: String,
        from: IpAddr,
        outbox: Outbox,
    ) -> Result<Place, Reason> {
        let origin = address(from);
        let mut inner = self.lock();
        if inner.sessions.len() >= MAX_SESSIONS {
            return Err(Reason::Busy);
        }
        let hosted = inner
            .sessions
            .values()
            .filter(|session| session.origin == origin)
            .count();
        if hosted >= HOSTED {
            return Err(Reason::TooMany);
        }
        let code = loop {
            let code = format!("{:06}", rand::rng().random_range(0..1_000_000));
            if !inner.sessions.contains_key(&code) {
                break code;
            }
        };
        let member = inner.member_id();
        let session = Session {
            provider,
            members: vec![Seat {
                id: member.clone(),
                name,
                image,
                host: true,
                outbox,
            }],
            state: None,
            emptied: None,
            origin,
        };
        session.welcome(&code, &member);
        tracing::info!(provider = %session.provider, "session {code} created");
        inner.sessions.insert(code.clone(), session);
        Ok(Place { code, member })
    }

    /// Adds the sender to the session `code` names, once it has an account for its provider.
    /// The first to come back to a session that emptied becomes its host.
    pub fn join(
        &self,
        code: &str,
        name: String,
        image: Option<String>,
        providers: &[String],
        from: IpAddr,
        outbox: Outbox,
    ) -> Result<Place, Refusal> {
        let from = address(from);
        let mut inner = self.lock();
        if inner
            .misses
            .get(&from)
            .is_some_and(|misses| misses.since.elapsed() < MISS_WINDOW && misses.count >= MISSES)
        {
            return Err((Reason::RateLimited, None));
        }
        if !inner.sessions.contains_key(code) {
            inner.miss(from);
            return Err((Reason::NotFound, None));
        }
        let member = inner.member_id();
        let session = inner.sessions.get_mut(code).expect("checked above");
        if !providers.contains(&session.provider) {
            return Err((Reason::Provider, Some(session.provider.clone())));
        }
        if session.members.len() >= MAX_MEMBERS {
            return Err((Reason::Full, None));
        }
        session.members.push(Seat {
            id: member.clone(),
            name,
            image,
            host: session.members.is_empty(),
            outbox,
        });
        session.emptied = None;
        session.welcome(code, &member);
        session.announce_members(Some(&member));
        tracing::info!(members = session.members.len(), "session {code} joined");
        Ok(Place {
            code: code.to_owned(),
            member,
        })
    }

    /// Takes what plays on the member at `place` as the session's and tells every member, the
    /// sender included, so all of them see the states in one order. Local files are left out, and
    /// a local track playing stands for nothing shared.
    pub fn share(&self, place: &Place, mut state: Playback, seq: u64) {
        let mut inner = self.lock();
        let Some(session) = inner.sessions.get_mut(&place.code) else {
            return;
        };
        if !session.members.iter().any(|seat| seat.id == place.member) {
            return;
        }
        if state.track.as_ref().is_some_and(local) {
            state.track = None;
        }
        state.upcoming.retain(|track| !local(track));
        state.upcoming.truncate(MAX_UPCOMING);
        state.at = now();
        session.state = Some(state.clone());
        session.broadcast(
            &Reply::State {
                state,
                by: place.member.clone(),
                seq,
            },
            None,
        );
    }

    /// Takes the member at `place` out of its session. A host leaving on purpose ends the
    /// session for everyone, while a host whose connection dropped hands over to whoever came
    /// next.
    pub fn leave(&self, place: &Place, deliberate: bool) {
        let mut inner = self.lock();
        let Some(session) = inner.sessions.get_mut(&place.code) else {
            return;
        };
        let Some(at) = session
            .members
            .iter()
            .position(|seat| seat.id == place.member)
        else {
            return;
        };
        let seat = session.members.remove(at);
        if seat.host && deliberate {
            session.broadcast(&Reply::Ended, None);
            inner.sessions.remove(&place.code);
            tracing::info!("session {} ended by its host", place.code);
            return;
        }
        if session.members.is_empty() {
            session.emptied = Some(Instant::now());
            tracing::info!("session {} emptied", place.code);
            return;
        }
        if seat.host {
            session.members[0].host = true;
        }
        session.announce_members(None);
    }

    /// Takes `member` out of the session when the member at `place` hosts it, and tells them so.
    /// A request from anyone else, or about the host itself, changes nothing.
    pub fn kick(&self, place: &Place, member: &str) {
        let mut inner = self.lock();
        let Some(session) = inner.sessions.get_mut(&place.code) else {
            return;
        };
        let hosting = session
            .members
            .iter()
            .any(|seat| seat.id == place.member && seat.host);
        if !hosting || member == place.member {
            return;
        }
        let Some(at) = session.members.iter().position(|seat| seat.id == member) else {
            return;
        };
        let seat = session.members.remove(at);
        send(&seat.outbox, &Reply::Kicked);
        session.announce_members(None);
        tracing::info!(
            members = session.members.len(),
            "session {} lost a member to its host",
            place.code
        );
    }

    /// Lets one more connection in from `from`, unless the server is full or that address holds
    /// `CONNECTIONS` open already. The connection counts until the admission is dropped.
    pub fn admit(self: &Arc<Self>, from: IpAddr) -> Result<Admission, Turned> {
        let from = address(from);
        let mut inner = self.lock();
        if inner.open >= self.capacity {
            return Err(Turned::Full);
        }
        let open = inner.connections.entry(from).or_default();
        if *open >= CONNECTIONS {
            return Err(Turned::Address);
        }
        *open += 1;
        inner.open += 1;
        Ok(Admission {
            hub: self.clone(),
            from,
        })
    }

    /// Forgets sessions nobody came back to and misses old enough not to count.
    pub fn sweep(&self) {
        let mut inner = self.lock();
        let before = inner.sessions.len();
        inner.sessions.retain(|_, session| {
            session
                .emptied
                .is_none_or(|emptied| emptied.elapsed() < LINGER)
        });
        inner
            .misses
            .retain(|_, misses| misses.since.elapsed() < MISS_WINDOW);
        let gone = before - inner.sessions.len();
        if gone > 0 {
            tracing::info!(live = inner.sessions.len(), "{gone} empty sessions closed");
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Inner {
    fn member_id(&mut self) -> String {
        self.next += 1;
        format!("m{}", self.next)
    }

    fn miss(&mut self, from: IpAddr) {
        let misses = self.misses.entry(from).or_insert(Misses {
            since: Instant::now(),
            count: 0,
        });
        if misses.since.elapsed() >= MISS_WINDOW {
            *misses = Misses {
                since: Instant::now(),
                count: 0,
            };
        }
        misses.count += 1;
    }
}

impl Session {
    fn members(&self) -> Vec<Member> {
        self.members
            .iter()
            .map(|seat| Member {
                id: seat.id.clone(),
                name: seat.name.clone(),
                image: seat.image.clone(),
                host: seat.host,
            })
            .collect()
    }

    /// Tells `member`, who just came, everything about the session.
    fn welcome(&self, code: &str, member: &str) {
        let reply = Reply::Joined {
            code: code.to_owned(),
            provider: self.provider.clone(),
            you: member.to_owned(),
            members: self.members(),
            state: self.state.clone(),
            now: now(),
        };
        if let Some(seat) = self.members.iter().find(|seat| seat.id == member) {
            send(&seat.outbox, &reply);
        }
    }

    fn announce_members(&self, except: Option<&str>) {
        self.broadcast(
            &Reply::Members {
                members: self.members(),
            },
            except,
        );
    }

    /// Sends `reply` to every member but `except`.
    fn broadcast(&self, reply: &Reply, except: Option<&str>) {
        let Some(text) = encode(reply) else {
            return;
        };
        for seat in &self.members {
            if Some(seat.id.as_str()) != except {
                deliver(&seat.outbox, text.clone());
            }
        }
    }
}

/// Sends one reply to one connection.
pub fn send(outbox: &Outbox, reply: &Reply) {
    if let Some(text) = encode(reply) {
        deliver(outbox, text);
    }
}

/// The server's clock in milliseconds since the Unix epoch.
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}

fn encode(reply: &Reply) -> Option<Arc<str>> {
    match serde_json::to_string(reply) {
        Ok(text) => Some(text.into()),
        Err(error) => {
            tracing::error!("cannot encode a reply: {error}");
            None
        }
    }
}

/// Queues `text` for a connection. One too slow to keep up loses the message rather than
/// holding up everyone else.
fn deliver(outbox: &Outbox, text: Arc<str>) {
    if let Err(mpsc::error::TrySendError::Full(_)) = outbox.try_send(text) {
        tracing::warn!("a client is not keeping up, a message was dropped");
    }
}

/// Whether a track is a file on its sharer's disk.
fn local(track: &Value) -> bool {
    track
        .get("id")
        .and_then(Value::as_str)
        .is_some_and(|id| id.starts_with(LOCAL_PREFIX))
}

/// One open connection, counted against its address until it is dropped.
pub struct Admission {
    hub: Arc<Hub>,
    from: IpAddr,
}

impl Drop for Admission {
    fn drop(&mut self) {
        let mut inner = self.hub.lock();
        inner.open = inner.open.saturating_sub(1);
        if let Some(open) = inner.connections.get_mut(&self.from) {
            *open = open.saturating_sub(1);
            if *open == 0 {
                inner.connections.remove(&self.from);
            }
        }
    }
}

/// The address limits count against: an IPv4 address as it is, and an IPv6 one by its `/64`,
/// since a single network hands out the rest of it freely.
fn address(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(_) => ip,
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => {
                let mut segments = v6.segments();
                segments[4..].fill(0);
                IpAddr::V6(std::net::Ipv6Addr::from(segments))
            }
        },
    }
}
