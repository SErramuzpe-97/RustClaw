//! HTTP surface: a Preact single-page UI plus a JSON/SSE API.
//!
//! OpenClaw's control plane is a WebSocket protocol with three frame types and
//! a registry of several hundred methods, backed by a Lit+Vite SPA of 1,477
//! files. Here a POST starts a turn, an SSE stream reports its progress, and
//! the UI is a handful of files embedded in the binary — no bundler, no Node,
//! nothing to install on the device.

mod assets;
mod auth;
mod dto;

pub use auth::generate as generate_token;

use crate::agent::{Agent, AgentEvent};
use crate::session::Session;
use anyhow::{Context, Result};
use axum::extract::{Path as AxPath, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response, Sse, sse};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use futures_util::stream::Stream;
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{Mutex, broadcast};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
struct AppState {
    agent: Arc<Mutex<Agent>>,
    events: broadcast::Sender<AgentEvent>,
    /// Cancels the turn currently in flight, if any.
    cancel: Arc<Mutex<Option<CancellationToken>>>,
    /// Queues a message into the running turn without taking the agent lock.
    steer: tokio::sync::mpsc::UnboundedSender<String>,
    home: PathBuf,
}

pub async fn run(agent: Agent) -> Result<()> {
    let (host, port) = {
        let c = agent.server_config();
        (c.bind.clone(), c.port)
    };
    let bind = format!("{host}:{port}");

    // Reaching this server means running shell commands on this machine, so
    // listening anywhere a stranger could reach requires a token. Refusing to
    // start is the only safe default: a warning would be ignored.
    let token = std::env::var("RUSTCLAW_TOKEN").ok().filter(|t| !t.is_empty());
    if !auth::is_loopback(&host) && token.is_none() {
        anyhow::bail!(
            "refusing to serve on {host}: it is reachable from other machines and \
             RUSTCLAW_TOKEN is not set.\n\n  Generate one with:  rustclaw token\n\n\
             Then reconnect, or set server.bind = \"127.0.0.1\" to stay local-only."
        );
    }
    let guard = auth::Auth::new(token.clone());
    let home = agent.home().to_path_buf();
    let events = agent.subscribe_sender();
    let steer = agent.steer_sender();
    let state = AppState {
        agent: Arc::new(Mutex::new(agent)),
        events,
        cancel: Arc::new(Mutex::new(None)),
        steer,
        home,
    };

    let app = Router::new()
        .route("/", get(assets::index))
        .route("/assets/{*path}", get(assets::serve))
        .route("/api/chat", post(chat))
        .route("/api/events", get(events_stream))
        .route("/api/abort", post(abort))
        .route("/api/steer", post(steer_turn))
        .route("/api/regenerate", post(regenerate))
        .route("/api/state", get(current_state))
        .route("/api/sessions", get(list_sessions).post(create_session))
        .route("/api/sessions/{id}", get(read_session).patch(rename_session))
        .route("/api/sessions/{id}", delete(remove_session))
        .route("/api/sessions/{id}/select", post(select_session))
        .route("/api/sessions/{id}/export", get(export_session))
        .layer(axum::middleware::from_fn_with_state(guard.clone(), auth::guard))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .with_context(|| format!("binding {bind}"))?;
    if guard.required() {
        let t = token.as_deref().unwrap_or_default();
        println!("rustclaw: http://{bind}/?token={t}");
        println!("rustclaw: token required — that link sets a cookie, then bookmark it");
    } else {
        println!("rustclaw: http://{bind}");
    }
    axum::serve(listener, app).await.context("http server")?;
    Ok(())
}

// --- turns -----------------------------------------------------------------

#[derive(Deserialize)]
struct ChatRequest {
    message: String,
}

#[derive(Serialize)]
struct ChatResponse {
    reply: String,
}

/// Take the agent for the duration of a turn.
///
/// One turn at a time: a second concurrent turn would interleave into the same
/// transcript. `try_lock` makes that an explicit 409 rather than a silent queue,
/// and every endpoint that mutates the agent goes through it.
macro_rules! lock_agent {
    ($state:expr) => {
        match $state.agent.try_lock() {
            Ok(a) => a,
            Err(_) => {
                return (StatusCode::CONFLICT, "a turn is already running").into_response();
            }
        }
    };
}

async fn chat(State(state): State<AppState>, Json(req): Json<ChatRequest>) -> Response {
    if req.message.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "message must not be empty").into_response();
    }
    let mut agent = lock_agent!(state);

    let cancel = CancellationToken::new();
    *state.cancel.lock().await = Some(cancel.clone());
    let result = agent.run_turn(req.message, cancel).await;
    *state.cancel.lock().await = None;

    match result {
        Ok(reply) => Json(ChatResponse { reply }).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
    }
}

