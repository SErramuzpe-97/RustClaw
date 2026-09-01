//! Exact-string replacement, following OpenClaw's
//! `src/agents/sessions/tools/edit.ts`.
//!
//! The uniqueness requirement is the whole point: if `old` appears more than
//! once the edit is ambiguous, and silently picking the first match is how an
//! agent corrupts a file.

use super::{Ctx, ToolOutput, ToolSpec, arg_str, resolve};
use serde_json::{Value, json};

pub const EDIT: ToolSpec = ToolSpec {
    name: "edit",
    description: "Replace an exact string in a file. `old` must appear exactly once, so \
include enough surrounding context to make it unique. Read the file first.",
    schema: || {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File to edit."},
                "old": {"type": "string", "description": "Exact text to replace. Must be unique in the file."},
                "new": {"type": "string", "description": "Replacement text."},
                "replace_all": {"type": "boolean", "description": "Replace every occurrence instead of requiring uniqueness."}
            },
            "required": ["path", "old", "new"]
        })
    },
    run: |args, ctx| Box::pin(edit(args, ctx)),
};

async fn edit(args: Value, ctx: &Ctx) -> ToolOutput {
    let (path, old, new) =
        match (arg_str(&args, "path"), arg_str(&args, "old"), arg_str(&args, "new")) {
            (Ok(p), Ok(o), Ok(n)) => (p, o, n),
            (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => return e,
        };
    if old == new {
        return ToolOutput::err("`old` and `new` are identical; nothing to do");
    }
    let path = resolve(ctx, path);
    let body = match tokio::fs::read_to_string(&path).await {
        Ok(b) => b,
        Err(e) => return ToolOutput::err(format!("cannot read {}: {e}", path.display())),
    };

    let replace_all = args.get("replace_all").and_then(Value::as_bool).unwrap_or(false);
    let count = body.matches(old).count();

    let updated = match count {
        0 => {
            return ToolOutput::err(format!(
                "`old` was not found in {}. Read the file and copy the text exactly, \
                 including indentation.",
                path.display()
            ));
        }
        _ if count > 1 && !replace_all => {
            return ToolOutput::err(format!(
                "`old` appears {count} times in {}. Add surrounding context to make it \
                 unique, or pass replace_all: true.",
                path.display()
            ));
        }
        _ => body.replace(old, new),
    };

    match tokio::fs::write(&path, &updated).await {
        Ok(()) => ToolOutput::ok(format!(
            "replaced {count} occurrence{} in {}",
            if count == 1 { "" } else { "s" },
            path.display()
        )),
        Err(e) => ToolOutput::err(format!("cannot write {}: {e}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("rustclaw-edit-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn ctx(d: &std::path::Path) -> Ctx {
        Ctx {
            cwd: d.to_path_buf(),
            exec_timeout: std::time::Duration::from_secs(5),
            http: reqwest::Client::new(),
        }
    }

    #[tokio::test]
    async fn replaces_a_unique_string() {
        let d = tmpdir("uniq");
        std::fs::write(d.join("f.txt"), "alpha beta gamma").unwrap();
        let out = edit(json!({"path": "f.txt", "old": "beta", "new": "DELTA"}), &ctx(&d)).await;
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(std::fs::read_to_string(d.join("f.txt")).unwrap(), "alpha DELTA gamma");
    }

    #[tokio::test]
    async fn refuses_an_ambiguous_edit_and_leaves_the_file_untouched() {
        let d = tmpdir("ambig");
        std::fs::write(d.join("f.txt"), "x\nx\n").unwrap();
        let out = edit(json!({"path": "f.txt", "old": "x", "new": "y"}), &ctx(&d)).await;
        assert!(out.is_error);
        assert!(out.content.contains("appears 2 times"));
        assert_eq!(std::fs::read_to_string(d.join("f.txt")).unwrap(), "x\nx\n");
    }

    #[tokio::test]
    async fn replace_all_overrides_the_uniqueness_check() {
        let d = tmpdir("all");
        std::fs::write(d.join("f.txt"), "x\nx\n").unwrap();
        let out = edit(
            json!({"path": "f.txt", "old": "x", "new": "y", "replace_all": true}),
            &ctx(&d),
        )
        .await;
        assert!(!out.is_error);
        assert_eq!(std::fs::read_to_string(d.join("f.txt")).unwrap(), "y\ny\n");
    }

    #[tokio::test]
    async fn a_missing_target_string_explains_how_to_fix_it() {
        let d = tmpdir("missing");
        std::fs::write(d.join("f.txt"), "hello").unwrap();
        let out = edit(json!({"path": "f.txt", "old": "nope", "new": "y"}), &ctx(&d)).await;
        assert!(out.is_error);
        assert!(out.content.contains("not found"));
    }

    #[tokio::test]
    async fn a_no_op_edit_is_rejected() {
        let d = tmpdir("noop");
        std::fs::write(d.join("f.txt"), "hello").unwrap();
        let out = edit(json!({"path": "f.txt", "old": "a", "new": "a"}), &ctx(&d)).await;
        assert!(out.is_error);
    }
}
