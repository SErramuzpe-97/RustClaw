//! The agent loop.
//!
//! Functionally this is OpenClaw's `runLoop()`
//! (`packages/agent-core/src/agent-loop.ts`) with IronClaw's stage ordering
//! (`ironclaw_agent_loop/src/executor/canonical.rs`):
//!
//! ```text
//! cancel → drain steering → compact → model → (reply | tool calls) → stop
//! ```
//!
//! One loop serves the REPL, the WebUI and the Telegram bridge; each subscribes
//! to the same broadcast of `AgentEvent`s rather than getting its own runner.

use crate::compact;
use crate::config::Config;
use crate::llm::{Provider, Request, ToolSchema};
use crate::session::Session;
use crate::tools::{self, Ctx};
use crate::types::{ContentBlock, Message, StopReason, StreamEvent, Usage};
use anyhow::Result;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

/// What a surface (REPL, WebUI, Telegram) sees while a turn runs.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    TurnStart,
    TextDelta(String),
    ThinkingDelta(String),
    ToolStart { name: String, input: String },
    ToolEnd { name: String, is_error: bool, preview: String },
    /// A complete assistant reply; surfaces that cannot render deltas use this.
    Reply(String),
    Compacted { dropped: usize },
    TurnEnd { usage: Usage },
    Error(String),
}

pub struct Agent {
    provider: Provider,
    cfg: Config,
    schemas: Vec<ToolSchema>,
    ctx: Ctx,
    pub session: Session,
    events: broadcast::Sender<AgentEvent>,
    /// Messages queued while a turn is in flight, injected before the next
    /// model call rather than starting a competing turn.
    steering: mpsc::UnboundedReceiver<String>,
    steer_tx: mpsc::UnboundedSender<String>,
    last_usage: Usage,
    home: std::path::PathBuf,
}

impl Agent {
    pub fn new(cfg: Config, session: Session) -> Result<Self> {
        let home = crate::config::home_dir()?;
        let provider = Provider::new(&cfg.model)?;
        let (events, _) = broadcast::channel(256);
        let (steer_tx, steering) = mpsc::unbounded_channel();
        let ctx = Ctx {
            cwd: std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
            exec_timeout: std::time::Duration::from_secs(cfg.agent.exec_timeout_secs),
            http: reqwest::Client::builder()
                .user_agent(concat!("rustclaw/", env!("CARGO_PKG_VERSION")))
                .timeout(std::time::Duration::from_secs(30))
                .build()?,
        };
        Ok(Self {
            provider,
            schemas: tools::schemas(),
            cfg,
            ctx,
            session,
            events,
            steering,
            steer_tx,
            last_usage: Usage::default(),
            home,
        })
    }

