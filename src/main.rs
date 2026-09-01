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

        Command::Sessions => {
            let root = config::home_dir()?;
            let ids = session::Session::list(&root);
            if ids.is_empty() {
                println!("no sessions yet");
            }
            for id in ids {
                println!("{id}");
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
