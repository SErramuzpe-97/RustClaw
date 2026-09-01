//! Append-only JSONL transcripts.
//!
//! OpenClaw pairs JSONL transcripts with a ~40-table SQLite index. We keep the
//! JSONL and drop the database: one `write` per entry against an `O_APPEND`
//! handle is the cheapest durable path there is, and it leaves no SQL engine
//! resident in memory.
//!
//! Tool output above a threshold is spilled to `tool-results/` and the
//! transcript keeps only a preview plus a reference — the pattern IronClaw uses
//! with `ThreadMessageRecord.tool_result_ref`, so a long session does not carry
//! megabytes of `exec` output through every serialization.

use crate::types::Message;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Entry {
    Message(Message),
    /// Records that the context window was summarized, so a later reader can
    /// tell a compacted transcript from a short one.
    Compaction { summary: String, dropped: usize, tokens_before: u32 },
}

pub struct Session {
    pub id: String,
    pub messages: Vec<Message>,
    dir: PathBuf,
    log: std::fs::File,
}

impl Session {
    /// Open `id`, replaying its transcript if one exists.
    pub fn open(root: &Path, id: &str) -> Result<Self> {
        let dir = root.join("sessions");
        std::fs::create_dir_all(dir.join("tool-results"))
            .with_context(|| format!("creating {}", dir.display()))?;
        let path = dir.join(format!("{id}.jsonl"));

        let messages = if path.exists() { replay(&path)? } else { Vec::new() };
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;

        Ok(Self { id: id.to_string(), messages, dir, log })
    }

    /// A session id derived from the wall clock, stable enough to sort by name.
    pub fn new_id() -> String {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        format!("s{now}")
    }

    pub fn append(&mut self, message: Message) -> Result<()> {
        self.write(&Entry::Message(message.clone()))?;
        self.messages.push(message);
        Ok(())
    }

    pub fn record_compaction(&mut self, summary: &str, dropped: usize, tokens_before: u32) -> Result<()> {
        self.write(&Entry::Compaction {
            summary: summary.to_string(),
            dropped,
            tokens_before,
        })
    }

    fn write(&mut self, entry: &Entry) -> Result<()> {
        let mut line = serde_json::to_string(entry)?;
        line.push('\n');
        // No fsync: losing the last line of a chat transcript to a power cut is
        // not worth a disk flush on every message.
        self.log.write_all(line.as_bytes()).context("appending to transcript")?;
        Ok(())
    }

    /// Spill an oversized tool result and return the preview plus its path.
    pub fn spill_tool_output(
        &self,
        tool_call_id: &str,
        body: &str,
        max_inline: usize,
    ) -> (String, Option<String>) {
        if body.len() <= max_inline {
            return (body.to_string(), None);
        }
        let path = self.dir.join("tool-results").join(format!("{}-{tool_call_id}.txt", self.id));
        if std::fs::write(&path, body).is_err() {
            // Spilling is an optimization; if the disk says no, truncate and
            // carry on rather than failing the turn.
            return (crate::tools::truncate_middle(body, max_inline), None);
        }
        let preview = crate::tools::truncate_middle(body, max_inline);
        (
            format!("{preview}\n\n[full output: {}]", path.display()),
            Some(path.to_string_lossy().into_owned()),
        )
    }

    /// Replace the transcript after compaction. Written to a temp file and
    /// renamed so a crash mid-rewrite cannot leave a half-written history.
    pub fn rewrite(&mut self, messages: Vec<Message>) -> Result<()> {
        let path = self.dir.join(format!("{}.jsonl", self.id));
        let tmp = path.with_extension("jsonl.tmp");
        {
            let mut f = std::fs::File::create(&tmp)?;
            for m in &messages {
                let mut line = serde_json::to_string(&Entry::Message(m.clone()))?;
                line.push('\n');
                f.write_all(line.as_bytes())?;
            }
            f.sync_all()?;
        }
        std::fs::rename(&tmp, &path)?;
        self.log = std::fs::OpenOptions::new().append(true).open(&path)?;
        self.messages = messages;
        Ok(())
    }

