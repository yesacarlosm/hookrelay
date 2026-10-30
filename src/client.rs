//! A small client for a running hookrelay server's HTTP API, used by the `list` and
//! `replay` subcommands. Going through the server rather than the database keeps it the
//! only thing that knows the forwarding targets and wakes the forwarder.

use std::{
    fmt::Write as _,
    time::{Duration, UNIX_EPOCH},
};

use reqwest::{Client, RequestBuilder, Url};
use serde::{Deserialize, de::DeserializeOwned};

use crate::db::Hook;

const TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("invalid server URL")]
    InvalidUrl(#[from] url::ParseError),
    #[error("request to the hookrelay server failed")]
    Http(#[from] reqwest::Error),
    #[error("server responded with {status}: {message}")]
    Api {
        status: reqwest::StatusCode,
        message: String,
    },
}

#[derive(Debug, Deserialize)]
struct Replayed {
    delivery_id: i64,
}

#[derive(Debug, Deserialize)]
struct ApiError {
    error: String,
}

#[derive(Debug, Clone)]
pub struct HookrelayClient {
    http: Client,
    base: Url,
}

impl HookrelayClient {
    pub fn new(base: &str) -> Result<Self, ClientError> {
        let mut base = Url::parse(base)?;
        // So `join` appends to a base path like `/relay` instead of replacing it.
        if !base.path().ends_with('/') {
            base.set_path(&format!("{}/", base.path()));
        }
        let http = Client::builder().timeout(TIMEOUT).build()?;
        Ok(Self { http, base })
    }

    /// Stored hooks, newest first.
    pub async fn list_hooks(
        &self,
        source: Option<&str>,
        limit: Option<i64>,
    ) -> Result<Vec<Hook>, ClientError> {
        let mut url = self.base.join("hooks")?;
        {
            let mut query = url.query_pairs_mut();
            if let Some(source) = source {
                query.append_pair("source", source);
            }
            if let Some(limit) = limit {
                query.append_pair("limit", &limit.to_string());
            }
        }
        send(self.http.get(url)).await
    }

    /// Queues hook `id` for redelivery and returns the new delivery's id.
    pub async fn replay(&self, id: i64) -> Result<i64, ClientError> {
        let url = self.base.join(&format!("hooks/id/{id}/replay"))?;
        let replayed: Replayed = send(self.http.post(url)).await?;
        Ok(replayed.delivery_id)
    }
}

async fn send<T: DeserializeOwned>(request: RequestBuilder) -> Result<T, ClientError> {
    let response = request.send().await?;
    let status = response.status();
    if status.is_success() {
        return Ok(response.json().await?);
    }
    let body = response.bytes().await.unwrap_or_default();
    let message = serde_json::from_slice::<ApiError>(&body)
        .map(|err| err.error)
        .unwrap_or_else(|_| String::from_utf8_lossy(&body).into_owned());
    Err(ClientError::Api { status, message })
}

/// Formats hooks as an aligned table, one row per hook.
pub fn hooks_table(hooks: &[Hook]) -> String {
    let rows: Vec<[String; 4]> = hooks
        .iter()
        .map(|hook| {
            [
                hook.id.to_string(),
                format_ms(hook.received_at_ms),
                hook.source.clone(),
                hook.body.len().to_string(),
            ]
        })
        .collect();
    let header = ["ID", "RECEIVED (UTC)", "SOURCE", "BYTES"].map(String::from);

    let mut widths = header.each_ref().map(String::len);
    for row in &rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.len());
        }
    }

    let mut out = String::new();
    for row in std::iter::once(&header).chain(&rows) {
        let [id, received, source, bytes] = row;
        let [w_id, w_received, w_source, w_bytes] = widths;
        // Writing to a String can't fail.
        let _ = writeln!(
            out,
            "{id:>w_id$}  {received:<w_received$}  {source:<w_source$}  {bytes:>w_bytes$}"
        );
    }
    out
}

fn format_ms(ms: i64) -> String {
    let ms = u64::try_from(ms).unwrap_or(0);
    humantime::format_rfc3339_seconds(UNIX_EPOCH + Duration::from_millis(ms)).to_string()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn hook(id: i64, source: &str, body: &str) -> Hook {
        Hook {
            id,
            source: source.into(),
            received_at_ms: 1_793_391_907_123,
            headers: BTreeMap::new(),
            body: body.into(),
        }
    }

    #[test]
    fn formats_hooks_as_an_aligned_table() {
        let table = hooks_table(&[
            hook(12, "github", "{\"ref\":\"main\"}"),
            hook(3, "my-app", ""),
        ]);
        assert_eq!(
            table,
            "\
ID  RECEIVED (UTC)        SOURCE  BYTES
12  2026-10-30T20:25:07Z  github     14
 3  2026-10-30T20:25:07Z  my-app      0
"
        );
    }

    #[test]
    fn keeps_the_base_path_of_the_server_url() {
        let client = HookrelayClient::new("http://localhost:8080/relay").unwrap();
        assert_eq!(
            client.base.join("hooks").unwrap().as_str(),
            "http://localhost:8080/relay/hooks"
        );
    }
}
