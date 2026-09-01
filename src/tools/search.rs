//! `glob` and `grep`, native rather than shelling out.
//!
//! Spawning find/grep would cost a process per call and produce unbounded
//! output; `globset` and `regex` are the engines ripgrep uses and let us cap
//! results at the source.

use super::{Ctx, ToolOutput, ToolSpec, arg_str, resolve};
use globset::{Glob, GlobMatcher};
use regex::Regex;
use serde_json::{Value, json};
use std::path::Path;
use walkdir::{DirEntry, WalkDir};

const MAX_RESULTS: usize = 500;
const MAX_DEPTH: usize = 25;
/// Skipped wholesale: walking these is slow and never what was asked for.
const SKIP_DIRS: &[&str] =
    &[".git", "node_modules", "target", ".venv", "__pycache__", "dist", "build", ".next"];

pub const GLOB: ToolSpec = ToolSpec {
    name: "glob",
    description: "Find files by glob pattern (for example `**/*.rs` or `src/**/test_*.py`). \
Skips .git, node_modules, target and similar directories.",
    schema: || {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Glob pattern, matched against the path relative to the search root."},
                "path": {"type": "string", "description": "Directory to search. Defaults to the session directory."}
            },
            "required": ["pattern"]
        })
    },
    run: |args, ctx| Box::pin(glob(args, ctx)),
};

async fn glob(args: Value, ctx: &Ctx) -> ToolOutput {
    let pattern = match arg_str(&args, "pattern") {
        Ok(p) => p,
        Err(e) => return e,
    };
    let root = args
        .get("path")
        .and_then(Value::as_str)
        .map(|p| resolve(ctx, p))
        .unwrap_or_else(|| ctx.cwd.clone());

    let matcher = match Glob::new(pattern) {
        Ok(g) => g.compile_matcher(),
        Err(e) => return ToolOutput::err(format!("invalid glob `{pattern}`: {e}")),
    };

    // The walk is synchronous and can be slow on a cold page cache; keeping it
    // off the reactor thread means the SSE stream and the Telegram poller stay
    // responsive while it runs.
    let root2 = root.clone();
    let found = tokio::task::spawn_blocking(move || walk_glob(&root2, &matcher))
        .await
        .unwrap_or_default();

    if found.is_empty() {
        return ToolOutput::ok(format!("no files match `{pattern}` under {}", root.display()));
    }
    let truncated = found.len() > MAX_RESULTS;
    let mut out = found.into_iter().take(MAX_RESULTS).collect::<Vec<_>>().join("\n");
    if truncated {
        out.push_str(&format!("\n\n[first {MAX_RESULTS} matches; narrow the pattern for more]"));
    }
    ToolOutput::ok(out)
}

fn walk_glob(root: &Path, matcher: &GlobMatcher) -> Vec<String> {
    let mut found = Vec::new();
    for entry in WalkDir::new(root)
        .max_depth(MAX_DEPTH)
        .into_iter()
        .filter_entry(|e| !is_skipped(e))
        .filter_map(Result::ok)
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = entry.path().strip_prefix(root).unwrap_or(entry.path());
        // Match the relative path so `**/*.rs` behaves as a shell would, and
        // the bare filename so `*.rs` works without a `**/` prefix.
        if matcher.is_match(rel) || rel.file_name().is_some_and(|n| matcher.is_match(n)) {
            found.push(rel.to_string_lossy().into_owned());
            if found.len() > MAX_RESULTS {
                break;
            }
        }
    }
    found.sort_unstable();
    found
}

pub const GREP: ToolSpec = ToolSpec {
    name: "grep",
    description: "Search file contents with a regular expression and return matching lines \
with their file and line number. Optionally restrict the files with a glob.",
    schema: || {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Regular expression to search for."},
                "path": {"type": "string", "description": "Directory to search. Defaults to the session directory."},
                "include": {"type": "string", "description": "Only search files matching this glob, for example `*.rs`."},
                "ignore_case": {"type": "boolean", "description": "Case-insensitive search."}
            },
            "required": ["pattern"]
        })
    },
    run: |args, ctx| Box::pin(grep(args, ctx)),
};

