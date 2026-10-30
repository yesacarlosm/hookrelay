use std::{collections::BTreeMap, fmt, str::FromStr};

use serde::Serialize;
use sqlx::{
    Row, SqlitePool,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};

/// A webhook as it was received.
#[derive(Debug, Clone, Serialize)]
pub struct Hook {
    pub id: i64,
    pub source: String,
    pub received_at_ms: i64,
    pub headers: BTreeMap<String, String>,
    /// Body decoded as UTF-8 (lossy). The raw bytes are kept in the database.
    pub body: String,
}

pub async fn connect(url: &str) -> sqlx::Result<SqlitePool> {
    let options: SqliteConnectOptions = url.parse()?;
    let pool = SqlitePoolOptions::new()
        // An in-memory database is per-connection, so keep a single one.
        .max_connections(if url.contains(":memory:") { 1 } else { 5 })
        .connect_with(options.create_if_missing(true))
        .await?;
    migrate(&pool).await?;
    Ok(pool)
}

async fn migrate(pool: &SqlitePool) -> sqlx::Result<()> {
    sqlx::migrate!().run(pool).await?;
    Ok(())
}

/// Stores a hook and, if it has a `target`, queues its delivery in the same transaction,
/// so a stored hook is never silently left unforwarded.
pub async fn insert_hook(
    pool: &SqlitePool,
    source: &str,
    received_at_ms: i64,
    headers: &BTreeMap<String, String>,
    body: &[u8],
    target: Option<&str>,
) -> sqlx::Result<i64> {
    let headers = serde_json::to_string(headers).expect("string map always serializes");
    let mut tx = pool.begin().await?;
    let hook_id = sqlx::query(
        "INSERT INTO hooks (source, received_at_ms, headers, body) VALUES (?, ?, ?, ?)",
    )
    .bind(source)
    .bind(received_at_ms)
    .bind(headers)
    .bind(body)
    .execute(&mut *tx)
    .await?
    .last_insert_rowid();
    if let Some(target) = target {
        sqlx::query(
            "INSERT INTO deliveries (hook_id, target_url, next_attempt_at_ms, updated_at_ms)
             VALUES (?1, ?2, ?3, ?3)",
        )
        .bind(hook_id)
        .bind(target)
        .bind(received_at_ms)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(hook_id)
}

pub async fn get_hook(pool: &SqlitePool, id: i64) -> sqlx::Result<Option<Hook>> {
    let row = sqlx::query("SELECT * FROM hooks WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|row| row_to_hook(&row)))
}

pub async fn list_hooks(
    pool: &SqlitePool,
    source: Option<&str>,
    limit: i64,
) -> sqlx::Result<Vec<Hook>> {
    let rows = sqlx::query(
        "SELECT * FROM hooks WHERE (?1 IS NULL OR source = ?1) ORDER BY id DESC LIMIT ?2",
    )
    .bind(source)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(row_to_hook).collect())
}

fn row_to_hook(row: &sqlx::sqlite::SqliteRow) -> Hook {
    let headers: String = row.get("headers");
    let body: Vec<u8> = row.get("body");
    Hook {
        id: row.get("id"),
        source: row.get("source"),
        received_at_ms: row.get("received_at_ms"),
        headers: serde_json::from_str(&headers).unwrap_or_default(),
        body: String::from_utf8_lossy(&body).into_owned(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DeliveryStatus {
    /// Waiting for its first attempt or a retry.
    Pending,
    Delivered,
    /// Gave up: out of attempts, or the target answered with a permanent error.
    Dead,
}

impl DeliveryStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            DeliveryStatus::Pending => "pending",
            DeliveryStatus::Delivered => "delivered",
            DeliveryStatus::Dead => "dead",
        }
    }
}

impl fmt::Display for DeliveryStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown delivery status `{0}`, expected pending, delivered or dead")]
pub struct UnknownStatus(String);

impl FromStr for DeliveryStatus {
    type Err = UnknownStatus;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "pending" => Ok(DeliveryStatus::Pending),
            "delivered" => Ok(DeliveryStatus::Delivered),
            "dead" => Ok(DeliveryStatus::Dead),
            other => Err(UnknownStatus(other.to_string())),
        }
    }
}

