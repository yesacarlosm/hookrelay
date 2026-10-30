use std::{collections::BTreeMap, sync::Arc};

use axum::{
    Json, Router,
    body::Bytes,
    extract::{FromRef, Path, Query, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::SqlitePool;
use tokio::sync::Notify;

use crate::{
    config::Config,
    db::{self, Delivery, DeliveryStatus, Hook},
    error::AppError,
    now_ms, verify,
};

const DEFAULT_LIST_LIMIT: i64 = 50;
const MAX_LIST_LIMIT: i64 = 500;

#[derive(Clone)]
pub struct AppState {
    pub pool: SqlitePool,
    pub config: Arc<Config>,
    /// Notified when a delivery is queued; hand it to [`crate::forward::run`].
    pub wake_forwarder: Arc<Notify>,
}

impl AppState {
    pub fn new(pool: SqlitePool, config: Config) -> Self {
        Self {
            pool,
            config: Arc::new(config),
            wake_forwarder: Arc::new(Notify::new()),
        }
    }
}

impl FromRef<AppState> for SqlitePool {
    fn from_ref(state: &AppState) -> Self {
        state.pool.clone()
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/hooks", get(list_hooks))
        .route("/hooks/{source}", post(receive_hook))
        .route("/hooks/id/{id}", get(get_hook))
        .route("/deliveries", get(list_deliveries))
        .with_state(state)
}

async fn health() -> &'static str {
    "ok"
}

async fn receive_hook(
    State(state): State<AppState>,
    Path(source): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<(StatusCode, Json<Value>), AppError> {
    if !is_valid_source(&source) {
        return Err(AppError::BadRequest(
            "source must be 1-64 chars of [a-z0-9_-]".into(),
        ));
    }

    let received_at_ms = now_ms();
    if let Some(verified) = state.config.source(&source) {
        let header = headers
            .get(verified.scheme.header_name())
            .and_then(|value| value.to_str().ok());
        verify::verify(
            verified.scheme,
            verified.secret.expose(),
            header,
            &body,
            received_at_ms / 1000,
            state.config.stripe_tolerance,
        )
        .map_err(|reason| {
            tracing::warn!(%source, %reason, "rejected webhook with invalid signature");
            AppError::Unauthorized
        })?;
    }

    let headers: BTreeMap<String, String> = headers
        .iter()
        .filter_map(|(name, value)| Some((name.to_string(), value.to_str().ok()?.to_string())))
        .collect();

    let target = state.config.target(&source).map(|url| url.as_str());
    let id = db::insert_hook(
        &state.pool,
        &source,
        received_at_ms,
        &headers,
        &body,
        target,
    )
    .await?;
    tracing::info!(id, %source, bytes = body.len(), forward = target.is_some(), "hook received");
    if target.is_some() {
        state.wake_forwarder.notify_one();
    }
    Ok((StatusCode::CREATED, Json(json!({ "id": id }))))
}

#[derive(Debug, Deserialize)]
struct ListParams {
    source: Option<String>,
    limit: Option<i64>,
}

async fn list_hooks(
    State(pool): State<SqlitePool>,
    Query(params): Query<ListParams>,
) -> Result<Json<Vec<Hook>>, AppError> {
    let limit = params
        .limit
        .unwrap_or(DEFAULT_LIST_LIMIT)
        .clamp(1, MAX_LIST_LIMIT);
    let hooks = db::list_hooks(&pool, params.source.as_deref(), limit).await?;
    Ok(Json(hooks))
}

async fn get_hook(
    State(pool): State<SqlitePool>,
    Path(id): Path<i64>,
) -> Result<Json<Hook>, AppError> {
    db::get_hook(&pool, id)
        .await?
        .map(Json)
        .ok_or(AppError::NotFound)
}

#[derive(Debug, Deserialize)]
struct DeliveryParams {
    status: Option<String>,
    hook_id: Option<i64>,
    limit: Option<i64>,
}

async fn list_deliveries(
    State(pool): State<SqlitePool>,
    Query(params): Query<DeliveryParams>,
) -> Result<Json<Vec<Delivery>>, AppError> {
    let status = params
        .status
        .as_deref()
        .map(str::parse::<DeliveryStatus>)
        .transpose()
        .map_err(|err| AppError::BadRequest(err.to_string()))?;
    let limit = params
        .limit
        .unwrap_or(DEFAULT_LIST_LIMIT)
        .clamp(1, MAX_LIST_LIMIT);
    let deliveries = db::list_deliveries(&pool, status, params.hook_id, limit).await?;
    Ok(Json(deliveries))
}

fn is_valid_source(source: &str) -> bool {
    (1..=64).contains(&source.len())
        && source
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

#[cfg(test)]
mod tests {
    use super::is_valid_source;

    #[test]
    fn validates_source_names() {
        assert!(is_valid_source("github"));
        assert!(is_valid_source("my_app-2"));
        assert!(!is_valid_source(""));
        assert!(!is_valid_source("GitHub"));
        assert!(!is_valid_source("a/b"));
        assert!(!is_valid_source(&"a".repeat(65)));
    }
}
