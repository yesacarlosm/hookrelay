use std::collections::BTreeMap;

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

pub async fn insert_hook(
    pool: &SqlitePool,
    source: &str,
    received_at_ms: i64,
    headers: &BTreeMap<String, String>,
    body: &[u8],
) -> sqlx::Result<i64> {
    let headers = serde_json::to_string(headers).expect("string map always serializes");
    let result = sqlx::query(
        "INSERT INTO hooks (source, received_at_ms, headers, body) VALUES (?, ?, ?, ?)",
    )
    .bind(source)
    .bind(received_at_ms)
    .bind(headers)
    .bind(body)
    .execute(pool)
    .await?;
    Ok(result.last_insert_rowid())
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
