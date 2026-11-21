use axum::{body::Body, http::{Request, StatusCode}, Router};
use http_body_util::BodyExt;
use message_board::{api, store::Store};
use serde_json::{json, Value};
use tower::ServiceExt;

fn app() -> Router {
    api::router(api::AppState::new(Store::in_memory().unwrap()))
}

async fn call(app: &Router, method: &str, uri: &str, token: Option<&str>, body: Option<Value>) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(uri).header("content-type", "application/json");
    if let Some(t) = token {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    let req = req.body(body.map_or(Body::empty(), |b| Body::from(b.to_string()))).unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

async fn register(app: &Router, name: &str) -> String {
    let (s, v) = call(app, "POST", "/agents", None, Some(json!({"name": name, "description": "test"}))).await;
    assert_eq!(s, StatusCode::CREATED);
    v["token"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn register_and_auth() {
    let app = app();
    let _ = register(&app, "alice").await;
    let (s, _) = call(&app, "POST", "/agents", None, Some(json!({"name": "alice"}))).await;
    assert_eq!(s, StatusCode::CONFLICT);
    let (s, _) = call(&app, "POST", "/agents", None, Some(json!({"name": "bad name!"}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = call(&app, "POST", "/channels", None, Some(json!({"name": "general"}))).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _) = call(&app, "POST", "/channels", Some("nope"), Some(json!({"name": "general"}))).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn channels_threads_and_cursor() {
    let app = app();
    let a = register(&app, "alice").await;
    let b = register(&app, "bob").await;
    let (s, _) = call(&app, "POST", "/channels", Some(&a), Some(json!({"name": "general"}))).await;
    assert_eq!(s, StatusCode::CREATED);
    let (s, _) = call(&app, "POST", "/channels", Some(&a), Some(json!({"name": "other"}))).await;
    assert_eq!(s, StatusCode::CREATED);

    let (_, m1) = call(&app, "POST", "/channels/general/messages", Some(&a), Some(json!({"body": "hello"}))).await;
    let id1 = m1["id"].as_i64().unwrap();
    let (s, m2) = call(&app, "POST", "/channels/general/messages", Some(&b), Some(json!({"body": "hi!", "reply_to": id1}))).await;
    assert_eq!(s, StatusCode::CREATED);
    let id2 = m2["id"].as_i64().unwrap();

    // reply across channels is rejected
    let (s, _) = call(&app, "POST", "/channels/other/messages", Some(&b), Some(json!({"body": "x", "reply_to": id1}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    // missing channel
    let (s, _) = call(&app, "POST", "/channels/nope/messages", Some(&b), Some(json!({"body": "x"}))).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    // empty body
    let (s, _) = call(&app, "POST", "/channels/general/messages", Some(&b), Some(json!({"body": "  "}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    let (_, all) = call(&app, "GET", "/channels/general/messages", None, None).await;
    assert_eq!(all.as_array().unwrap().len(), 2);
    let (_, after) = call(&app, "GET", &format!("/channels/general/messages?after={id1}"), None, None).await;
    assert_eq!(after.as_array().unwrap().len(), 1);
    assert_eq!(after[0]["id"], id2);

    let (_, th) = call(&app, "GET", &format!("/messages/{id1}/thread"), None, None).await;
    assert_eq!(th.as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn dms_and_mentions_show_in_inbox() {
    let app = app();
    let a = register(&app, "alice").await;
    let b = register(&app, "bob").await;
    let c = register(&app, "carol").await;
    call(&app, "POST", "/channels", Some(&a), Some(json!({"name": "general"}))).await;

    call(&app, "POST", "/agents/bob/messages", Some(&a), Some(json!({"body": "secret"}))).await;
    let (_, m) = call(&app, "POST", "/channels/general/messages", Some(&a), Some(json!({"body": "ping @bob and @ghost, not me@bobby.com"}))).await;
    assert_eq!(m["mentions"], json!(["bob"]));
    let (s, _) = call(&app, "POST", "/agents/ghost/messages", Some(&a), Some(json!({"body": "x"}))).await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    let (_, inbox) = call(&app, "GET", "/inbox", Some(&b), None).await;
    assert_eq!(inbox.as_array().unwrap().len(), 2);
    let (_, inbox) = call(&app, "GET", "/inbox", Some(&c), None).await;
    assert!(inbox.as_array().unwrap().is_empty());
    let (_, inbox) = call(&app, "GET", "/inbox", Some(&a), None).await;
    assert!(inbox.as_array().unwrap().is_empty());
}
