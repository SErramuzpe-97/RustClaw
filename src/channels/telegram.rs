//! Telegram bridge over `getUpdates` long-polling.
//!
//! Hand-rolled rather than using teloxide: this is the whole bridge in a few
//! hundred lines against 62k in OpenClaw's `extensions/telegram`. Long-polling
//! also means no inbound port, no TLS certificate and no public IP — which
//! matters for a Jetson sitting behind NAT.

use crate::agent::Agent;
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Telegram rejects messages over 4096 characters.
const MAX_MESSAGE_CHARS: usize = 4000;

pub async fn run(mut agent: Agent) -> Result<()> {
    let cfg = {
        let c = agent.telegram_config();
        c.clone()
    };
    let token = cfg.token().with_context(|| {
        format!("set {} to your bot token (config: telegram.token_env)", cfg.token_env)
    })?;

    let http = reqwest::Client::builder()
        // Must exceed the long-poll timeout or every poll would abort.
        .timeout(Duration::from_secs(cfg.poll_timeout_secs as u64 + 15))
        .build()?;
    let api = format!("https://api.telegram.org/bot{token}");

    let me = get_me(&http, &api).await?;
    println!("rustclaw: telegram bridge live as @{me}");
    if cfg.allowed_chat_ids.is_empty() {
        println!("rustclaw: any chat may talk to this bot (telegram.allowed_chat_ids is empty)");
    }

    let mut offset: i64 = 0;
    loop {
        let updates = match get_updates(&http, &api, offset, cfg.poll_timeout_secs).await {
            Ok(u) => u,
            Err(e) => {
                // A transient network blip must not end the bridge.
                eprintln!("rustclaw: getUpdates failed: {e:#}; retrying in 5s");
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
        };

        for update in updates {
            if let Some(id) = update.get("update_id").and_then(Value::as_i64) {
                offset = offset.max(id + 1);
            }
            let Some(msg) = update.get("message") else { continue };
            let Some(chat_id) = msg.get("chat").and_then(|c| c.get("id")).and_then(Value::as_i64)
            else {
                continue;
            };
            let Some(text) = msg.get("text").and_then(Value::as_str) else {
                let _ = send(&http, &api, chat_id, "I can only read text messages.").await;
                continue;
            };

            if !cfg.allowed_chat_ids.is_empty() && !cfg.allowed_chat_ids.contains(&chat_id) {
                eprintln!("rustclaw: ignoring chat {chat_id} (not in allowed_chat_ids)");
                continue;
            }

            // Typing indicators expire after ~5s, so refresh while the turn runs.
            let typing = spawn_typing(http.clone(), api.clone(), chat_id);
            let reply = agent.run_turn(text.to_string(), CancellationToken::new()).await;
            typing.abort();

            let body = match reply {
                Ok(r) if r.trim().is_empty() => "(no reply)".to_string(),
                Ok(r) => r,
                Err(e) => format!("error: {e:#}"),
            };
            for chunk in split_message(&body) {
                if let Err(e) = send(&http, &api, chat_id, &chunk).await {
                    eprintln!("rustclaw: sendMessage failed: {e:#}");
                    break;
                }
            }
        }
    }
}

fn spawn_typing(http: reqwest::Client, api: String, chat_id: i64) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let _ = http
                .post(format!("{api}/sendChatAction"))
                .json(&serde_json::json!({"chat_id": chat_id, "action": "typing"}))
                .send()
                .await;
            tokio::time::sleep(Duration::from_secs(4)).await;
        }
    })
}

async fn get_me(http: &reqwest::Client, api: &str) -> Result<String> {
    let v: Value = http.get(format!("{api}/getMe")).send().await?.json().await?;
    if v.get("ok").and_then(Value::as_bool) != Some(true) {
        bail!("getMe rejected the token: {v}");
    }
    Ok(v["result"]["username"].as_str().unwrap_or("unknown").to_string())
}

async fn get_updates(
    http: &reqwest::Client,
    api: &str,
    offset: i64,
    timeout: u32,
) -> Result<Vec<Value>> {
    let v: Value = http
        .post(format!("{api}/getUpdates"))
        .json(&serde_json::json!({
            "offset": offset,
            "timeout": timeout,
            // Only message updates; the rest would be discarded anyway.
            "allowed_updates": ["message"],
        }))
        .send()
        .await?
        .json()
        .await?;
    if v.get("ok").and_then(Value::as_bool) != Some(true) {
        bail!("getUpdates returned {v}");
    }
    Ok(v["result"].as_array().cloned().unwrap_or_default())
}

async fn send(http: &reqwest::Client, api: &str, chat_id: i64, text: &str) -> Result<()> {
    let v: Value = http
        .post(format!("{api}/sendMessage"))
        .json(&serde_json::json!({"chat_id": chat_id, "text": text}))
        .send()
        .await?
        .json()
        .await?;
    if v.get("ok").and_then(Value::as_bool) != Some(true) {
        bail!("sendMessage returned {v}");
    }
    Ok(())
}

/// Split a reply into Telegram-sized chunks, preferring line boundaries so code
/// blocks and lists do not break mid-line.
fn split_message(text: &str) -> Vec<String> {
    if text.chars().count() <= MAX_MESSAGE_CHARS {
        return vec![text.to_string()];
    }
    let mut chunks = Vec::new();
    let mut current = String::new();
    for line in text.split_inclusive('\n') {
        if current.chars().count() + line.chars().count() > MAX_MESSAGE_CHARS
            && !current.is_empty()
        {
            chunks.push(std::mem::take(&mut current));
        }
        // A single line longer than the limit still has to be broken somewhere.
        if line.chars().count() > MAX_MESSAGE_CHARS {
            let mut buf = String::new();
            for ch in line.chars() {
                buf.push(ch);
                if buf.chars().count() == MAX_MESSAGE_CHARS {
                    chunks.push(std::mem::take(&mut buf));
                }
            }
            current.push_str(&buf);
        } else {
            current.push_str(line);
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_reply_is_sent_as_one_message() {
        assert_eq!(split_message("hola"), vec!["hola".to_string()]);
    }

    #[test]
    fn a_long_reply_is_split_within_telegrams_limit() {
        let text: String = (0..1000).map(|i| format!("line {i}\n")).collect();
        let chunks = split_message(&text);
        assert!(chunks.len() > 1);
        for c in &chunks {
            assert!(c.chars().count() <= MAX_MESSAGE_CHARS, "chunk over the limit");
        }
        assert_eq!(chunks.concat(), text, "splitting must not lose or reorder text");
    }

    #[test]
    fn a_single_over_long_line_is_still_broken_up() {
        let text = "x".repeat(MAX_MESSAGE_CHARS * 2 + 50);
        let chunks = split_message(&text);
        assert!(chunks.len() >= 3);
        for c in &chunks {
            assert!(c.chars().count() <= MAX_MESSAGE_CHARS);
        }
        assert_eq!(chunks.concat(), text);
    }

    #[test]
    fn multibyte_text_is_counted_in_characters_not_bytes() {
        let text = "á".repeat(MAX_MESSAGE_CHARS - 1);
        assert_eq!(split_message(&text).len(), 1, "counted bytes instead of chars");
    }
}
