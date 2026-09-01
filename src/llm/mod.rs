//! Model access.
//!
//! Two backends behind an enum rather than a trait object: with only two
//! implementations, static dispatch keeps the hot path free of boxed futures.
//! IronClaw stacks six decorators (retry, failover, circuit breaker, cache,
//! smart routing, recording) around its provider; a single-user deployment
//! needs only the retry.

pub mod anthropic;
pub mod openai;

use crate::config::{Backend, ModelConfig};
use crate::types::{Message, StreamEvent};
use anyhow::{Result, bail};
use std::time::Duration;

/// A tool as advertised to the model.
#[derive(Debug, Clone)]
pub struct ToolSchema {
    pub name: &'static str,
    pub description: &'static str,
    pub input_schema: serde_json::Value,
}

pub struct Request<'a> {
    pub system: &'a str,
    pub messages: &'a [Message],
    pub tools: &'a [ToolSchema],
    pub max_tokens: u32,
    /// `None` omits the parameter. Required for current Anthropic models,
    /// which reject any sampling parameter.
    pub temperature: Option<f32>,
}

/// Sink for streaming deltas. A plain `FnMut` rather than a trait object: the
/// REPL prints, the server forwards to a broadcast channel, and compaction
/// discards.
pub type EventSink<'a> = &'a mut (dyn FnMut(StreamEvent) + Send);

pub enum Provider {
    Anthropic(anthropic::Client),
    OpenaiCompat(openai::Client),
}

impl Provider {
    pub fn new(cfg: &ModelConfig) -> Result<Self> {
        // One shared HTTP client: connection reuse matters more than usual when
        // every turn makes several round-trips to the same host.
        let http = reqwest::Client::builder()
            .user_agent(concat!("rustclaw/", env!("CARGO_PKG_VERSION")))
            // No total-request timeout: a long generation is not a hang. The
            // read timeout below catches an actually-dead stream.
            .read_timeout(Duration::from_secs(120))
            .connect_timeout(Duration::from_secs(15))
            .pool_idle_timeout(Duration::from_secs(90))
            .build()?;
        Ok(match cfg.backend {
            Backend::Anthropic => Self::Anthropic(anthropic::Client::new(http, cfg)?),
            Backend::OpenaiCompat => Self::OpenaiCompat(openai::Client::new(http, cfg)),
        })
    }

    pub fn model_name(&self) -> &str {
        match self {
            Self::Anthropic(c) => &c.model,
            Self::OpenaiCompat(c) => &c.model,
        }
    }

    /// Stream one assistant response, invoking `sink` for each delta.
    ///
    /// Providers report failure by returning `Err`; the caller turns that into
    /// an assistant message with `StopReason::Error` so the transcript stays
    /// well-formed.
    pub async fn stream(&self, req: &Request<'_>, sink: EventSink<'_>) -> Result<()> {
        match self {
            Self::Anthropic(c) => c.stream(req, sink).await,
            Self::OpenaiCompat(c) => c.stream(req, sink).await,
        }
    }

    /// `stream` with exponential backoff. Retries transport errors and 429/5xx;
    /// an auth failure or an over-long context will not improve on a retry.
    pub async fn stream_with_retry(&self, req: &Request<'_>, sink: EventSink<'_>) -> Result<()> {
        const MAX_ATTEMPTS: u32 = 3;
        let mut delay = Duration::from_millis(500);

        for attempt in 1..=MAX_ATTEMPTS {
            // Deltas already handed to the caller must not be replayed, so the
            // sink is only armed once we know this attempt produced output.
            let mut emitted = false;
            let mut guarded = |ev: StreamEvent| {
                emitted = true;
                sink(ev);
            };
            match self.stream(req, &mut guarded).await {
                Ok(()) => return Ok(()),
                Err(e) if emitted => {
                    // Partial output is already downstream; retrying would
                    // duplicate it.
                    return Err(e);
                }
                Err(e) if attempt == MAX_ATTEMPTS || !is_retryable(&e) => return Err(e),
                Err(e) => {
                    let wait = retry_after(&e).unwrap_or(delay);
                    tracing_note(&format!("model call failed ({e}); retrying in {wait:?}"));
                    tokio::time::sleep(wait).await;
                    delay *= 2;
                }
            }
        }
        bail!("exhausted retries")
    }
}

/// Marker error carrying a server-supplied `Retry-After`.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct ApiError {
    pub status: Option<u16>,
    pub message: String,
    pub retry_after: Option<Duration>,
}

fn is_retryable(e: &anyhow::Error) -> bool {
    if let Some(api) = e.downcast_ref::<ApiError>() {
        return match api.status {
            // 408 timeout, 409 conflict, 429 rate limit, 5xx server-side.
            Some(s) => s == 408 || s == 409 || s == 429 || (500..600).contains(&s),
            None => true,
        };
    }
    if let Some(re) = e.downcast_ref::<reqwest::Error>() {
        return re.is_timeout() || re.is_connect() || re.is_request();
    }
    false
}

fn retry_after(e: &anyhow::Error) -> Option<Duration> {
    e.downcast_ref::<ApiError>()?.retry_after
}

fn tracing_note(msg: &str) {
    eprintln!("\x1b[2mrustclaw: {msg}\x1b[0m");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn api(status: u16) -> anyhow::Error {
        ApiError { status: Some(status), message: "x".into(), retry_after: None }.into()
    }

    #[test]
    fn retries_only_transient_statuses() {
        for s in [429, 500, 502, 503, 408] {
            assert!(is_retryable(&api(s)), "{s} should retry");
        }
        for s in [400, 401, 403, 404, 422] {
            assert!(!is_retryable(&api(s)), "{s} should not retry");
        }
    }

    #[test]
    fn honours_server_supplied_retry_after() {
        let e: anyhow::Error = ApiError {
            status: Some(429),
            message: "slow down".into(),
            retry_after: Some(Duration::from_secs(7)),
        }
        .into();
        assert_eq!(retry_after(&e), Some(Duration::from_secs(7)));
    }
}
