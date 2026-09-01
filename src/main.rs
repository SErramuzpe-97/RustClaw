//! RustClaw: a lean OpenClaw-style assistant sized for an NVIDIA Jetson.

// musl's mallocng is slow enough to erase the benefit of a static build, so the
// static target gets a real allocator.
#[cfg(target_env = "musl")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod agent;
mod agent_e2e_tests;
mod channels;
mod compact;
mod config;
mod llm;
mod prompt;
mod repl;
mod server;
mod session;
mod tools;
mod types;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "rustclaw", version, about = "Your assistant, on your own hardware")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Chat in the terminal (the default).
    Repl {
        /// Resume an existing session instead of starting a new one.
        #[arg(short, long)]
        session: Option<String>,
    },
    /// Serve the web UI and HTTP API.
    Serve {
        #[arg(short, long)]
        port: Option<u16>,
    },
    /// Run the Telegram bridge.
    Telegram,
    /// Show or initialize the configuration.
    Config {
        /// Write a default config file if none exists.
        #[arg(long)]
        init: bool,
    },
    /// List stored sessions.
    Sessions,
    /// Print the access token, creating one if there is none.
    ///
    /// Required before the server may listen on anything but loopback.
    Token {
        /// Replace the existing token instead of printing it.
        #[arg(long)]
        rotate: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // A current-thread runtime: the workload is IO-bound on the model, so
    // worker threads would only cost stacks and contention. Blocking work
    // (filesystem walks) goes through spawn_blocking.
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    rt.block_on(run(cli))
}

async fn run(cli: Cli) -> Result<()> {
    match cli.command.unwrap_or(Command::Repl { session: None }) {
        Command::Config { init } => {
            let path = config::config_path()?;
            if init {
                if path.exists() {
                    println!("{} already exists", path.display());
                } else {
                    config::Config::write_default(&path)?;
                    println!("wrote {}", path.display());
                }
                return Ok(());
            }
            let cfg = config::Config::load()?;
            println!("# {}\n", path.display());
            print!("{}", toml::to_string_pretty(&cfg)?);
            Ok(())
        }

        Command::Token { rotate } => {
            let path = config::home_dir()?.join("secrets.env");
            let existing = std::fs::read_to_string(&path).unwrap_or_default();
            let current = existing.lines().find_map(|l| {
                l.trim()
                    .strip_prefix("export RUSTCLAW_TOKEN=")
                    .map(|v| v.trim_matches('"').to_string())
            });

            let token = match (&current, rotate) {
                (Some(t), false) => t.clone(),
                _ => {
                    let fresh = server::generate_token()?;
                    // Rewrite the file without the old line, then append.
                    let kept: Vec<&str> = existing
                        .lines()
                        .filter(|l| !l.trim().starts_with("export RUSTCLAW_TOKEN="))
                        .collect();
                    let mut body = kept.join("\n");
                    if !body.is_empty() && !body.ends_with('\n') {
                        body.push('\n');
                    }
                    body.push_str(&format!("export RUSTCLAW_TOKEN=\"{fresh}\"\n"));
                    if let Some(parent) = path.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::write(&path, body)?;
                    // The file holds API keys too; keep it owner-only.
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
                    }
                    fresh
                }
            };
            println!("{token}");
            eprintln!("stored in {}", path.display());
            Ok(())
        }

        Command::Sessions => {
            let root = config::home_dir()?;
            let metas = session::Session::list(&root);
            if metas.is_empty() {
                println!("no sessions yet");
            }
            for m in metas {
                println!("{:<14} {:>3} msg  {}", m.id, m.messages, m.title);
            }
            Ok(())
        }

        Command::Repl { session: id } => {
            let agent = build_agent(id, None).await?;
            repl::run(agent).await
        }

        Command::Serve { port } => {
            let agent = build_agent(None, port).await?;
            server::run(agent).await
        }

        Command::Telegram => {
            let agent = build_agent(None, None).await?;
            channels::telegram::run(agent).await
        }
    }
}

async fn build_agent(session_id: Option<String>, port: Option<u16>) -> Result<agent::Agent> {
    let mut cfg = config::Config::load()?;
    if let Some(p) = port {
        cfg.server.port = p;
    }
    let root = config::home_dir()?;
    let id = session_id.unwrap_or_else(session::Session::new_id);
    let session = session::Session::open(&root, &id)
        .with_context(|| format!("opening session {id}"))?;
    agent::Agent::new(cfg, session)
}
