//! `read`, `write` and `ls`.

use super::{Ctx, ToolOutput, ToolSpec, arg_str, resolve};
use serde_json::{Value, json};

/// Files above this are refused rather than pulled into the context window.
const MAX_READ_BYTES: u64 = 1024 * 1024;
const DEFAULT_READ_LINES: usize = 2000;

pub const READ: ToolSpec = ToolSpec {
    name: "read",
    description: "Read a text file. Returns numbered lines so they can be referenced later. \
Use `offset` and `limit` to page through a large file.",
    schema: || {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File to read."},
                "offset": {"type": "integer", "description": "First line to return (1-based)."},
                "limit": {"type": "integer", "description": "Maximum lines to return."}
            },
            "required": ["path"]
        })
    },
    run: |args, ctx| Box::pin(read(args, ctx)),
};

async fn read(args: Value, ctx: &Ctx) -> ToolOutput {
    let path = match arg_str(&args, "path") {
        Ok(p) => p,
        Err(e) => return e,
    };
    let path = resolve(ctx, path);

    match tokio::fs::metadata(&path).await {
        Ok(m) if m.is_dir() => {
            return ToolOutput::err(format!("{} is a directory; use `ls`", path.display()));
        }
        Ok(m) if m.len() > MAX_READ_BYTES => {
            return ToolOutput::err(format!(
                "{} is {} bytes, over the {MAX_READ_BYTES} byte limit; read it in chunks with \
                 `offset`/`limit` or filter it with `grep`",
                path.display(),
                m.len()
            ));
        }
        Ok(_) => {}
        Err(e) => return ToolOutput::err(format!("cannot read {}: {e}", path.display())),
    }

    let body = match tokio::fs::read(&path).await {
        Ok(b) => b,
        Err(e) => return ToolOutput::err(format!("cannot read {}: {e}", path.display())),
    };
    // Binary files would otherwise arrive as replacement characters and waste
    // a large slice of the context window.
    if body.contains(&0) {
        return ToolOutput::err(format!("{} looks like a binary file", path.display()));
    }
    let text = String::from_utf8_lossy(&body);

    let offset = args.get("offset").and_then(Value::as_u64).unwrap_or(1).max(1) as usize;
    let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(DEFAULT_READ_LINES as u64)
        as usize;

    let total = text.lines().count();
    let mut out = String::new();
    for (i, line) in text.lines().enumerate().skip(offset - 1).take(limit) {
        out.push_str(&format!("{:>6}\t{}\n", i + 1, line));
    }
    if out.is_empty() {
        return ToolOutput::ok(if total == 0 {
            format!("{} is empty", path.display())
        } else {
            format!("no lines at offset {offset} ({total} lines total)")
        });
    }
    let shown = offset - 1 + out.lines().count();
    if shown < total {
        out.push_str(&format!("\n[showing lines {offset}-{shown} of {total}]"));
    }
    ToolOutput::ok(out)
}

pub const WRITE: ToolSpec = ToolSpec {
    name: "write",
    description: "Write text to a file, creating parent directories and overwriting any \
existing content. To change part of a file, prefer `edit`.",
    schema: || {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File to write."},
                "content": {"type": "string", "description": "Full new contents."}
            },
            "required": ["path", "content"]
        })
    },
    run: |args, ctx| Box::pin(write(args, ctx)),
};

async fn write(args: Value, ctx: &Ctx) -> ToolOutput {
    let (path, content) = match (arg_str(&args, "path"), arg_str(&args, "content")) {
        (Ok(p), Ok(c)) => (p, c),
        (Err(e), _) | (_, Err(e)) => return e,
    };
    let path = resolve(ctx, path);
    if let Some(parent) = path.parent()
        && let Err(e) = tokio::fs::create_dir_all(parent).await
    {
        return ToolOutput::err(format!("cannot create {}: {e}", parent.display()));
    }
    match tokio::fs::write(&path, content).await {
        Ok(()) => ToolOutput::ok(format!(
            "wrote {} bytes to {}",
            content.len(),
            path.display()
        )),
        Err(e) => ToolOutput::err(format!("cannot write {}: {e}", path.display())),
    }
}