    /// Clone of the steering sender. A message queued here is injected before
    /// the next model call of the turn already in flight, rather than starting
    /// a competing turn; the surface can queue while the turn holds the lock.
    pub fn steer_sender(&self) -> mpsc::UnboundedSender<String> {
        self.steer_tx.clone()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<AgentEvent> {
        self.events.subscribe()
    }

    /// Clone of the sender, so a surface can hand out receivers of its own
    /// without holding the agent lock.
    pub fn subscribe_sender(&self) -> broadcast::Sender<AgentEvent> {
        self.events.clone()
    }

    /// Point tool execution at a specific directory. Defaults to the process
    /// working directory. Only the tests need this today.
    #[cfg(test)]
    pub fn set_cwd(&mut self, dir: std::path::PathBuf) {
        self.ctx.cwd = dir;
    }

    /// Replace the active session. Only one transcript is held in memory, so
    /// RSS does not grow with the number of saved conversations — which is what
    /// matters on a machine with 8 GB shared with the GPU.
    pub fn switch_session(&mut self, session: Session) {
        self.session = session;
        // The old counts describe a transcript that is no longer loaded.
        self.last_usage = Usage::default();
    }

    pub fn home(&self) -> &std::path::Path {
        &self.home
    }

    /// Re-run the last user turn, discarding the answer it produced.
    pub async fn regenerate(&mut self, cancel: CancellationToken) -> Result<Option<String>> {
        let Some(prompt) = self.session.rewind_to_last_user()? else {
            return Ok(None);
        };
        self.run_turn(prompt, cancel).await.map(Some)
    }

    pub fn model_name(&self) -> &str {
        self.provider.model_name()
    }

    pub fn telegram_config(&self) -> &crate::config::TelegramConfig {
        &self.cfg.telegram
    }

    pub fn server_config(&self) -> &crate::config::ServerConfig {
        &self.cfg.server
    }

    fn emit(&self, event: AgentEvent) {
        // A send failure only means nobody is listening yet.
        let _ = self.events.send(event);
    }

    /// Run one turn to completion and return the assistant's final text.
    ///
    /// `TurnEnd` is the signal every surface waits on to stop rendering and
    /// re-enable input, so it is emitted here on *every* exit path. Emitting it
    /// only on the success path deadlocked the REPL and left the web UI's form
    /// disabled after any error.
    pub async fn run_turn(&mut self, input: String, cancel: CancellationToken) -> Result<String> {
        let untitled = self.session.title.is_none() && self.session.messages.is_empty();
        let result = self.drive_turn(input.clone(), cancel).await;
        if untitled {
            // Derived from the first message rather than asked of the model: a
            // title is not worth a round trip and tokens.
            let _ = self.session.set_title(&input);
        }
        Session::touch_index(&self.home, self.session.meta());
        self.emit(AgentEvent::TurnEnd { usage: self.last_usage.clone() });
        result
    }

    async fn drive_turn(&mut self, input: String, cancel: CancellationToken) -> Result<String> {
        self.session.append(Message::user(input))?;
        self.emit(AgentEvent::TurnStart);

        let mut final_text = String::new();
        let mut iterations = 0u32;

        loop {
            if cancel.is_cancelled() {
                self.emit(AgentEvent::Error("cancelled".into()));
                return Ok(final_text);
            }

            iterations += 1;
            if iterations > self.cfg.agent.max_iterations {
                let msg = format!(
                    "stopped after {} model calls in one turn",
                    self.cfg.agent.max_iterations
                );
                self.emit(AgentEvent::Error(msg.clone()));
                return Ok(if final_text.is_empty() { msg } else { final_text });
            }

            // Anything the user typed while the model was working joins the
            // context now, before the next call.
            while let Ok(extra) = self.steering.try_recv() {
                self.session.append(Message::user(extra))?;
            }

            self.maybe_compact().await?;

            let assistant = match self.stream_once(&cancel).await {
                Ok(m) => m,
                Err(e) => {
                    let text = format!("{e:#}");
                    self.emit(AgentEvent::Error(text.clone()));
                    self.session.append(Message::Assistant {
                        content: vec![ContentBlock::text(format!("(error: {text})"))],
                        usage: Usage::default(),
                        stop_reason: StopReason::Error,
                        error: Some(text.clone()),
                    })?;
                    return Ok(text);
                }
            };

            let stop = match &assistant {
                Message::Assistant { stop_reason, content, usage, .. } => {
                    for b in content {
                        if let ContentBlock::Text { text } = b {
                            final_text.push_str(text);
                        }
                    }
                    self.last_usage = usage.clone();
                    *stop_reason
                }
                _ => StopReason::Stop,
            };
            let calls: Vec<ContentBlock> = assistant
                .tool_calls()
                .iter()
                .filter(|b| matches!(b, ContentBlock::ToolCall { .. }))
                .cloned()
                .collect();

            self.session.append(assistant)?;

            match stop {
                StopReason::Error | StopReason::Aborted => break,
                // Some servers report `stop` while still emitting tool calls,
                // so dispatch on the calls rather than trusting the flag alone.
                _ if !calls.is_empty() => {
                    self.execute_tools(&calls, &cancel).await?;
                    final_text.clear();
                }
                _ => break,
            }
        }

        self.emit(AgentEvent::Reply(final_text.clone()));
        Ok(final_text)
    }

    /// One model call, assembling deltas into an assistant message.
    async fn stream_once(&mut self, cancel: &CancellationToken) -> Result<Message> {
        let mut text = String::new();
        let mut thinking = String::new();
        let mut calls: Vec<(String, String, String)> = Vec::new(); // id, name, json args
        let mut usage = Usage::default();
        let mut stop = StopReason::Stop;
        let events = self.events.clone();

        {
            let mut sink = |ev: StreamEvent| match ev {
                StreamEvent::TextDelta(t) => {
                    let _ = events.send(AgentEvent::TextDelta(t.clone()));
                    text.push_str(&t);
                }
                StreamEvent::ThinkingDelta(t) => {
                    let _ = events.send(AgentEvent::ThinkingDelta(t.clone()));
                    thinking.push_str(&t);
                }
                StreamEvent::ToolCallStart { id, name } => calls.push((id, name, String::new())),
                StreamEvent::ToolCallDelta(chunk) => {
                    if let Some(last) = calls.last_mut() {
                        last.2.push_str(&chunk);
                    }
                }
                StreamEvent::ToolCallEnd => {}
                StreamEvent::Usage(u) => {
                    // Providers report input and output in separate events;
                    // keep the larger of each rather than overwriting.
                    usage.input = usage.input.max(u.input);
                    usage.output = usage.output.max(u.output);
                    usage.cache_read = usage.cache_read.max(u.cache_read);
                    usage.cache_write = usage.cache_write.max(u.cache_write);
                }
                StreamEvent::Done(r) => stop = r,
            };

            let req = Request {
                system: &self.cfg.agent.system_prompt,
                messages: &self.session.messages,
                tools: &self.schemas,
                max_tokens: self.cfg.model.max_tokens,
                temperature: self.cfg.model.temperature,
                stream_usage: self.cfg.model.stream_usage,
            };

            tokio::select! {
                r = self.provider.stream_with_retry(&req, &mut sink) => r?,
                _ = cancel.cancelled() => stop = StopReason::Aborted,
            }
        }

        let mut content = Vec::new();
        if !thinking.is_empty() {
            content.push(ContentBlock::Thinking { text: thinking, signature: None });
        }
        if !text.is_empty() {
            content.push(ContentBlock::text(text));
        }
        for (id, name, args) in calls {
            // A model can emit malformed or empty arguments; an empty object is
            // a better guess than dropping the call, and the tool will report
            // the missing field itself.
            let input = serde_json::from_str(args.trim())
                .unwrap_or_else(|_| serde_json::json!({}));
            content.push(ContentBlock::ToolCall { id, name, input });
        }
        if content.iter().any(|b| matches!(b, ContentBlock::ToolCall { .. })) {
            stop = StopReason::ToolUse;
        }

        Ok(Message::Assistant { content, usage, stop_reason: stop, error: None })
    }

    /// Run each requested tool in order and append its result.
    ///
    /// Sequential rather than parallel: on a TX2 two concurrent `exec` calls
    /// contend for the same cores and the ordering guarantee is worth more than
    /// the overlap.
    async fn execute_tools(
        &mut self,
        calls: &[ContentBlock],
        cancel: &CancellationToken,
    ) -> Result<()> {
        for call in calls {
            let ContentBlock::ToolCall { id, name, input } = call else { continue };

            if cancel.is_cancelled() {
                self.session.append(Message::ToolResult {
                    tool_call_id: id.clone(),
                    tool_name: name.clone(),
                    content: "cancelled before execution".into(),
                    is_error: true,
                    blob: None,
                })?;
                continue;
            }

            self.emit(AgentEvent::ToolStart {
                name: name.clone(),
                input: summarize_input(input),
            });

            let out = match tools::find(name) {
                Some(spec) => (spec.run)(input.clone(), &self.ctx).await,
                None => tools::ToolOutput::err(format!(
                    "unknown tool `{name}`; available: {}",
                    tools::TOOLS.iter().map(|t| t.name).collect::<Vec<_>>().join(", ")
                )),
            };

            let (content, blob) = self.session.spill_tool_output(
                id,
                &out.content,
                self.cfg.agent.max_tool_output_bytes,
            );

            self.emit(AgentEvent::ToolEnd {
                name: name.clone(),
                is_error: out.is_error,
                preview: content.chars().take(200).collect(),
            });

            self.session.append(Message::ToolResult {
                tool_call_id: id.clone(),
                tool_name: name.clone(),
                content,
                is_error: out.is_error,
                blob,
            })?;
        }
        Ok(())
    }

    async fn maybe_compact(&mut self) -> Result<()> {
        let used = compact::context_tokens(&self.session.messages, &self.last_usage);
        if !compact::should_compact(used, self.cfg.model.context_window) {
            return Ok(());
        }

        let cut = compact::find_cut_point(&self.session.messages);
        if cut == 0 {
            // Nothing to drop; compacting again would loop.
            return Ok(());
        }

        let summary =
            match compact::generate_summary(&self.provider, &self.session.messages[..cut]).await {
                Ok(s) => s,
                Err(e) => {
                    // A failed compaction must not abort the turn: the request
                    // may still fit, and if it does not the provider will say so.
                    self.emit(AgentEvent::Error(format!("compaction failed: {e:#}")));
                    return Ok(());
                }
            };

        let compacted = compact::apply(&summary, &self.session.messages, cut);
        self.session.record_compaction(&summary, cut, used)?;
        self.session.rewrite(compacted)?;
        // The old counts describe a transcript that no longer exists.
        self.last_usage = Usage::default();
        self.emit(AgentEvent::Compacted { dropped: cut });
        Ok(())
    }
}

/// A short, single-line rendering of tool input for progress display.
pub fn summarize_input(input: &serde_json::Value) -> String {
    let s = match input {
        serde_json::Value::Object(map) => map
            .iter()
            .map(|(k, v)| match v {
                serde_json::Value::String(s) => format!("{k}={s}"),
                other => format!("{k}={other}"),
            })
            .collect::<Vec<_>>()
            .join(" "),
        other => other.to_string(),
    };
    let one_line = s.replace('\n', "⏎");
    one_line.chars().take(120).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_input_is_rendered_on_one_line_and_bounded() {
        let v = serde_json::json!({"command": "echo a\nb", "cwd": "/tmp"});
        let s = summarize_input(&v);
        assert!(!s.contains('\n'), "progress lines must not wrap the display");
        assert!(s.contains("echo a"));

        let long = serde_json::json!({"content": "x".repeat(1000)});
        assert!(summarize_input(&long).chars().count() <= 120);
    }

    #[test]
    fn non_object_tool_input_still_renders() {
        assert_eq!(summarize_input(&serde_json::json!("bare")), "\"bare\"");
    }
}
