use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
};
use hookrelay::{AppState, config::Config};
use tower::ServiceExt;

const TOKEN: &str = "t0ken";

async fn app() -> Router {
    let pool = hookrelay::db::connect("sqlite::memory:").await.unwrap();
    let config = Config::default()
        .with_admin_token(TOKEN)
        .with_target("github", "http://127.0.0.1:9/in".parse().unwrap());
    hookrelay::router(AppState::new(pool, config))
}

async fn status(app: &Router, method: &str, uri: &str, auth: Option<&str>) -> StatusCode {
    let mut req = Request::builder().method(method).uri(uri);
    if let Some(auth) = auth {
        req = req.header(header::AUTHORIZATION, auth);
    }
    let res = app
        .clone()
        .oneshot(req.body(Body::from("{}")).unwrap())
        .await
        .unwrap();
    res.status()
}

const ADMIN_ROUTES: [(&str, &str); 4] = [
    ("GET", "/hooks"),
    ("GET", "/hooks/id/1"),
    ("POST", "/hooks/id/1/replay"),
    ("GET", "/deliveries"),
];

#[tokio::test]
async fn admin_routes_require_the_token() {
    let app = app().await;
    assert_eq!(
        status(&app, "POST", "/hooks/github", None).await,
        StatusCode::CREATED
    );

    for (method, uri) in ADMIN_ROUTES {
        for auth in [
            None,
            Some("Bearer wrong"),
            Some("Basic dDBrZW4="),
            Some(TOKEN),
        ] {
            assert_eq!(
                status(&app, method, uri, auth).await,
                StatusCode::UNAUTHORIZED,
                "{method} {uri} with {auth:?}"
            );
        }
        let ok = status(&app, method, uri, Some(&format!("Bearer {TOKEN}"))).await;
        assert!(ok.is_success(), "{method} {uri}: {ok}");
    }
}

#[tokio::test]
async fn unauthorized_responses_ask_for_a_bearer_token() {
    let app = app().await;
    let res = app
        .oneshot(Request::get("/hooks").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(res.headers()[header::WWW_AUTHENTICATE], "Bearer");
}

#[tokio::test]
async fn health_and_receiving_stay_open() {
    let app = app().await;
    assert_eq!(status(&app, "GET", "/health", None).await, StatusCode::OK);
    assert_eq!(
        status(&app, "POST", "/hooks/stripe", None).await,
        StatusCode::CREATED
    );
}
