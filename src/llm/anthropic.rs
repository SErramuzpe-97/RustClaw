//! Anthropic Messages API with SSE streaming.
//!
//! Follows the shape IronClaw uses in `anthropic_oauth.rs`: POST /v1/messages
//! with `stream: true`, then match on `content_block_delta` events.

use super::{ApiError, EventSink, Request, ToolSchema};
use crate::config::ModelConfig;
use crate::types::{ContentBlock, Message, StopReason, StreamEvent, Usage};
use anyhow::{Context, Result, bail};
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::time::Duration;

const API_VERSION: &str = "2023-06-01";

pub struct Client {
    http: reqwest::Client,
    base_url: String,
    pub model: String,
    api_key: String,
}

impl Client {
    pub fn new(http: reqwest::Client, cfg: &ModelConfig) -> Result<Self> {
        let api_key = cfg.api_key().with_context(|| {
            format!(
                "the anthropic backend needs an API key; set {} (config: model.api_key_env)",
                if cfg.api_key_env.is_empty() { "ANTHROPIC_API_KEY" } else { &cfg.api_key_env }
            )
        })?;
        let base_url = if cfg.base_url.is_empty() || cfg.base_url.contains("11434") {
            // The default base_url points at a local ollama; it is meaningless
            // for this backend.
            "https://api.anthropic.com/v1".to_string()
        } else {
            cfg.base_url.trim_end_matches('/').to_string()
        };
        Ok(Self { http, base_url, model: cfg.model.clone(), api_key })
    }

