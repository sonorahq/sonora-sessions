# sonora-session

The server behind Sonora Sessions. One listener starts a session and gets a six-digit code,
friends type the code into Sonora, and from then on whatever anyone in the session plays, pauses
or seeks to plays the same way for everyone.

Every member has to be signed in to the provider the session was started on. The server only
passes the provider's track models between clients. Local files are never shared.

## Running

```sh
cargo run                    # listens on :8080
PORT=8787 cargo run          # another port
docker build -t sonora-session .
docker run -p 8080:8080 sonora-session
```

`GET /health` answers `ok`. Clients connect a WebSocket to `/ws`. Logging follows `RUST_LOG`
and defaults to `sonora_session=info`.

## Protocol

One JSON object per text frame, its kind in `type`. The types live in `src/protocol.rs`, and
Sonora keeps a copy of them in `crates/state/src/listening/wire.rs`, so a new field has to be
optional until every client knows it.

A client sends `create` or `join` first, then `state` whenever its playback changes and `ping`
every few seconds to keep the connection and to measure the server's clock. The server answers
with `joined`, then `members`, `state`, `pong`, `ended` and `error` as things happen. Every
member hears each state, its own included, in the order the server took them, so a client that
has a state of its own on the way ignores older ones from others and everyone settles on the
last. A state's `at` is the server's clock when it arrived, so a client works out where the
track is now from it.

A session outlives its last member by two minutes so a dropped client can come back with the
same code. The host leaving on purpose ends it for everyone, and a host whose connection drops
hands over to whoever joined next. An address that gets twenty codes wrong in ten minutes is
turned away until the window passes.
