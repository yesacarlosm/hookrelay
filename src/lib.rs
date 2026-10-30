//! hookrelay: receive, verify, store, forward, and replay webhooks.

use std::time::{SystemTime, UNIX_EPOCH};

pub mod client;
pub mod config;
pub mod db;
pub mod error;
pub mod forward;
pub mod routes;
pub mod verify;

pub use routes::{AppState, router};

pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}
