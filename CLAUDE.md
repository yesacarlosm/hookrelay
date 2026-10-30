# hookrelay: project memory

Personal open-source project of Carlos Yesa (GitHub: `yesacarlosm`). It is a portfolio
piece, so code quality, small commits, and a readable history matter as much as features.
It has **no relation to Fincast**: never copy Fincast code, names, or infra here.

## Git
- Commit identity for this repo is the GitHub noreply address `34103698+yesacarlosm@users.noreply.github.com`
  (set locally; the gmail is private and GitHub rejects pushes with it; the global config
  is the work email). Check with `git config user.email` before committing.
- Branch off `main` for each milestone, use small focused commits, no force push.
- Never commit or push without asking first.
- No Claude / AI co-author trailers in commits or PRs.

## Rust conventions
- Edition 2024, stable toolchain. CI runs exactly:
  `cargo fmt --all --check && cargo clippy --all-targets -- -D warnings && cargo test`.
  Run that before calling any change done.
- Errors: `thiserror` for library errors (`src/error.rs` → `AppError`, which maps to HTTP
  status codes), `anyhow` only in `main.rs`. No `unwrap()` outside tests, except for
  invariants explained in an `expect("...")` message.
- Never leak internal error details (DB errors) in HTTP responses; log them with `tracing`.
- Logging: `tracing` with structured fields (`tracing::info!(id, %source, "...")`), not `println!`.
- Async: tokio; don't block the runtime (use `spawn_blocking` for CPU-heavy or sync IO work).
- Prefer borrowing (`&str`, `&[u8]`) in function params; clone only at ownership boundaries.
- Keep comments sparse: explain *why*, not *what*.

## Layout
- `src/main.rs`: clap CLI (`serve`), tracing setup, graceful shutdown.
- `src/lib.rs`: exports `router()` so integration tests can drive the app in-process.
- `src/routes.rs`: axum 0.8 handlers (path syntax is `/{param}`, not `/:param`). Router
  state is `AppState { pool, config: Arc<Config>, wake_forwarder: Arc<Notify> }`; `SqlitePool` is extractable via `FromRef`.
- `src/verify.rs`: pure HMAC-SHA256 verification (`Scheme::{Github, Stripe}`), no axum types.
- `src/config.rs`: `Config::from_vars` reads `HOOKRELAY_SECRET_<SOURCE>` /
  `HOOKRELAY_SCHEME_<SOURCE>` / `HOOKRELAY_TARGET_<SOURCE>`; `Secret` has a redacted
  `Debug`. Ambiguous config fails at startup. Target URLs may carry tokens: never log or
  return them in full (log `origin()` only).
- `src/db.rs`: sqlx + SQLite with runtime queries (no `query!` macros, so no `DATABASE_URL`
  needed to build). Schema lives in `migrations/` and is embedded with `sqlx::migrate!`;
  never edit an applied migration, add a new one. `0001_hooks.sql` uses `IF NOT EXISTS`
  so pre-migration databases adopt it.
- `src/forward.rs`: background worker. The queue is the `deliveries` table
  (`pending` → `delivered` | `dead`; `dead` rows are the dead-letter list). Woken by
  `AppState::wake_forwarder` on insert, otherwise sleeps until the next due retry.
  Pure helpers (`backoff`, `classify`, `forward_headers`) are unit-tested.
- `tests/`: integration tests use `sqlite::memory:` (pool is capped at 1 connection because
  each in-memory connection is a separate DB) and `tower::ServiceExt::oneshot`.
  `tests/forwarding.rs` runs the real worker against a stub axum target on `127.0.0.1:0`.

## Roadmap (keep README in sync)
1. v0.1: receive/store/list/get ✅
2. v0.2: HMAC-SHA256 signature verification (`hmac`, `sha2`, constant-time compare via
   `Mac::verify_slice`). GitHub: `X-Hub-Signature-256: sha256=<hex>`. Stripe:
   `Stripe-Signature: t=<ts>,v1=<hex>` over `"{t}.{body}"` with a timestamp tolerance.
   Per-source secrets from config/env. ✅
3. v0.3: forwarding with `reqwest`, exponential backoff with jitter, dead-letter table. ✅
4. v0.4: replay endpoint and `list`/`replay` CLI subcommands.
5. Dockerfile + release workflow.

## Skills to use (Claude Code)
- `rust-best-practices`: when writing or reviewing Rust (ownership, errors, tests).
- `rust-async-patterns`: for tokio and concurrency work (forwarder, retries, shutdown).
- `coding-guidelines`: naming, formatting, clippy questions.
- `security-audit`: before v0.2 ships (signature verification is security-sensitive).
