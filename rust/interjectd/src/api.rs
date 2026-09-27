//! HTTP surface. Implements `docs/PROTOCOL.md`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rusqlite::Connection;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::broadcast;

use crate::store::{self, AnswerOutcome};
use crate::triage::{self, Decision, Policy};
use crate::types::{
    DigestResponse, Inbox, NewAnswer, NewHeartbeat, NewQuestion, Signals, Snapshot,
};

/// How long a single long-poll may hold a connection, unless overridden.
pub const DEFAULT_MAX_WAIT_SECS: u64 = 300;

#[derive(Clone)]
pub struct AppState {
    db: Arc<Mutex<Connection>>,
    /// Broadcasts the key of every question that has just been answered, so a
    /// waiting long-poll wakes immediately instead of rediscovering it on a
    /// timer. One channel for all keys keeps this free of map bookkeeping;
    /// waiters filter, and treat a lagged channel as "re-read the database".
    answered: broadcast::Sender<String>,
    max_wait: Duration,
    token: Option<String>,
}

impl AppState {
    pub fn new(conn: Connection, max_wait_secs: u64, token: Option<String>) -> Self {
        let (answered, _) = broadcast::channel(1024);
        Self {
            db: Arc::new(Mutex::new(conn)),
            answered,
            max_wait: Duration::from_secs(max_wait_secs),
            token,
        }
    }

    /// Run a blocking store operation off the async runtime.
    async fn with_db<T, F>(&self, f: F) -> Result<T, ApiError>
    where
        F: FnOnce(&Connection) -> anyhow::Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || {
            let conn = db.lock().expect("store mutex poisoned");
            f(&conn)
        })
        .await
        .map_err(|e| ApiError::internal(format!("store task failed: {e}")))?
        .map_err(|e| ApiError::internal(e.to_string()))
    }

    pub fn db(&self) -> Arc<Mutex<Connection>> {
        Arc::clone(&self.db)
    }

    pub fn notify_answered(&self, key: &str) {
        // An error here only means nobody is waiting, which is not a problem.
        let _ = self.answered.send(key.to_string());
    }
}

pub struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }
    fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", message)
    }
    fn unknown_key(key: &str) -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "unknown_key",
            format!("no question with key {key}"),
        )
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = json!({"error": {"code": self.code, "message": self.message}});
        (self.status, Json(body)).into_response()
    }
}

fn bearer(headers: &HeaderMap) -> Option<String> {
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::to_string)
}

fn header_project(headers: &HeaderMap) -> String {
    headers
        .get("x-interject-project")
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.is_empty())
        .unwrap_or("default")
        .to_string()
}

/// Resolve which project a request speaks for.
///
/// Three modes, in order of precedence:
///
/// 1. a token minted with `interjectd token create` — the project comes from the
///    token, so a caller cannot name someone else's project;
/// 2. the `--token` shared secret, which still selects a project by header;
/// 3. open mode, header only, for a local daemon with no tokens at all.
///
/// Once any minted token exists, open mode is off: adding the first token is what
/// turns a single-user daemon into a multi-tenant one (D4).
async fn project_of(state: &AppState, headers: &HeaderMap) -> Result<String, ApiError> {
    let presented = bearer(headers);

    if let Some(token) = presented.clone() {
        let resolved = state
            .with_db(move |conn| store::resolve_token(conn, &token))
            .await?;
        if let Some(project) = resolved {
            return Ok(project);
        }
    }

    if let Some(expected) = &state.token {
        if presented.as_deref() != Some(expected.as_str()) {
            return Err(ApiError::new(
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "missing or incorrect token",
            ));
        }
        return Ok(header_project(headers));
    }

    let tokens_exist = state.with_db(store_any_tokens).await?;
    if tokens_exist {
        return Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "this daemon requires a token; pair a device or create one with `interjectd token create`",
        ));
    }
    Ok(header_project(headers))
}

