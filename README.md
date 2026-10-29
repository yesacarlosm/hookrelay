# hookrelay

[![CI](https://github.com/yesacarlosm/hookrelay/actions/workflows/ci.yml/badge.svg)](https://github.com/yesacarlosm/hookrelay/actions/workflows/ci.yml)

A small, self-hosted webhook relay written in Rust. Point any webhook provider at it,
and hookrelay verifies its signature, stores every delivery so you can inspect it, and
(soon) forwards and replays it.

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
| POST   | `/hooks/{source}`  | Store a webhook. `source` is `[a-z0-9_-]{1,64}`. `401` if the source has a secret and the signature is missing or invalid |
| GET    | `/hooks`           | List hooks. Query: `source`, `limit` (1–500, default 50) |
| GET    | `/hooks/id/{id}`   | Fetch one hook                                      |

## Configuration

| Flag                      | Env var                          | Default                 |
|---------------------------|----------------------------------|-------------------------|
| `--addr`                  | `HOOKRELAY_ADDR`                 | `127.0.0.1:8080`        |
| `--database-url`          | `HOOKRELAY_DATABASE_URL`         | `sqlite://hookrelay.db` |
| `--stripe-tolerance-secs` | `HOOKRELAY_STRIPE_TOLERANCE_SECS` | `300`                   |
|                           | `HOOKRELAY_SECRET_<SOURCE>`      | unset (no verification) |
|                           | `HOOKRELAY_SCHEME_<SOURCE>`      | by source name          |

Logging is controlled by `RUST_LOG` (default `info`).

## Signature verification

Set `HOOKRELAY_SECRET_<SOURCE>` to turn on HMAC-SHA256 verification for a source.
`<SOURCE>` is the source name in uppercase, with `-` written as `_`
(`HOOKRELAY_SECRET_MY_APP` covers `/hooks/my-app` and `/hooks/my_app`).
Sources without a secret are stored unverified, as in v0.1.

| Scheme   | Header                                    | Signed payload    |
|----------|-------------------------------------------|-------------------|
| `github` | `X-Hub-Signature-256: sha256=<hex>`       | raw body          |
| `stripe` | `Stripe-Signature: t=<unix>,v1=<hex>,...` | `"{t}.{raw body}"` |

The scheme defaults to the source name for `github` and `stripe`. Any other source needs
`HOOKRELAY_SCHEME_<SOURCE>=github|stripe`, and hookrelay refuses to start if it's missing,
so a typo can't silently turn verification off. Stripe deliveries are accepted if any
`v1` signature matches and the timestamp is within `--stripe-tolerance-secs` of now.
Signatures are compared in constant time, and secrets are never logged.

Rejected deliveries get `401 {"error":"invalid or missing signature"}` and are not stored.

```sh
HOOKRELAY_SECRET_GITHUB=s3cret cargo run -- serve

body='{"ref":"main"}'
sig=$(printf '%s' "$body" | openssl dgst -sha256 -hmac s3cret | awk '{print $NF}')
curl -X POST localhost:8080/hooks/github \
  -H "x-hub-signature-256: sha256=$sig" -d "$body"
# {"id":1}

curl -X POST localhost:8080/hooks/github -H 'x-hub-signature-256: sha256=00' -d "$body"
# {"error":"invalid or missing signature"}
```

## Roadmap

- [x] **v0.1**: receive and store webhooks, list and inspect them
- [x] **v0.2**: signature verification (GitHub `X-Hub-Signature-256`, Stripe `Stripe-Signature`)
- [ ] **v0.3**: forward to target URLs with exponential backoff and a dead-letter list
- [ ] **v0.4**: replay endpoint plus `hookrelay list` / `hookrelay replay` CLI commands
- [ ] Docker image (multi-stage, distroless)

## Development

```sh
cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test
```

## License

MIT
