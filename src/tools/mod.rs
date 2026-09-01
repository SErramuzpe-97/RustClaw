//! Built-in tools.
//!
//! The registry is a static slice of function pointers rather than a
//! `HashMap<String, Box<dyn Tool>>`: lookup is a short linear scan over eight
//! entries, and nothing is allocated to build it at startup.
//!
//! Per the project's brief this layer is deliberately permissive. There is no
//! `beforeToolCall` hook, no policy pipeline and no approval prompt — the
//! equivalent surface in OpenClaw (`agent-tools.before-tool-call.*`) is ~10.9k
//! lines on its own.

mod edit;
mod exec;
mod fetch;
mod fs;
mod search;

pub(crate) use exec::truncate_middle;

use crate::llm::ToolSchema;
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;

pub struct Ctx {
    pub cwd: std::path::PathBuf,
    pub exec_timeout: std::time::Duration,
    pub http: reqwest::Client,
}

pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
}

impl ToolOutput {
    pub fn ok(content: impl Into<String>) -> Self {
        Self { content: content.into(), is_error: false }
    }
    pub fn err(content: impl Into<String>) -> Self {
        Self { content: content.into(), is_error: true }
    }
}

type Fut<'a> = Pin<Box<dyn Future<Output = ToolOutput> + Send + 'a>>;

pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub schema: fn() -> Value,
    pub run: for<'a> fn(Value, &'a Ctx) -> Fut<'a>,
}

pub static TOOLS: &[ToolSpec] = &[
    exec::EXEC,
    fs::READ,
    fs::WRITE,
    edit::EDIT,
    fs::LS,
    search::GLOB,
    search::GREP,
    fetch::WEB_FETCH,
];

pub fn find(name: &str) -> Option<&'static ToolSpec> {
    TOOLS.iter().find(|t| t.name == name)
}

/// Schemas advertised to the model, built once per process.
pub fn schemas() -> Vec<ToolSchema> {
    TOOLS
        .iter()
        .map(|t| ToolSchema {
            name: t.name,
            description: t.description,
            input_schema: (t.schema)(),
        })
        .collect()
}

/// Helper for reading a required string argument.
pub(crate) fn arg_str<'v>(args: &'v Value, key: &str) -> Result<&'v str, ToolOutput> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| ToolOutput::err(format!("missing required string argument `{key}`")))
}

/// Resolve a possibly-relative path against the session cwd.
pub(crate) fn resolve(ctx: &Ctx, path: &str) -> std::path::PathBuf {
    let p = std::path::Path::new(path);
    if p.is_absolute() { p.to_path_buf() } else { ctx.cwd.join(p) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tool_advertises_an_object_schema_with_required_fields() {
        for t in TOOLS {
            let s = (t.schema)();
            assert_eq!(s["type"], "object", "{} must be an object schema", t.name);
            assert!(s.get("properties").is_some(), "{} has no properties", t.name);
            assert!(!t.description.is_empty(), "{} has no description", t.name);
        }
    }

    #[test]
    fn tool_names_are_unique_and_findable() {
        let mut names: Vec<_> = TOOLS.iter().map(|t| t.name).collect();
        let count = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), count, "duplicate tool name");
        assert!(find("exec").is_some());
        assert!(find("nope").is_none());
    }
}
