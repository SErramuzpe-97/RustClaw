//! Message model, ported from OpenClaw's `packages/llm-core/src/types.ts`.
//!
//! Large tool outputs are not inlined here. `ToolResult::content` holds a
//! bounded preview and `blob` points at a file under `tool-results/`, so a long
//! transcript does not carry megabytes of `exec` output through every
//! serialization.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StopReason {
    Stop,
    Length,
    ToolUse,
    Error,
    Aborted,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    Thinking {
        text: String,
        /// Opaque provider signature; Anthropic rejects replayed thinking
        /// blocks whose signature was dropped.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    ToolCall {
        id: String,
        name: String,
        input: serde_json::Value,
    },
}

impl ContentBlock {
    pub fn text(s: impl Into<String>) -> Self {
        Self::Text { text: s.into() }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub input: u32,
    #[serde(default)]
    pub output: u32,
    #[serde(default)]
    pub cache_read: u32,
    #[serde(default)]
    pub cache_write: u32,
}

impl Usage {
    /// Tokens occupying the context window on the next call.
    pub fn context_tokens(&self) -> u32 {
        self.input
            .saturating_add(self.cache_read)
            .saturating_add(self.cache_write)
            .saturating_add(self.output)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "camelCase")]
pub enum Message {
    User {
        content: String,
    },
    Assistant {
        content: Vec<ContentBlock>,
        #[serde(default)]
        usage: Usage,
        stop_reason: StopReason,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    ToolResult {
        tool_call_id: String,
        tool_name: String,
        /// Bounded preview sent to the model.
        content: String,
        #[serde(default)]
        is_error: bool,
        /// Path under the session's `tool-results/` dir when the full output
        /// was spilled to disk.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        blob: Option<String>,
    },
}

impl Message {
    pub fn user(content: impl Into<String>) -> Self {
        Self::User { content: content.into() }
    }

    /// Character count used by the chars/4 token estimate.
    pub fn char_len(&self) -> usize {
        match self {
            Self::User { content } => content.len(),
            Self::Assistant { content, .. } => content
                .iter()
                .map(|b| match b {
                    ContentBlock::Text { text } | ContentBlock::Thinking { text, .. } => text.len(),
                    ContentBlock::ToolCall { name, input, .. } => {
                        name.len() + input.to_string().len()
                    }
                })
                .sum(),
            Self::ToolResult { content, .. } => content.len(),
        }
    }

    pub fn tool_calls(&self) -> &[ContentBlock] {
        match self {
            Self::Assistant { content, .. } => content,
            _ => &[],
        }
    }
}

/// IronClaw estimates tokens as chars/4 rather than linking a tokenizer, and
/// corrects with the provider's real `usage`. We do the same: a tokenizer would
/// cost tens of MB of vocabulary and CPU we do not have to spend.
pub const CHARS_PER_TOKEN: usize = 4;

pub fn estimate_tokens(chars: usize) -> u32 {
    (chars / CHARS_PER_TOKEN) as u32
}

pub fn estimate_context_tokens(messages: &[Message]) -> u32 {
    estimate_tokens(messages.iter().map(Message::char_len).sum())
}

/// Streaming events emitted by a provider while a response is in flight.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    TextDelta(String),
    ThinkingDelta(String),
    ToolCallStart { id: String, name: String },
    ToolCallDelta(String),
    ToolCallEnd,
    Usage(Usage),
    Done(StopReason),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimates_tokens_at_four_chars_each() {
        assert_eq!(estimate_tokens(400), 100);
        assert_eq!(estimate_tokens(3), 0);
    }

    #[test]
    fn context_estimate_spans_every_message_kind() {
        let msgs = vec![
            Message::user("a".repeat(40)),
            Message::Assistant {
                content: vec![ContentBlock::text("b".repeat(40))],
                usage: Usage::default(),
                stop_reason: StopReason::Stop,
                error: None,
            },
            Message::ToolResult {
                tool_call_id: "t1".into(),
                tool_name: "read".into(),
                content: "c".repeat(40),
                is_error: false,
                blob: None,
            },
        ];
        assert_eq!(estimate_context_tokens(&msgs), 30);
    }

    #[test]
    fn messages_round_trip_through_jsonl() {
        let m = Message::Assistant {
            content: vec![ContentBlock::ToolCall {
                id: "call_1".into(),
                name: "exec".into(),
                input: serde_json::json!({"command": "ls"}),
            }],
            usage: Usage { input: 10, output: 5, ..Default::default() },
            stop_reason: StopReason::ToolUse,
            error: None,
        };
        let line = serde_json::to_string(&m).unwrap();
        assert!(!line.contains('\n'), "a transcript entry must stay on one line");
        assert_eq!(serde_json::from_str::<Message>(&line).unwrap(), m);
    }
}
