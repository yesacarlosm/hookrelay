use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use axum::{
    Router,
    body::{Body, Bytes},
    extract::State,
    http::{HeaderMap, Request, StatusCode},
    routing::post,
};
use hookrelay::{AppState, config::Config, forward::ForwardSettings};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

/// A downstream service that records what it receives and answers with scripted
/// statuses, falling back to `200` once the script runs out.
#[derive(Clone, Default)]
struct Target {
    received: Arc<Mutex<Vec<(HeaderMap, Bytes)>>>,
    script: Arc<Mutex<VecDeque<StatusCode>>>,
}

impl Target {
    async fn start(script: &[u16]) -> (Self, reqwest::Url) {
        let target = Target::default();
        target.script.lock().unwrap().extend(
            script
                .iter()
                .map(|&code| StatusCode::from_u16(code).unwrap()),
        );
        let app = Router::new()
            .route("/in", post(receive))
            .with_state(target.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (target, format!("http://{addr}/in").parse().unwrap())
    }

    fn received(&self) -> Vec<(HeaderMap, Bytes)> {
        self.received.lock().unwrap().clone()
    }
}

async fn receive(State(target): State<Target>, headers: HeaderMap, body: Bytes) -> StatusCode {
    target.received.lock().unwrap().push((headers, body));
    target
        .script
        .lock()
        .unwrap()
        .pop_front()
        .unwrap_or(StatusCode::OK)
}

/// The relay with the forwarder running, tuned for fast retries.
async fn relay(config: Config, max_attempts: u32) -> Router {
    let pool = hookrelay::db::connect("sqlite::memory:").await.unwrap();
    let state = AppState::new(pool.clone(), config);
    let settings = ForwardSettings {
        max_attempts,
        timeout: Duration::from_secs(2),
        retry_base: Duration::from_millis(10),
        retry_max: Duration::from_millis(50),
    };
    let client = hookrelay::forward::client(&settings).unwrap();
    tokio::spawn(hookrelay::forward::run(
        pool,
        client,
        settings,
        state.wake_forwarder.clone(),
        std::future::pending(),
    ));
    hookrelay::router(state)
}

async fn send(app: &Router, req: Request<Body>) -> (StatusCode, Value) {
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

async fn post_hook(app: &Router, source: &str, body: &'static str) -> i64 {
    let (status, created) = send(
        app,
        Request::post(format!("/hooks/{source}"))
            .header("content-type", "application/json")
            .header("x-github-event", "push")
            .body(Body::from(body))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    created["id"].as_i64().unwrap()
}

async fn deliveries(app: &Router, query: &str) -> Vec<Value> {
    let (status, json) = send(
        app,
        Request::get(format!("/deliveries?{query}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    json.as_array().unwrap().clone()
}

/// Polls until the hook's delivery leaves `pending`, and returns it.
async fn settled_delivery(app: &Router, hook_id: i64) -> Value {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let found = deliveries(app, &format!("hook_id={hook_id}")).await;
        if let [delivery] = found.as_slice()
            && delivery["status"] != "pending"
        {
            return delivery.clone();
        }
        assert!(
            Instant::now() < deadline,
            "delivery never settled: {found:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn forwards_body_and_headers_to_the_target() {
    let (target, url) = Target::start(&[]).await;
    let app = relay(Config::default().with_target("github", url), 3).await;

    let hook_id = post_hook(&app, "github", r#"{"ref":"main"}"#).await;
    let delivery = settled_delivery(&app, hook_id).await;

    assert_eq!(delivery["status"], "delivered");
    assert_eq!(delivery["attempts"], 1);
    assert_eq!(delivery["last_status_code"], 200);
    assert!(delivery.get("target_url").is_none());

    let received = target.received();
    let [(headers, body)] = received.as_slice() else {
        panic!("expected one request, got {}", received.len());
    };
    assert_eq!(body.as_ref(), br#"{"ref":"main"}"#);
    assert_eq!(headers["content-type"], "application/json");
    assert_eq!(headers["x-github-event"], "push");
    assert_eq!(headers["x-hookrelay-hook-id"], hook_id.to_string().as_str());
    assert_eq!(headers["x-hookrelay-source"], "github");
}

#[tokio::test]
async fn retries_server_errors_until_delivered() {
    let (target, url) = Target::start(&[503, 500]).await;
    let app = relay(Config::default().with_target("github", url), 5).await;

    let hook_id = post_hook(&app, "github", "{}").await;
    let delivery = settled_delivery(&app, hook_id).await;

    assert_eq!(delivery["status"], "delivered");
    assert_eq!(delivery["attempts"], 3);
    assert!(delivery["last_error"].is_null());
    let attempts: Vec<_> = target
        .received()
        .iter()
        .map(|(headers, _)| headers["x-hookrelay-attempt"].to_str().unwrap().to_owned())
        .collect();
    assert_eq!(attempts, ["1", "2", "3"]);
}

#[tokio::test]
async fn dead_letters_after_max_attempts() {
    let (target, url) = Target::start(&[500, 500, 500, 500]).await;
    let app = relay(Config::default().with_target("github", url), 3).await;

    let hook_id = post_hook(&app, "github", "{}").await;
    let delivery = settled_delivery(&app, hook_id).await;

    assert_eq!(delivery["status"], "dead");
    assert_eq!(delivery["attempts"], 3);
    assert_eq!(delivery["last_status_code"], 500);
    assert!(delivery["next_attempt_at_ms"].is_null());
    assert_eq!(target.received().len(), 3);

    let dead = deliveries(&app, "status=dead").await;
    assert_eq!(dead.len(), 1);
    assert_eq!(dead[0]["hook_id"], hook_id);
}

#[tokio::test]
async fn dead_letters_permanent_errors_without_retrying() {
    let (target, url) = Target::start(&[404]).await;
    let app = relay(Config::default().with_target("github", url), 5).await;

    let hook_id = post_hook(&app, "github", "{}").await;
    let delivery = settled_delivery(&app, hook_id).await;

    assert_eq!(delivery["status"], "dead");
    assert_eq!(delivery["attempts"], 1);
    assert_eq!(target.received().len(), 1);
}

#[tokio::test]
async fn retries_when_the_target_is_unreachable() {
    // Bind and drop a listener to get a port that refuses connections.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/in", listener.local_addr().unwrap());
    drop(listener);
    let app = relay(
        Config::default().with_target("github", url.parse().unwrap()),
        2,
    )
    .await;

    let hook_id = post_hook(&app, "github", "{}").await;
    let delivery = settled_delivery(&app, hook_id).await;

    assert_eq!(delivery["status"], "dead");
    assert_eq!(delivery["attempts"], 2);
    assert!(delivery["last_status_code"].is_null());
    let error = delivery["last_error"].as_str().unwrap();
    assert!(!error.contains(&url), "error leaks the target URL: {error}");
}

#[tokio::test]
async fn hooks_without_a_target_are_only_stored() {
    let (target, url) = Target::start(&[]).await;
    let app = relay(Config::default().with_target("github", url), 3).await;

    let stripe_id = post_hook(&app, "stripe", "{}").await;
    let github_id = post_hook(&app, "github", "{}").await;
    settled_delivery(&app, github_id).await;

    assert!(
        deliveries(&app, &format!("hook_id={stripe_id}"))
            .await
            .is_empty()
    );
    assert_eq!(target.received().len(), 1);
}

#[tokio::test]
async fn rejects_unknown_delivery_status() {
    let app = relay(Config::default(), 3).await;
    let (status, body) = send(
        &app,
        Request::get("/deliveries?status=lost")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap().contains("lost"));
}
