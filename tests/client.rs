use std::time::Duration;

use hookrelay::{
    AppState,
    client::{ClientError, HookrelayClient},
    config::Config,
};
use reqwest::StatusCode;

/// Serves the relay on a random local port, without a forwarder: replays stay queued.
async fn server(config: Config) -> String {
    let pool = hookrelay::db::connect("sqlite::memory:").await.unwrap();
    let app = hookrelay::router(AppState::new(pool, config));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

async fn post_hook(base: &str, source: &str) -> i64 {
    let res = reqwest::Client::new()
        .post(format!("{base}/hooks/{source}"))
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    let json: serde_json::Value = res.json().await.unwrap();
    json["id"].as_i64().unwrap()
}

#[tokio::test]
async fn lists_hooks_with_filters() {
    let base = server(Config::default()).await;
    for source in ["github", "stripe", "github"] {
        post_hook(&base, source).await;
    }
    let client = HookrelayClient::new(&base, None).unwrap();

    let hooks = client.list_hooks(Some("github"), None).await.unwrap();
    assert_eq!(hooks.iter().map(|h| h.id).collect::<Vec<_>>(), [3, 1]);

    let hooks = client.list_hooks(None, Some(1)).await.unwrap();
    assert_eq!(hooks.len(), 1);
}

#[tokio::test]
async fn replays_a_hook_and_reports_api_errors() {
    let target = "http://127.0.0.1:9/in".parse().unwrap();
    let base = server(Config::default().with_target("github", target)).await;
    let github_id = post_hook(&base, "github").await;
    let stripe_id = post_hook(&base, "stripe").await;
    let client = HookrelayClient::new(&base, None).unwrap();

    // Delivery 1 was queued on receipt, so the replay is delivery 2.
    assert_eq!(client.replay(github_id).await.unwrap(), 2);

    let err = client.replay(stripe_id).await.unwrap_err();
    let ClientError::Api { status, message } = err else {
        panic!("expected an API error, got {err:?}");
    };
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(message, "source `stripe` has no forwarding target");

    let err = client.replay(999).await.unwrap_err();
    assert!(
        matches!(err, ClientError::Api { status, .. } if status == StatusCode::NOT_FOUND),
        "{err:?}"
    );
}

#[tokio::test]
async fn reports_an_unreachable_server() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let client = HookrelayClient::new(&base, None).unwrap();

    let err = tokio::time::timeout(Duration::from_secs(5), client.list_hooks(None, None))
        .await
        .unwrap()
        .unwrap_err();
    assert!(matches!(err, ClientError::Http(_)), "{err:?}");
}
