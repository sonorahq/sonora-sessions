# sonora-sessions

The server behind Sonora Sessions.

## Run

```sh
docker build -t sonora-sessions .
docker run -d -p 8080:8080 --name sonora-sessions sonora-sessions
```

Or without Docker: `cargo run --release`.

| Variable   | Default                | Meaning            |
| ---------- | ---------------------- | ------------------ |
| `PORT`     | `8080`                 | Port to listen on  |
| `RUST_LOG` | `sonora_sessions=info` | Log level          |

Put it behind a reverse proxy with TLS and WebSocket upgrades allowed.

## Endpoints

| Path          | Answer                                          |
| ------------- | ----------------------------------------------- |
| `GET /`       | `sonora-sessions`                               |
| `GET /health` | `ok` while the server is up                     |
| `GET /ws`     | The WebSocket Sonora connects to                |

`sonora-sessions health` exits with 0 while the local server answers `/health`. The Docker image
uses it as its health check.

## Use it in Sonora

Settings → Playback → Sessions: turn off **Use the official Sonora Sessions server**, type the
server's address, such as `wss://sessions.example.com`, and press **Save**. Sonora keeps it only
once `/health` answers.