fn store_any_tokens(conn: &rusqlite::Connection) -> anyhow::Result<bool> {
    store::any_tokens(conn)
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(web_inbox))
        .route("/healthz", get(healthz))
        .route("/v0/pair", post(redeem_pairing))
        .route("/v0/questions", post(register_question))
        .route("/v0/questions/{key}", get(poll_question))
        .route("/v0/answers", post(submit_answer))
        .route("/v0/inbox", get(read_inbox))
        .route("/v0/signals", get(read_signals))
        .route("/v0/signals/heartbeat", post(submit_heartbeat))
        .route("/v0/digest", get(read_digest))
        .route("/v0/calibration", get(read_calibration))
        .route("/v0/policies", get(read_policies).put(write_policy))
        .with_state(state)
}

/// The web inbox: one file, no build step, phone-first.
///
/// Served by the daemon itself so that "answer from your phone" needs no hosting
/// and no separate front end deployment.
async fn web_inbox() -> Html<&'static str> {
    Html(include_str!("inbox.html"))
}

#[derive(Debug, Deserialize)]
pub struct PairRequest {
    pub code: String,
}

/// Exchange a short pairing code for a long-lived device token (D9).
///
/// Deliberately the one endpoint that needs no authentication — it *is* the
/// authentication. Safe because codes are single-use and expire in minutes.
async fn redeem_pairing(
    State(state): State<AppState>,
    Json(request): Json<PairRequest>,
) -> Result<Json<Value>, ApiError> {
    let outcome = state
        .with_db(move |conn| store::redeem_pairing(conn, &request.code))
        .await?;
    match outcome {
        store::PairOutcome::Paired { token, project } => {
            tracing::info!(%project, "a device paired");
            Ok(Json(json!({"token": token, "project": project})))
        }
        store::PairOutcome::UnknownOrExpired => Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "that pairing code is unknown, already used, or expired",
        )),
    }
}

async fn healthz(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let count = state
        .with_db(|conn| {
            Ok(conn.query_row(
                "SELECT COUNT(*) FROM questions WHERE state = 'open'",
                [],
                |row| row.get::<_, i64>(0),
            )?)
        })
        .await?;
    Ok(Json(json!({
        "status": "ok",
        "schema_version": store::SCHEMA_VERSION,
        "open_questions": count,
    })))
}

async fn register_question(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(question): Json<NewQuestion>,
) -> Result<Json<Snapshot>, ApiError> {
    let project = project_of(&state, &headers).await?;
    if question.key.len() != 64 || !question.key.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "key must be 64 lowercase hex characters",
        ));
    }

    let key = question.key.clone();
    let owned_project = project.clone();
    // Registration and triage happen in one hop so that a caller asking with
    // wait=0 gets an auto-answer immediately rather than on a later poll.
    let created = state
        .with_db(move |conn| {
            let created = store::upsert_question(conn, &owned_project, &question)?;
            if created {
                // Only on first registration: a replay must never re-decide.
                if let Decision::Auto { shadow } = triage::decide(conn, &owned_project, &question)?
                {
                    triage::apply_auto_answer(conn, &owned_project, &question, shadow)?;
                }
            }
            Ok(created)
        })
        .await?;

    let lookup_key = key.clone();
    let lookup_project = project.clone();
    let mut snapshot = state
        .with_db(move |conn| store::snapshot(conn, &lookup_project, &lookup_key))
        .await?
        .ok_or_else(|| ApiError::unknown_key(&key))?;
    snapshot.created = Some(created);
    Ok(Json(snapshot))
}

#[derive(Debug, Deserialize)]
pub struct PollParams {
    #[serde(default)]
    wait: Option<u64>,
}

/// Long-poll for an answer.
///
/// Subscribes to the answered channel *before* reading the database, so an
/// answer landing between the read and the wait cannot be missed.
async fn poll_question(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(key): Path<String>,
    Query(params): Query<PollParams>,
) -> Result<Json<Snapshot>, ApiError> {
    let project = project_of(&state, &headers).await?;
    let wait = Duration::from_secs(params.wait.unwrap_or(0)).min(state.max_wait);
    let deadline = tokio::time::Instant::now() + wait;

    let mut answered = state.answered.subscribe();
    loop {
        let (p, k) = (project.clone(), key.clone());
        let snapshot = state
            .with_db(move |conn| store::snapshot(conn, &p, &k))
            .await?
            .ok_or_else(|| ApiError::unknown_key(&key))?;

        if snapshot.state != "open" {
            return Ok(Json(snapshot));
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(Json(snapshot));
        }

        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => {}
            received = answered.recv() => {
                match received {
                    // Another key was answered; keep waiting without a re-read.
                    Ok(answered_key) if answered_key != key => continue,
                    // Ours, or the channel lagged: fall through and re-read.
                    _ => {}
                }
            }
        }
    }
}