async fn grep(args: Value, ctx: &Ctx) -> ToolOutput {
    let pattern = match arg_str(&args, "pattern") {
        Ok(p) => p,
        Err(e) => return e,
    };
    let root = args
        .get("path")
        .and_then(Value::as_str)
        .map(|p| resolve(ctx, p))
        .unwrap_or_else(|| ctx.cwd.clone());

    let re = match Regex::new(&if args.get("ignore_case").and_then(Value::as_bool).unwrap_or(false)
    {
        format!("(?i){pattern}")
    } else {
        pattern.to_string()
    }) {
        Ok(r) => r,
        Err(e) => return ToolOutput::err(format!("invalid regex `{pattern}`: {e}")),
    };

    let include = match args.get("include").and_then(Value::as_str) {
        Some(g) => match Glob::new(g) {
            Ok(g) => Some(g.compile_matcher()),
            Err(e) => return ToolOutput::err(format!("invalid include glob: {e}")),
        },
        None => None,
    };

    let root2 = root.clone();
    let hits = tokio::task::spawn_blocking(move || walk_grep(&root2, &re, include.as_ref()))
        .await
        .unwrap_or_default();

    if hits.is_empty() {
        return ToolOutput::ok(format!("no matches for `{pattern}` under {}", root.display()));
    }
    let truncated = hits.len() > MAX_RESULTS;
    let mut out = hits.into_iter().take(MAX_RESULTS).collect::<Vec<_>>().join("\n");
    if truncated {
        out.push_str(&format!("\n\n[first {MAX_RESULTS} matches; narrow the search for more]"));
    }
    ToolOutput::ok(out)
}

fn walk_grep(root: &Path, re: &Regex, include: Option<&GlobMatcher>) -> Vec<String> {
    let mut hits = Vec::new();
    'files: for entry in WalkDir::new(root)
        .max_depth(MAX_DEPTH)
        .into_iter()
        .filter_entry(|e| !is_skipped(e))
        .filter_map(Result::ok)
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = entry.path().strip_prefix(root).unwrap_or(entry.path());
        if let Some(m) = include
            && !(m.is_match(rel) || rel.file_name().is_some_and(|n| m.is_match(n)))
        {
            continue;
        }
        // Skip anything that is not UTF-8 text rather than emitting noise.
        let Ok(body) = std::fs::read_to_string(entry.path()) else { continue };
        for (i, line) in body.lines().enumerate() {
            if re.is_match(line) {
                hits.push(format!(
                    "{}:{}: {}",
                    rel.display(),
                    i + 1,
                    line.trim_end().chars().take(300).collect::<String>()
                ));
                if hits.len() > MAX_RESULTS {
                    break 'files;
                }
            }
        }
    }
    hits
}

fn is_skipped(e: &DirEntry) -> bool {
    e.depth() > 0
        && e.file_type().is_dir()
        && e.file_name().to_str().is_some_and(|n| SKIP_DIRS.contains(&n))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("rustclaw-search-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("src")).unwrap();
        std::fs::create_dir_all(d.join("node_modules/pkg")).unwrap();
        std::fs::write(d.join("src/main.rs"), "fn main() {\n    let needle = 1;\n}\n").unwrap();
        std::fs::write(d.join("src/lib.rs"), "pub fn helper() {}\n").unwrap();
        std::fs::write(d.join("README.md"), "needle in the docs\n").unwrap();
        std::fs::write(d.join("node_modules/pkg/index.js"), "needle\n").unwrap();
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
    async fn glob_matches_recursively_and_skips_vendor_dirs() {
        let d = fixture("glob");
        let out = glob(json!({"pattern": "**/*.rs"}), &ctx(&d)).await;
        assert!(out.content.contains("src/main.rs"));
        assert!(out.content.contains("src/lib.rs"));
        let js = glob(json!({"pattern": "**/*.js"}), &ctx(&d)).await;
        assert!(js.content.contains("no files match"), "node_modules must be skipped: {}", js.content);
    }

    #[tokio::test]
    async fn a_bare_extension_glob_works_without_a_double_star_prefix() {
        let d = fixture("bare");
        let out = glob(json!({"pattern": "*.rs"}), &ctx(&d)).await;
        assert!(out.content.contains("main.rs"), "got: {}", out.content);
    }

    #[tokio::test]
    async fn grep_reports_file_and_line_and_honours_include() {
        let d = fixture("grep");
        let out = grep(json!({"pattern": "needle"}), &ctx(&d)).await;
        assert!(out.content.contains("src/main.rs:2:"));
        assert!(out.content.contains("README.md:1:"));

        let scoped = grep(json!({"pattern": "needle", "include": "*.rs"}), &ctx(&d)).await;
        assert!(scoped.content.contains("main.rs"));
        assert!(!scoped.content.contains("README.md"));
    }

    #[tokio::test]
    async fn grep_supports_case_insensitive_search() {
        let d = fixture("case");
        let out = grep(json!({"pattern": "NEEDLE", "ignore_case": true}), &ctx(&d)).await;
        assert!(out.content.contains("main.rs"));
    }

    #[tokio::test]
    async fn an_invalid_regex_is_reported_rather_than_panicking() {
        let d = fixture("badre");
        let out = grep(json!({"pattern": "[unclosed"}), &ctx(&d)).await;
        assert!(out.is_error);
        assert!(out.content.contains("invalid regex"));
    }

    #[tokio::test]
    async fn no_matches_reads_as_success_not_failure() {
        let d = fixture("none");
        let out = grep(json!({"pattern": "zzzznotpresent"}), &ctx(&d)).await;
        assert!(!out.is_error);
        assert!(out.content.contains("no matches"));
    }
}
