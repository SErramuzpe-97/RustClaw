//! Wire shapes for the UI.
//!
//! The internal `Message` enum carries provider replay detail the browser has
//! no use for — thinking signatures, tool-result blob paths, cache token
//! counts. Flattening here keeps that out of the network and lets the internal
//! model change without breaking the page.

use crate::types::{ContentBlock, Message};
use serde::Serialize;

#[derive(Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct UiMessage {
    /// "user" | "assistant" | "tool"
    pub role: &'static str,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub text: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<UiToolCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<UiToolResult>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub is_error: bool,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct UiToolCall {
    pub name: String,
    /// One-line rendering of the arguments, as the progress display shows them.
    pub input: String,
}

#[derive(Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct UiToolResult {
    pub name: String,
    pub is_error: bool,
    pub preview: String,
}

/// How much of a stored tool result the browser gets. The full output is on
/// disk under `tool-results/`; sending megabytes of `exec` output into the DOM
/// would stall the page for no benefit.
const MAX_TOOL_PREVIEW: usize = 2_000;

pub fn transcript(messages: &[Message]) -> Vec<UiMessage> {
    messages.iter().filter_map(render).collect()
}

fn render(m: &Message) -> Option<UiMessage> {
    match m {
        Message::User { content } => Some(UiMessage {
            role: "user",
            text: content.clone(),
            tool_calls: Vec::new(),
            tool: None,
            is_error: false,
        }),

        Message::Assistant { content, error, .. } => {
            let mut text = String::new();
            let mut tool_calls = Vec::new();
            for block in content {
                match block {
                    ContentBlock::Text { text: t } => text.push_str(t),
                    // Reasoning is not rendered: on the current models it
                    // arrives empty anyway, and it is not part of the answer.
                    ContentBlock::Thinking { .. } => {}
                    ContentBlock::ToolCall { name, input, .. } => tool_calls.push(UiToolCall {
                        name: name.clone(),
                        input: crate::agent::summarize_input(input),
                    }),
                }
            }
            // An assistant turn that produced neither text nor a tool call is
            // an artefact of replay, not something to draw an empty bubble for.
            if text.is_empty() && tool_calls.is_empty() && error.is_none() {
                return None;
            }
            Some(UiMessage {
                role: "assistant",
                text,
                tool_calls,
                tool: None,
                is_error: error.is_some(),
            })
        }

        Message::ToolResult { tool_name, content, is_error, .. } => Some(UiMessage {
            role: "tool",
            text: String::new(),
            tool_calls: Vec::new(),
            tool: Some(UiToolResult {
                name: tool_name.clone(),
                is_error: *is_error,
                preview: content.chars().take(MAX_TOOL_PREVIEW).collect(),
            }),
            is_error: *is_error,
        }),
    }
}

/// Render a transcript as a Markdown document.
///
/// The assistant already writes Markdown, so its turns are emitted verbatim —
/// re-escaping them would break the code fences that are the main reason to
/// export in the first place. Tool activity is folded into detail blocks so the
/// conversation stays readable without losing what the agent actually ran.
pub fn to_markdown(title: &str, messages: &[Message]) -> String {
    let mut out = String::with_capacity(messages.len() * 256);
    out.push_str("# ");
    out.push_str(title);
    out.push_str("\n\n");

    for m in messages {
        match m {
            Message::User { content } => {
                out.push_str("## You\n\n");
                out.push_str(content.trim());
                out.push_str("\n\n");
            }
            Message::Assistant { content, error, .. } => {
                let mut text = String::new();
                let mut calls = Vec::new();
                for b in content {
                    match b {
                        ContentBlock::Text { text: t } => text.push_str(t),
                        ContentBlock::Thinking { .. } => {}
                        ContentBlock::ToolCall { name, input, .. } => {
                            calls.push(format!("`{name}` {}", crate::agent::summarize_input(input)))
                        }
                    }
                }
                if text.trim().is_empty() && calls.is_empty() && error.is_none() {
                    continue;
                }
                out.push_str("## Assistant\n\n");
                if !text.trim().is_empty() {
                    out.push_str(text.trim());
                    out.push_str("\n\n");
                }
                for c in calls {
                    out.push_str("> ran ");
                    out.push_str(&c);
                    out.push('\n');
                }
                if let Some(e) = error {
                    out.push_str("> **error:** ");
                    out.push_str(e);
                    out.push('\n');
                }
                if !out.ends_with("\n\n") {
                    out.push('\n');
                }
            }
            Message::ToolResult { tool_name, content, is_error, .. } => {
                out.push_str(&format!(
                    "<details><summary>{} {tool_name}</summary>\n\n```\n{}\n```\n\n</details>\n\n",
                    if *is_error { "error from" } else { "output of" },
                    // A fence inside the output would end the block early.
                    content.replace("```", "\u{200b}`\u{200b}`\u{200b}`").trim_end(),
                ));
            }
        }
    }
    out
}

