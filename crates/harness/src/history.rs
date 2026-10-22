//! Reading another agent CLI's on-disk history so it can be imported as chats.
//!
//! Each driver that has a store worth importing (`claude::transcript`,
//! `codex::transcript`) lists sessions with [`DiscoveredSession`] and replays
//! one as a stream of [`TranscriptEvent`]s — the same [`AgentEvent`]s its live
//! wire decoder produces, so imported tool chips render like live ones.
//!
//! Discovery only ever reads a bounded head and tail of each file, and a full
//! read streams line by line: a multi-GB transcript is never held in memory,
//! and a line that does not parse is counted and skipped, never fatal.

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use zeron_proto::AgentEvent;

/// Discovery's head budget: enough to reach the first prompt past a large
/// metadata preamble, small enough that listing thousands of files stays cheap.
pub(crate) const HEAD_PROBE_BYTES: u64 = 512 * 1024;
/// Discovery's tail budget: titles and the latest timestamps cluster at the end.
pub(crate) const TAIL_PROBE_BYTES: u64 = 64 * 1024;
/// Fallback titles (a session's first prompt) are cut here.
const TITLE_CHARS: usize = 80;

/// One importable session, as listed by a driver's `discover`.
#[derive(Debug, Clone, PartialEq)]
pub struct DiscoveredSession {
    /// The CLI's own session id — the key resume and the import ledger use.
    pub id: String,
    pub path: PathBuf,
    /// Working directory the session ran in, when the transcript names one.
    pub cwd: Option<String>,
    /// The CLI's own title for the session (renamed or generated), if any.
    pub title: Option<String>,
    /// The first prompt a person typed, already filtered of harness wrappers.
    pub first_prompt: Option<String>,
    pub model: Option<String>,
    pub branch: Option<String>,
    /// Epoch millis of the first timestamped record.
    pub started_ms: Option<i64>,
    /// Epoch millis of the latest activity (last record, else file mtime).
    pub updated_ms: i64,
    pub size_bytes: u64,
}

impl DiscoveredSession {
    /// The title to show: the CLI's own, else the first prompt, else nothing.
    pub fn display_title(&self) -> Option<String> {
        self.title
            .clone()
            .or_else(|| self.first_prompt.as_deref().map(summarize_title))
    }
}

/// One replayed event with the wall-clock time it happened.
#[derive(Debug, Clone, PartialEq)]
pub struct TranscriptEvent {
    pub at_ms: i64,
    pub event: AgentEvent,
}

/// What a full read learned about the session besides its events.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TranscriptInfo {
    pub cwd: Option<String>,
    pub title: Option<String>,
    pub model: Option<String>,
    pub branch: Option<String>,
    pub started_ms: Option<i64>,
    pub last_ms: Option<i64>,
    /// Lines skipped because they were not valid JSON for their record shape.
    pub malformed_lines: usize,
}

impl TranscriptInfo {
    pub(crate) fn note_time(&mut self, at_ms: Option<i64>) {
        if let Some(ms) = at_ms {
            self.started_ms.get_or_insert(ms);
            self.last_ms = Some(self.last_ms.map_or(ms, |last| last.max(ms)));
        }
    }
}

/// Parse an RFC 3339 timestamp into epoch millis.
pub fn parse_timestamp(raw: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|at| at.timestamp_millis())
}

pub(crate) fn modified_ms(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

/// A prompt cut down to a sidebar-sized title: its first non-empty line.
pub fn summarize_title(prompt: &str) -> String {
    let line = prompt
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    if line.chars().count() <= TITLE_CHARS {
        return line.to_string();
    }
    let cut: String = line.chars().take(TITLE_CHARS).collect();
    format!("{}…", cut.trim_end())
}

/// Stream every line of `path` through `on_record`, parsed as `T`. Lines that
/// fail to parse (a torn write after a crash, an unexpected shape) are counted
/// and skipped. Only an unreadable file is an error.
pub(crate) fn for_each_record<T: DeserializeOwned>(
    path: &Path,
    mut on_record: impl FnMut(T),
) -> std::io::Result<usize> {
    let mut reader = BufReader::with_capacity(256 * 1024, File::open(path)?);
    let mut line = Vec::new();
    let mut malformed = 0;
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        match serde_json::from_slice::<T>(&line) {
            Ok(record) => on_record(record),
            Err(_) => malformed += 1,
        }
    }
    Ok(malformed)
}

/// Parse the leading records of `path`, at most [`HEAD_PROBE_BYTES`] of them,
/// until `on_record` returns false.
pub(crate) fn probe_head<T: DeserializeOwned>(
    path: &Path,
    on_record: impl FnMut(T) -> bool,
) -> Option<()> {
    let file = File::open(path).ok()?;
    let mut reader = BufReader::with_capacity(64 * 1024, file.take(HEAD_PROBE_BYTES));
    probe_lines(&mut reader, on_record);
    Some(())
}

