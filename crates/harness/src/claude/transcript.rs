//! Claude Code's own transcripts (`<config>/projects/<slug>/<session>.jsonl`),
//! read for import.
//!
//! A transcript record carries the same message envelope the stream-json wire
//! does, so tool calls decode through [`decode_tool_use`] and imported chips
//! render like live ones. Only what a person actually sent becomes a user
//! turn: `promptSource` separates typed prompts from the CLI's own injections,
//! and older transcripts fall back to dropping meta records and wrapper markup
//! (slash commands, `!` bash mode and its captured output, caveats).
//! Sidechain records and `subagents/` transcripts are left out.

use std::collections::HashSet;
use std::path::Path;

use serde::{Deserialize, Deserializer};
use zeron_proto::AgentEvent;

use super::normalize::decode_tool_use;
use super::wire::ContentBlock;
use crate::history::{
    self, DiscoveredSession, TranscriptEvent, TranscriptInfo, is_injected_text, parse_timestamp,
    strip_system_reminders,
};

/// The fields import reads from one transcript line. Everything else —
/// tool results' bodies and `toolUseResult` above all — is skipped by the
/// deserializer without being materialized.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct Record {
    #[serde(rename = "type")]
    kind: String,
    uuid: Option<String>,
    timestamp: Option<String>,
    cwd: Option<String>,
    git_branch: Option<String>,
    is_sidechain: bool,
    is_meta: bool,
    prompt_source: Option<String>,
    origin: Option<Origin>,
    message: Option<Message>,
    custom_title: Option<String>,
    ai_title: Option<String>,
    summary: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Origin {
    kind: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Message {
    model: Option<String>,
    content: Content,
}

/// A message body: a plain prompt string or an array of content blocks.
#[derive(Debug, Default)]
enum Content {
    #[default]
    Empty,
    Text(String),
    Blocks(Vec<ContentBlock>),
}

impl<'de> Deserialize<'de> for Content {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = Content;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a string or an array of content blocks")
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Content, E> {
                Ok(Content::Text(v.to_string()))
            }
            fn visit_string<E: serde::de::Error>(self, v: String) -> Result<Content, E> {
                Ok(Content::Text(v))
            }
            fn visit_unit<E: serde::de::Error>(self) -> Result<Content, E> {
                Ok(Content::Empty)
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<Content, A::Error> {
                let mut blocks = Vec::new();
                while let Some(block) = seq.next_element::<ContentBlock>()? {
                    blocks.push(block);
                }
                Ok(Content::Blocks(blocks))
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}

impl Record {
    fn at_ms(&self) -> Option<i64> {
        self.timestamp.as_deref().and_then(parse_timestamp)
    }

    /// The prompt a person sent in this record, or `None` when the record is
    /// a tool result, a CLI injection, or wrapper markup.
    fn human_prompt(&self) -> Option<String> {
        if self.kind != "user" || self.is_sidechain || self.is_meta {
            return None;
        }
        let human = match self.prompt_source.as_deref() {
            Some("typed" | "queued" | "suggestion_accepted") => true,
            // SDK drivers send prompts too, but also relay task notifications.
            Some("sdk") => self
                .origin
                .as_ref()
                .and_then(|o| o.kind.as_deref())
                .is_none_or(|kind| kind == "human"),
            Some(_) => false,
            // Older transcripts: no source; the wrapper filter below decides.
            None => true,
        };
        if !human {
            return None;
        }
        let text = match &self.message.as_ref()?.content {
            Content::Text(text) => strip_system_reminders(text),
            Content::Blocks(blocks) => {
                if blocks.iter().any(|b| b.kind == "tool_result") {
                    return None;
                }
                blocks
                    .iter()
                    .filter(|b| b.kind == "text" && !is_injected_text(&b.text))
                    .map(|b| strip_system_reminders(&b.text))
                    .filter(|t| !t.is_empty())
                    .collect::<Vec<_>>()
                    .join("\n\n")
            }
            Content::Empty => return None,
        };
        (!is_injected_text(&text)).then_some(text)
    }

    fn model(&self) -> Option<String> {
        let model = self.message.as_ref()?.model.as_deref()?;
        (self.kind == "assistant" && !model.is_empty() && !model.starts_with('<'))
            .then(|| strip_snapshot_date(model).to_string())
    }
}

/// `claude-opus-4-5-20251101` → `claude-opus-4-5`, the id the composer offers.
fn strip_snapshot_date(model: &str) -> &str {
    match model.rsplit_once('-') {
        Some((base, date)) if date.len() == 8 && date.bytes().all(|b| b.is_ascii_digit()) => base,
        _ => model,
    }
}

/// Title records a transcript carries, newest of each kind winning.
#[derive(Default)]
struct Titles {
    custom: Option<String>,
    ai: Option<String>,
    summary: Option<String>,
}

impl Titles {
    fn note(&mut self, record: &Record) {
        let set = |slot: &mut Option<String>, value: &Option<String>| {
            if let Some(value) = value.as_deref().map(str::trim).filter(|v| !v.is_empty()) {
                *slot = Some(value.to_string());
            }
        };
        match record.kind.as_str() {
            "custom-title" => set(&mut self.custom, &record.custom_title),
            "ai-title" => set(&mut self.ai, &record.ai_title),
            "summary" => set(&mut self.summary, &record.summary),
            _ => {}
        }
    }

    fn best(self) -> Option<String> {
        self.custom.or(self.ai).or(self.summary)
    }
}

/// Every top-level transcript under `<config>/projects`, newest first.
/// `subagents/` transcripts live in per-session subfolders and are skipped.
pub fn discover(config_dir: &Path) -> Vec<DiscoveredSession> {
    let Ok(projects) = std::fs::read_dir(config_dir.join("projects")) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for project in projects.flatten() {
        if !project.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(project.path()) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "jsonl")
                && entry.file_type().is_ok_and(|t| t.is_file())
                && let Some(found) = probe(&path)
            {
                out.push(found);
            }
        }
    }
    out.sort_by_key(|s| std::cmp::Reverse(s.updated_ms));
    out
}

/// Summarize one transcript from its head and tail. `None` when it holds no
/// conversation (an empty file, a title-only stub, a sidechain, a session
/// that only ran local slash commands).
pub fn probe(path: &Path) -> Option<DiscoveredSession> {
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() == 0 {
        return None;
    }
    let id = path.file_stem()?.to_string_lossy().into_owned();
    let mut found = DiscoveredSession {
        id,
        path: path.to_path_buf(),
        cwd: None,
        title: None,
        first_prompt: None,
        model: None,
        branch: None,
        started_ms: None,
        updated_ms: history::modified_ms(&meta),
        size_bytes: meta.len(),
    };
    let mut replied = false;
    let mut sidechain = false;
    let mut titles = Titles::default();
    history::probe_head::<Record>(path, |record| {
        titles.note(&record);
        if matches!(record.kind.as_str(), "user" | "assistant") {
            sidechain |= record.is_sidechain;
        }
        replied |= record.kind == "assistant";
        if found.cwd.is_none() {
            found.cwd = record.cwd.clone().filter(|c| !c.is_empty());
            found.branch = record.git_branch.clone().filter(|b| !b.is_empty());
        }
        if found.started_ms.is_none() {
            found.started_ms = record.at_ms();
        }
        if found.model.is_none() {
            found.model = record.model();
        }
        if found.first_prompt.is_none() {
            found.first_prompt = record.human_prompt();
        }
        !(found.cwd.is_some() && found.first_prompt.is_some() && found.model.is_some())
    })?;
    if sidechain || !(replied || found.first_prompt.is_some()) {
        return None;
    }
    history::probe_tail::<Record>(path, meta.len(), |record| {
        titles.note(&record);
        if let Some(at) = record.at_ms() {
            found.updated_ms = at;
        }
        true
    });
    found.title = titles.best();
    Some(found)
}

/// Replay a whole transcript as events, streamed line by line.
pub fn read(path: &Path, mut emit: impl FnMut(TranscriptEvent)) -> std::io::Result<TranscriptInfo> {
    let mut info = TranscriptInfo::default();
    let mut titles = Titles::default();
    // A resumed or compacted session can repeat records; uuids are unique.
    let mut seen = HashSet::new();
    let malformed = history::for_each_record::<Record>(path, |record| {
        titles.note(&record);
        if record.is_sidechain || !matches!(record.kind.as_str(), "user" | "assistant") {
            return;
        }
        if let Some(uuid) = &record.uuid
            && !seen.insert(uuid.clone())
        {
            return;
        }
        let at_ms = record.at_ms();
        info.note_time(at_ms);
        if info.cwd.is_none() {
            info.cwd = record.cwd.clone().filter(|c| !c.is_empty());
        }
        if let Some(branch) = record.git_branch.clone().filter(|b| !b.is_empty()) {
            info.branch = Some(branch);
        }
        if let Some(model) = record.model() {
            info.model = Some(model);
        }
        let at_ms = at_ms.or(info.last_ms).unwrap_or_default();
        for event in record_events(&record) {
            emit(TranscriptEvent { at_ms, event });
        }
    })?;
    info.malformed_lines = malformed;
    info.title = titles.best();
    Ok(info)
}

/// The events one `user`/`assistant` record contributes.
fn record_events(record: &Record) -> Vec<AgentEvent> {
    if let Some(text) = record.human_prompt() {
        return vec![AgentEvent::UserMessage { text }];
    }
    let Some(Content::Blocks(blocks)) = record.message.as_ref().map(|m| &m.content) else {
        return Vec::new();
    };
    let assistant = record.kind == "assistant";
    blocks
        .iter()
        .filter_map(|block| match block.kind.as_str() {
            "text" if assistant && !block.text.trim().is_empty() => Some(AgentEvent::TextDelta {
                text: block.text.clone(),
            }),
            "thinking" if assistant && !block.thinking.trim().is_empty() => {
                Some(AgentEvent::ReasoningDelta {
                    text: block.thinking.clone(),
                })
            }
            "tool_use" if assistant => Some(AgentEvent::ToolCall {
                id: block.id.clone(),
                call: decode_tool_use(&block.name, &block.input),
            }),
            // Outputs stay out, exactly as the live adapter leaves them.
            "tool_result" if !assistant => Some(AgentEvent::ToolResult {
                id: block.tool_use_id.clone(),
                is_error: block.is_error.unwrap_or(false),
                output: None,
                diff: None,
            }),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, lines: &[&str]) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, format!("{}\n", lines.join("\n"))).unwrap();
        path
    }

    fn record(json: serde_json::Value) -> Record {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn only_human_prompts_become_user_messages() {
        let typed = record(serde_json::json!({
            "type": "user", "promptSource": "typed",
            "message": {"role": "user", "content": "hello"}
        }));
        let notification = record(serde_json::json!({
            "type": "user", "promptSource": "system",
            "message": {"role": "user", "content": "<task-notification>done</task-notification>"}
        }));
        let sdk_notification = record(serde_json::json!({
            "type": "user", "promptSource": "sdk", "origin": {"kind": "task-notification"},
            "message": {"role": "user", "content": "a background task finished"}
        }));
        let legacy = record(serde_json::json!({
            "type": "user", "message": {"role": "user", "content": "from an older CLI"}
        }));
        let meta = record(serde_json::json!({
            "type": "user", "isMeta": true,
            "message": {"role": "user", "content": "Caveat: The messages below were generated…"}
        }));
        let slash = record(serde_json::json!({
            "type": "user",
            "message": {"role": "user", "content": "<command-name>/model</command-name>\n<command-message>model</command-message>\n<command-args></command-args>"}
        }));
        let bash = record(serde_json::json!({
            "type": "user",
            "message": {"role": "user", "content": "<bash-input>cat .env.local</bash-input>"}
        }));
        let stdout = record(serde_json::json!({
            "type": "user",
            "message": {"role": "user", "content": "<bash-stdout>TOKEN=secret</bash-stdout><bash-stderr></bash-stderr>"}
        }));
        assert_eq!(typed.human_prompt().as_deref(), Some("hello"));
        assert_eq!(legacy.human_prompt().as_deref(), Some("from an older CLI"));
        for injected in [notification, sdk_notification, meta, slash, bash, stdout] {
            assert_eq!(injected.human_prompt(), None, "{injected:?}");
        }
    }

    #[test]
    fn assistant_blocks_map_to_events() {
        let events = record_events(&record(serde_json::json!({
            "type": "assistant",
            "message": {"role": "assistant", "model": "claude-opus-4-5-20251101", "content": [
                {"type": "thinking", "thinking": "weighing it up", "signature": "sig"},
                {"type": "text", "text": "Here is the answer."},
                {"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {"command": "ls"}},
            ]}
        })));
        assert!(
            matches!(&events[0], AgentEvent::ReasoningDelta { text } if text == "weighing it up")
        );
        assert!(
            matches!(&events[1], AgentEvent::TextDelta { text } if text == "Here is the answer.")
        );
        assert!(matches!(
            &events[2],
            AgentEvent::ToolCall { id, call: zeron_proto::ToolCall::Exec { command } }
                if id == "toolu_1" && command == "ls"
        ));
    }

    #[test]
    fn reads_a_transcript_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "sess-1.jsonl",
            &[
                r#"{"type":"mode","mode":"default"}"#,
                r#"{"type":"user","isMeta":true,"cwd":"/w/proj","timestamp":"2026-09-16T20:27:10.000Z","message":{"role":"user","content":"Caveat: The messages below were generated by the user while running local commands."}}"#,
                r#"{"type":"user","cwd":"/w/proj","timestamp":"2026-09-16T20:27:11.000Z","message":{"role":"user","content":"<bash-input>cat .env.local</bash-input>"}}"#,
                r#"{"type":"user","cwd":"/w/proj","timestamp":"2026-09-16T20:27:12.000Z","message":{"role":"user","content":"<bash-stdout>TOKEN=secret</bash-stdout><bash-stderr></bash-stderr>"}}"#,
                r#"{"type":"user","uuid":"u1","promptSource":"typed","cwd":"/w/proj","gitBranch":"main","timestamp":"2026-09-16T20:27:13.490Z","message":{"role":"user","content":"fix the test"}}"#,
                r#"{"type":"assistant","uuid":"a1","timestamp":"2026-09-16T20:27:20.000Z","message":{"role":"assistant","model":"claude-opus-4-5-20251101","content":[{"type":"text","text":"On it."},{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"cargo test"}}]}}"#,
                r#"{"type":"assistant","uuid":"a1","timestamp":"2026-09-16T20:27:20.000Z","message":{"role":"assistant","content":[{"type":"text","text":"On it."}]}}"#,
                r#"{"type":"user","uuid":"u2","timestamp":"2026-09-16T20:27:25.000Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","is_error":true,"content":"boom"}]},"toolUseResult":{"stdout":"boom"}}"#,
                r#"{"type":"assistant","uuid":"s1","isSidechain":true,"message":{"role":"assistant","content":[{"type":"text","text":"child"}]}}"#,
                r#"not json at all"#,
                r#"{"type":"ai-title","aiTitle":"Fixing the flaky test"}"#,
            ],
        );
        let mut events = Vec::new();
        let info = read(&path, |e| events.push(e)).unwrap();
        assert_eq!(info.cwd.as_deref(), Some("/w/proj"));
        assert_eq!(info.branch.as_deref(), Some("main"));
        assert_eq!(info.model.as_deref(), Some("claude-opus-4-5"));
        assert_eq!(info.title.as_deref(), Some("Fixing the flaky test"));
        assert_eq!(info.malformed_lines, 1);
        let kinds: Vec<&str> = events
            .iter()
            .map(|e| match &e.event {
                AgentEvent::UserMessage { .. } => "user",
                AgentEvent::TextDelta { .. } => "text",
                AgentEvent::ToolCall { .. } => "call",
                AgentEvent::ToolResult { is_error: true, .. } => "error",
                _ => "other",
            })
            .collect();
        assert_eq!(kinds, ["user", "text", "call", "error"]);
        assert_eq!(
            events[0].at_ms,
            parse_timestamp("2026-09-16T20:27:13.490Z").unwrap()
        );
    }

    #[test]
    fn discovery_reads_head_and_tail() {
        let dir = tempfile::tempdir().unwrap();
        let projects = dir.path().join("projects").join("-w-proj");
        std::fs::create_dir_all(projects.join("sess-titled").join("subagents")).unwrap();
        write(
            &projects,
            "sess-titled.jsonl",
            &[
                r#"{"type":"user","promptSource":"typed","cwd":"/w/proj","timestamp":"2026-09-16T20:27:13.490Z","message":{"role":"user","content":"hello"}}"#,
                r#"{"type":"custom-title","customTitle":"My renamed chat"}"#,
            ],
        );
        write(
            &projects.join("sess-titled").join("subagents"),
            "agent-1.jsonl",
            &[r#"{"type":"user","isSidechain":true,"message":{"role":"user","content":"child"}}"#],
        );
        write(
            &projects,
            "sess-untitled.jsonl",
            &[
                r#"{"type":"user","cwd":"/w/proj","message":{"role":"user","content":"<command-name>/clear</command-name>"}}"#,
                r#"{"type":"user","promptSource":"typed","cwd":"/w/proj","timestamp":"2026-09-16T20:27:13.490Z","message":{"role":"user","content":"a prompt that names the session"}}"#,
            ],
        );
        write(
            &projects,
            "stub.jsonl",
            &[r#"{"type":"summary","summary":"x"}"#],
        );
        write(
            &projects,
            "commands-only.jsonl",
            &[
                r#"{"type":"user","cwd":"/w/proj","message":{"role":"user","content":"<command-name>/login</command-name>"}}"#,
                r#"{"type":"user","cwd":"/w/proj","message":{"role":"user","content":"<local-command-stdout>Login successful</local-command-stdout>"}}"#,
            ],
        );
        std::fs::write(projects.join("empty.jsonl"), "").unwrap();
        let found = discover(dir.path());
        assert_eq!(found.len(), 2, "{found:?}");
        let titled = found.iter().find(|s| s.id == "sess-titled").unwrap();
        assert_eq!(titled.title.as_deref(), Some("My renamed chat"));
        assert_eq!(titled.cwd.as_deref(), Some("/w/proj"));
        let untitled = found.iter().find(|s| s.id == "sess-untitled").unwrap();
        assert_eq!(untitled.title, None);
        assert_eq!(
            untitled.display_title().as_deref(),
            Some("a prompt that names the session")
        );
    }

    #[test]
    fn snapshot_dates_are_dropped_from_models() {
        assert_eq!(
            strip_snapshot_date("claude-haiku-4-5-20251001"),
            "claude-haiku-4-5"
        );
        assert_eq!(strip_snapshot_date("claude-opus-4-7"), "claude-opus-4-7");
    }
}
