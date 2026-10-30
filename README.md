# hookrelay

[![CI](https://github.com/yesacarlosm/hookrelay/actions/workflows/ci.yml/badge.svg)](https://github.com/yesacarlosm/hookrelay/actions/workflows/ci.yml)

A small, self-hosted webhook relay written in Rust. Point any webhook provider at it,
and hookrelay verifies its signature, stores every delivery so you can inspect it, and
forwards it to your service, retrying until it gets through. Anything that still fails
can be replayed later.

```
 GitHub / Stripe / ...  ──POST──▶  hookrelay  ──▶  SQLite
                                      │
                                      └──────────▶  your service (with retries)
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
| POST   | `/hooks/id/{id}/replay` | Queue the hook for delivery again. `202 {"delivery_id":n}`; `404` if there's no such hook, `409` if its source has no target |
| GET    | `/deliveries`      | List forwarding attempts. Query: `status` (`pending`, `delivered`, `dead`), `hook_id`, `limit` |

## Configuration

| Flag                      | Env var                          | Default                 |
|---------------------------|----------------------------------|-------------------------|
| `--addr`                  | `HOOKRELAY_ADDR`                 | `127.0.0.1:8080`        |
| `--database-url`          | `HOOKRELAY_DATABASE_URL`         | `sqlite://hookrelay.db` |
| `--stripe-tolerance-secs` | `HOOKRELAY_STRIPE_TOLERANCE_SECS` | `300`                   |
|                           | `HOOKRELAY_SECRET_<SOURCE>`      | unset (no verification) |
|                           | `HOOKRELAY_SCHEME_<SOURCE>`      | by source name          |
|                           | `HOOKRELAY_TARGET_<SOURCE>`      | unset (store only)      |
| `--forward-max-attempts`  | `HOOKRELAY_FORWARD_MAX_ATTEMPTS` | `8`                     |
| `--forward-timeout-secs`  | `HOOKRELAY_FORWARD_TIMEOUT_SECS` | `10`                    |
| `--retry-base-ms`         | `HOOKRELAY_RETRY_BASE_MS`        | `1000`                  |
| `--retry-max-secs`        | `HOOKRELAY_RETRY_MAX_SECS`       | `3600`                  |
| `--server` (`list`, `replay`) | `HOOKRELAY_SERVER`           | `http://127.0.0.1:8080` |

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

## Forwarding

Set `HOOKRELAY_TARGET_<SOURCE>` to an `http(s)` URL to forward that source's hooks
(`<SOURCE>` is written the same way as for secrets). Each stored hook is queued for
delivery in the same transaction that stores it, and a background worker POSTs it to the
target with the original body and headers, plus the ones below. Hop-by-hop headers
(`Host`, `Connection`, ...) are dropped, and so are credentials and client identity that a
proxy in front of hookrelay may add (`Authorization`, `Cookie`, `Forwarded`,
`X-Forwarded-*`, `X-Real-IP`), since those were meant for hookrelay, not the target.

| Header                    | Value                                              |
|---------------------------|----------------------------------------------------|
| `X-Hookrelay-Hook-Id`     | the stored hook's id                               |
| `X-Hookrelay-Delivery-Id` | the delivery's id: same on retries, new on replays |
| `X-Hookrelay-Source`      | the source name                                    |
| `X-Hookrelay-Attempt`     | `1` for the first attempt, then `2`, …             |

The original signature headers are passed through, so the target can verify them too.

- **`2xx`**: delivered.
- **`5xx`, `408`, `429`, timeouts, connection errors**: retried with exponential backoff
  and jitter. The delay before retry *n* is `retry-base * 2^(n-1)`, capped at
  `retry-max`, then scaled randomly between 50% and 100% so hooks that failed together
  don't retry together.
- **Anything else** (other `4xx`, redirects, which are never followed): won't fix itself,
  so it's dead-lettered right away.

After `--forward-max-attempts` attempts the delivery is dead-lettered too. Dead
deliveries stay in the database with their last status code and error:

```sh
HOOKRELAY_TARGET_GITHUB=http://localhost:3000/webhooks cargo run -- serve

curl 'localhost:8080/deliveries?status=dead'
# [{"id":7,"hook_id":12,"source":"github","status":"dead","attempts":8,
#   "last_status_code":503,"last_error":"target responded with 503 Service Unavailable",...}]
```

The queue lives in SQLite, so pending retries survive a restart. Delivery is at least
once: if hookrelay stops between the target answering and recording that, the hook is
sent again, so targets should dedupe on `X-Hookrelay-Delivery-Id` (or on
`X-Hookrelay-Hook-Id` to ignore replays too). Target URLs may carry a token, so they're
never logged in full or returned by the API.

## Replay

Replaying a hook queues a new delivery to its source's *current* target, so you can fix
the target (or its config) and send dead-lettered hooks again. Earlier deliveries are
kept as history. Replays only ever go to configured targets.

The `list` and `replay` commands talk to a running server (`--server`, default
`http://127.0.0.1:8080`):

```sh
hookrelay list --source github --limit 10   # add --json for the raw API response
# ID  RECEIVED (UTC)        SOURCE  BYTES
# 12  2026-10-30T20:42:19Z  github     14

hookrelay replay 12 15
# hook 12: queued as delivery 31
# hook 15: server responded with 409 Conflict: source `stripe` has no forwarding target
```

`replay` exits non-zero if any replay failed. Over HTTP it's
`curl -X POST localhost:8080/hooks/id/12/replay`.

## Roadmap

- [x] **v0.1**: receive and store webhooks, list and inspect them
- [x] **v0.2**: signature verification (GitHub `X-Hub-Signature-256`, Stripe `Stripe-Signature`)
- [x] **v0.3**: forward to target URLs with exponential backoff and a dead-letter list
- [x] **v0.4**: replay endpoint plus `hookrelay list` / `hookrelay replay` CLI commands
- [ ] Docker image (multi-stage, distroless)

## Development

```sh
cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test
```

## License

MIT
