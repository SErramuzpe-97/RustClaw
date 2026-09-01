//! Shell execution. Mirrors IronClaw's `first_party_tools/shell.rs`: a hard
//! timeout and a bounded capture, so a runaway command cannot hang the turn or
//! exhaust the Jetson's memory.

use super::{Ctx, ToolOutput, ToolSpec, arg_str};
use serde_json::{Value, json};
use tokio::process::Command;

/// Captured output above this is truncated in the middle: the head shows what
/// ran, the tail shows how it ended, and the middle is rarely what matters.
const MAX_CAPTURE: usize = 32 * 1024;

pub const EXEC: ToolSpec = ToolSpec {
    name: "exec",
    description: "Run a shell command and return its stdout, stderr and exit code. \
Use this for anything the other tools do not cover. Prefer `glob` and `grep` for \
finding files and searching contents.",
    schema: || {
        json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": "Shell command to run."},
                "cwd": {"type": "string", "description": "Working directory. Defaults to the session directory."}
            },
            "required": ["command"]
        })
    },
    run: |args, ctx| Box::pin(run(args, ctx)),
};

async fn run(args: Value, ctx: &Ctx) -> ToolOutput {
    let command = match arg_str(&args, "command") {
        Ok(c) => c,
        Err(e) => return e,
    };
    let cwd = args
        .get("cwd")
        .and_then(Value::as_str)
        .map(|d| super::resolve(ctx, d))
        .unwrap_or_else(|| ctx.cwd.clone());

    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c").arg(command).current_dir(&cwd).kill_on_drop(true);

    let output = match tokio::time::timeout(ctx.exec_timeout, cmd.output()).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return ToolOutput::err(format!("failed to spawn command: {e}")),
        Err(_) => {
            return ToolOutput::err(format!(
                "command timed out after {}s and was killed",
                ctx.exec_timeout.as_secs()
            ));
        }
    };

    let code = output.status.code().unwrap_or(-1);
    let stdout = truncate_middle(&String::from_utf8_lossy(&output.stdout), MAX_CAPTURE);
    let stderr = truncate_middle(&String::from_utf8_lossy(&output.stderr), MAX_CAPTURE);

    let mut body = String::new();
    if !stdout.is_empty() {
        body.push_str(&stdout);
    }
    if !stderr.is_empty() {
        if !body.is_empty() {
            body.push('\n');
        }
        body.push_str("stderr:\n");
        body.push_str(&stderr);
    }
    if body.is_empty() {
        body.push_str("(no output)");
    }
    if code != 0 {
        body.push_str(&format!("\n\nexit code: {code}"));
    }

    // A non-zero exit is reported to the model as an error result so it does
    // not read a failure as success.
    ToolOutput { content: body, is_error: code != 0 }
}

/// Keep the head and the tail, drop the middle, and say how much was dropped.
pub(crate) fn truncate_middle(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let half = max / 2;
    let head_end = floor_char_boundary(s, half);
    let tail_start = ceil_char_boundary(s, s.len() - half);
    let omitted = tail_start - head_end;
    format!(
        "{}\n\n... [{omitted} bytes omitted] ...\n\n{}",
        &s[..head_end],
        &s[tail_start..]
    )
}

fn floor_char_boundary(s: &str, mut i: usize) -> usize {
    i = i.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_char_boundary(s: &str, mut i: usize) -> usize {
    i = i.min(s.len());
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> Ctx {
        Ctx {
            cwd: std::env::temp_dir(),
            exec_timeout: std::time::Duration::from_secs(10),
            http: reqwest::Client::new(),
        }
    }

    #[test]
    fn truncation_keeps_both_ends_and_respects_char_boundaries() {
        let s = "á".repeat(10_000);
        let out = truncate_middle(&s, 1000);
        assert!(out.starts_with('á'));
        assert!(out.ends_with('á'));
        assert!(out.contains("bytes omitted"));
        // The real test: slicing a multi-byte string must not panic, and the
        // result must still be valid UTF-8.
        assert!(out.is_char_boundary(0));
    }

    #[test]
    fn short_output_is_returned_untouched() {
        assert_eq!(truncate_middle("hello", 1000), "hello");
    }

    #[tokio::test]
    async fn reports_stdout_and_success() {
        let out = run(json!({"command": "echo hola"}), &ctx()).await;
        assert!(!out.is_error);
        assert!(out.content.contains("hola"));
    }

    #[tokio::test]
    async fn a_non_zero_exit_is_flagged_as_an_error_with_its_code() {
        let out = run(json!({"command": "echo oops >&2; exit 3"}), &ctx()).await;
        assert!(out.is_error, "non-zero exit must not read as success");
        assert!(out.content.contains("exit code: 3"));
        assert!(out.content.contains("oops"));
    }

    #[tokio::test]
    async fn a_hanging_command_is_killed_at_the_timeout() {
        let c = Ctx { exec_timeout: std::time::Duration::from_millis(150), ..ctx() };
        let out = run(json!({"command": "sleep 30"}), &c).await;
        assert!(out.is_error);
        assert!(out.content.contains("timed out"));
    }

    #[tokio::test]
    async fn missing_command_argument_is_reported_not_panicked() {
        let out = run(json!({}), &ctx()).await;
        assert!(out.is_error);
        assert!(out.content.contains("command"));
    }
}
