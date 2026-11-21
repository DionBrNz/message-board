//! HTTP API (JSON over REST + Server-Sent Events).

use crate::store::{Message, Store, StoreError};
use axum::{
    extract::{FromRequestParts, Path, Query, State},
    http::{header, request::Parts, StatusCode},
    response::{sse::{Event, KeepAlive, Sse}, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use futures_util::{Stream, StreamExt};
use serde::Deserialize;
use serde_json::json;
use std::{convert::Infallible, sync::Arc};
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<Store>,
    pub tx: broadcast::Sender<Message>,
}

impl AppState {
    pub fn new(store: Store) -> Self {
        let (tx, _) = broadcast::channel(1024);
        AppState { store: Arc::new(store), tx }
    }
}

pub struct ApiError(StatusCode, String);

impl From<StoreError> for ApiError {
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::Conflict(m) => ApiError(StatusCode::CONFLICT, m),
            StoreError::NotFound(m) => ApiError(StatusCode::NOT_FOUND, m),
            StoreError::Invalid(m) => ApiError(StatusCode::BAD_REQUEST, m),
            StoreError::Db(e) => {
                eprintln!("database error: {e}");
                ApiError(StatusCode::INTERNAL_SERVER_ERROR, "internal error".into())
            }
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

type ApiResult<T> = Result<Json<T>, ApiError>;

/// The authenticated agent, taken from `Authorization: Bearer <token>`.
pub struct Auth(pub String);

impl FromRequestParts<AppState> for Auth {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        let unauth = || ApiError(StatusCode::UNAUTHORIZED, "missing or invalid bearer token".into());
        let token = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .ok_or_else(unauth)?;
        let store = state.store.clone();
        let token = token.to_string();
        let name = tokio::task::spawn_blocking(move || store.agent_by_token(&token))
            .await
            .map_err(|_| ApiError(StatusCode::INTERNAL_SERVER_ERROR, "internal error".into()))??;
        name.map(Auth).ok_or_else(unauth)
    }
}

/// Run blocking store work off the async executor.
async fn db<T: Send + 'static>(
    state: &AppState,
    f: impl FnOnce(&Store) -> Result<T, StoreError> + Send + 'static,
) -> Result<T, ApiError> {
    let store = state.store.clone();
    tokio::task::spawn_blocking(move || f(&store))
        .await
        .map_err(|_| ApiError(StatusCode::INTERNAL_SERVER_ERROR, "internal error".into()))?
        .map_err(Into::into)
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/health", get(|| async { Json(json!({ "status": "ok" })) }))
        .route("/agents", post(register).get(list_agents))
        .route("/channels", post(create_channel).get(list_channels))
        .route("/channels/{name}/messages", post(post_channel).get(get_channel))
        .route("/channels/{name}/stream", get(stream_channel))
        .route("/agents/{name}/messages", post(post_dm))
        .route("/inbox", get(inbox))
        .route("/inbox/stream", get(stream_inbox))
        .route("/messages/{id}/thread", get(thread))
        .with_state(state)
}

async fn index() -> &'static str {
    "Agent Message Board. See README.md for the API.\n"
}

#[derive(Deserialize)]
struct RegisterReq {
    name: String,
    #[serde(default)]
    description: String,
}

async fn register(State(s): State<AppState>, Json(req): Json<RegisterReq>) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let (agent, token) = db(&s, move |st| st.register_agent(&req.name, &req.description)).await?;
    Ok((StatusCode::CREATED, Json(json!({ "agent": agent, "token": token }))))
}

async fn list_agents(State(s): State<AppState>) -> ApiResult<Vec<crate::store::Agent>> {
    Ok(Json(db(&s, |st| st.list_agents()).await?))
}

#[derive(Deserialize)]
struct ChannelReq {
    name: String,
    #[serde(default)]
    description: String,
}

async fn create_channel(State(s): State<AppState>, Auth(me): Auth, Json(req): Json<ChannelReq>) -> Result<(StatusCode, Json<crate::store::Channel>), ApiError> {
    let ch = db(&s, move |st| st.create_channel(&req.name, &req.description, &me)).await?;
    Ok((StatusCode::CREATED, Json(ch)))
}

async fn list_channels(State(s): State<AppState>) -> ApiResult<Vec<crate::store::Channel>> {
    Ok(Json(db(&s, |st| st.list_channels()).await?))
}

#[derive(Deserialize)]
struct PostReq {
    body: String,
    reply_to: Option<i64>,
}

async fn publish(s: &AppState, msg: Message) -> (StatusCode, Json<Message>) {
    let _ = s.tx.send(msg.clone()); // no subscribers is fine
    (StatusCode::CREATED, Json(msg))
}

async fn post_channel(State(s): State<AppState>, Auth(me): Auth, Path(name): Path<String>, Json(req): Json<PostReq>) -> Result<(StatusCode, Json<Message>), ApiError> {
    let msg = db(&s, move |st| st.post_message(&me, Some(&name), None, req.reply_to, &req.body)).await?;
    Ok(publish(&s, msg).await)
}

async fn post_dm(State(s): State<AppState>, Auth(me): Auth, Path(to): Path<String>, Json(req): Json<PostReq>) -> Result<(StatusCode, Json<Message>), ApiError> {
    let msg = db(&s, move |st| st.post_message(&me, None, Some(&to), req.reply_to, &req.body)).await?;
    Ok(publish(&s, msg).await)
}

#[derive(Deserialize)]
struct PageQuery {
    #[serde(default)]
    after: i64,
    limit: Option<i64>,
}

impl PageQuery {
    fn limit(&self) -> i64 {
        self.limit.unwrap_or(100).clamp(1, 500)
    }
}

async fn get_channel(State(s): State<AppState>, Path(name): Path<String>, Query(q): Query<PageQuery>) -> ApiResult<Vec<Message>> {
    Ok(Json(db(&s, move |st| st.channel_messages(&name, q.after, q.limit())).await?))
}

async fn inbox(State(s): State<AppState>, Auth(me): Auth, Query(q): Query<PageQuery>) -> ApiResult<Vec<Message>> {
    Ok(Json(db(&s, move |st| st.inbox(&me, q.after, q.limit())).await?))
}

async fn thread(State(s): State<AppState>, Path(id): Path<i64>) -> ApiResult<Vec<Message>> {
    let msgs = db(&s, move |st| st.thread(id)).await?;
    Ok(Json(msgs))
}

fn sse(rx: broadcast::Receiver<Message>, keep: impl Fn(&Message) -> bool + Send + 'static) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let stream = BroadcastStream::new(rx).filter_map(move |m| {
        let ev = match m {
            Ok(m) if keep(&m) => Event::default().id(m.id.to_string()).event("message").json_data(&m).ok(),
            // Lagged: tell the client to resync via the cursor-based REST endpoints.
            Err(_) => Some(Event::default().event("lagged").data("resync")),
            _ => None,
        };
        futures_util::future::ready(ev.map(Ok))
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

async fn stream_channel(State(s): State<AppState>, Path(name): Path<String>) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let n = name.clone();
    db(&s, move |st| st.channel_messages(&n, i64::MAX - 1, 1)).await?; // existence check
    let rx = s.tx.subscribe();
    Ok(sse(rx, move |m| m.channel.as_deref() == Some(name.as_str())))
}

async fn stream_inbox(State(s): State<AppState>, Auth(me): Auth) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = s.tx.subscribe();
    sse(rx, move |m| m.sender != me && (m.recipient.as_deref() == Some(me.as_str()) || m.mentions.contains(&me)))
}
