//! HTTP surface: a single-page web UI plus a small JSON/SSE API.
//!
//! OpenClaw's control plane is a WebSocket protocol with three frame types and
//! a registry of several hundred methods, backed by a Lit+Vite SPA of 1,477
//! files. Here a POST to start a turn and an SSE stream to watch it do the same
//! job, and the UI is one embedded HTML file — no bundler, no Node, no assets
//! on disk.

use crate::agent::{Agent, AgentEvent};
use anyhow::{Context, Result};
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response, Sse, sse};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::stream::Stream;
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::sync::Arc;
use tokio::sync::{Mutex, broadcast};
use tokio_util::sync::CancellationToken;

const UI: &str = include_str!("ui.html");

#[derive(Clone)]
struct AppState {
    agent: Arc<Mutex<Agent>>,
    events: broadcast::Sender<AgentEvent>,
    /// Cancels the turn currently in flight, if any.
    cancel: Arc<Mutex<Option<CancellationToken>>>,
    /// Queues a message into the running turn without taking the agent lock.
    steer: tokio::sync::mpsc::UnboundedSender<String>,
}

pub async fn run(agent: Agent) -> Result<()> {
    let bind = {
        let c = agent.server_config();
        format!("{}:{}", c.bind, c.port)
    };
    let events = agent.subscribe_sender();
    let steer_tx = agent.steer_sender();
    let state = AppState {
        agent: Arc::new(Mutex::new(agent)),
        events,
        cancel: Arc::new(Mutex::new(None)),
        steer: steer_tx,
    };

    let app = Router::new()
        .route("/", get(index))
        .route("/chat", post(chat))
        .route("/events", get(events_stream))
        .route("/abort", post(abort))
        .route("/steer", post(steer))
        .route("/sessions", get(sessions))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .with_context(|| format!("binding {bind}"))?;
    println!("rustclaw: http://{bind}");
    axum::serve(listener, app).await.context("http server")?;
    Ok(())
}

async fn index() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], UI)
}

#[derive(Deserialize)]
struct ChatRequest {
    message: String,
}

#[derive(Serialize)]
struct ChatResponse {
    reply: String,
}

async fn chat(State(state): State<AppState>, Json(req): Json<ChatRequest>) -> Response {
    if req.message.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "message must not be empty").into_response();
    }

    // One turn at a time: a second concurrent turn would interleave into the
    // same transcript. `try_lock` makes that an explicit 409 rather than a
    // silent queue.
    let Ok(mut agent) = state.agent.try_lock() else {
        return (StatusCode::CONFLICT, "a turn is already running").into_response();
    };

    let cancel = CancellationToken::new();
    *state.cancel.lock().await = Some(cancel.clone());
    let result = agent.run_turn(req.message, cancel).await;
    *state.cancel.lock().await = None;

    match result {
        Ok(reply) => Json(ChatResponse { reply }).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
    }
}

/// Inject a message into the turn already running. `/chat` answers 409 while a
/// turn is in flight; this is how the user adds to it instead of waiting.
async fn steer(State(state): State<AppState>, Json(req): Json<ChatRequest>) -> Response {
    if req.message.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "message must not be empty").into_response();
    }
    // Deliberately does not take the agent lock: the running turn holds it.
    match state.steer.send(req.message) {
        Ok(()) => (StatusCode::ACCEPTED, "queued").into_response(),
        Err(_) => (StatusCode::GONE, "agent is gone").into_response(),
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

async fn sessions(State(state): State<AppState>) -> impl IntoResponse {
    let current = state.agent.lock().await.session.id.clone();
    let ids = crate::config::home_dir()
        .map(|r| crate::session::Session::list(&r))
        .unwrap_or_default();
    Json(serde_json::json!({"current": current, "sessions": ids}))
}

/// Live turn events. Every connected browser sees the same stream, and so does
/// the REPL if one is attached.
async fn events_stream(
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<sse::Event, Infallible>>> {
    let rx = state.events.subscribe();
    let stream = async_stream::stream(rx);
    Sse::new(stream).keep_alive(sse::KeepAlive::default())
}

/// Turn the broadcast receiver into an SSE stream.
mod async_stream {
    use super::*;
    use futures_util::stream::unfold;

    pub fn stream(
        rx: broadcast::Receiver<AgentEvent>,
    ) -> impl Stream<Item = Result<sse::Event, Infallible>> {
        unfold(rx, |mut rx| async move {
            loop {
                match rx.recv().await {
                    Ok(ev) => {
                        let (name, data) = encode(&ev);
                        return Some((Ok(sse::Event::default().event(name).data(data)), rx));
                    }
                    // A slow client that fell behind resyncs rather than
                    // dropping the connection.
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => return None,
                }
            }
        })
    }
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

/// SSE frames are newline-delimited, so every payload is JSON-encoded to keep
/// a multi-line delta from terminating the frame early.
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

    #[test]
    fn the_embedded_ui_is_a_complete_page() {
        assert!(UI.contains("<title>"));
        assert!(UI.contains("/events"), "the page must subscribe to the stream");
        assert!(UI.contains("/chat"));
    }
}