async fn regenerate(State(state): State<AppState>) -> Response {
    let mut agent = lock_agent!(state);

    let cancel = CancellationToken::new();
    *state.cancel.lock().await = Some(cancel.clone());
    let result = agent.regenerate(cancel).await;
    *state.cancel.lock().await = None;

    match result {
        Ok(Some(reply)) => Json(ChatResponse { reply }).into_response(),
        Ok(None) => (StatusCode::BAD_REQUEST, "nothing to regenerate").into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
    }
}

async fn abort(State(state): State<AppState>) -> impl IntoResponse {
    match state.cancel.lock().await.as_ref() {
        Some(c) => {
            c.cancel();
            (StatusCode::OK, "cancelled")
        }
        None => (StatusCode::OK, "nothing running"),
    }
}

/// Inject a message into the turn already running. `/api/chat` answers 409 while
/// a turn is in flight; this is how the user adds to it instead of waiting.
async fn steer_turn(State(state): State<AppState>, Json(req): Json<ChatRequest>) -> Response {
    if req.message.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "message must not be empty").into_response();
    }
    // Deliberately does not take the agent lock: the running turn holds it.
    match state.steer.send(req.message) {
        Ok(()) => (StatusCode::ACCEPTED, "queued").into_response(),
        Err(_) => (StatusCode::GONE, "agent is gone").into_response(),
    }
}

// --- sessions --------------------------------------------------------------

async fn current_state(State(state): State<AppState>) -> Response {
    let agent = state.agent.lock().await;
    Json(serde_json::json!({
        "sessionId": agent.session.id,
        "model": agent.model_name(),
        "version": env!("CARGO_PKG_VERSION"),
    }))
    .into_response()
}

async fn list_sessions(State(state): State<AppState>) -> Response {
    let active = state.agent.lock().await.session.id.clone();
    Json(serde_json::json!({
        "active": active,
        "sessions": Session::list(&state.home),
    }))
    .into_response()
}

