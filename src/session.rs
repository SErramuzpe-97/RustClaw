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
//!
//! Titles are entries too, not a separate store: renaming appends a new
//! `Entry::Title` and the last one wins on replay, so the log stays append-only.
//! `sessions/index.json` is only a listing cache — it can be deleted at any time
//! and is rebuilt from the transcripts themselves.

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
    /// Conversation title. Appended on rename; the last one wins.
    Title { text: String },
}

/// What the sidebar needs to draw a row, without loading the transcript.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: String,
    pub title: String,
    /// Unix seconds. The UI does the formatting.
    pub updated_at: u64,
    pub messages: usize,
}

/// Titles are cut to this so the sidebar stays readable.
const MAX_TITLE_CHARS: usize = 60;

pub struct Session {
    pub id: String,
    pub messages: Vec<Message>,
    pub title: Option<String>,
    dir: PathBuf,
    log: std::fs::File,
}

/// Whether `id` is a well-formed session id.
///
/// Ids are minted by `new_id` as `s<digits>`, but they arrive from the network
/// as free-form path parameters. Anything outside this set — a slash, a dot, a
/// percent-escape that decoded to one — could escape the sessions directory and
/// read or delete arbitrary `.jsonl` files, so it is rejected at every boundary
/// rather than trusted because the id "should" be clean.
pub fn is_valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

impl Session {
    /// Open `id`, replaying its transcript if one exists.
    pub fn open(root: &Path, id: &str) -> Result<Self> {
        anyhow::ensure!(is_valid_id(id), "invalid session id");
        let dir = root.join("sessions");
        std::fs::create_dir_all(dir.join("tool-results"))
            .with_context(|| format!("creating {}", dir.display()))?;
        let path = dir.join(format!("{id}.jsonl"));

        let (messages, title) = if path.exists() { replay(&path)? } else { (Vec::new(), None) };
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;

        Ok(Self { id: id.to_string(), messages, title, dir, log })
    }