async fn submit_answer(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(answer): Json<NewAnswer>,
) -> Result<Json<Snapshot>, ApiError> {
    let project = project_of(&state, &headers).await?;
    let key = answer.key.clone();
    let source = answer.source.clone().unwrap_or_else(|| "human".to_string());
    let outcome = state
        .with_db(move |conn| {
            store::put_answer(
                conn,
                &project,
                &answer.key,
                &answer.value,
                &source,
                answer.answered_by.as_deref(),
            )
        })
        .await?;

    match outcome {
        AnswerOutcome::Stored(snapshot) => {
            state.notify_answered(&key);
            Ok(Json(snapshot))
        }
        AnswerOutcome::UnknownKey => Err(ApiError::unknown_key(&key)),
        AnswerOutcome::AlreadyAnswered => Err(ApiError::new(
            StatusCode::CONFLICT,
            "already_answered",
            "that question already has an answer",
        )),
    }
}

#[derive(Debug, Deserialize)]
pub struct InboxParams {
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    batch_key: Option<String>,
}

async fn read_inbox(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<InboxParams>,
) -> Result<Json<Inbox>, ApiError> {
    let project = project_of(&state, &headers).await?;
    let limit = params.limit.unwrap_or(50).min(1000);
    let batch_key = params.batch_key.clone();
    let batches = state
        .with_db(move |conn| store::inbox(conn, &project, limit, batch_key.as_deref()))
        .await?;
    Ok(Json(Inbox { batches }))
}

async fn read_signals(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Signals>, ApiError> {
    let project = project_of(&state, &headers).await?;
    let signals = state
        .with_db(move |conn| store::signals(conn, &project))
        .await?;
    Ok(Json(Signals { signals }))
}

async fn read_digest(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<DigestResponse>, ApiError> {
    let project = project_of(&state, &headers).await?;
    let digest = state
        .with_db(move |conn| store::digest(conn, &project))
        .await?;
    Ok(Json(DigestResponse {
        open: digest.open,
        oldest_created_at: digest.oldest_created_at,
        silent_signals: digest.silent_signals,
    }))
}

async fn read_calibration(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let project = project_of(&state, &headers).await?;
    let classes = state
        .with_db(move |conn| triage::calibration_report(conn, &project))
        .await?;
    Ok(Json(json!({"classes": classes})))
}

async fn read_policies(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let project = project_of(&state, &headers).await?;
    let policies = state
        .with_db(move |conn| triage::policies(conn, &project))
        .await?;
    Ok(Json(json!({"policies": policies})))
}

async fn write_policy(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(policy): Json<Policy>,
) -> Result<Json<Policy>, ApiError> {
    let project = project_of(&state, &headers).await?;
    for (name, value) in [
        ("threshold", policy.threshold),
        ("agreement_target", policy.agreement_target),
        ("shadow_rate", policy.shadow_rate),
    ] {
        if !(0.0..=1.0).contains(&value) {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                format!("{name} must be between 0 and 1"),
            ));
        }
    }
    if policy.min_samples < 1 {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "min_samples must be at least 1",
        ));
    }
    let stored = policy.clone();
    state
        .with_db(move |conn| triage::save_policy(conn, &project, &stored))
        .await?;
    Ok(Json(policy))
}

async fn submit_heartbeat(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(beat): Json<NewHeartbeat>,
) -> Result<Json<Value>, ApiError> {
    let project = project_of(&state, &headers).await?;
    if beat.expect_every_seconds.is_some() == beat.expect_by.is_some() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "pass exactly one of expect_every_seconds or expect_by",
        ));
    }
    let name = beat.name.clone();
    state
        .with_db(move |conn| {
            store::heartbeat(
                conn,
                &project,
                &beat.name,
                beat.expect_every_seconds,
                beat.expect_by.as_deref(),
            )
        })
        .await?;
    Ok(Json(json!({"name": name, "state": "live"})))
}
