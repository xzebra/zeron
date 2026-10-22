//! Codex rollouts (`$CODEX_HOME/sessions/YYYY/MM/DD/rollout-*.jsonl`), read
//! for import.
//!
//! Rollouts have shipped in three shapes, and one file can change shape when
//! an old thread is resumed by a newer CLI:
//!
//! - current: every finished thread item is an `event_msg`/`item_completed`
//!   record; its typed items (`UserMessage`, `AgentMessage`, `Reasoning`,
//!   `CommandExecution`, …) are adapted to the app-server item shape and
//!   decoded by [`map_item`], so chips match a live run's;
//! - older: prompts as `event_msg`/`user_message`, replies, reasoning and
//!   tool calls as `response_item`s;
//! - oldest: `response_item`s only.
//!
//! Only what a person typed becomes a user turn — never the CLI's injected
//! `environment_context`, AGENTS.md instructions or developer messages.
//! Subagent threads are not offered; their parent keeps its spawn chip.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{Value, json};
use zeron_proto::{AgentEvent, ToolCall};

use super::normalize::{Phase, map_item, plan_update_events};
use crate::history::{
    self, DiscoveredSession, TranscriptEvent, TranscriptInfo, is_injected_text, parse_timestamp,
};

/// The fields import reads from one rollout line. Command output, file
/// contents of world-state snapshots, base instructions and compaction
/// history are skipped by the deserializer without being materialized.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Line {
    timestamp: Option<String>,
    #[serde(rename = "type")]
    kind: String,
    payload: Payload,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Payload {
    #[serde(rename = "type")]
    kind: Option<String>,
    // session_meta
    id: Option<String>,
    cwd: Option<String>,
    source: Option<Value>,
    thread_source: Option<String>,
    git: Option<Git>,
    // turn_context / thread_settings_applied
    model: Option<String>,
    thread_settings: Option<ThreadSettings>,
    // event_msg / item_completed
    item: Option<Item>,
    message: Option<Value>,
    exit_code: Option<i64>,
    success: Option<bool>,
    // response_item
    role: Option<String>,
    content: Option<Value>,
    summary: Option<Value>,
    name: Option<String>,
    arguments: Option<Value>,
    input: Option<Value>,
    call_id: Option<String>,
    action: Option<Value>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Git {
    branch: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ThreadSettings {
    model: Option<String>,
}

/// One completed thread item. Outputs (`stdout`, `aggregated_output`,
/// `result`, …) are deliberately not declared.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Item {
    #[serde(rename = "type")]
    kind: String,
    id: String,
    content: Option<Value>,
    command: Option<Value>,
    changes: Option<Value>,
    status: Option<String>,
    exit_code: Option<i64>,
    server: Option<String>,
    tool: Option<String>,
    arguments: Option<Value>,
    query: Option<String>,
    items: Option<Value>,
    #[serde(rename = "kind")]
    activity: Option<String>,
    agent_path: Option<String>,
    summary_text: Option<Value>,
    raw_content: Option<Value>,
}

impl Line {
    fn at_ms(&self) -> Option<i64> {
        self.timestamp.as_deref().and_then(parse_timestamp)
    }

    fn payload_kind(&self) -> &str {
        self.payload.kind.as_deref().unwrap_or("")
    }

    fn model(&self) -> Option<String> {
        match self.kind.as_str() {
            "turn_context" => self.payload.model.clone(),
            "event_msg" if self.payload_kind() == "thread_settings_applied" => self
                .payload
                .thread_settings
                .as_ref()
                .and_then(|s| s.model.clone()),
            _ => None,
        }
        .filter(|m| !m.is_empty())
    }

    /// A prompt a person typed, from either shape that records one directly.
    fn typed_prompt(&self) -> Option<String> {
        let text = match (self.kind.as_str(), self.payload_kind()) {
            ("event_msg", "item_completed") => {
                let item = self.payload.item.as_ref()?;
                (item.kind == "UserMessage")
                    .then(|| text_blocks(item.content.as_ref()))
                    .flatten()?
            }
            ("event_msg", "user_message") => self.payload.message.as_ref()?.as_str()?.to_string(),
            _ => return None,
        };
        clean_prompt(&text)
    }
}

/// Which records carry the conversation in this file (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    Items,
    Responses { prompts_from_events: bool },
}

