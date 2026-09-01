//! OpenAI-compatible `/v1/chat/completions` with SSE streaming.
//!
//! One client covers llama.cpp's server, ollama, vLLM, Groq, OpenRouter and
//! DeepSeek, which is why it is worth more care than the line count suggests.

use super::{ApiError, EventSink, Request, ToolSchema};
use crate::config::ModelConfig;
use crate::types::{ContentBlock, Message, StopReason, StreamEvent, Usage};
use anyhow::{Context, Result, bail};
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::time::Duration;

pub struct Client {
    http: reqwest::Client,
    base_url: String,
    pub model: String,
    api_key: Option<String>,
}

impl Client {
    pub fn new(http: reqwest::Client, cfg: &ModelConfig) -> Self {
        Self {
            http,
            base_url: cfg.base_url.trim_end_matches('/').to_string(),
            model: cfg.model.clone(),
            api_key: cfg.api_key(),
        }
    }

    pub async fn stream(&self, req: &Request<'_>, sink: EventSink<'_>) -> Result<()> {
        let body = self.build_body(req);
        let mut http = self
            .http
            .post(format!("{}/chat/completions", self.base_url))
            .header("accept", "text/event-stream")
            .json(&body);
        if let Some(key) = &self.api_key {
            http = http.bearer_auth(key);
        }

        let resp = http.send().await.context("POST /chat/completions")?;
        let status = resp.status();
        if !status.is_success() {
            let retry_after = parse_retry_after(resp.headers());
            let message = resp.text().await.unwrap_or_default();
            bail!(ApiError {
                status: Some(status.as_u16()),
                message: format!("{status}: {}", truncate(&message, 500)),
                retry_after,
            });
        }

        self.consume(resp, sink).await
    }

    fn build_body(&self, req: &Request<'_>) -> Value {
        let mut messages = vec![json!({"role": "system", "content": req.system})];
        for m in req.messages {
            messages.push(encode_message(m));
        }

        let mut body = json!({
            "model": self.model,
            "messages": messages,
            "max_tokens": req.max_tokens,
            "stream": true,
        });
        // Servers that honour this give exact token counts instead of the
        // chars/4 guess; strict ones reject the unknown field outright.
        if req.stream_usage {
            body["stream_options"] = json!({"include_usage": true});
        }
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
        // Tool calls stream as fragments: the name arrives once and the
        // arguments dribble in as JSON text. OpenAI keys the fragments by
        // `index`; Mistral and some other compatible servers send only an `id`,
        // or one whole call per delta. Track both so two parallel calls are not
        // concatenated into one when `index` is absent.
        let mut open_tool: Option<(u64, Option<String>)> = None;
        let mut stop = StopReason::Stop;

        while let Some(event) = stream.next().await {
            let event = event.context("reading SSE stream")?;
            if event.data == "[DONE]" {
                break;
            }
            let chunk: Value = match serde_json::from_str(&event.data) {
                Ok(v) => v,
                // A keepalive or a comment frame is not fatal.
                Err(_) => continue,
            };

            if let Some(err) = chunk.get("error") {
                bail!(ApiError {
                    status: None,
                    message: err
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("provider returned an error")
                        .to_string(),
                    retry_after: None,
                });
            }

            if let Some(usage) = chunk.get("usage").filter(|u| !u.is_null()) {
                sink(StreamEvent::Usage(decode_usage(usage)));
            }

            let Some(choice) = chunk.get("choices").and_then(|c| c.get(0)) else {
                continue;
            };

            if let Some(delta) = choice.get("delta") {
                if let Some(text) = delta.get("content").and_then(Value::as_str)
                    && !text.is_empty()
                {
                    sink(StreamEvent::TextDelta(text.to_string()));
                }
                // DeepSeek-R1 and friends put chain-of-thought here.
                if let Some(text) = delta.get("reasoning_content").and_then(Value::as_str)
                    && !text.is_empty()
                {
                    sink(StreamEvent::ThinkingDelta(text.to_string()));
                }

                if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                    for call in calls {
                        let index = call.get("index").and_then(Value::as_u64).unwrap_or(0);
                        let id = call.get("id").and_then(Value::as_str).map(str::to_string);
                        let name = call
                            .get("function")
                            .and_then(|f| f.get("name"))
                            .and_then(Value::as_str)
                            .filter(|n| !n.is_empty())
                            .map(str::to_string);

                        // A fragment opens a new call when its index moves, when
                        // it carries a different id, or when it names a function
                        // while one is already open (a server that sends one
                        // complete call per delta and no index at all).
                        let starts_new = match &open_tool {
                            None => true,
                            Some((open_index, open_id)) => {
                                *open_index != index
                                    || (id.is_some() && id != *open_id)
                                    || (id.is_none() && name.is_some())
                            }
                        };

                        if starts_new {
                            if open_tool.is_some() {
                                sink(StreamEvent::ToolCallEnd);
                            }
                            sink(StreamEvent::ToolCallStart {
                                id: id.clone().unwrap_or_else(|| format!("call_{index}")),
                                name: name.unwrap_or_default(),
                            });
                            open_tool = Some((index, id));
                        }
                        if let Some(args) = call
                            .get("function")
                            .and_then(|f| f.get("arguments"))
                            .and_then(Value::as_str)
                            && !args.is_empty()
                        {
                            sink(StreamEvent::ToolCallDelta(args.to_string()));
                        }
                    }
                }
            }

            if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                stop = decode_stop(reason);
            }
        }

        if open_tool.is_some() {
            sink(StreamEvent::ToolCallEnd);
        }
        sink(StreamEvent::Done(stop));
        Ok(())
    }
}

