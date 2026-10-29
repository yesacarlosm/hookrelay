use std::time::{SystemTime, UNIX_EPOCH};

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use hmac::{Hmac, KeyInit, Mac};
use hookrelay::{AppState, config::Config, verify::Scheme};
use http_body_util::BodyExt;
use serde_json::Value;
use sha2::Sha256;
use tower::ServiceExt;

// Test vector published in GitHub's "Validating webhook deliveries" docs.
const GITHUB_SECRET: &str = "It's a Secret to Everybody";
const GITHUB_BODY: &str = "Hello, World!";
const GITHUB_SIGNATURE: &str =
    "sha256=757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17";

const STRIPE_SECRET: &str = "whsec_test_secret";

async fn app() -> Router {
    let pool = hookrelay::db::connect("sqlite::memory:").await.unwrap();
    let config = Config::default()
        .with_source("github", Scheme::Github, GITHUB_SECRET)
        .with_source("stripe", Scheme::Stripe, STRIPE_SECRET);
    hookrelay::router(AppState::new(pool, config))
}

async fn send(app: &Router, req: Request<Body>) -> (StatusCode, Value) {
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

async fn stored_count(app: &Router) -> usize {
    let (_, hooks) = send(app, Request::get("/hooks").body(Body::empty()).unwrap()).await;
    hooks.as_array().unwrap().len()
}

fn github_hook(signature: Option<&str>, body: &'static str) -> Request<Body> {
    let mut req = Request::post("/hooks/github");
    if let Some(signature) = signature {
        req = req.header("x-hub-signature-256", signature);
    }
    req.body(Body::from(body)).unwrap()
}

fn stripe_signature(body: &str) -> String {
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let mut mac = Hmac::<Sha256>::new_from_slice(STRIPE_SECRET.as_bytes()).unwrap();
    mac.update(format!("{t}.{body}").as_bytes());
    format!("t={t},v1={}", hex::encode(mac.finalize().into_bytes()))
}

#[tokio::test]
async fn accepts_github_hook_with_valid_signature() {
    let app = app().await;

    let (status, _) = send(&app, github_hook(Some(GITHUB_SIGNATURE), GITHUB_BODY)).await;

    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(stored_count(&app).await, 1);
}

#[tokio::test]
async fn rejects_github_hook_with_bad_signature_without_storing_it() {
    let app = app().await;

    let (status, body) = send(&app, github_hook(Some(GITHUB_SIGNATURE), "Tampered!")).await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"], "invalid or missing signature");
    assert_eq!(stored_count(&app).await, 0);
}

#[tokio::test]
async fn rejects_github_hook_without_signature_when_secret_is_set() {
    let app = app().await;

    let (status, _) = send(&app, github_hook(None, GITHUB_BODY)).await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(stored_count(&app).await, 0);
}

#[tokio::test]
async fn accepts_stripe_hook_with_valid_signature() {
    let app = app().await;
    let body = r#"{"id":"evt_1"}"#;

    let (status, _) = send(
        &app,
        Request::post("/hooks/stripe")
            .header("stripe-signature", stripe_signature(body))
            .body(Body::from(body))
            .unwrap(),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED);
}

#[tokio::test]
async fn rejects_stripe_hook_signed_for_another_body() {
    let app = app().await;

    let (status, _) = send(
        &app,
        Request::post("/hooks/stripe")
            .header("stripe-signature", stripe_signature(r#"{"id":"evt_1"}"#))
            .body(Body::from(r#"{"id":"evt_2"}"#))
            .unwrap(),
    )
    .await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn stores_unsigned_hook_for_source_without_secret() {
    let app = app().await;

    let (status, _) = send(
        &app,
        Request::post("/hooks/other")
            .body(Body::from("{}"))
            .unwrap(),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED);
}