fn clean_prompt(text: &str) -> Option<String> {
    let text = text.trim();
    (!is_injected_text(text)).then(|| text.to_string())
}

/// The text of a content array (`[{type, text}]`, any text-ish type), or of a
/// plain string.
fn text_blocks(content: Option<&Value>) -> Option<String> {
    let text = match content? {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| match b {
                Value::String(text) => Some(text.as_str()),
                _ => b.get("text").and_then(Value::as_str),
            })
            .filter(|t| !t.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n\n"),
        _ => return None,
    };
    (!text.trim().is_empty()).then_some(text)
}

/// A command as one line: `["bash", "-lc", "cargo test"]` → `cargo test`.
fn command_text(command: Option<&Value>) -> String {
    match command {
        Some(Value::String(command)) => command.clone(),
        Some(Value::Array(parts)) => {
            let parts: Vec<&str> = parts.iter().filter_map(Value::as_str).collect();
            match parts.as_slice() {
                [_, flag, script] if matches!(*flag, "-lc" | "-c") => script.to_string(),
                parts => parts.join(" "),
            }
        }
        _ => String::new(),
    }
}

/// `$CODEX_HOME/session_index.jsonl`: the CLI's thread names, newest winning.
pub fn thread_names(codex_home: &Path) -> HashMap<String, String> {
    #[derive(Deserialize)]
    struct Entry {
        id: String,
        #[serde(default)]
        thread_name: Option<String>,
    }
    let mut names = HashMap::new();
    let _ = history::for_each_record::<Entry>(&codex_home.join("session_index.jsonl"), |e| {
        if let Some(name) = e.thread_name.filter(|n| !n.trim().is_empty()) {
            names.insert(e.id, name.trim().to_string());
        }
    });
    names
}

/// Every rollout under `$CODEX_HOME/sessions`, newest first. Unreadable
/// directories are skipped rather than failing the listing.
pub fn discover(codex_home: &Path) -> Vec<DiscoveredSession> {
    let mut files = Vec::new();
    collect_rollouts(&codex_home.join("sessions"), &mut files);
    let names = thread_names(codex_home);
    let mut out: Vec<DiscoveredSession> = files
        .iter()
        .filter_map(|path| {
            let mut found = probe(path)?;
            found.title = names.get(&found.id).cloned();
            Some(found)
        })
        .collect();
    out.sort_by_key(|s| std::cmp::Reverse(s.updated_ms));
    out
}

fn collect_rollouts(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        match entry.file_type() {
            Ok(t) if t.is_dir() => collect_rollouts(&path, out),
            Ok(t)
                if t.is_file()
                    && path.extension().is_some_and(|e| e == "jsonl")
                    && path
                        .file_name()
                        .is_some_and(|n| n.to_string_lossy().starts_with("rollout-")) =>
            {
                out.push(path)
            }
            _ => {}
        }
    }
}

/// The session id a rollout's file name ends in (`rollout-<time>-<uuid>`).
fn id_from_file_name(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_string_lossy();
    let id = stem.get(stem.len().checked_sub(36)?..)?;
    (id.chars().filter(|c| *c == '-').count() == 4).then(|| id.to_string())
}

