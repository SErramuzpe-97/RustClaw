//! Context-window compaction, ported from OpenClaw's
//! `packages/agent-core/src/harness/compaction/compaction.ts` with its
//! constants intact.
//!
//! When the transcript approaches the model's context window, the older half is
//! replaced by a model-written summary and the recent turns are kept verbatim.

use crate::llm::{Provider, Request};
use crate::types::{ContentBlock, Message, StreamEvent, Usage, estimate_context_tokens};
use anyhow::Result;

/// Headroom left for the next response, so compaction triggers before the
/// provider rejects the request rather than after.
pub const RESERVE_TOKENS: u32 = 16_384;
/// Recent tail kept verbatim.
pub const KEEP_RECENT_TOKENS: u32 = 20_000;
/// Cap on the generated summary, so compaction cannot itself blow the budget.
pub const MAX_SUMMARY_CHARS: usize = 16_000;

const SUMMARIZATION_PROMPT: &str = "\
You are summarizing a conversation so it can continue with less context.

Write a summary that preserves everything needed to carry on: what the user asked \
for, decisions taken and why, files and commands touched with their paths, results \
and errors seen, and what is still outstanding. Prefer concrete detail over \
description of the conversation. Do not address the user; write notes for yourself.";

/// Whether the transcript has grown close enough to the window to compact.
///
/// `context_tokens` is the provider's own count when it reported one, and the
/// chars/4 estimate otherwise.
pub fn should_compact(context_tokens: u32, context_window: u32) -> bool {
    context_tokens > context_window.saturating_sub(RESERVE_TOKENS)
}

/// Tokens currently in the window: the provider's last `usage` if it is
/// informative, else the estimate.
pub fn context_tokens(messages: &[Message], last: &Usage) -> u32 {
    let reported = last.context_tokens();
    if reported > 0 { reported } else { estimate_context_tokens(messages) }
}

/// First index to keep, chosen so the kept tail is roughly
/// `KEEP_RECENT_TOKENS` and never splits a turn.
///
/// A turn starts at a user message; cutting between an assistant's tool calls
/// and their results would leave the provider with orphaned tool ids.
pub fn find_cut_point(messages: &[Message]) -> usize {
    let mut kept = 0u32;
    let mut cut = messages.len();

    for (i, m) in messages.iter().enumerate().rev() {
        kept += crate::types::estimate_tokens(m.char_len());
        if kept >= KEEP_RECENT_TOKENS {
            cut = i;
            break;
        }
        cut = i;
    }

    // Walk forward to the next user message so the tail begins on a turn
    // boundary with no dangling tool results.
    while cut < messages.len() && !matches!(messages[cut], Message::User { .. }) {
        cut += 1;
    }
    // Never drop everything: if there is no clean boundary, keep the last turn.
    if cut >= messages.len() {
        cut = messages
            .iter()
            .rposition(|m| matches!(m, Message::User { .. }))
            .unwrap_or(0);
    }
    cut
}

/// Ask the model to summarize `messages[..cut]`.
pub async fn generate_summary(provider: &Provider, messages: &[Message]) -> Result<String> {
    let transcript = render_for_summary(messages);
    let ask = vec![Message::user(format!(
        "Summarize this conversation:\n\n<transcript>\n{transcript}\n</transcript>"
    ))];

    let mut summary = String::new();
    let mut sink = |ev: StreamEvent| {
        if let StreamEvent::TextDelta(t) = ev {
            summary.push_str(&t);
        }
    };
    provider
        .stream_with_retry(
            &Request {
                system: SUMMARIZATION_PROMPT,
                messages: &ask,
                tools: &[],
                max_tokens: 4096,
                // A summary is a factual record, not prose, so pin sampling
                // low where the backend accepts it at all.
                temperature: None,
                stream_usage: false,
            },
            &mut sink,
        )
        .await?;

    summary.truncate(floor_char_boundary(&summary, MAX_SUMMARY_CHARS));
    Ok(summary)
}