/// A delivery as shown by the API. The target URL is left out because it may carry a
/// token; the source tells which target it is.
#[derive(Debug, Clone, Serialize)]
pub struct Delivery {
    pub id: i64,
    pub hook_id: i64,
    pub source: String,
    pub status: DeliveryStatus,
    pub attempts: i64,
    pub next_attempt_at_ms: Option<i64>,
    pub last_status_code: Option<i64>,
    pub last_error: Option<String>,
    pub updated_at_ms: i64,
}

/// Everything needed to attempt one delivery.
#[derive(Debug, Clone)]
pub struct DueDelivery {
    pub id: i64,
    pub hook_id: i64,
    pub source: String,
    pub target_url: String,
    pub attempts: i64,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

/// The result of one attempt, written back to the delivery's row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptRecord {
    pub status: DeliveryStatus,
    pub attempts: i64,
    /// Only meaningful while the delivery is still pending.
    pub next_attempt_at_ms: i64,
    pub status_code: Option<u16>,
    pub error: Option<String>,
    pub at_ms: i64,
}

/// Pending deliveries whose next attempt is due, oldest first.
pub async fn due_deliveries(
    pool: &SqlitePool,
    now_ms: i64,
    limit: i64,
) -> sqlx::Result<Vec<DueDelivery>> {
    let rows = sqlx::query(
        "SELECT d.id, d.hook_id, h.source, d.target_url, d.attempts, h.headers, h.body
         FROM deliveries d JOIN hooks h ON h.id = d.hook_id
         WHERE d.status = 'pending' AND d.next_attempt_at_ms <= ?
         ORDER BY d.next_attempt_at_ms, d.id
         LIMIT ?",
    )
    .bind(now_ms)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .map(|row| {
            let headers: String = row.get("headers");
            DueDelivery {
                id: row.get("id"),
                hook_id: row.get("hook_id"),
                source: row.get("source"),
                target_url: row.get("target_url"),
                attempts: row.get("attempts"),
                headers: serde_json::from_str(&headers).unwrap_or_default(),
                body: row.get("body"),
            }
        })
        .collect())
}

/// When the earliest pending delivery is due, if there is one.
pub async fn next_due_at_ms(pool: &SqlitePool) -> sqlx::Result<Option<i64>> {
    sqlx::query_scalar("SELECT MIN(next_attempt_at_ms) FROM deliveries WHERE status = 'pending'")
        .fetch_one(pool)
        .await
}

pub async fn record_attempt(
    pool: &SqlitePool,
    id: i64,
    record: &AttemptRecord,
) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE deliveries
         SET status = ?, attempts = ?, next_attempt_at_ms = ?, last_status_code = ?,
             last_error = ?, updated_at_ms = ?
         WHERE id = ?",
    )
    .bind(record.status.as_str())
    .bind(record.attempts)
    .bind(record.next_attempt_at_ms)
    .bind(record.status_code)
    .bind(record.error.as_deref())
    .bind(record.at_ms)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Deliveries, newest first, optionally filtered by status and hook.
pub async fn list_deliveries(
    pool: &SqlitePool,
    status: Option<DeliveryStatus>,
    hook_id: Option<i64>,
    limit: i64,
) -> sqlx::Result<Vec<Delivery>> {
    let rows = sqlx::query(
        "SELECT d.*, h.source FROM deliveries d JOIN hooks h ON h.id = d.hook_id
         WHERE (?1 IS NULL OR d.status = ?1) AND (?2 IS NULL OR d.hook_id = ?2)
         ORDER BY d.id DESC LIMIT ?3",
    )
    .bind(status.map(DeliveryStatus::as_str))
    .bind(hook_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    rows.iter().map(row_to_delivery).collect()
}

fn row_to_delivery(row: &sqlx::sqlite::SqliteRow) -> sqlx::Result<Delivery> {
    let status: String = row.get("status");
    let status = status.parse().map_err(|err| sqlx::Error::ColumnDecode {
        index: "status".into(),
        source: Box::new(err),
    })?;
    Ok(Delivery {
        id: row.get("id"),
        hook_id: row.get("hook_id"),
        source: row.get("source"),
        status,
        attempts: row.get("attempts"),
        next_attempt_at_ms: (status == DeliveryStatus::Pending)
            .then(|| row.get("next_attempt_at_ms")),
        last_status_code: row.get("last_status_code"),
        last_error: row.get("last_error"),
        updated_at_ms: row.get("updated_at_ms"),
    })
}