/// Summarize one rollout from its head and tail. `None` for subagent
/// threads and files with no conversation.
pub fn probe(path: &Path) -> Option<DiscoveredSession> {
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() == 0 {
        return None;
    }
    let mut found = DiscoveredSession {
        id: String::new(),
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
    let mut subagent = false;
    let mut seen_meta = false;
    history::probe_head::<Line>(path, |line| {
        if line.kind == "session_meta" && !seen_meta {
            seen_meta = true;
            let p = &line.payload;
            subagent = p.thread_source.as_deref() == Some("subagent")
                || p.source
                    .as_ref()
                    .is_some_and(|s| s.get("subagent").is_some());
            found.id = p.id.clone().unwrap_or_default();
            found.cwd = p.cwd.clone().filter(|c| !c.is_empty());
            found.branch = p.git.as_ref().and_then(|g| g.branch.clone());
        }
        if found.started_ms.is_none() {
            found.started_ms = line.at_ms();
        }
        if found.model.is_none() {
            found.model = line.model();
        }
        if found.first_prompt.is_none() {
            found.first_prompt = line.typed_prompt();
        }
        !subagent && !(seen_meta && found.first_prompt.is_some() && found.model.is_some())
    })?;
    if subagent {
        return None;
    }
    if found.id.is_empty() {
        found.id = id_from_file_name(path)?;
    }
    history::probe_tail::<Line>(path, meta.len(), |line| {
        if let Some(at) = line.at_ms() {
            found.updated_ms = at;
        }
        true
    });
    Some(found)
}

/// The shape a file starts in. Every file starts response-shaped and
/// switches at its first completed item (dropping that turn's response
/// records, which the items repeat); the head only tells whether prompts are
/// recorded as events at all, or only as user messages (the oldest shape).
fn initial_shape(path: &Path) -> Shape {
    let mut events = false;
    history::probe_head::<Line>(path, |line| {
        events = line.kind == "event_msg";
        !events
    });
    Shape::Responses {
        prompts_from_events: events,
    }
}

/// Replay a whole rollout as events, streamed line by line.
pub fn read(path: &Path, mut emit: impl FnMut(TranscriptEvent)) -> std::io::Result<TranscriptInfo> {
    let mut reader = Reader {
        shape: initial_shape(path),
        pending: Vec::new(),
        errored: HashSet::new(),
        info: TranscriptInfo::default(),
        seen_meta: false,
    };
    let malformed = history::for_each_record::<Line>(path, |line| {
        reader.line(line, &mut emit);
    })?;
    reader.flush(&mut emit);
    let mut info = reader.info;
    info.malformed_lines = malformed;
    Ok(info)
}

struct Reader {
    shape: Shape,
    /// Response-shaped events since the last turn boundary: a file whose
    /// later turns switch to completed items drops the overlap.
    pending: Vec<TranscriptEvent>,
    /// Calls whose end event reported a failure, so their output resolves
    /// as an error.
    errored: HashSet<String>,
    info: TranscriptInfo,
    seen_meta: bool,
}

impl Reader {
    fn line(&mut self, line: Line, emit: &mut impl FnMut(TranscriptEvent)) {
        let at = line.at_ms();
        self.info.note_time(at);
        if let Some(model) = line.model() {
            self.info.model = Some(model);
        }
        let at_ms = at.or(self.info.last_ms).unwrap_or_default();
        match (line.kind.as_str(), line.payload_kind()) {
            ("session_meta", _) if !self.seen_meta => {
                self.seen_meta = true;
                self.info.cwd = line.payload.cwd.clone().filter(|c| !c.is_empty());
                self.info.branch = line.payload.git.as_ref().and_then(|g| g.branch.clone());
            }
            ("turn_context", _) | ("event_msg", "task_started") => {
                if self.info.cwd.is_none() {
                    self.info.cwd = line.payload.cwd.clone().filter(|c| !c.is_empty());
                }
                self.flush(emit);
            }
            ("event_msg", "item_completed") => {
                if self.shape != Shape::Items {
                    self.pending.clear();
                    self.shape = Shape::Items;
                }
                if let Some(item) = &line.payload.item {
                    for event in item_events(item) {
                        emit(TranscriptEvent { at_ms, event });
                    }
                }
            }
            ("event_msg", "user_message") => {
                if let Shape::Responses {
                    prompts_from_events: true,
                } = self.shape
                    && let Some(text) = line.typed_prompt()
                {
                    self.pending.push(TranscriptEvent {
                        at_ms,
                        event: AgentEvent::UserMessage { text },
                    });
                }
            }
            ("event_msg", "exec_command_end") => {
                if line.payload.exit_code.is_some_and(|code| code != 0)
                    && let Some(id) = &line.payload.call_id
                {
                    self.errored.insert(id.clone());
                }
            }
            ("event_msg", "patch_apply_end") => {
                if line.payload.success == Some(false)
                    && let Some(id) = &line.payload.call_id
                {
                    self.errored.insert(id.clone());
                }
            }
            ("response_item", _) => {
                let Shape::Responses {
                    prompts_from_events,
                } = self.shape
                else {
                    return;
                };
                for event in self.response_events(&line.payload, prompts_from_events) {
                    self.pending.push(TranscriptEvent { at_ms, event });
                }
            }
            _ => {}
        }
    }

    fn flush(&mut self, emit: &mut impl FnMut(TranscriptEvent)) {
        for event in self.pending.drain(..) {
            emit(event);
        }
    }

    /// The events one `response_item` contributes in the older shapes.
    fn response_events(&self, p: &Payload, prompts_from_events: bool) -> Vec<AgentEvent> {
        let kind = p.kind.as_deref().unwrap_or("");
        let call_id = || p.call_id.clone().unwrap_or_default();
        match kind {
            "message" => match p.role.as_deref() {
                Some("assistant") => text_blocks(p.content.as_ref())
                    .map(|text| vec![AgentEvent::TextDelta { text }])
                    .unwrap_or_default(),
                // The oldest shape has no prompt events: user messages are
                // the prompts, less the CLI's injected context.
                Some("user") if !prompts_from_events => text_blocks(p.content.as_ref())
                    .and_then(|text| clean_prompt(&text))
                    .map(|text| vec![AgentEvent::UserMessage { text }])
                    .unwrap_or_default(),
                _ => Vec::new(),
            },
            "reasoning" => text_blocks(p.summary.as_ref())
                .or_else(|| text_blocks(p.content.as_ref()))
                .map(|text| vec![AgentEvent::ReasoningDelta { text }])
                .unwrap_or_default(),
            "function_call" => {
                let args = match &p.arguments {
                    Some(Value::String(raw)) => serde_json::from_str(raw).unwrap_or(Value::Null),
                    Some(other) => other.clone(),
                    None => Value::Null,
                };
                let name = p.name.as_deref().unwrap_or("");
                if name == "update_plan" {
                    return plan_update_events(&args);
                }
                vec![AgentEvent::ToolCall {
                    id: call_id(),
                    call: function_call(name, &args),
                }]
            }
            "custom_tool_call" => {
                let name = p.name.as_deref().unwrap_or("");
                let call = match (name, p.input.as_ref().and_then(Value::as_str)) {
                    ("apply_patch", Some(patch)) => patch_call(patch),
                    _ => ToolCall::Unknown {
                        name: name.to_string(),
                        input: None,
                    },
                };
                vec![AgentEvent::ToolCall {
                    id: call_id(),
                    call,
                }]
            }
            "local_shell_call" => vec![AgentEvent::ToolCall {
                id: call_id(),
                call: ToolCall::Exec {
                    command: command_text(p.action.as_ref().and_then(|a| a.get("command"))),
                },
            }],
            "web_search_call" => vec![AgentEvent::ToolCall {
                id: call_id(),
                call: ToolCall::WebSearch {
                    query: p
                        .action
                        .as_ref()
                        .and_then(|a| a.get("query"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                },
            }],
            "function_call_output" | "custom_tool_call_output" => {
                let id = call_id();
                vec![AgentEvent::ToolResult {
                    is_error: self.errored.contains(&id),
                    id,
                    output: None,
                    diff: None,
                }]
            }
            _ => Vec::new(),
        }
    }
}

/// An older rollout's `function_call` as the chip a live run would show.
fn function_call(name: &str, args: &Value) -> ToolCall {
    match name {
        "shell" | "container.exec" | "local_shell" | "shell_command" => ToolCall::Exec {
            command: command_text(args.get("command")),
        },
        "exec_command" => ToolCall::Exec {
            command: command_text(args.get("cmd").or_else(|| args.get("command"))),
        },
        "apply_patch" => args
            .get("input")
            .and_then(Value::as_str)
            .map(patch_call)
            .unwrap_or(ToolCall::ApplyPatch { path: None }),
        "web_search" => ToolCall::WebSearch {
            query: args
                .get("query")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        },
        _ => match name.split_once("__") {
            Some((server, tool)) if !server.is_empty() && !tool.is_empty() => ToolCall::Mcp {
                server: server.to_string(),
                tool: tool.to_string(),
                input: Some(args.clone()).filter(|a| !a.is_null()),
            },
            _ => ToolCall::Unknown {
                name: name.to_string(),
                input: None,
            },
        },
    }
}

/// An `apply_patch` envelope reduced to the files it touches, the same
/// mapping a live `fileChange` gets.
fn patch_call(patch: &str) -> ToolCall {
    let changes: Vec<Value> = patch
        .lines()
        .filter_map(|line| {
            let (kind, path) = if let Some(path) = line.strip_prefix("*** Add File: ") {
                ("add", path)
            } else if let Some(path) = line.strip_prefix("*** Update File: ") {
                ("update", path)
            } else if let Some(path) = line.strip_prefix("*** Delete File: ") {
                ("delete", path)
            } else {
                return None;
            };
            Some(json!({ "path": path.trim(), "kind": kind }))
        })
        .collect();
    let item = json!({ "type": "fileChange", "id": "", "changes": changes });
    map_item(Phase::Started, &item)
        .into_iter()
        .find_map(|event| match event {
            AgentEvent::ToolCall { call, .. } => Some(call),
            _ => None,
        })
        .unwrap_or(ToolCall::ApplyPatch { path: None })
}

/// A completed thread item as events: conversation items directly, tool
/// items through the live app-server decoder after adapting their shape.
fn item_events(item: &Item) -> Vec<AgentEvent> {
    let status = item.status.as_deref().unwrap_or("").to_ascii_lowercase();
    let adapted = match item.kind.as_str() {
        "UserMessage" => {
            return text_blocks(item.content.as_ref())
                .and_then(|text| clean_prompt(&text))
                .map(|text| vec![AgentEvent::UserMessage { text }])
                .unwrap_or_default();
        }
        "AgentMessage" => {
            return text_blocks(item.content.as_ref())
                .map(|text| vec![AgentEvent::TextDelta { text }])
                .unwrap_or_default();
        }
        "Reasoning" => {
            return text_blocks(item.summary_text.as_ref())
                .or_else(|| text_blocks(item.raw_content.as_ref()))
                .map(|text| vec![AgentEvent::ReasoningDelta { text }])
                .unwrap_or_default();
        }
        "CommandExecution" => json!({
            "type": "commandExecution",
            "id": item.id,
            "command": command_text(item.command.as_ref()),
            "status": status,
            "exitCode": item.exit_code.unwrap_or(0),
        }),
        "FileChange" => {
            let changes: Vec<Value> = match &item.changes {
                Some(Value::Object(map)) => map
                    .iter()
                    .map(|(path, change)| {
                        let kind = change.get("type").or_else(|| change.get("kind"));
                        json!({ "path": path, "kind": kind })
                    })
                    .collect(),
                Some(Value::Array(changes)) => changes.clone(),
                _ => Vec::new(),
            };
            json!({ "type": "fileChange", "id": item.id, "changes": changes, "status": status })
        }
        "McpToolCall" => json!({
            "type": "mcpToolCall",
            "id": item.id,
            "server": item.server,
            "tool": item.tool,
            "arguments": item.arguments,
            "status": status,
        }),
        "WebSearch" => json!({ "type": "webSearch", "id": item.id, "query": item.query }),
        "TodoList" => json!({ "type": "todoList", "id": item.id, "items": item.items }),
        "SubAgentActivity" => json!({
            "type": "subAgentActivity",
            "id": item.id,
            "kind": item.activity,
            "agentPath": item.agent_path,
        }),
        "ImageView" => {
            return vec![
                AgentEvent::ToolCall {
                    id: item.id.clone(),
                    call: ToolCall::Unknown {
                        name: "View image".into(),
                        input: None,
                    },
                },
                AgentEvent::ToolResult {
                    id: item.id.clone(),
                    is_error: false,
                    output: None,
                    diff: None,
                },
            ];
        }
        _ => return Vec::new(),
    };
    let mut events = map_item(Phase::Started, &adapted);
    events.extend(map_item(Phase::Completed, &adapted));
    events
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, lines: &[String]) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, format!("{}\n", lines.join("\n"))).unwrap();
        path
    }

    fn kinds(events: &[TranscriptEvent]) -> Vec<String> {
        events
            .iter()
            .map(|e| match &e.event {
                AgentEvent::UserMessage { text } => format!("user:{text}"),
                AgentEvent::TextDelta { text } => format!("text:{text}"),
                AgentEvent::ReasoningDelta { text } => format!("think:{text}"),
                AgentEvent::ToolCall { id, call } => format!("call:{id}:{}", call_name(call)),
                AgentEvent::ToolResult { id, is_error, .. } => format!("result:{id}:{is_error}"),
                other => format!("{other:?}"),
            })
            .collect()
    }

    fn call_name(call: &ToolCall) -> String {
        match call {
            ToolCall::Exec { command } => format!("exec {command}"),
            ToolCall::EditFile { path, .. } => format!("edit {path}"),
            ToolCall::WriteFile { path, .. } => format!("write {path}"),
            ToolCall::Mcp { server, tool, .. } => format!("mcp {server}/{tool}"),
            ToolCall::Todo { items } => format!("todo {}", items.len()),
            other => format!("{other:?}"),
        }
    }

    const META: &str = r#"{"timestamp":"2026-01-01T00:00:00.000Z","type":"session_meta","payload":{"id":"019a0000-0000-7000-8000-000000000001","timestamp":"2026-01-01T00:00:00.000Z","cwd":"/w/proj","source":"cli","git":{"branch":"main"},"base_instructions":{"text":"long"}}}"#;

    #[test]
    fn current_rollouts_read_completed_items() {
        let dir = tempfile::tempdir().unwrap();
        let lines = vec![
            META.to_string(),
            r#"{"timestamp":"2026-01-01T00:00:01.000Z","type":"turn_context","payload":{"cwd":"/w/proj","model":"gpt-5.5"}}"#.into(),
            r#"{"timestamp":"2026-01-01T00:00:01.000Z","type":"event_msg","payload":{"type":"task_started"}}"#.into(),
            r#"{"timestamp":"2026-01-01T00:00:01.100Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<environment_context>\n<cwd>/w/proj</cwd>\n</environment_context>"}]}}"#.into(),
            r#"{"timestamp":"2026-01-01T00:00:01.200Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"fix the build"}]}}"#.into(),
            r#"{"timestamp":"2026-01-01T00:00:01.200Z","type":"event_msg","payload":{"type":"item_completed","item":{"type":"UserMessage","id":"u1","content":[{"type":"text","text":"fix the build","text_elements":[]}]}}}"#.into(),
            r#"{"timestamp":"2026-01-01T00:00:02.000Z","type":"event_msg","payload":{"type":"item_completed","item":{"type":"Reasoning","id":"r1","summary_text":["Looking at the build"],"raw_content":[]}}}"#.into(),
            r#"{"timestamp":"2026-01-01T00:00:03.000Z","type":"event_msg","payload":{"type":"item_completed","item":{"type":"CommandExecution","id":"c1","command":["/bin/zsh","-lc","cargo build"],"status":"completed","stdout":"huge","aggregated_output":"huge","exit_code":101}}}"#.into(),
            r#"{"timestamp":"2026-01-01T00:00:04.000Z","type":"event_msg","payload":{"type":"item_completed","item":{"type":"FileChange","id":"f1","changes":{"/w/proj/src/lib.rs":{"type":"update","unified_diff":"@@"}},"status":"completed"}}}"#.into(),
            r#"{"timestamp":"2026-01-01T00:00:05.000Z","type":"event_msg","payload":{"type":"item_completed","item":{"type":"McpToolCall","id":"m1","server":"zeron","tool":"list_chats","arguments":{},"status":"completed","result":{"content":[{"type":"text","text":"big"}],"isError":false}}}}"#.into(),
            r#"{"timestamp":"2026-01-01T00:00:05.500Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Fixed."}]}}"#.into(),
            r#"{"timestamp":"2026-01-01T00:00:06.000Z","type":"event_msg","payload":{"type":"item_completed","item":{"type":"AgentMessage","id":"a1","content":[{"type":"text","text":"Fixed."}]}}}"#.into(),
            r#"{"timestamp":"2026-01-01T00:00:06.000Z","type":"event_msg","payload":{"type":"token_count","info":null}}"#.into(),
            r#"{"timestamp":"2026-01-01T00:00:06.500Z","type":"event_msg","payload":{"type":"item_completed","item":{"type":"Unheard","id":"x"#.into(),
        ];
        let path = write(
            dir.path(),
            "rollout-2026-01-01T00-00-00-019a0000-0000-7000-8000-000000000001.jsonl",
            &lines,
        );
        let mut events = Vec::new();
        let info = read(&path, |e| events.push(e)).unwrap();
        assert_eq!(info.cwd.as_deref(), Some("/w/proj"));
        assert_eq!(info.model.as_deref(), Some("gpt-5.5"));
        assert_eq!(info.branch.as_deref(), Some("main"));
        assert_eq!(info.malformed_lines, 1);
        assert_eq!(
            kinds(&events),
            [
                "user:fix the build",
                "think:Looking at the build",
                "call:c1:exec cargo build",
                "result:c1:true",
                "call:f1:edit /w/proj/src/lib.rs",
                "call:f1:edit /w/proj/src/lib.rs",
                "result:f1:false",
                "call:m1:mcp zeron/list_chats",
                "result:m1:false",
                "text:Fixed.",
            ]
        );
    }

    #[test]
    fn older_rollouts_read_response_items_and_prompt_events() {
        let dir = tempfile::tempdir().unwrap();
        let lines = vec![
            META.to_string(),
            r#"{"timestamp":"2025-09-01T00:00:01.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<user_instructions>be nice</user_instructions>"}]}}"#.into(),
            r#"{"timestamp":"2025-09-01T00:00:01.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"list files"}]}}"#.into(),
            r#"{"timestamp":"2025-09-01T00:00:01.000Z","type":"event_msg","payload":{"type":"user_message","message":"list files","images":[]}}"#.into(),
            r#"{"timestamp":"2025-09-01T00:00:02.000Z","type":"response_item","payload":{"type":"reasoning","summary":[{"type":"summary_text","text":"**Listing**"}],"encrypted_content":"x"}}"#.into(),
            r#"{"timestamp":"2025-09-01T00:00:03.000Z","type":"response_item","payload":{"type":"function_call","name":"shell","arguments":"{\"command\":[\"bash\",\"-lc\",\"ls\"]}","call_id":"call_1"}}"#.into(),
            r#"{"timestamp":"2025-09-01T00:00:03.500Z","type":"event_msg","payload":{"type":"exec_command_end","call_id":"call_1","exit_code":2,"stdout":"no"}}"#.into(),
            r#"{"timestamp":"2025-09-01T00:00:04.000Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call_1","output":"{\"output\":\"no\"}"}}"#.into(),
            r#"{"timestamp":"2025-09-01T00:00:04.500Z","type":"response_item","payload":{"type":"custom_tool_call","name":"apply_patch","call_id":"call_2","input":"*** Begin Patch\n*** Add File: notes.md\n+hi\n*** End Patch"}}"#.into(),
            r#"{"timestamp":"2025-09-01T00:00:05.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Done."}]}}"#.into(),
            r#"{"timestamp":"2025-09-01T00:00:05.000Z","type":"event_msg","payload":{"type":"agent_message","message":"Done."}}"#.into(),
        ];
        let path = write(dir.path(), "rollout-old.jsonl", &lines);
        let mut events = Vec::new();
        read(&path, |e| events.push(e)).unwrap();
        assert_eq!(
            kinds(&events),
            [
                "user:list files",
                "think:**Listing**",
                "call:call_1:exec ls",
                "result:call_1:true",
                "call:call_2:write notes.md",
                "text:Done.",
            ]
        );
    }

    #[test]
    fn the_oldest_rollouts_take_prompts_from_user_messages() {
        let dir = tempfile::tempdir().unwrap();
        let lines = vec![
            r#"{"id":"x","timestamp":"2025-04-01T00:00:00.000Z"}"#.to_string(),
            r#"{"type":"message","role":"user","content":[{"type":"input_text","text":"<environment_context>x</environment_context>"}]}"#.into(),
            r#"{"timestamp":"2025-05-01T00:00:01.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}}"#.into(),
            r#"{"timestamp":"2025-05-01T00:00:02.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hi"}]}}"#.into(),
        ];
        let path = write(dir.path(), "rollout-oldest.jsonl", &lines);
        let mut events = Vec::new();
        read(&path, |e| events.push(e)).unwrap();
        assert_eq!(kinds(&events), ["user:hello", "text:hi"]);
    }

    #[test]
    fn a_resumed_old_thread_switches_to_items_without_doubling() {
        let dir = tempfile::tempdir().unwrap();
        let lines = vec![
            META.to_string(),
            r#"{"timestamp":"2025-09-01T00:00:01.000Z","type":"event_msg","payload":{"type":"user_message","message":"first"}}"#.into(),
            r#"{"timestamp":"2025-09-01T00:00:02.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"one"}]}}"#.into(),
            r#"{"timestamp":"2026-01-01T00:00:00.000Z","type":"turn_context","payload":{"model":"gpt-5.5"}}"#.into(),
            r#"{"timestamp":"2026-01-01T00:00:01.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"two"}]}}"#.into(),
            r#"{"timestamp":"2026-01-01T00:00:01.000Z","type":"event_msg","payload":{"type":"item_completed","item":{"type":"UserMessage","id":"u2","content":[{"type":"text","text":"second"}]}}}"#.into(),
            r#"{"timestamp":"2026-01-01T00:00:02.000Z","type":"event_msg","payload":{"type":"item_completed","item":{"type":"AgentMessage","id":"a2","content":[{"type":"text","text":"two"}]}}}"#.into(),
        ];
        let path = write(dir.path(), "rollout-mixed.jsonl", &lines);
        let mut events = Vec::new();
        read(&path, |e| events.push(e)).unwrap();
        assert_eq!(
            kinds(&events),
            ["user:first", "text:one", "user:second", "text:two"]
        );
    }

    #[test]
    fn discovery_skips_subagents_and_names_threads() {
        let dir = tempfile::tempdir().unwrap();
        let day = dir.path().join("sessions/2026/01/01");
        std::fs::create_dir_all(&day).unwrap();
        write(
            &day,
            "rollout-2026-01-01T00-00-00-019a0000-0000-7000-8000-000000000001.jsonl",
            &[
                META.to_string(),
                r#"{"timestamp":"2026-01-01T00:00:01.000Z","type":"event_msg","payload":{"type":"item_completed","item":{"type":"UserMessage","id":"u1","content":[{"type":"text","text":"fix the build"}]}}}"#.into(),
                r#"{"timestamp":"2026-01-02T00:00:00.000Z","type":"event_msg","payload":{"type":"task_complete"}}"#.into(),
            ],
        );
        write(
            &day,
            "rollout-2026-01-01T00-00-00-019a0000-0000-7000-8000-000000000002.jsonl",
            &[r#"{"timestamp":"2026-01-01T00:00:00.000Z","type":"session_meta","payload":{"id":"019a0000-0000-7000-8000-000000000002","cwd":"/w","source":{"subagent":{"thread_spawn":{"parent_thread_id":"p"}}},"thread_source":"subagent"}}"#.into()],
        );
        write(
            dir.path(),
            "session_index.jsonl",
            &[
                r#"{"id":"019a0000-0000-7000-8000-000000000001","thread_name":"Old name","updated_at":"x"}"#.into(),
                r#"{"id":"019a0000-0000-7000-8000-000000000001","thread_name":"Build fix","updated_at":"y"}"#.into(),
            ],
        );
        let found = discover(dir.path());
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].id, "019a0000-0000-7000-8000-000000000001");
        assert_eq!(found[0].title.as_deref(), Some("Build fix"));
        assert_eq!(found[0].first_prompt.as_deref(), Some("fix the build"));
        assert_eq!(found[0].cwd.as_deref(), Some("/w/proj"));
        assert_eq!(
            found[0].updated_ms,
            parse_timestamp("2026-01-02T00:00:00.000Z").unwrap()
        );
    }
}
