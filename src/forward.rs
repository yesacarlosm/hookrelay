//! Background worker that forwards stored hooks to their targets, retrying with
//! exponential backoff and jitter until they are delivered or dead-lettered.
//!
//! The queue lives in the `deliveries` table, so pending retries survive restarts.
//! Delivery is at least once: a crash between a successful POST and recording it means
//! the hook is sent again, so targets should dedupe on `X-Hookrelay-Delivery-Id`. Retries
//! keep the delivery id; replays get a new one.

use std::{error::Error, future::Future, sync::Arc, time::Duration};

use reqwest::{
    Client, StatusCode,
    header::{HeaderMap, HeaderName, HeaderValue},
};
use sqlx::SqlitePool;
use tokio::{sync::Notify, task::JoinSet};

use crate::{
    db::{self, AttemptRecord, DeliveryStatus, DueDelivery},
    now_ms,
};

/// How many deliveries are attempted concurrently.
const BATCH_SIZE: i64 = 32;
/// Upper bound on how long the worker sleeps, as a safety net for missed wake-ups.
const IDLE_POLL: Duration = Duration::from_secs(5);

/// Headers that are never forwarded: hop-by-hop and framing headers describe the original
/// connection, not the hook, and credentials or client identity added by a proxy in front
/// of hookrelay were meant for hookrelay, not the target.
const SKIPPED_HEADERS: &[&str] = &[
    "authorization",
    "connection",
    "content-length",
    "cookie",
    "expect",
    "forwarded",
    "host",
    "keep-alive",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "x-real-ip",
];
/// Prefixes of header families that are skipped for the same reasons.
const SKIPPED_PREFIXES: &[&str] = &["x-forwarded-", "x-hookrelay-"];

#[derive(Debug, Clone)]
pub struct ForwardSettings {
    /// Total attempts, including the first, before a delivery is dead-lettered.
    pub max_attempts: u32,
    /// Per-attempt timeout, covering connect and response headers.
    pub timeout: Duration,
    /// Delay before the first retry; doubles on each retry.
    pub retry_base: Duration,
    /// Cap on the delay between retries.
    pub retry_max: Duration,
}

impl Default for ForwardSettings {
    fn default() -> Self {
        Self {
            max_attempts: 8,
            timeout: Duration::from_secs(10),
            retry_base: Duration::from_secs(1),
            retry_max: Duration::from_secs(3600),
        }
    }
}

/// The HTTP client used for forwarding. Redirects are not followed, so a target can't
/// bounce hooks (and their signature headers) to a host that wasn't configured.
pub fn client(settings: &ForwardSettings) -> reqwest::Result<Client> {
    Client::builder()
        .timeout(settings.timeout)
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!("hookrelay/", env!("CARGO_PKG_VERSION")))
        .build()
}

/// Runs the worker until `shutdown` resolves. In-flight attempts are finished first.
///
/// `wake` should be notified whenever a delivery is queued, so it goes out right away
/// instead of on the next poll.
pub async fn run(
    pool: SqlitePool,
    client: Client,
    settings: ForwardSettings,
    wake: Arc<Notify>,
    shutdown: impl Future<Output = ()>,
) {
    let settings = Arc::new(settings);
    tokio::pin!(shutdown);
    loop {
        let wait = match db::due_deliveries(&pool, now_ms(), BATCH_SIZE).await {
            Ok(batch) => {
                let full = batch.len() as i64 == BATCH_SIZE;
                attempt_batch(&pool, &client, &settings, batch).await;
                if full {
                    Duration::ZERO
                } else {
                    time_until_next_due(&pool).await
                }
            }
            Err(err) => {
                tracing::error!(error = %err, "failed to load due deliveries");
                IDLE_POLL
            }
        };

        tokio::select! {
            biased;
            () = &mut shutdown => break,
            () = wake.notified() => {}
            () = tokio::time::sleep(wait) => {}
        }
    }
    tracing::info!("forwarder stopped");
}

async fn time_until_next_due(pool: &SqlitePool) -> Duration {
    match db::next_due_at_ms(pool).await {
        Ok(Some(due_ms)) => {
            let ms = u64::try_from(due_ms.saturating_sub(now_ms())).unwrap_or(0);
            Duration::from_millis(ms).min(IDLE_POLL)
        }
        Ok(None) => IDLE_POLL,
        Err(err) => {
            tracing::error!(error = %err, "failed to find the next due delivery");
            IDLE_POLL
        }
    }
}

