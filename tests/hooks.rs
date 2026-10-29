use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use hookrelay::{AppState, config::Config};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

async fn app() -> Router {
    let pool = hookrelay::db::connect("sqlite::memory:").await.unwrap();
    hookrelay::router(AppState::new(pool, Config::default()))
}

async fn send(app: &Router, req: Request<Body>) -> (StatusCode, Value) {
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

#[tokio::test]
async fn stores_and_returns_a_hook() {
    let app = app().await;

    let (status, created) = send(
        &app,
        Request::post("/hooks/github")
            .header("x-github-event", "push")
            .body(Body::from(r#"{"ref":"main"}"#))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let id = created["id"].as_i64().unwrap();

    let (status, hook) = send(
        &app,
        Request::get(format!("/hooks/id/{id}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(hook["source"], "github");
    assert_eq!(hook["headers"]["x-github-event"], "push");
    assert_eq!(hook["body"], r#"{"ref":"main"}"#);
}

#[tokio::test]
async fn lists_hooks_filtered_by_source_newest_first() {
    let app = app().await;
    for source in ["github", "stripe", "github"] {
        send(
            &app,
            Request::post(format!("/hooks/{source}"))
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await;
    }

    let (status, hooks) = send(
        &app,
        Request::get("/hooks?source=github")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let hooks = hooks.as_array().unwrap();
    assert_eq!(hooks.len(), 2);
    assert!(hooks[0]["id"].as_i64() > hooks[1]["id"].as_i64());
}

#[tokio::test]
async fn rejects_invalid_source_and_unknown_id() {
    let app = app().await;

    let (status, _) = send(
        &app,
        Request::post("/hooks/Not-Valid")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, _) = send(
        &app,
        Request::get("/hooks/id/999").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
