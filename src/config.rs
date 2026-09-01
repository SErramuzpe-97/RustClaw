//! TOML config at `~/.rustclaw/config.toml`.
//!
//! Precedence follows IronClaw's: compiled defaults < config.toml < env vars.
//! API keys are read from the environment only, so the config file stays safe
//! to copy around.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Backend {
    /// Anthropic Messages API.
    Anthropic,
    /// Any OpenAI-compatible `/v1/chat/completions` endpoint: llama.cpp server,
    /// ollama, vLLM, Groq, OpenRouter, DeepSeek.
    OpenaiCompat,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelConfig {
    pub backend: Backend,
    pub base_url: String,
    pub model: String,
    /// Name of the env var holding the API key. Empty means no auth, which is
    /// the normal case for a local llama.cpp or ollama server.
    pub api_key_env: String,
    pub max_tokens: u32,
    pub context_window: u32,
    /// Omitted from the request when unset. Leave it unset for Anthropic:
    /// Opus 5, Sonnet 5 and the 4.6+ family removed sampling parameters and
    /// return 400 if one is sent. Local servers accept it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
}

impl Default for ModelConfig {
    fn default() -> Self {
        // Defaults point at a local ollama so a fresh install works offline on
        // the Jetson with no key and no network.
        Self {
            backend: Backend::OpenaiCompat,
            base_url: "http://127.0.0.1:11434/v1".into(),
            model: "qwen2.5:3b".into(),
            api_key_env: String::new(),
            max_tokens: 4096,
            context_window: 32_768,
            temperature: None,
        }
    }
}

impl ModelConfig {
    pub fn api_key(&self) -> Option<String> {
        if self.api_key_env.is_empty() {
            return None;
        }
        std::env::var(&self.api_key_env).ok().filter(|v| !v.is_empty())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AgentConfig {
    pub system_prompt: String,
    /// Hard cap on model round-trips per turn, so a tool loop cannot spin forever.
    pub max_iterations: u32,
    /// Seconds before an `exec` tool call is killed.
    pub exec_timeout_secs: u64,
    /// Tool output beyond this many bytes is spilled to disk and replaced with
    /// a head/tail preview.
    pub max_tool_output_bytes: usize,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            system_prompt: crate::prompt::DEFAULT_SYSTEM_PROMPT.into(),
            max_iterations: 50,
            exec_timeout_secs: 120,
            max_tool_output_bytes: 32_768,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    pub bind: String,
    pub port: u16,
}

impl Default for ServerConfig {
    fn default() -> Self {
        // 0.0.0.0 so the WebUI is reachable from the LAN; the Jetson is
        // normally headless.
        Self { bind: "0.0.0.0".into(), port: 8080 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TelegramConfig {
    pub token_env: String,
    /// Chat ids allowed to talk to the agent. Empty allows everyone.
    pub allowed_chat_ids: Vec<i64>,
    /// Long-poll timeout handed to `getUpdates`.
    pub poll_timeout_secs: u32,
}

impl Default for TelegramConfig {
    fn default() -> Self {
        Self {
            token_env: "TELEGRAM_BOT_TOKEN".into(),
            allowed_chat_ids: Vec::new(),
            poll_timeout_secs: 50,
        }
    }
}

impl TelegramConfig {
    pub fn token(&self) -> Option<String> {
        std::env::var(&self.token_env).ok().filter(|v| !v.is_empty())
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub model: ModelConfig,
    pub agent: AgentConfig,
    pub server: ServerConfig,
    pub telegram: TelegramConfig,
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = config_path()?;
        let mut cfg = if path.exists() {
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?
        } else {
            Self::default()
        };
        cfg.apply_env();
        Ok(cfg)
    }

    /// Env overrides, applied after the file so they always win.
    fn apply_env(&mut self) {
        if let Ok(v) = std::env::var("RUSTCLAW_BASE_URL") {
            self.model.base_url = v;
        }
        if let Ok(v) = std::env::var("RUSTCLAW_MODEL") {
            self.model.model = v;
        }
        if let Ok(v) = std::env::var("RUSTCLAW_PORT")
            && let Ok(p) = v.parse()
        {
            self.server.port = p;
        }
    }

    pub fn write_default(path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let toml = toml::to_string_pretty(&Self::default())?;
        std::fs::write(path, toml).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }
}

/// `~/.rustclaw`, overridable with `RUSTCLAW_HOME`.
pub fn home_dir() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("RUSTCLAW_HOME") {
        return Ok(PathBuf::from(dir));
    }
    let home = std::env::var("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".rustclaw"))
}

pub fn config_path() -> Result<PathBuf> {
    Ok(home_dir()?.join("config.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_round_trips_through_toml() {
        let toml = toml::to_string_pretty(&Config::default()).unwrap();
        let parsed: Config = toml::from_str(&toml).unwrap();
        assert_eq!(parsed.model.backend, Backend::OpenaiCompat);
        assert_eq!(parsed.server.port, 8080);
    }

    #[test]
    fn unknown_keys_are_rejected_rather_than_silently_ignored() {
        let err = toml::from_str::<Config>("[model]\nnot_a_real_key = 1\n").unwrap_err();
        assert!(err.to_string().contains("not_a_real_key"), "got: {err}");
    }

    #[test]
    fn absent_api_key_env_means_no_auth() {
        let cfg = ModelConfig::default();
        assert!(cfg.api_key_env.is_empty());
        assert!(cfg.api_key().is_none());
    }
}
