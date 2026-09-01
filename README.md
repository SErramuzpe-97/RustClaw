# RustClaw

A personal AI assistant in the shape of [OpenClaw](https://github.com/openclaw/openclaw),
rewritten in Rust to run on an **NVIDIA Jetson TX2** — a single static binary with
no runtime, no Node, and nothing to install on the device.

## Why

| | OpenClaw | IronClaw | RustClaw |
|---|---|---|---|
| Language | TypeScript / Node ≥22 | Rust | Rust |
| Source | 15,851 files in `src/` | 1.35M lines, 70 crates | ~4k lines, 1 crate |
| Dependencies | 66 runtime npm deps | 952 crates | **185 crates** |
| Deploy | Node + npm install | dynamic binary | **one static ELF** |

OpenClaw's design is the reference; IronClaw supplied the Rust engineering worth
copying (SSE handling, no tokenizer, no TUI framework). Both security surfaces —
IronClaw's WASM sandbox and capability kernel, OpenClaw's approval pipeline — are
deliberately absent. **RustClaw runs every tool call it is given.** Point it at
hardware and data you are willing to hand to a model.

## Install

Build a static aarch64 binary on macOS or Linux:

```bash
brew install rustup zig && cargo install cargo-zigbuild   # once
./scripts/build-jetson.sh
scp target/aarch64-unknown-linux-musl/release/rustclaw nvidia@jetson:~/
```

For local development just `cargo run`.

## Use

```bash
rustclaw config --init     # write ~/.rustclaw/config.toml
rustclaw                   # terminal chat (default)
rustclaw serve             # web UI on :8080
rustclaw telegram          # Telegram bot bridge
rustclaw sessions          # list stored transcripts
```

## Configuration

`~/.rustclaw/config.toml`. API keys are read from the environment only, never
from the file.

**A local model on the Jetson** (the default — works offline, no key):

```toml
[model]
backend = "openai-compat"
base_url = "http://127.0.0.1:11434/v1"
model = "qwen2.5:3b"
context_window = 32768
```

The same backend covers Mistral, llama.cpp's `server`, vLLM, Groq, OpenRouter and
DeepSeek. For Mistral:

```toml
[model]
backend = "openai-compat"
base_url = "https://api.mistral.ai/v1"
model = "mistral-large-latest"
api_key_env = "MISTRAL_API_KEY"
context_window = 262144
```

Set `stream_usage = false` for a compatible server that rejects the
`stream_options` field.

**Anthropic:**

```toml
[model]
backend = "anthropic"
base_url = "https://api.anthropic.com/v1"
model = "claude-opus-5"
api_key_env = "ANTHROPIC_API_KEY"
context_window = 1000000
max_tokens = 32000
```

Do **not** set `temperature` for these models: Opus 5, Sonnet 5 and the 4.6+
family removed the sampling parameters and return a 400 if one is sent. The key
is read from the environment; get one from console.anthropic.com. A Claude Code
or Claude.ai subscription login is not an API credential and will not work here.

**Telegram** — set `TELEGRAM_BOT_TOKEN`, then restrict who may talk to it:

```toml
[telegram]
allowed_chat_ids = [123456789]   # empty means anyone
```

## Tools

`exec`, `read`, `write`, `edit`, `ls`, `glob`, `grep`, `web_fetch`.

`glob` and `grep` are native (globset + regex, the engines ripgrep uses) rather
than shelling out, so their output is bounded at the source. `exec` is killed at
`agent.exec_timeout_secs`. Output over `agent.max_tool_output_bytes` is spilled
to `~/.rustclaw/sessions/tool-results/` and the transcript keeps a preview plus a
path.

## How it is kept small

- **`current_thread` Tokio.** The work is IO-bound on the model; worker threads
  would only cost stacks and contention.
- **`mimalloc` on musl.** musl's `mallocng` is slow enough to erase the benefit
  of a static build.
- **Compiled-in TLS roots** (`webpki-roots`). Ubuntu 18.04's CA bundle is stale
  and the device may never be updated.
- **No tokenizer.** Tokens are estimated at chars/4 and corrected from the
  provider's own `usage`, the approach IronClaw takes.
- **No SQLite.** Transcripts are append-only JSONL; there is no SQL engine
  resident in memory.
- **No TUI framework and no SPA.** The REPL is line-oriented ANSI; the web UI is
  one embedded HTML file with no build step.
- **Static dispatch.** Providers and tools are an enum and a slice of function
  pointers, so no boxed future is allocated per tool call.

Build profile: `opt-level = 3`, fat LTO, one codegen unit, `panic = "abort"`,
stripped, tuned for `cortex-a57` (safe on the TX2's Denver2 cluster too).

## Not included

MCP, sandboxing, tool approvals, vector memory, cron, subagents, channels beyond
Telegram, and media generation. The tool and channel layers are shaped so any of
these can be added without touching the loop.

## License

MIT