fn encode_tool(t: &ToolSchema) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": t.name,
            "description": t.description,
            "parameters": t.input_schema,
        }
    })
}

fn encode_message(m: &Message) -> Value {
    match m {
        Message::User { content } => json!({"role": "user", "content": content}),
        Message::Assistant { content, .. } => {
            let mut text = String::new();
            let mut calls = Vec::new();
            for block in content {
                match block {
                    ContentBlock::Text { text: t } => text.push_str(t),
                    // Reasoning is not replayed: most OpenAI-compatible servers
                    // reject an unknown field, and none require it.
                    ContentBlock::Thinking { .. } => {}
                    ContentBlock::ToolCall { id, name, input } => calls.push(json!({
                        "id": id,
                        "type": "function",
                        "function": {"name": name, "arguments": input.to_string()},
                    })),
                }
            }
            let mut msg = json!({"role": "assistant"});
            // An assistant turn that is only tool calls must still carry a
            // content key; some servers 400 without it.
            msg["content"] = if text.is_empty() { Value::Null } else { Value::String(text) };
            if !calls.is_empty() {
                msg["tool_calls"] = Value::Array(calls);
            }
            msg
        }
        Message::ToolResult { tool_call_id, content, .. } => json!({
            "role": "tool",
            "tool_call_id": tool_call_id,
            "content": content,
        }),
    }
}

fn decode_usage(u: &Value) -> Usage {
    let get = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0) as u32;
    Usage {
        input: get("prompt_tokens"),
        output: get("completion_tokens"),
        cache_read: u
            .get("prompt_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32,
        cache_write: 0,
    }
}

fn decode_stop(reason: &str) -> StopReason {
    match reason {
        "tool_calls" | "function_call" => StopReason::ToolUse,
        "length" => StopReason::Length,
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

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let cut = s.char_indices().take_while(|(i, _)| *i < max).count();
    format!("{}…", &s[..s.char_indices().nth(cut).map_or(s.len(), |(i, _)| i)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Usage;

    #[test]
    fn tool_only_assistant_turn_keeps_a_content_key() {
        let m = Message::Assistant {
            content: vec![ContentBlock::ToolCall {
                id: "c1".into(),
                name: "exec".into(),
                input: json!({"command": "ls"}),
            }],
            usage: Usage::default(),
            stop_reason: StopReason::ToolUse,
            error: None,
        };
        let v = encode_message(&m);
        assert!(v.get("content").is_some(), "content key must be present even when null");
        assert_eq!(v["tool_calls"][0]["function"]["name"], "exec");
        // Arguments go over the wire as a JSON *string*, not an object.
        assert!(v["tool_calls"][0]["function"]["arguments"].is_string());
    }

    #[test]
    fn thinking_blocks_are_not_replayed_to_openai_servers() {
        let m = Message::Assistant {
            content: vec![
                ContentBlock::Thinking { text: "secret".into(), signature: None },
                ContentBlock::text("hello"),
            ],
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error: None,
        };
        assert_eq!(encode_message(&m)["content"], "hello");
    }

    #[test]
    fn temperature_is_omitted_unless_explicitly_configured() {
        let client = Client {
            http: reqwest::Client::new(),
            base_url: "http://localhost/v1".into(),
            model: "m".into(),
            api_key: None,
        };
        let msgs = [Message::user("hi")];
        let req = |temperature| Request {
            system: "s",
            messages: &msgs,
            tools: &[],
            max_tokens: 100,
            temperature,
            stream_usage: true,
        };
        assert!(client.build_body(&req(None)).get("temperature").is_none());
        // f32 -> f64 widening makes an exact compare brittle; the point is
        // that the field is present and carries the value.
        let sent = client.build_body(&req(Some(0.5)));
        assert!((sent["temperature"].as_f64().unwrap() - 0.5).abs() < 1e-6);
    }

    #[test]
    fn stream_options_is_sent_only_when_enabled() {
        let client = Client {
            http: reqwest::Client::new(),
            base_url: "http://localhost/v1".into(),
            model: "m".into(),
            api_key: None,
        };
        let msgs = [Message::user("hi")];
        let req = |stream_usage| Request {
            system: "s",
            messages: &msgs,
            tools: &[],
            max_tokens: 100,
            temperature: None,
            stream_usage,
        };
        assert!(client.build_body(&req(true)).get("stream_options").is_some());
        // A strict server answers 400/422 on the unknown field.
        assert!(client.build_body(&req(false)).get("stream_options").is_none());
    }

    #[test]
    fn maps_finish_reasons() {
        assert_eq!(decode_stop("tool_calls"), StopReason::ToolUse);
        assert_eq!(decode_stop("length"), StopReason::Length);
        assert_eq!(decode_stop("stop"), StopReason::Stop);
    }

    #[test]
    fn reads_cached_prompt_tokens_when_the_server_reports_them() {
        let u = decode_usage(&json!({
            "prompt_tokens": 100,
            "completion_tokens": 20,
            "prompt_tokens_details": {"cached_tokens": 64}
        }));
        assert_eq!(u, Usage { input: 100, output: 20, cache_read: 64, cache_write: 0 });
    }
}