    /// A session id derived from the wall clock, stable enough to sort by name.
    ///
    /// Two sessions created in the same second would otherwise share an id and
    /// overwrite each other's transcript, so a per-second sequence disambiguates
    /// them while the common case stays a bare `s{secs}`.
    pub fn new_id() -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static LAST_SECS: AtomicU64 = AtomicU64::new(0);
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let secs = now_secs();
        // The runtime is current-thread, so this read-modify-write is race-free.
        let n = if LAST_SECS.swap(secs, Ordering::Relaxed) == secs {
            SEQ.fetch_add(1, Ordering::Relaxed) + 1
        } else {
            SEQ.store(0, Ordering::Relaxed);
            0
        };
        if n == 0 { format!("s{secs}") } else { format!("s{secs}_{n}") }
    }

    /// Title for display: the stored one, else derived from the first user
    /// message, else a placeholder. Deriving it costs nothing — asking the model
    /// for a title would cost a round trip and tokens on every new chat.
    pub fn display_title(&self) -> String {
        if let Some(t) = &self.title {
            return t.clone();
        }
        self.messages
            .iter()
            .find_map(|m| match m {
                Message::User { content } => Some(truncate_title(content)),
                _ => None,
            })
            .unwrap_or_else(|| "New chat".to_string())
    }

    pub fn set_title(&mut self, text: &str) -> Result<()> {
        let text = truncate_title(text);
        self.write(&Entry::Title { text: text.clone() })?;
        self.title = Some(text);
        Ok(())
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

    /// Replace the transcript, used by compaction and by regenerate. Written to
    /// a temp file and renamed so a crash mid-rewrite cannot leave a
    /// half-written history.
    pub fn rewrite(&mut self, messages: Vec<Message>) -> Result<()> {
        let path = self.dir.join(format!("{}.jsonl", self.id));
        let tmp = path.with_extension("jsonl.tmp");
        {
            let mut f = std::fs::File::create(&tmp)?;
            // The title is an entry like any other, so a rewrite that only
            // emitted messages would silently drop it.
            if let Some(t) = &self.title {
                let mut line = serde_json::to_string(&Entry::Title { text: t.clone() })?;
                line.push('\n');
                f.write_all(line.as_bytes())?;
            }
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

    /// Drop everything from the last user message onward and return its text,
    /// so the caller can run that turn again. `None` when there is nothing to
    /// regenerate.
    pub fn rewind_to_last_user(&mut self) -> Result<Option<String>> {
        let Some(at) = self
            .messages
            .iter()
            .rposition(|m| matches!(m, Message::User { .. }))
        else {
            return Ok(None);
        };
        let Message::User { content } = self.messages[at].clone() else { unreachable!() };
        // Truncating at the user message also removes the assistant turn and
        // every tool result that followed it, so no tool_result is left
        // referring to a tool_use that is no longer in the transcript.
        let kept = self.messages[..at].to_vec();
        self.rewrite(kept)?;
        Ok(Some(content))
    }

    pub fn meta(&self) -> SessionMeta {
        SessionMeta {
            id: self.id.clone(),
            title: self.display_title(),
            updated_at: file_mtime(&self.dir.join(format!("{}.jsonl", self.id))),
            messages: self.messages.len(),
        }
    }

    /// Sessions newest first, from the index cache when it is usable and by
    /// scanning the transcripts when it is not.
    pub fn list(root: &Path) -> Vec<SessionMeta> {
        let dir = root.join("sessions");
        let ids = transcript_ids(&dir);
        if ids.is_empty() {
            return Vec::new();
        }

        // The cache is authoritative only while it covers exactly the
        // transcripts on disk; anything else means it went stale behind our
        // back (a file copied in, a manual delete) and we rebuild.
        if let Some(cached) = read_index(&dir)
            && cached.len() == ids.len()
            && cached.iter().all(|m| ids.contains(&m.id))
        {
            let mut cached = cached;
            sort_newest_first(&mut cached);
            return cached;
        }

        let mut metas: Vec<SessionMeta> = ids.iter().filter_map(|id| scan_meta(&dir, id)).collect();
        sort_newest_first(&mut metas);
        write_index(&dir, &metas);
        metas
    }

    /// Refresh one row of the listing cache. Cheap enough to call after every
    /// turn; a failure is ignored because the cache is always rebuildable.
    pub fn touch_index(root: &Path, meta: SessionMeta) {
        let dir = root.join("sessions");
        let mut metas = read_index(&dir).unwrap_or_default();
        match metas.iter_mut().find(|m| m.id == meta.id) {
            Some(slot) => *slot = meta,
            None => metas.push(meta),
        }
        sort_newest_first(&mut metas);
        write_index(&dir, &metas);
    }

    /// Remove a transcript and the tool output spilled for it.
    pub fn delete(root: &Path, id: &str) -> Result<()> {
        anyhow::ensure!(is_valid_id(id), "invalid session id");
        let dir = root.join("sessions");
        let path = dir.join(format!("{id}.jsonl"));
        if !path.exists() {
            anyhow::bail!("no session {id}");
        }
        std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;

        // Spilled tool output is named "<session>-<call>.txt", so it would
        // otherwise accumulate forever once its transcript is gone.
        if let Ok(entries) = std::fs::read_dir(dir.join("tool-results")) {
            let prefix = format!("{id}-");
            for e in entries.filter_map(Result::ok) {
                if e.file_name().to_string_lossy().starts_with(&prefix) {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }

        let mut metas = read_index(&dir).unwrap_or_default();
        metas.retain(|m| m.id != id);
        write_index(&dir, &metas);
        Ok(())
    }

    pub fn exists(root: &Path, id: &str) -> bool {
        // A malformed id is treated as non-existent, so callers that gate on
        // exists() reject a traversal attempt before touching the filesystem.
        is_valid_id(id) && root.join("sessions").join(format!("{id}.jsonl")).exists()
    }
}

fn sort_newest_first(metas: &mut [SessionMeta]) {
    metas.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then_with(|| b.id.cmp(&a.id)));
}

fn transcript_ids(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    entries
        .filter_map(Result::ok)
        .filter_map(|e| {
            e.file_name()
                .to_string_lossy()
                .strip_suffix(".jsonl")
                .map(str::to_string)
        })
        .collect()
}

fn index_path(dir: &Path) -> PathBuf {
    dir.join("index.json")
}

fn read_index(dir: &Path) -> Option<Vec<SessionMeta>> {
    let raw = std::fs::read_to_string(index_path(dir)).ok()?;
    serde_json::from_str(&raw).ok()
}

fn write_index(dir: &Path, metas: &[SessionMeta]) {
    // Best effort: the cache is derived data, so a write failure must not fail
    // the request that triggered it.
    if let Ok(json) = serde_json::to_string(metas) {
        let _ = std::fs::write(index_path(dir), json);
    }
}

/// Build one row by reading a transcript. This is the cold path taken only when
/// the cache is missing or stale, so it reads the whole file: a rename appends
/// its `Title` at the end, and stopping at the header would miss it.
fn scan_meta(dir: &Path, id: &str) -> Option<SessionMeta> {
    let path = dir.join(format!("{id}.jsonl"));
    let body = std::fs::read_to_string(&path).ok()?;
    let mut title: Option<String> = None;
    let mut first_user: Option<String> = None;
    let mut messages = 0usize;

    for line in body.lines() {
        match serde_json::from_str::<Entry>(line) {
            Ok(Entry::Title { text }) => title = Some(text),
            Ok(Entry::Message(m)) => {
                messages += 1;
                if first_user.is_none()
                    && let Message::User { content } = &m
                {
                    first_user = Some(truncate_title(content));
                }
            }
            _ => {}
        }
    }

    Some(SessionMeta {
        id: id.to_string(),
        title: title
            .or(first_user)
            .unwrap_or_else(|| "New chat".to_string()),
        updated_at: file_mtime(&path),
        messages,
    })
}

fn file_mtime(path: &Path) -> u64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// First line, collapsed and cut on a char boundary.
fn truncate_title(text: &str) -> String {
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= MAX_TITLE_CHARS {
        return one_line;
    }
    let cut: String = one_line.chars().take(MAX_TITLE_CHARS).collect();
    format!("{}…", cut.trim_end())
}

/// Read a transcript back into messages plus its last title.
///
/// A truncated final line (a crash mid-append) is skipped rather than treated
/// as corruption — the rest of the history is still good.
fn replay(path: &Path) -> Result<(Vec<Message>, Option<String>)> {
    let body = std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;
    let mut messages = Vec::new();
    let mut title = None;
    for line in body.lines() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Entry>(line) {
            Ok(Entry::Message(m)) => messages.push(m),
            Ok(Entry::Title { text }) => title = Some(text),
            Ok(Entry::Compaction { .. }) => {}
            Err(_) => continue,
        }
    }
    Ok((messages, title))
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
    fn lists_sessions_with_titles_derived_from_the_first_user_message() {
        let r = root("list");
        let mut a = Session::open(&r, "s1").unwrap();
        a.append(Message::user("como configuro el bridge de telegram")).unwrap();
        let mut b = Session::open(&r, "s2").unwrap();
        b.append(Message::user("otra cosa")).unwrap();

        let metas = Session::list(&r);
        assert_eq!(metas.len(), 2);
        let by_id: std::collections::HashMap<_, _> =
            metas.iter().map(|m| (m.id.as_str(), m)).collect();
        assert_eq!(by_id["s1"].title, "como configuro el bridge de telegram");
        assert_eq!(by_id["s1"].messages, 1);
    }

    #[test]
    fn an_explicit_title_wins_over_the_derived_one_and_survives_reopening() {
        let r = root("title");
        {
            let mut s = Session::open(&r, "s1").unwrap();
            s.append(Message::user("primer mensaje")).unwrap();
            assert_eq!(s.display_title(), "primer mensaje");
            s.set_title("Notas del proyecto").unwrap();
        }
        let s = Session::open(&r, "s1").unwrap();
        assert_eq!(s.display_title(), "Notas del proyecto");
    }

    #[test]
    fn the_last_title_wins_because_renames_are_appended() {
        let r = root("retitle");
        {
            let mut s = Session::open(&r, "s1").unwrap();
            s.append(Message::user("hola")).unwrap();
            s.set_title("primero").unwrap();
            s.set_title("segundo").unwrap();
        }
        assert_eq!(Session::open(&r, "s1").unwrap().display_title(), "segundo");
        // And the scan path, used when the cache is cold, must agree.
        assert_eq!(Session::list(&r)[0].title, "segundo");
    }

    #[test]
    fn a_rewrite_does_not_lose_the_title() {
        let r = root("rewritetitle");
        let mut s = Session::open(&r, "s1").unwrap();
        s.append(Message::user("a")).unwrap();
        s.set_title("Conservar").unwrap();
        s.rewrite(vec![Message::user("b")]).unwrap();
        assert_eq!(Session::open(&r, "s1").unwrap().display_title(), "Conservar");
    }

    #[test]
    fn the_listing_cache_is_rebuilt_when_it_disagrees_with_the_disk() {
        let r = root("cache");
        {
            let mut s = Session::open(&r, "s1").unwrap();
            s.append(Message::user("uno")).unwrap();
        }
        assert_eq!(Session::list(&r).len(), 1);
        assert!(r.join("sessions/index.json").exists(), "the cache should be written");

        // A session appearing without going through us must still be listed.
        {
            let mut s = Session::open(&r, "s2").unwrap();
            s.append(Message::user("dos")).unwrap();
        }
        std::fs::write(r.join("sessions/index.json"), "garbage not json").unwrap();
        let metas = Session::list(&r);
        assert_eq!(metas.len(), 2, "a corrupt cache must be rebuilt, not trusted");
    }

    #[test]
    fn deleting_removes_the_transcript_and_its_spilled_tool_output() {
        let r = root("delete");
        {
            let mut s = Session::open(&r, "s1").unwrap();
            s.append(Message::user("hola")).unwrap();
            s.spill_tool_output("call1", &"x".repeat(5_000), 100);
        }
        let spill = r.join("sessions/tool-results/s1-call1.txt");
        assert!(spill.exists(), "precondition: the spill file was written");

        Session::delete(&r, "s1").unwrap();
        assert!(!r.join("sessions/s1.jsonl").exists());
        assert!(!spill.exists(), "spilled output must not outlive its transcript");
        assert!(Session::list(&r).is_empty());
        assert!(Session::delete(&r, "s1").is_err(), "deleting twice should report it");
    }

    #[test]
    fn rewind_drops_the_assistant_turn_and_every_tool_result_after_it() {
        let r = root("rewind");
        let mut s = Session::open(&r, "s1").unwrap();
        s.append(Message::user("primero")).unwrap();
        s.append(assistant("respuesta vieja")).unwrap();
        s.append(Message::user("segundo")).unwrap();
        s.append(Message::Assistant {
            content: vec![ContentBlock::ToolCall {
                id: "c1".into(),
                name: "exec".into(),
                input: serde_json::json!({}),
            }],
            usage: Usage::default(),
            stop_reason: StopReason::ToolUse,
            error: None,
        })
        .unwrap();
        s.append(Message::ToolResult {
            tool_call_id: "c1".into(),
            tool_name: "exec".into(),
            content: "salida".into(),
            is_error: false,
            blob: None,
        })
        .unwrap();

        let again = s.rewind_to_last_user().unwrap();
        assert_eq!(again.as_deref(), Some("segundo"));
        // Everything from that user turn on is gone, so no tool_result refers
        // to a tool_use that is no longer present.
        assert_eq!(s.messages.len(), 2);
        assert!(!s.messages.iter().any(|m| matches!(m, Message::ToolResult { .. })));
        assert_eq!(Session::open(&r, "s1").unwrap().messages.len(), 2);
    }

    #[test]
    fn rewinding_an_empty_session_reports_nothing_to_redo() {
        let r = root("rewind-empty");
        let mut s = Session::open(&r, "s1").unwrap();
        assert_eq!(s.rewind_to_last_user().unwrap(), None);
    }

    #[test]
    fn traversal_ids_are_rejected_at_every_boundary() {
        for bad in ["../victima/notas", "..", "a/b", "a.jsonl", "", "a b",
                    "../../etc/passwd", "a\\b", &"x".repeat(200)] {
            assert!(!is_valid_id(bad), "{bad:?} must be rejected");
        }
        for ok in ["s1788269019", "abc", "a-b_c", "S1"] {
            assert!(is_valid_id(ok), "{ok:?} must be accepted");
        }

        // And the filesystem-touching entry points must refuse them.
        let r = root("traversal");
        std::fs::create_dir_all(r.join("victima")).unwrap();
        std::fs::write(r.join("victima/notas.jsonl"), "{\"type\":\"Title\",\"text\":\"x\"}\n").unwrap();

        assert!(Session::open(&r, "../victima/notas").is_err());
        assert!(!Session::exists(&r, "../victima/notas"));
        assert!(Session::delete(&r, "../victima/notas").is_err());
        // The out-of-tree file was never touched.
        assert!(r.join("victima/notas.jsonl").exists(), "traversal must not reach it");
    }

    #[test]
    fn new_ids_are_unique_within_a_second() {
        // Two "new chat" clicks in the same second must not collide, or the
        // second session overwrites the first's transcript.
        let ids: Vec<String> = (0..5).map(|_| Session::new_id()).collect();
        let unique: std::collections::HashSet<&String> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len(), "ids collided: {ids:?}");
        for id in &ids {
            assert!(is_valid_id(id), "{id} must survive path validation");
        }
    }

    #[test]
    fn long_titles_are_cut_on_a_char_boundary() {
        let title = truncate_title(&"á".repeat(200));
        assert!(title.chars().count() <= MAX_TITLE_CHARS + 1, "{title}");
        assert!(title.ends_with('…'));
        // Multi-line input collapses to one line for the sidebar.
        assert_eq!(truncate_title("hola\n   mundo"), "hola mundo");
    }
}
