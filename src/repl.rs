//! Terminal chat.
//!
//! No TUI framework. IronClaw's REPL is a plain `BufReader(stdin)` loop and
//! that is the right shape here too: a full-screen framework redraws on every
//! delta, which is real CPU on a Cortex-A57 and buys nothing for a
//! line-oriented chat.

use crate::agent::{Agent, AgentEvent};
use anyhow::Result;
use std::io::Write as _;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio_util::sync::CancellationToken;

const DIM: &str = "\x1b[2m";
const BOLD: &str = "\x1b[1m";
const CYAN: &str = "\x1b[36m";
const RED: &str = "\x1b[31m";
const RESET: &str = "\x1b[0m";

pub async fn run(mut agent: Agent) -> Result<()> {
    println!(
        "{BOLD}rustclaw{RESET} {DIM}v{} · {} · session {}{RESET}",
        env!("CARGO_PKG_VERSION"),
        agent.model_name(),
        agent.session.id
    );
    println!("{DIM}Type your message. /help for commands, Ctrl-C to interrupt, /quit to exit.{RESET}\n");

    let mut lines = BufReader::new(tokio::io::stdin()).lines();

    loop {
        print!("{CYAN}› {RESET}");
        std::io::stdout().flush()?;

        let Some(line) = lines.next_line().await? else { break };
        let input = line.trim();
        if input.is_empty() {
            continue;
        }

        match input {
            "/quit" | "/exit" => break,
            "/help" => {
                println!("{DIM}/quit   end the session");
                println!("/new    start a fresh session");
                println!("/tools  list available tools");
                println!("/usage  token usage of the last turn{RESET}");
                continue;
            }
            "/tools" => {
                for t in crate::tools::TOOLS {
                    println!("{DIM}{:<10}{RESET} {}", t.name, first_sentence(t.description));
                }
                continue;
            }
            _ => {}
        }

        // Ctrl-C cancels the turn in flight rather than killing the process.
        let cancel = CancellationToken::new();
        let printer = spawn_printer(agent.subscribe());
        let interrupt = {
            let cancel = cancel.clone();
            tokio::spawn(async move {
                if tokio::signal::ctrl_c().await.is_ok() {
                    cancel.cancel();
                }
            })
        };

        let result = agent.run_turn(input.to_string(), cancel.clone()).await;
        interrupt.abort();
        // Let the printer drain the events already queued before we redraw the
        // prompt over them.
        printer.await.ok();

        match result {
            Ok(_) => println!(),
            Err(e) => println!("{RED}error:{RESET} {e:#}\n"),
        }
    }

    println!("{DIM}bye{RESET}");
    Ok(())
}

/// Render events as they arrive. Runs as its own task so streaming output does
/// not block the turn.
fn spawn_printer(
    mut rx: tokio::sync::broadcast::Receiver<AgentEvent>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut streaming = false;
        loop {
            match rx.recv().await {
                Ok(AgentEvent::TextDelta(t)) => {
                    print!("{t}");
                    let _ = std::io::stdout().flush();
                    streaming = true;
                }
                Ok(AgentEvent::ToolStart { name, input }) => {
                    if streaming {
                        println!();
                        streaming = false;
                    }
                    println!("{DIM}  ⚙ {name} {input}{RESET}");
                }
                Ok(AgentEvent::ToolEnd { name, is_error, preview }) => {
                    let mark = if is_error { format!("{RED}✗{RESET}") } else { format!("{DIM}✓{RESET}") };
                    println!("{DIM}  {mark}{DIM} {name}: {}{RESET}", one_line(&preview));
                }
                Ok(AgentEvent::Compacted { dropped }) => {
                    println!("{DIM}  ⓘ compacted context ({dropped} messages summarized){RESET}");
                }
                Ok(AgentEvent::Error(e)) => {
                    if streaming {
                        println!();
                        streaming = false;
                    }
                    println!("{RED}  ✗ {e}{RESET}");
                }
                Ok(AgentEvent::TurnEnd { usage }) => {
                    if streaming {
                        println!();
                    }
                    if usage.input > 0 || usage.output > 0 {
                        println!(
                            "{DIM}  {} in / {} out{}{RESET}",
                            usage.input,
                            usage.output,
                            if usage.cache_read > 0 {
                                format!(" / {} cached", usage.cache_read)
                            } else {
                                String::new()
                            }
                        );
                    }
                    return;
                }
                // The turn produced no TurnEnd (an early error path); stop
                // rather than hang the prompt.
                Ok(_) => {}
                Err(_) => return,
            }
        }
    })
}

fn one_line(s: &str) -> String {
    s.replace('\n', " ").chars().take(100).collect()
}

fn first_sentence(s: &str) -> String {
    s.split_once(". ").map(|(a, _)| format!("{a}.")).unwrap_or_else(|| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_previews_are_flattened_and_bounded() {
        let s = one_line(&format!("a\nb{}", "c".repeat(500)));
        assert!(!s.contains('\n'));
        assert!(s.chars().count() <= 100);
    }

    #[test]
    fn help_text_shows_only_the_first_sentence_of_a_description() {
        assert_eq!(first_sentence("Does a thing. And more detail here."), "Does a thing.");
        assert_eq!(first_sentence("No period"), "No period");
    }
}