async fn attempt_batch(
    pool: &SqlitePool,
    client: &Client,
    settings: &Arc<ForwardSettings>,
    batch: Vec<DueDelivery>,
) {
    let mut tasks = JoinSet::new();
    for delivery in batch {
        let (pool, client, settings) = (pool.clone(), client.clone(), Arc::clone(settings));
        tasks.spawn(async move { attempt(&pool, &client, &settings, delivery).await });
    }
    while let Some(result) = tasks.join_next().await {
        if let Err(err) = result {
            tracing::error!(error = %err, "delivery task panicked");
        }
    }
}

async fn attempt(
    pool: &SqlitePool,
    client: &Client,
    settings: &ForwardSettings,
    delivery: DueDelivery,
) {
    let attempt = delivery.attempts + 1;
    let headers = forward_headers(&delivery, attempt);
    let response = client
        .post(&delivery.target_url)
        .headers(headers)
        .body(delivery.body)
        .send()
        .await;

    let outcome = match response {
        Ok(response) => classify(response.status()),
        // `without_url` because the target URL may carry a token.
        Err(err) => Outcome::Retry {
            status_code: None,
            error: error_chain(&err.without_url()),
        },
    };

    let at_ms = now_ms();
    let record = match outcome {
        Outcome::Delivered { status_code } => AttemptRecord {
            status: DeliveryStatus::Delivered,
            attempts: attempt,
            next_attempt_at_ms: at_ms,
            status_code: Some(status_code),
            error: None,
            at_ms,
        },
        Outcome::Retry { status_code, error } if attempt < i64::from(settings.max_attempts) => {
            let delay = backoff(
                attempt,
                settings.retry_base,
                settings.retry_max,
                rand::random(),
            );
            AttemptRecord {
                status: DeliveryStatus::Pending,
                attempts: attempt,
                next_attempt_at_ms: at_ms.saturating_add(delay.as_millis() as i64),
                status_code,
                error: Some(error),
                at_ms,
            }
        }
        Outcome::Retry { status_code, error } | Outcome::Permanent { status_code, error } => {
            AttemptRecord {
                status: DeliveryStatus::Dead,
                attempts: attempt,
                next_attempt_at_ms: at_ms,
                status_code,
                error: Some(error),
                at_ms,
            }
        }
    };

    let (id, hook_id, source) = (delivery.id, delivery.hook_id, &delivery.source);
    match record.status {
        DeliveryStatus::Delivered => {
            tracing::info!(id, hook_id, %source, attempt, "hook forwarded");
        }
        DeliveryStatus::Pending => tracing::warn!(
            id, hook_id, %source, attempt,
            error = record.error.as_deref(),
            retry_in_ms = record.next_attempt_at_ms - at_ms,
            "forward failed, will retry"
        ),
        DeliveryStatus::Dead => tracing::error!(
            id, hook_id, %source, attempt,
            error = record.error.as_deref(),
            "forward failed, dead-lettered"
        ),
    }

    if let Err(err) = db::record_attempt(pool, id, &record).await {
        // The row stays pending at its old schedule, so the attempt is simply repeated.
        tracing::error!(id, error = %err, "failed to record delivery attempt");
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Delivered {
        status_code: u16,
    },
    Retry {
        status_code: Option<u16>,
        error: String,
    },
    Permanent {
        status_code: Option<u16>,
        error: String,
    },
}

/// 2xx is delivered. Timeouts, rate limits and server errors may clear up, so they are
/// retried; anything else (redirects, other 4xx) won't fix itself and is dead-lettered.
fn classify(status: StatusCode) -> Outcome {
    let status_code = status.as_u16();
    if status.is_success() {
        return Outcome::Delivered { status_code };
    }
    let error = format!("target responded with {status}");
    let retryable = status.is_server_error()
        || status == StatusCode::REQUEST_TIMEOUT
        || status == StatusCode::TOO_MANY_REQUESTS;
    let status_code = Some(status_code);
    if retryable {
        Outcome::Retry { status_code, error }
    } else {
        Outcome::Permanent { status_code, error }
    }
}

/// Delay before the retry that follows failed attempt number `attempt` (1-based):
/// `base * 2^(attempt - 1)`, capped at `max`, then scaled into its upper half by `jitter`
/// (in `[0, 1)`). Keeping half the delay fixed guarantees retries back off, while the
/// random half spreads out hooks that failed together.
fn backoff(attempt: i64, base: Duration, max: Duration, jitter: f64) -> Duration {
    let exponent = u32::try_from(attempt.max(1) - 1).unwrap_or(u32::MAX);
    let delay = base.saturating_mul(2u32.saturating_pow(exponent)).min(max);
    delay.mul_f64(0.5 + 0.5 * jitter.clamp(0.0, 1.0))
}

fn forward_headers(delivery: &DueDelivery, attempt: i64) -> HeaderMap {
    let mut headers: HeaderMap = delivery
        .headers
        .iter()
        .filter(|(name, _)| {
            !SKIPPED_HEADERS.contains(&name.as_str())
                && !SKIPPED_PREFIXES
                    .iter()
                    .any(|prefix| name.starts_with(prefix))
        })
        .filter_map(|(name, value)| {
            Some((
                HeaderName::from_bytes(name.as_bytes()).ok()?,
                HeaderValue::from_str(value).ok()?,
            ))
        })
        .collect();
    headers.insert("x-hookrelay-hook-id", HeaderValue::from(delivery.hook_id));
    headers.insert("x-hookrelay-delivery-id", HeaderValue::from(delivery.id));
    headers.insert("x-hookrelay-attempt", HeaderValue::from(attempt));
    if let Ok(source) = HeaderValue::from_str(&delivery.source) {
        headers.insert("x-hookrelay-source", source);
    }
    headers
}

/// `reqwest` errors put the useful part ("connection refused") in their sources.
fn error_chain(err: &dyn Error) -> String {
    let mut message = err.to_string();
    let mut source = err.source();
    while let Some(err) = source {
        message.push_str(": ");
        message.push_str(&err.to_string());
        source = err.source();
    }
    message
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    const BASE: Duration = Duration::from_secs(1);
    const MAX: Duration = Duration::from_secs(60);

    #[test]
    fn backoff_doubles_and_is_capped() {
        let delays: Vec<_> = (1..=8)
            .map(|attempt| backoff(attempt, BASE, MAX, 1.0).as_secs())
            .collect();
        assert_eq!(delays, [1, 2, 4, 8, 16, 32, 60, 60]);
    }

    #[test]
    fn backoff_jitter_keeps_at_least_half_the_delay() {
        assert_eq!(backoff(3, BASE, MAX, 0.0), Duration::from_secs(2));
        assert_eq!(backoff(3, BASE, MAX, 0.5), Duration::from_secs(3));
        assert_eq!(backoff(3, BASE, MAX, 2.0), Duration::from_secs(4));
    }

    #[test]
    fn backoff_does_not_overflow_on_huge_attempt_counts() {
        assert_eq!(backoff(i64::MAX, BASE, MAX, 1.0), MAX);
        assert_eq!(backoff(0, BASE, MAX, 1.0), BASE);
    }

    #[test]
    fn classifies_responses() {
        assert!(matches!(
            classify(StatusCode::NO_CONTENT),
            Outcome::Delivered { status_code: 204 }
        ));
        for status in [500, 502, 503, 408, 429] {
            let status = StatusCode::from_u16(status).unwrap();
            assert!(
                matches!(classify(status), Outcome::Retry { .. }),
                "{status}"
            );
        }
        for status in [301, 400, 401, 404, 410] {
            let status = StatusCode::from_u16(status).unwrap();
            assert!(
                matches!(classify(status), Outcome::Permanent { .. }),
                "{status}"
            );
        }
    }

    #[test]
    fn forwards_original_headers_without_hop_by_hop_or_proxy_ones() {
        let delivery = DueDelivery {
            id: 1,
            hook_id: 42,
            source: "github".into(),
            target_url: "http://localhost/".into(),
            attempts: 0,
            headers: BTreeMap::from(
                [
                    ("content-type", "application/json"),
                    ("x-hub-signature-256", "sha256=abc"),
                    ("host", "relay.example.com"),
                    ("content-length", "2"),
                    ("connection", "keep-alive"),
                    ("authorization", "Basic cmVsYXk6cGFzcw=="),
                    ("cookie", "session=abc"),
                    ("forwarded", "for=203.0.113.7"),
                    ("x-forwarded-for", "203.0.113.7"),
                    ("x-forwarded-proto", "https"),
                    ("x-real-ip", "203.0.113.7"),
                    ("x-hookrelay-hook-id", "spoofed"),
                ]
                .map(|(k, v)| (k.to_string(), v.to_string())),
            ),
            body: b"{}".to_vec(),
        };
        let headers = forward_headers(&delivery, 2);
        assert_eq!(headers["content-type"], "application/json");
        assert_eq!(headers["x-hub-signature-256"], "sha256=abc");
        assert_eq!(headers["x-hookrelay-hook-id"], "42");
        assert_eq!(headers["x-hookrelay-delivery-id"], "1");
        assert_eq!(headers["x-hookrelay-attempt"], "2");
        assert_eq!(headers["x-hookrelay-source"], "github");
        for skipped in [
            "host",
            "content-length",
            "connection",
            "authorization",
            "cookie",
            "forwarded",
            "x-forwarded-for",
            "x-forwarded-proto",
            "x-real-ip",
        ] {
            assert!(!headers.contains_key(skipped), "{skipped}");
        }
        assert_eq!(headers.get_all("x-hookrelay-hook-id").iter().count(), 1);
    }
}
