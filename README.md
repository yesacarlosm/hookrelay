# hookrelay

[![CI](https://github.com/yesacarlosm/hookrelay/actions/workflows/ci.yml/badge.svg)](https://github.com/yesacarlosm/hookrelay/actions/workflows/ci.yml)

A small, self-hosted webhook relay written in Rust. Point any webhook provider at it,
and hookrelay stores every delivery so you can inspect it, and (soon) verify it,
forward it, and replay it.

```
 GitHub / Stripe / ...  ──POST──▶  hookrelay  ──▶  SQLite
                                      │
                                      └──(v0.3)──▶  your service (with retries)
```

## Quick start

```sh
cargo run -- serve                       # listens on 127.0.0.1:8080, writes hookrelay.db

curl -X POST localhost:8080/hooks/github \
  -H 'x-github-event: push' -d '{"ref":"main"}'
# {"id":1}

curl localhost:8080/hooks?source=github  # newest first
curl localhost:8080/hooks/id/1           # one hook, with headers and body
```

## API

| Method | Path               | Description                                         |
|--------|--------------------|-----------------------------------------------------|
| GET    | `/health`          | Liveness check                                      |
| POST   | `/hooks/{source}`  | Store a webhook. `source` is `[a-z0-9_-]{1,64}`      |
| GET    | `/hooks`           | List hooks. Query: `source`, `limit` (1–500, default 50) |
| GET    | `/hooks/id/{id}`   | Fetch one hook                                      |

## Configuration

| Flag             | Env var                  | Default                 |
|------------------|--------------------------|-------------------------|
| `--addr`         | `HOOKRELAY_ADDR`         | `127.0.0.1:8080`        |
| `--database-url` | `HOOKRELAY_DATABASE_URL` | `sqlite://hookrelay.db` |

Logging is controlled by `RUST_LOG` (default `info`).

## Roadmap

- [x] **v0.1**: receive and store webhooks, list and inspect them
- [ ] **v0.2**: signature verification (GitHub `X-Hub-Signature-256`, Stripe `Stripe-Signature`)
- [ ] **v0.3**: forward to target URLs with exponential backoff and a dead-letter list
- [ ] **v0.4**: replay endpoint plus `hookrelay list` / `hookrelay replay` CLI commands
- [ ] Docker image (multi-stage, distroless)

## Development

```sh
cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test
```

## License

MIT
