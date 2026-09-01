//! End-to-end coverage of the loop against a stub OpenAI-compatible server.
//!
//! This exercises the path that unit tests cannot: real HTTP, real SSE framing
//! split across packets, tool-call assembly from deltas, tool execution, and
//! the second model call that follows a tool result.

#![cfg(test)]

use crate::agent::{Agent, AgentEvent};
use crate::config::{Backend, Config};
use crate::session::Session;
use crate::types::{Message, StopReason};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

/// A stub server that replies to each POST with the next canned SSE script.
struct StubModel {
    port: u16,
    /// Request bodies received, so a test can assert what was sent upstream.
    seen: Arc<Mutex<Vec<String>>>,
}

impl StubModel {
    async fn start(scripts: Vec<Vec<&'static str>>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen2 = seen.clone();

        tokio::spawn(async move {
            let mut scripts = scripts.into_iter();
            while let Ok((mut sock, _)) = listener.accept().await {
                let Some(script) = scripts.next() else { return };
                let seen = seen2.clone();
                tokio::spawn(async move {
                    // Read headers, then the body indicated by Content-Length.
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    let body_start = loop {
                        let n = match sock.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => n,
                        };
                        buf.extend_from_slice(&chunk[..n]);
                        if let Some(p) = find(&buf, b"\r\n\r\n") {
                            break p + 4;
                        }
                    };
                    let head = String::from_utf8_lossy(&buf[..body_start]).to_lowercase();
                    let len = head
                        .split("content-length:")
                        .nth(1)
                        .and_then(|s| s.split(['\r', '\n']).next())
                        .and_then(|s| s.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    while buf.len() < body_start + len {
                        let n = match sock.read(&mut chunk).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => n,
                        };
                        buf.extend_from_slice(&chunk[..n]);
                    }
                    seen.lock()
                        .await
                        .push(String::from_utf8_lossy(&buf[body_start..]).to_string());

                    let _ = sock
                        .write_all(
                            b"HTTP/1.1 200 OK\r\n\
                              Content-Type: text/event-stream\r\n\
                              Cache-Control: no-cache\r\n\
                              Connection: close\r\n\r\n",
                        )
                        .await;
                    // Write each frame separately so the client has to handle
                    // an SSE event arriving across several reads.
                    for frame in script {
                        if sock.write_all(frame.as_bytes()).await.is_err() {
                            return;
                        }
                        let _ = sock.flush().await;
                        tokio::task::yield_now().await;
                    }
                    let _ = sock.shutdown().await;
                });
            }
        });