    pub fn list(root: &Path) -> Vec<String> {
        let dir = root.join("sessions");
        let Ok(entries) = std::fs::read_dir(&dir) else { return Vec::new() };
        let mut ids: Vec<String> = entries
            .filter_map(Result::ok)
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                name.strip_suffix(".jsonl").map(str::to_string)
            })
            .collect();
        ids.sort_unstable();
        ids
    }
}

/// Read a transcript back into messages.
///
/// A truncated final line (a crash mid-append) is skipped rather than treated
/// as corruption — the rest of the history is still good.
fn replay(path: &Path) -> Result<Vec<Message>> {
    let body = std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;
    let mut messages = Vec::new();
    for line in body.lines() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Entry>(line) {
            Ok(Entry::Message(m)) => messages.push(m),
            Ok(Entry::Compaction { .. }) => {}
            Err(_) => continue,
        }
    }
    Ok(messages)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ContentBlock, StopReason, Usage};

    fn root(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rustclaw-sess-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn assistant(text: &str) -> Message {
        Message::Assistant {
            content: vec![ContentBlock::text(text)],
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error: None,
        }
    }

    #[test]
    fn a_transcript_survives_reopening() {
        let r = root("reopen");
        {
            let mut s = Session::open(&r, "s1").unwrap();
            s.append(Message::user("hola")).unwrap();
            s.append(assistant("qué tal")).unwrap();
        }
        let s = Session::open(&r, "s1").unwrap();
        assert_eq!(s.messages.len(), 2);
        assert_eq!(s.messages[0], Message::user("hola"));
    }

    #[test]
    fn a_truncated_final_line_does_not_lose_the_rest_of_the_history() {
        let r = root("torn");
        {
            let mut s = Session::open(&r, "s1").unwrap();
            s.append(Message::user("one")).unwrap();
            s.append(Message::user("two")).unwrap();
        }
        // Simulate a crash partway through an append.
        let path = r.join("sessions/s1.jsonl");
        let mut body = std::fs::read_to_string(&path).unwrap();
        body.push_str("{\"type\":\"Message\",\"role\":\"user\",\"cont");
        std::fs::write(&path, body).unwrap();

        let s = Session::open(&r, "s1").unwrap();
        assert_eq!(s.messages.len(), 2, "the two intact messages must survive");
    }

    #[test]
    fn large_tool_output_is_spilled_and_referenced() {
        let r = root("spill");
        let s = Session::open(&r, "s1").unwrap();
        let big = "x".repeat(10_000);
        let (preview, blob) = s.spill_tool_output("call_1", &big, 500);
        assert!(preview.len() < big.len(), "preview must be smaller than the body");
        assert!(preview.contains("full output:"));
        let blob = blob.expect("a spill path");
        assert_eq!(std::fs::read_to_string(blob).unwrap().len(), 10_000);
    }

    #[test]
    fn small_tool_output_stays_inline() {
        let r = root("inline");
        let s = Session::open(&r, "s1").unwrap();
        let (preview, blob) = s.spill_tool_output("call_1", "short", 500);
        assert_eq!(preview, "short");
        assert!(blob.is_none());
    }

    #[test]
    fn rewrite_replaces_history_atomically_and_appends_still_work() {
        let r = root("rewrite");
        let mut s = Session::open(&r, "s1").unwrap();
        s.append(Message::user("a")).unwrap();
        s.append(Message::user("b")).unwrap();
        s.rewrite(vec![Message::user("summary")]).unwrap();
        s.append(Message::user("c")).unwrap();

        let reopened = Session::open(&r, "s1").unwrap();
        assert_eq!(reopened.messages.len(), 2);
        assert_eq!(reopened.messages[0], Message::user("summary"));
        assert_eq!(reopened.messages[1], Message::user("c"));
        assert!(!r.join("sessions/s1.jsonl.tmp").exists(), "temp file must be renamed away");
    }

    #[test]
    fn lists_sessions_by_id() {
        let r = root("list");
        Session::open(&r, "s1").unwrap();
        Session::open(&r, "s2").unwrap();
        assert_eq!(Session::list(&r), vec!["s1".to_string(), "s2".to_string()]);
    }
}