    pub async fn stream(&self, req: &Request<'_>, sink: EventSink<'_>) -> Result<()> {
        let resp = self
            .http
            .post(format!("{}/messages", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", API_VERSION)
            .header("accept", "text/event-stream")
            .json(&self.build_body(req))
            .send()
            .await
            .context("POST /v1/messages")?;

        let status = resp.status();
        if !status.is_success() {
            let retry_after = parse_retry_after(resp.headers());
            let message = resp.text().await.unwrap_or_default();
            bail!(ApiError {
                status: Some(status.as_u16()),
                message: format!("{status}: {message}"),
                retry_after,
            });
        }

        self.consume(resp, sink).await
    }

    fn build_body(&self, req: &Request<'_>) -> Value {
        let mut body = json!({
            "model": self.model,
            "max_tokens": req.max_tokens,
            "stream": true,
            // Anthropic takes the system prompt as a top-level field rather
            // than a message.
            "system": req.system,
            "messages": encode_messages(req.messages),
        });
        // Sampling parameters were removed from Opus 5, Sonnet 5 and the 4.6+
        // family: sending `temperature` at all returns a 400. Only forward it
        // when the operator explicitly asked for one (older models, or a
        // proxy that still accepts it).
        if let Some(t) = req.temperature {
            body["temperature"] = json!(t);
        }
        if !req.tools.is_empty() {
            body["tools"] = Value::Array(req.tools.iter().map(encode_tool).collect());
        }
        body
    }

    async fn consume(&self, resp: reqwest::Response, sink: EventSink<'_>) -> Result<()> {
        let mut stream = resp.bytes_stream().eventsource();
        let mut stop = StopReason::Stop;
        let mut tool_open = false;

        while let Some(event) = stream.next().await {
            let event = event.context("reading SSE stream")?;
            let data: Value = match serde_json::from_str(&event.data) {
                Ok(v) => v,
                Err(_) => continue,
            };

            match event.event.as_str() {
                "message_start" => {
                    if let Some(u) = data.get("message").and_then(|m| m.get("usage")) {
                        sink(StreamEvent::Usage(decode_usage(u)));
                    }
                }
                "content_block_start" => {
                    let block = data.get("content_block");
                    if block.and_then(|b| b.get("type")).and_then(Value::as_str) == Some("tool_use")
                    {
                        let b = block.unwrap();
                        sink(StreamEvent::ToolCallStart {
                            id: b.get("id").and_then(Value::as_str).unwrap_or_default().to_string(),
                            name: b
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                        });
                        tool_open = true;
                    }
                }
                "content_block_delta" => {
                    let Some(delta) = data.get("delta") else { continue };
                    match delta.get("type").and_then(Value::as_str) {
                        Some("text_delta") => {
                            if let Some(t) = delta.get("text").and_then(Value::as_str) {
                                sink(StreamEvent::TextDelta(t.to_string()));
                            }
                        }
                        Some("thinking_delta") => {
                            if let Some(t) = delta.get("thinking").and_then(Value::as_str) {
                                sink(StreamEvent::ThinkingDelta(t.to_string()));
                            }
                        }
                        Some("input_json_delta") => {
                            if let Some(t) = delta.get("partial_json").and_then(Value::as_str) {
                                sink(StreamEvent::ToolCallDelta(t.to_string()));
                            }
                        }
                        _ => {}
                    }
                }
                "content_block_stop" => {
                    if tool_open {
                        sink(StreamEvent::ToolCallEnd);
                        tool_open = false;
                    }
                }
                "message_delta" => {
                    if let Some(r) =
                        data.get("delta").and_then(|d| d.get("stop_reason")).and_then(Value::as_str)
                    {
                        stop = decode_stop(r);
                    }
                    // The output token count only appears in this event.
                    if let Some(u) = data.get("usage") {
                        sink(StreamEvent::Usage(decode_usage(u)));
                    }
                }
                "error" => {
                    bail!(ApiError {
                        status: None,
                        message: data
                            .get("error")
                            .and_then(|e| e.get("message"))
                            .and_then(Value::as_str)
                            .unwrap_or("anthropic stream error")
                            .to_string(),
                        retry_after: None,
                    });
                }
                _ => {}
            }
        }

        if tool_open {
            sink(StreamEvent::ToolCallEnd);
        }
        sink(StreamEvent::Done(stop));
        Ok(())
    }
}

fn encode_tool(t: &ToolSchema) -> Value {
    json!({"name": t.name, "description": t.description, "input_schema": t.input_schema})
}

/// Anthropic requires tool results to be `user` messages carrying
/// `tool_result` blocks, and consecutive results must be merged into one
/// message. That reshaping is the whole reason this is not a simple map.
fn encode_messages(messages: &[Message]) -> Value {
    let mut out: Vec<Value> = Vec::with_capacity(messages.len());

    for m in messages {
        match m {
            Message::User { content } => {
                out.push(json!({"role": "user", "content": [{"type": "text", "text": content}]}));
            }
            Message::Assistant { content, .. } => {
                let blocks: Vec<Value> = content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text { text } if !text.is_empty() => {
                            Some(json!({"type": "text", "text": text}))
                        }
                        ContentBlock::Text { .. } => None,
                        // A thinking block without its signature is rejected on
                        // replay, so drop those rather than send them.
                        ContentBlock::Thinking { text, signature } => signature
                            .as_ref()
                            .map(|s| json!({"type": "thinking", "thinking": text, "signature": s})),
                        ContentBlock::ToolCall { id, name, input } => {
                            Some(json!({"type": "tool_use", "id": id, "name": name, "input": input}))
                        }
                    })
                    .collect();
                if !blocks.is_empty() {
                    out.push(json!({"role": "assistant", "content": blocks}));
                }
            }
            Message::ToolResult { tool_call_id, content, is_error, .. } => {
                let block = json!({
                    "type": "tool_result",
                    "tool_use_id": tool_call_id,
                    "content": content,
                    "is_error": is_error,
                });
                // Append to the previous user message when it is already a
                // run of tool results.
                match out.last_mut() {
                    Some(prev)
                        if prev["role"] == "user"
                            && prev["content"][0]["type"] == "tool_result" =>
                    {
                        prev["content"].as_array_mut().expect("content is an array").push(block);
                    }
                    _ => out.push(json!({"role": "user", "content": [block]})),
                }
            }
        }
    }

    Value::Array(out)
}