pub const LS: ToolSpec = ToolSpec {
    name: "ls",
    description: "List the entries of a directory, marking directories with a trailing slash.",
    schema: || {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Directory to list. Defaults to the session directory."}
            }
        })
    },
    run: |args, ctx| Box::pin(ls(args, ctx)),
};

async fn ls(args: Value, ctx: &Ctx) -> ToolOutput {
    let path = args
        .get("path")
        .and_then(Value::as_str)
        .map(|p| resolve(ctx, p))
        .unwrap_or_else(|| ctx.cwd.clone());

    let mut dir = match tokio::fs::read_dir(&path).await {
        Ok(d) => d,
        Err(e) => return ToolOutput::err(format!("cannot list {}: {e}", path.display())),
    };
    let mut entries = Vec::new();
    while let Ok(Some(e)) = dir.next_entry().await {
        let name = e.file_name().to_string_lossy().into_owned();
        let is_dir = e.file_type().await.map(|t| t.is_dir()).unwrap_or(false);
        entries.push(if is_dir { format!("{name}/") } else { name });
    }
    if entries.is_empty() {
        return ToolOutput::ok(format!("{} is empty", path.display()));
    }
    entries.sort_unstable();
    ToolOutput::ok(entries.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(dir: &std::path::Path) -> Ctx {
        Ctx {
            cwd: dir.to_path_buf(),
            exec_timeout: std::time::Duration::from_secs(5),
            http: reqwest::Client::new(),
        }
    }

    /// Per-test directory: these tests run in parallel, so a shared path
    /// would have them deleting each other's fixtures.
    fn tmpdir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("rustclaw-fs-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[tokio::test]
    async fn write_then_read_round_trips_with_line_numbers() {
        let d = tmpdir("roundtrip");
        let c = ctx(&d);
        let w = write(json!({"path": "a/b.txt", "content": "one\ntwo\n"}), &c).await;
        assert!(!w.is_error, "{}", w.content);
        let r = read(json!({"path": "a/b.txt"}), &c).await;
        assert!(r.content.contains("     1\tone"));
        assert!(r.content.contains("     2\ttwo"));
    }

    #[tokio::test]
    async fn read_pages_with_offset_and_limit() {
        let d = tmpdir("paging");
        let c = ctx(&d);
        let body: String = (1..=100).map(|i| format!("line{i}\n")).collect();
        write(json!({"path": "big.txt", "content": body}), &c).await;
        let r = read(json!({"path": "big.txt", "offset": 50, "limit": 2}), &c).await;
        assert!(r.content.contains("line50"));
        assert!(r.content.contains("line51"));
        assert!(!r.content.contains("line52"));
        assert!(r.content.contains("of 100"));
    }

    #[tokio::test]
    async fn reading_a_directory_points_at_ls_instead_of_failing_opaquely() {
        let d = tmpdir("isdir");
        let r = read(json!({"path": "."}), &ctx(&d)).await;
        assert!(r.is_error);
        assert!(r.content.contains("use `ls`"));
    }

    #[tokio::test]
    async fn binary_files_are_refused() {
        let d = tmpdir("binary");
        std::fs::write(d.join("bin"), [0u8, 1, 2, 0]).unwrap();
        let r = read(json!({"path": "bin"}), &ctx(&d)).await;
        assert!(r.is_error);
        assert!(r.content.contains("binary"));
    }

    #[tokio::test]
    async fn ls_marks_directories() {
        let d = tmpdir("ls");
        std::fs::create_dir_all(d.join("sub")).unwrap();
        std::fs::write(d.join("f.txt"), "x").unwrap();
        let out = ls(json!({}), &ctx(&d)).await;
        assert!(out.content.contains("sub/"));
        assert!(out.content.contains("f.txt"));
    }

    #[tokio::test]
    async fn missing_file_is_an_error_result_not_a_panic() {
        let d = tmpdir("missing");
        let r = read(json!({"path": "nope.txt"}), &ctx(&d)).await;
        assert!(r.is_error);
    }
}