/// Parse the trailing records of `path` (the last [`TAIL_PROBE_BYTES`]).
pub(crate) fn probe_tail<T: DeserializeOwned>(
    path: &Path,
    len: u64,
    on_record: impl FnMut(T) -> bool,
) -> Option<()> {
    let mut file = File::open(path).ok()?;
    let from = len.saturating_sub(TAIL_PROBE_BYTES);
    file.seek(SeekFrom::Start(from)).ok()?;
    let mut reader = BufReader::with_capacity(64 * 1024, file);
    if from > 0 {
        // Mid-line: the first partial line is not a record.
        reader.read_until(b'\n', &mut Vec::new()).ok()?;
    }
    probe_lines(&mut reader, on_record);
    Some(())
}

fn probe_lines<T: DeserializeOwned>(
    reader: &mut impl BufRead,
    mut on_record: impl FnMut(T) -> bool,
) {
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        // A line cut off by the byte budget is not a record.
        if line.last() != Some(&b'\n') {
            break;
        }
        if let Ok(record) = serde_json::from_slice::<T>(&line)
            && !on_record(record)
        {
            break;
        }
    }
}

/// Text a CLI injected into a user turn rather than something a person typed:
/// a message made only of tagged blocks (`<command-name>…</command-name>`,
/// `<environment_context>…`, `<bash-stdout>…`), or one of the fixed notices.
pub(crate) fn is_injected_text(text: &str) -> bool {
    let text = text.trim();
    text.is_empty()
        || text.starts_with("[Request interrupted")
        || text.starts_with("Caveat: The messages below")
        || text.starts_with("# AGENTS.md instructions for ")
        || only_tagged_blocks(text)
}

/// Whether `text` is nothing but a sequence of `<tag>…</tag>` blocks.
fn only_tagged_blocks(text: &str) -> bool {
    let mut rest = text;
    let mut blocks = 0;
    loop {
        rest = rest.trim_start();
        if rest.is_empty() {
            return blocks > 0;
        }
        let Some(after_open) = rest.strip_prefix('<') else {
            return false;
        };
        let Some(close) = after_open.find('>') else {
            return false;
        };
        let tag = &after_open[..close];
        let name = tag.split_whitespace().next().unwrap_or("");
        if name.is_empty()
            || !name.starts_with(|c: char| c.is_ascii_alphabetic())
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return false;
        }
        let body = &after_open[close + 1..];
        if tag.ends_with('/') {
            rest = body;
        } else {
            let end = format!("</{name}>");
            let Some(at) = body.find(&end) else {
                return false;
            };
            rest = &body[at + end.len()..];
        }
        blocks += 1;
    }
}

/// `text` with any `<system-reminder>` blocks a CLI appended removed.
pub(crate) fn strip_system_reminders(text: &str) -> String {
    const OPEN: &str = "<system-reminder>";
    const CLOSE: &str = "</system-reminder>";
    if !text.contains(OPEN) {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(OPEN) {
        out.push_str(&rest[..start]);
        match rest[start..].find(CLOSE) {
            Some(end) => rest = &rest[start + end + CLOSE.len()..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn injected_text_is_recognized() {
        assert!(is_injected_text(
            "<command-name>/model</command-name>\n<command-message>model</command-message>\n<command-args></command-args>"
        ));
        assert!(is_injected_text("<bash-input>cat .env</bash-input>"));
        assert!(is_injected_text(
            "<local-command-stdout>SECRET=1</local-command-stdout>"
        ));
        assert!(is_injected_text(
            "<environment_context>\n  <cwd>/w</cwd>\n</environment_context>"
        ));
        assert!(is_injected_text("[Request interrupted by user]"));
        assert!(is_injected_text(
            "Caveat: The messages below were generated by the user while running local commands."
        ));
        assert!(!is_injected_text("fix the <div> alignment"));
        assert!(!is_injected_text("<b>bold</b> is wrong here"));
        assert!(!is_injected_text("make the tests pass"));
    }

    #[test]
    fn system_reminders_are_stripped() {
        assert_eq!(
            strip_system_reminders("do it\n<system-reminder>ctx</system-reminder>"),
            "do it"
        );
        assert_eq!(strip_system_reminders("plain"), "plain");
    }

    #[test]
    fn titles_take_the_first_line() {
        assert_eq!(summarize_title("\n  hello there\nmore"), "hello there");
        let long = "x".repeat(200);
        assert!(summarize_title(&long).ends_with('…'));
    }

    #[test]
    fn a_torn_line_is_counted_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        std::fs::write(&path, "{\"a\":1}\n{\"a\":\n{\"a\":2}\n{\"a\":3").unwrap();
        let mut seen = Vec::new();
        let malformed =
            for_each_record::<serde_json::Value>(&path, |v| seen.push(v["a"].clone())).unwrap();
        assert_eq!(seen, vec![serde_json::json!(1), serde_json::json!(2)]);
        assert_eq!(malformed, 2);
    }
}