/// A filesystem-safe name derived from the conversation title.
pub fn export_filename(title: &str) -> String {
    let stem: String = title
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect();
    let stem = stem.trim_matches('-').to_lowercase();
    let stem: String = stem.chars().take(60).collect();
    if stem.is_empty() { "conversation.md".into() } else { format!("{stem}.md") }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{StopReason, Usage};

    fn assistant(blocks: Vec<ContentBlock>) -> Message {
        Message::Assistant {
            content: blocks,
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error: None,
        }
    }

    #[test]
    fn a_user_turn_carries_its_text() {
        let out = transcript(&[Message::user("hola")]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].role, "user");
        assert_eq!(out[0].text, "hola");
    }

    #[test]
    fn tool_calls_are_flattened_beside_the_text() {
        let out = transcript(&[assistant(vec![
            ContentBlock::text("voy a mirar"),
            ContentBlock::ToolCall {
                id: "c1".into(),
                name: "grep".into(),
                input: serde_json::json!({"pattern": "TODO"}),
            },
        ])]);
        assert_eq!(out[0].text, "voy a mirar");
        assert_eq!(out[0].tool_calls.len(), 1);
        assert_eq!(out[0].tool_calls[0].name, "grep");
        assert!(out[0].tool_calls[0].input.contains("TODO"));
    }

    #[test]
    fn thinking_blocks_never_reach_the_browser() {
        let out = transcript(&[assistant(vec![
            ContentBlock::Thinking { text: "interno".into(), signature: None },
            ContentBlock::text("respuesta"),
        ])]);
        assert_eq!(out[0].text, "respuesta");
        assert!(!serde_json::to_string(&out).unwrap().contains("interno"));
    }

    #[test]
    fn empty_assistant_turns_do_not_become_empty_bubbles() {
        assert!(transcript(&[assistant(vec![ContentBlock::text("")])]).is_empty());
    }

    #[test]
    fn tool_results_are_previewed_not_sent_whole() {
        let out = transcript(&[Message::ToolResult {
            tool_call_id: "c1".into(),
            tool_name: "exec".into(),
            content: "x".repeat(100_000),
            is_error: false,
            blob: Some("/tmp/spill.txt".into()),
        }]);
        let tool = out[0].tool.as_ref().unwrap();
        assert_eq!(tool.name, "exec");
        assert!(tool.preview.chars().count() <= MAX_TOOL_PREVIEW);
        // The on-disk path is an internal detail, not something to leak.
        assert!(!serde_json::to_string(&out).unwrap().contains("/tmp/spill.txt"));
    }

    #[test]
    fn markdown_export_keeps_assistant_prose_verbatim() {
        // The whole point of exporting is to get the model's Markdown back;
        // escaping it would destroy the code fences.
        let md = to_markdown("Notas", &[
            Message::user("hazme un ejemplo"),
            assistant(vec![ContentBlock::text("Aquí va:\n\n```rust\nfn main() {}\n```")]),
        ]);
        assert!(md.starts_with("# Notas\n\n"));
        assert!(md.contains("## You\n\nhazme un ejemplo"));
        assert!(md.contains("```rust\nfn main() {}\n```"), "fence mangled:\n{md}");
    }

    #[test]
    fn tool_output_cannot_break_out_of_its_fence() {
        let md = to_markdown("t", &[Message::ToolResult {
            tool_call_id: "c".into(),
            tool_name: "read".into(),
            content: "antes\n```\ndentro\n```\ndespues".into(),
            is_error: false,
            blob: None,
        }]);
        // Exactly the opening and closing fence this block owns.
        assert_eq!(md.matches("```").count(), 2, "a fence in the output escaped:\n{md}");
        assert!(md.contains("dentro"));
    }

    #[test]
    fn tool_calls_are_recorded_in_the_export() {
        let md = to_markdown("t", &[assistant(vec![ContentBlock::ToolCall {
            id: "c".into(),
            name: "grep".into(),
            input: serde_json::json!({"pattern": "TODO"}),
        }])]);
        assert!(md.contains("ran `grep`"), "{md}");
        assert!(md.contains("TODO"));
    }

    #[test]
    fn filenames_are_safe_and_bounded() {
        assert_eq!(export_filename("Notas del proyecto"), "notas-del-proyecto.md");
        assert_eq!(export_filename("../../etc/passwd"), "etc-passwd.md");
        assert_eq!(export_filename("   "), "conversation.md");
        assert!(export_filename(&"x".repeat(500)).len() <= 63);
        // No separator may survive into the name.
        assert!(!export_filename("a/b\\c").contains(['/', '\\']));
    }

    #[test]
    fn a_failed_tool_keeps_its_error_flag() {
        let out = transcript(&[Message::ToolResult {
            tool_call_id: "c1".into(),
            tool_name: "read".into(),
            content: "no such file".into(),
            is_error: true,
            blob: None,
        }]);
        assert!(out[0].is_error);
        assert!(out[0].tool.as_ref().unwrap().is_error);
    }
}