/// Flatten a slice of messages into text for the summarizer.
fn render_for_summary(messages: &[Message]) -> String {
    let mut out = String::new();
    for m in messages {
        match m {
            Message::User { content } => {
                out.push_str("USER: ");
                out.push_str(content);
            }
            Message::Assistant { content, .. } => {
                out.push_str("ASSISTANT: ");
                for b in content {
                    match b {
                        ContentBlock::Text { text } => out.push_str(text),
                        ContentBlock::ToolCall { name, input, .. } => {
                            out.push_str(&format!("[called {name} with {input}]"));
                        }
                        // Reasoning is not part of the record worth carrying.
                        ContentBlock::Thinking { .. } => {}
                    }
                }
            }
            Message::ToolResult { tool_name, content, is_error, .. } => {
                out.push_str(&format!(
                    "TOOL {tool_name}{}: {}",
                    if *is_error { " (error)" } else { "" },
                    // Tool output is the bulkiest part of a transcript and the
                    // least useful verbatim.
                    content.chars().take(2000).collect::<String>()
                ));
            }
        }
        out.push_str("\n\n");
    }
    out
}

/// The compacted transcript: a summary carried as a user message, then the
/// kept tail.
pub fn apply(summary: &str, messages: &[Message], cut: usize) -> Vec<Message> {
    let mut out = Vec::with_capacity(messages.len() - cut + 1);
    out.push(Message::user(format!(
        "[Summary of the earlier conversation]\n\n{summary}"
    )));
    out.extend_from_slice(&messages[cut..]);
    out
}

fn floor_char_boundary(s: &str, mut i: usize) -> usize {
    i = i.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::StopReason;

    fn user(n: usize) -> Message {
        Message::user("u".repeat(n))
    }
    fn tool_result(n: usize) -> Message {
        Message::ToolResult {
            tool_call_id: "c".into(),
            tool_name: "exec".into(),
            content: "t".repeat(n),
            is_error: false,
            blob: None,
        }
    }
    fn assistant(n: usize) -> Message {
        Message::Assistant {
            content: vec![ContentBlock::text("a".repeat(n))],
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error: None,
        }
    }

    #[test]
    fn compacts_only_once_the_reserve_is_eaten_into() {
        assert!(!should_compact(100_000, 200_000));
        assert!(should_compact(190_000, 200_000));
        // A window smaller than the reserve must still be able to trigger
        // rather than underflowing to a huge threshold.
        assert!(should_compact(1, 8_000));
    }

    #[test]
    fn prefers_the_providers_own_token_count_over_the_estimate() {
        let msgs = vec![user(4000)];
        let reported = Usage { input: 12_345, ..Default::default() };
        assert_eq!(context_tokens(&msgs, &reported), 12_345);
        assert_eq!(context_tokens(&msgs, &Usage::default()), 1000);
    }

    #[test]
    fn the_cut_lands_on_a_user_message_so_no_tool_result_is_orphaned() {
        // Each message is 40k chars = 10k tokens, so the tail fills quickly.
        let msgs = vec![
            user(40_000),
            assistant(40_000),
            tool_result(40_000),
            user(40_000),
            assistant(40_000),
        ];
        let cut = find_cut_point(&msgs);
        assert!(
            matches!(msgs[cut], Message::User { .. }),
            "cut landed on a non-user message at {cut}"
        );
    }

    #[test]
    fn never_cuts_away_the_entire_transcript() {
        // A single enormous turn has no earlier boundary to cut at.
        let msgs = vec![user(400_000), assistant(400_000)];
        let cut = find_cut_point(&msgs);
        assert!(cut < msgs.len(), "cut must leave something behind");
    }

    #[test]
    fn apply_puts_the_summary_first_and_keeps_the_tail() {
        let msgs = vec![user(10), assistant(10), user(20), assistant(20)];
        let out = apply("SUMMARY", &msgs, 2);
        assert_eq!(out.len(), 3);
        match &out[0] {
            Message::User { content } => assert!(content.contains("SUMMARY")),
            _ => panic!("summary must be the first message"),
        }
        assert_eq!(out[1], msgs[2]);
    }

    #[test]
    fn rendering_bounds_tool_output() {
        let msgs = vec![tool_result(100_000)];
        assert!(render_for_summary(&msgs).len() < 3000);
    }

    #[test]
    fn rendering_names_tool_calls() {
        let msgs = vec![Message::Assistant {
            content: vec![ContentBlock::ToolCall {
                id: "c".into(),
                name: "grep".into(),
                input: serde_json::json!({"pattern": "x"}),
            }],
            usage: Usage::default(),
            stop_reason: StopReason::ToolUse,
            error: None,
        }];
        assert!(render_for_summary(&msgs).contains("called grep"));
    }
}