async fn create_session(State(state): State<AppState>) -> Response {
    let mut agent = lock_agent!(state);
    let id = Session::new_id();
    match Session::open(&state.home, &id) {
        Ok(s) => {
            agent.switch_session(s);
            Json(serde_json::json!({"id": id})).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
    }
}

/// The transcript of any session, flattened for rendering. Reads from disk so a
/// conversation can be previewed without making it active.
async fn read_session(State(state): State<AppState>, AxPath(id): AxPath<String>) -> Response {
    if !Session::exists(&state.home, &id) {
        return (StatusCode::NOT_FOUND, "no such session").into_response();
    }
    match Session::open(&state.home, &id) {
        Ok(s) => Json(serde_json::json!({
            "id": s.id,
            "title": s.display_title(),
            "messages": dto::transcript(&s.messages),
        }))
        .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
    }
}

async fn select_session(State(state): State<AppState>, AxPath(id): AxPath<String>) -> Response {
    if !Session::exists(&state.home, &id) {
        return (StatusCode::NOT_FOUND, "no such session").into_response();
    }
    // Switching mid-turn would leave the running turn writing into a transcript
    // the user is no longer looking at, so it waits for the lock like a turn.
    let mut agent = lock_agent!(state);
    match Session::open(&state.home, &id) {
        Ok(s) => {
            agent.switch_session(s);
            Json(serde_json::json!({"id": id})).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
    }
}

/// The conversation as a Markdown document, offered as a download.
async fn export_session(State(state): State<AppState>, AxPath(id): AxPath<String>) -> Response {
    if !Session::exists(&state.home, &id) {
        return (StatusCode::NOT_FOUND, "no such session").into_response();
    }
    match Session::open(&state.home, &id) {
        Ok(s) => {
            let title = s.display_title();
            let body = dto::to_markdown(&title, &s.messages);
            (
                [
                    (header::CONTENT_TYPE, "text/markdown; charset=utf-8".to_string()),
                    // The filename is derived from the title, so it is sanitized
                    // before it reaches a header the browser will write to disk.
                    (
                        header::CONTENT_DISPOSITION,
                        format!("attachment; filename=\"{}\"", dto::export_filename(&title)),
                    ),
                ],
                body,
            )
                .into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
    }
}

#[derive(Deserialize)]
struct RenameRequest {
    title: String,
}

async fn rename_session(
    State(state): State<AppState>,
    AxPath(id): AxPath<String>,
    Json(req): Json<RenameRequest>,
) -> Response {
    if req.title.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "title must not be empty").into_response();
    }
    if !Session::exists(&state.home, &id) {
        return (StatusCode::NOT_FOUND, "no such session").into_response();
    }
    let mut agent = lock_agent!(state);

    // Renaming appends to the transcript, so it must go through whichever
    // handle owns the file: the live one when it is the active session.
    let meta = if agent.session.id == id {
        if let Err(e) = agent.session.set_title(&req.title) {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response();
        }
        agent.session.meta()
    } else {
        match Session::open(&state.home, &id) {
            Ok(mut s) => {
                if let Err(e) = s.set_title(&req.title) {
                    return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response();
                }
                s.meta()
            }
            Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
        }
    };
    Session::touch_index(&state.home, meta);
    Json(serde_json::json!({"id": id, "title": req.title})).into_response()
}

async fn remove_session(State(state): State<AppState>, AxPath(id): AxPath<String>) -> Response {
    let mut agent = lock_agent!(state);

    if let Err(e) = Session::delete(&state.home, &id) {
        return (StatusCode::NOT_FOUND, format!("{e:#}")).into_response();
    }

    // Deleting the conversation on screen has to leave the agent somewhere
    // valid, so it lands in a fresh one rather than a dangling handle.
    let mut active = agent.session.id.clone();
    if active == id {
        let fresh = Session::new_id();
        match Session::open(&state.home, &fresh) {
            Ok(s) => {
                agent.switch_session(s);
                active = fresh;
            }
            Err(e) => {
                return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response();
            }
        }
    }
    Json(serde_json::json!({"deleted": id, "active": active})).into_response()
}

// --- events ----------------------------------------------------------------

/// Live turn events. Every connected browser sees the same stream, and so does
/// the REPL if one is attached.
async fn events_stream(
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<sse::Event, Infallible>>> {
    let rx = state.events.subscribe();
    Sse::new(event_stream(rx)).keep_alive(sse::KeepAlive::default())
}

fn event_stream(
    rx: broadcast::Receiver<AgentEvent>,
) -> impl Stream<Item = Result<sse::Event, Infallible>> {
    futures_util::stream::unfold(rx, |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(ev) => {
                    let (name, data) = encode(&ev);
                    return Some((Ok(sse::Event::default().event(name).data(data)), rx));
                }
                // A slow client that fell behind resyncs rather than dropping
                // the connection.
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    })
}

fn encode(ev: &AgentEvent) -> (&'static str, String) {
    match ev {
        AgentEvent::TurnStart => ("turn_start", "{}".into()),
        AgentEvent::TextDelta(t) => ("delta", json_str(t)),
        AgentEvent::ThinkingDelta(t) => ("thinking", json_str(t)),
        AgentEvent::ToolStart { name, input } => (
            "tool_start",
            serde_json::json!({"name": name, "input": input}).to_string(),
        ),
        AgentEvent::ToolEnd { name, is_error, preview } => (
            "tool_end",
            serde_json::json!({"name": name, "isError": is_error, "preview": preview})
                .to_string(),
        ),
        AgentEvent::Reply(t) => ("reply", json_str(t)),
        AgentEvent::Compacted { dropped } => (
            "compacted",
            serde_json::json!({"dropped": dropped}).to_string(),
        ),
        AgentEvent::TurnEnd { usage } => (
            "turn_end",
            serde_json::json!({
                "input": usage.input, "output": usage.output,
                "cacheRead": usage.cache_read
            })
            .to_string(),
        ),
        AgentEvent::Error(e) => ("error", json_str(e)),
    }
}

/// SSE frames are newline-delimited, so every payload is JSON-encoded to keep a
/// multi-line delta from terminating the frame early.
fn json_str(s: &str) -> String {
    serde_json::Value::String(s.to_string()).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Usage;

    #[test]
    fn a_multiline_delta_stays_inside_one_sse_frame() {
        let (name, data) = encode(&AgentEvent::TextDelta("line one\nline two".into()));
        assert_eq!(name, "delta");
        assert!(!data.contains('\n'), "a raw newline would end the SSE frame early");
        assert_eq!(serde_json::from_str::<String>(&data).unwrap(), "line one\nline two");
    }

    #[test]
    fn tool_events_carry_their_error_flag() {
        let (name, data) = encode(&AgentEvent::ToolEnd {
            name: "exec".into(),
            is_error: true,
            preview: "boom".into(),
        });
        assert_eq!(name, "tool_end");
        let v: serde_json::Value = serde_json::from_str(&data).unwrap();
        assert_eq!(v["isError"], true);
    }

    #[test]
    fn turn_end_reports_usage() {
        let (_, data) = encode(&AgentEvent::TurnEnd {
            usage: Usage { input: 7, output: 3, cache_read: 1, cache_write: 0 },
        });
        let v: serde_json::Value = serde_json::from_str(&data).unwrap();
        assert_eq!(v["input"], 7);
        assert_eq!(v["cacheRead"], 1);
    }
}