fn decode_usage(u: &Value) -> Usage {
    let get = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0) as u32;
    Usage {
        input: get("input_tokens"),
        output: get("output_tokens"),
        cache_read: get("cache_read_input_tokens"),
        cache_write: get("cache_creation_input_tokens"),
    }
}

fn decode_stop(reason: &str) -> StopReason {
    match reason {
        "tool_use" => StopReason::ToolUse,
        "max_tokens" => StopReason::Length,
        // A safety refusal arrives as HTTP 200 with this stop reason; treating
        // it as a normal stop would show the user an empty reply with no cause.
        "refusal" => StopReason::Error,
        _ => StopReason::Stop,
    }
}

fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_result(id: &str, body: &str) -> Message {
        Message::ToolResult {
            tool_call_id: id.into(),
            tool_name: "exec".into(),
            content: body.into(),
            is_error: false,
            blob: None,
        }
    }

    #[test]
    fn consecutive_tool_results_merge_into_one_user_message() {
        let msgs = vec![tool_result("a", "1"), tool_result("b", "2")];
        let v = encode_messages(&msgs);
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), 1, "Anthropic rejects back-to-back user messages");
        assert_eq!(arr[0]["content"].as_array().unwrap().len(), 2);
        assert_eq!(arr[0]["content"][1]["tool_use_id"], "b");
    }

    #[test]
    fn a_user_turn_after_tool_results_starts_a_new_message() {
        let msgs = vec![tool_result("a", "1"), Message::user("thanks")];
        let arr = encode_messages(&msgs).as_array().unwrap().clone();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[1]["content"][0]["text"], "thanks");
    }

    #[test]
    fn unsigned_thinking_blocks_are_dropped_on_replay() {
        let msgs = vec![Message::Assistant {
            content: vec![
                ContentBlock::Thinking { text: "unsigned".into(), signature: None },
                ContentBlock::Thinking {
                    text: "signed".into(),
                    signature: Some("sig".into()),
                },
                ContentBlock::text("answer"),
            ],
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error: None,
        }];
        let arr = encode_messages(&msgs).as_array().unwrap().clone();
        let blocks = arr[0]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["signature"], "sig");
        assert_eq!(blocks[1]["text"], "answer");
    }

    #[test]
    fn empty_assistant_turns_are_omitted() {
        let msgs = vec![Message::Assistant {
            content: vec![ContentBlock::text("")],
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error: None,
        }];
        assert!(encode_messages(&msgs).as_array().unwrap().is_empty());
    }

    fn client_for(model: &str) -> Client {
        Client {
            http: reqwest::Client::new(),
            base_url: "https://api.anthropic.com/v1".into(),
            model: model.into(),
            api_key: "k".into(),
        }
    }

    fn body_for(temperature: Option<f32>) -> Value {
        let msgs = [Message::user("hi")];
        client_for("claude-opus-5").build_body(&Request {
            system: "s",
            messages: &msgs,
            tools: &[],
            max_tokens: 100,
            temperature,
        })
    }

    #[test]
    fn temperature_is_omitted_unless_explicitly_configured() {
        // Opus 5, Sonnet 5 and the 4.6+ family removed sampling parameters:
        // sending `temperature` at all returns a 400.
        assert!(
            body_for(None).get("temperature").is_none(),
            "an unset temperature must not reach the wire"
        );
        assert_eq!(body_for(Some(0.5))["temperature"], 0.5);
    }

    #[test]
    fn a_safety_refusal_is_not_reported_as_a_normal_stop() {
        assert_eq!(decode_stop("refusal"), StopReason::Error);
        assert_eq!(decode_stop("end_turn"), StopReason::Stop);
    }

    #[test]
    fn reads_cache_token_fields() {
        let u = decode_usage(&json!({
            "input_tokens": 10, "output_tokens": 3,
            "cache_read_input_tokens": 900, "cache_creation_input_tokens": 40
        }));
        assert_eq!(u, Usage { input: 10, output: 3, cache_read: 900, cache_write: 40 });
    }
}