        Self { port, seen }
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn frame(json: &str) -> String {
    format!("data: {json}\n\n")
}

fn home(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("rustclaw-e2e-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

async fn agent_for(tag: &str, port: u16) -> (Agent, PathBuf) {
    let root = home(tag);
    let mut cfg = Config::default();
    cfg.model.backend = Backend::OpenaiCompat;
    cfg.model.base_url = format!("http://127.0.0.1:{port}/v1");
    cfg.model.model = "stub".into();
    cfg.agent.system_prompt = "be brief".into();
    let session = Session::open(&root, "t1").unwrap();
    (Agent::new(cfg, session).unwrap(), root)
}

#[tokio::test]
async fn a_plain_reply_streams_through_and_lands_in_the_transcript() {
    let script = [
        frame(r#"{"choices":[{"delta":{"content":"Hola"}}]}"#),
        frame(r#"{"choices":[{"delta":{"content":", qué tal"}}]}"#),
        frame(r#"{"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":11,"completion_tokens":4}}"#),
        "data: [DONE]\n\n".to_string(),
    ];
    let stub = StubModel::start(vec![script.iter().map(|s| leak(s)).collect()]).await;
    let (mut agent, _root) = agent_for("plain", stub.port).await;

    let mut events = agent.subscribe();
    let reply = agent.run_turn("hola".into(), CancellationToken::new()).await.unwrap();

    assert_eq!(reply, "Hola, qué tal");

    // The transcript holds the user turn and the assistant turn, with usage.
    assert_eq!(agent.session.messages.len(), 2);
    match &agent.session.messages[1] {
        Message::Assistant { usage, stop_reason, .. } => {
            assert_eq!(usage.input, 11);
            assert_eq!(usage.output, 4);
            assert_eq!(*stop_reason, StopReason::Stop);
        }
        other => panic!("expected an assistant message, got {other:?}"),
    }

    // Deltas were streamed, not just delivered at the end.
    let mut deltas = Vec::new();
    while let Ok(ev) = events.try_recv() {
        if let AgentEvent::TextDelta(t) = ev {
            deltas.push(t);
        }
    }
    assert_eq!(deltas, vec!["Hola", ", qué tal"]);
}

#[tokio::test]
async fn a_tool_call_is_executed_and_its_result_feeds_the_next_model_call() {
    // Turn 1: the model asks to run a command, with the arguments split across
    // deltas the way a real server sends them.
    let call = [
        frame(r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"exec","arguments":""}}]}}]}"#),
        frame(r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"comm"}}]}}]}"#),
        frame(r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"and\": \"echo ping\"}"}}]}}]}"#),
        frame(r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#),
        "data: [DONE]\n\n".to_string(),
    ];
    // Turn 2: having seen the output, the model answers.
    let answer = [
        frame(r#"{"choices":[{"delta":{"content":"it said ping"}}]}"#),
        frame(r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#),
        "data: [DONE]\n\n".to_string(),
    ];
    let stub = StubModel::start(vec![
        call.iter().map(|s| leak(s)).collect(),
        answer.iter().map(|s| leak(s)).collect(),
    ])
    .await;
    let (mut agent, _root) = agent_for("tool", stub.port).await;

    let reply = agent.run_turn("run echo ping".into(), CancellationToken::new()).await.unwrap();
    assert_eq!(reply, "it said ping");

    // user, assistant(tool_call), toolResult, assistant(answer)
    assert_eq!(agent.session.messages.len(), 4, "{:#?}", agent.session.messages);
    match &agent.session.messages[2] {
        Message::ToolResult { tool_name, content, is_error, .. } => {
            assert_eq!(tool_name, "exec");
            assert!(!is_error, "echo should succeed: {content}");
            assert!(content.contains("ping"), "tool output not captured: {content}");
        }
        other => panic!("expected a tool result, got {other:?}"),
    }

    // The second request must carry the tool result back to the model,
    // otherwise the model is answering blind.
    let seen = stub.seen.lock().await;
    assert_eq!(seen.len(), 2, "expected two model calls");
    assert!(seen[1].contains("\"role\":\"tool\""), "tool result was not replayed upstream");
    assert!(seen[1].contains("ping"));
}

#[tokio::test]
async fn a_failing_tool_is_reported_as_an_error_result_and_the_turn_continues() {
    let call = [
        frame(r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"read","arguments":"{\"path\":\"/definitely/not/here\"}"}}]}}]}"#),
        frame(r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#),
        "data: [DONE]\n\n".to_string(),
    ];
    let answer = [
        frame(r#"{"choices":[{"delta":{"content":"that file is missing"}}]}"#),
        frame(r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#),
        "data: [DONE]\n\n".to_string(),
    ];
    let stub = StubModel::start(vec![
        call.iter().map(|s| leak(s)).collect(),
        answer.iter().map(|s| leak(s)).collect(),
    ])
    .await;
    let (mut agent, _root) = agent_for("toolerr", stub.port).await;

    let reply = agent.run_turn("read it".into(), CancellationToken::new()).await.unwrap();
    assert_eq!(reply, "that file is missing");
    match &agent.session.messages[2] {
        Message::ToolResult { is_error, .. } => {
            assert!(*is_error, "a failed read must be flagged, not passed off as success")
        }
        other => panic!("expected a tool result, got {other:?}"),
    }
}

#[tokio::test]
async fn an_unknown_tool_is_reported_back_to_the_model_rather_than_crashing() {
    let call = [
        frame(r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"teleport","arguments":"{}"}}]}}]}"#),
        frame(r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#),
        "data: [DONE]\n\n".to_string(),
    ];
    let answer = [
        frame(r#"{"choices":[{"delta":{"content":"no such tool"}}]}"#),
        frame(r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#),
        "data: [DONE]\n\n".to_string(),
    ];
    let stub = StubModel::start(vec![
        call.iter().map(|s| leak(s)).collect(),
        answer.iter().map(|s| leak(s)).collect(),
    ])
    .await;
    let (mut agent, _root) = agent_for("unknown", stub.port).await;

    agent.run_turn("teleport me".into(), CancellationToken::new()).await.unwrap();
    match &agent.session.messages[2] {
        Message::ToolResult { content, is_error, .. } => {
            assert!(*is_error);
            assert!(content.contains("unknown tool"), "{content}");
            // The message should tell the model what it *can* call.
            assert!(content.contains("exec"), "{content}");
        }
        other => panic!("expected a tool result, got {other:?}"),
    }
}

#[tokio::test]
async fn an_http_error_from_the_provider_ends_the_turn_without_losing_the_session() {
    // No scripts: the listener accepts and drops, so the request fails.
    let stub = StubModel::start(vec![]).await;
    let (mut agent, root) = agent_for("httperr", stub.port).await;

    let reply = agent.run_turn("hola".into(), CancellationToken::new()).await.unwrap();
    assert!(!reply.is_empty(), "the failure should be reported, not silently empty");

    // The user turn and an error assistant turn are both on disk.
    let reopened = Session::open(&root, "t1").unwrap();
    assert_eq!(reopened.messages.len(), 2);
    match &reopened.messages[1] {
        Message::Assistant { stop_reason, error, .. } => {
            assert_eq!(*stop_reason, StopReason::Error);
            assert!(error.is_some());
        }
        other => panic!("expected an error assistant message, got {other:?}"),
    }
}

#[tokio::test]
async fn a_cancelled_turn_stops_before_running_the_tool() {
    let call = [
        frame(r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"exec","arguments":"{\"command\":\"echo should-not-run\"}"}}]}}]}"#),
        frame(r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#),
        "data: [DONE]\n\n".to_string(),
    ];
    let stub = StubModel::start(vec![call.iter().map(|s| leak(s)).collect()]).await;
    let (mut agent, _root) = agent_for("cancel", stub.port).await;

    let cancel = CancellationToken::new();
    // Cancel up front: the model call still completes, but the tool must not run.
    cancel.cancel();
    agent.run_turn("go".into(), cancel).await.unwrap();

    let ran_tool = agent
        .session
        .messages
        .iter()
        .any(|m| matches!(m, Message::ToolResult { content, .. } if content.contains("should-not-run")));
    assert!(!ran_tool, "a cancelled turn must not execute tools");
}

/// The stub server needs `&'static str` frames; the scripts are built per test
/// and live for the process, which is fine in a test binary.
fn leak(s: &str) -> &'static str {
    Box::leak(s.to_string().into_boxed_str())
}
