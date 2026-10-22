//! The durable record of which source sessions were imported, and as which
//! chat. It lives beside the profile's store, apart from the registry: a
//! deleted chat's row is gone, but its ledger entry keeps "import all" from
//! bringing it back.
//!
//! Append-only JSON lines, one per imported session, written as each import
//! lands, so an engine crash mid-run never re-imports what already finished.
//! No fsync per line: a full sync per session cost more than the import
//! itself, and the registry rows it guards are themselves persisted on a
//! debounce. A torn last line is skipped on load.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub(crate) const LEDGER_FILE: &str = "history-import.jsonl";

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Entry {
    source: String,
    session_id: String,
    chat_id: String,
    imported_at_ms: i64,
}

pub(crate) struct Ledger {
    path: PathBuf,
    /// `(source, session id)` → chat id.
    entries: HashMap<(String, String), String>,
    /// The file ends in a torn line: the next entry starts on a fresh one.
    torn_tail: bool,
}

impl Ledger {
    pub(crate) fn open(store_root: &Path) -> Self {
        let path = store_root.join(LEDGER_FILE);
        let mut entries = HashMap::new();
        let mut torn_tail = false;
        if let Ok(raw) = std::fs::read_to_string(&path) {
            torn_tail = !raw.is_empty() && !raw.ends_with('\n');
            for line in raw.lines() {
                if let Ok(entry) = serde_json::from_str::<Entry>(line) {
                    entries.insert((entry.source, entry.session_id), entry.chat_id);
                }
            }
        }
        Self {
            path,
            entries,
            torn_tail,
        }
    }

    /// The chat a session was imported as, if it ever was.
    pub(crate) fn chat_for(&self, source: &str, session_id: &str) -> Option<&str> {
        self.entries
            .get(&(source.to_string(), session_id.to_string()))
            .map(String::as_str)
    }

    pub(crate) fn record(
        &mut self,
        source: &str,
        session_id: &str,
        chat_id: &str,
    ) -> std::io::Result<()> {
        let mut line = serde_json::to_vec(&Entry {
            source: source.to_string(),
            session_id: session_id.to_string(),
            chat_id: chat_id.to_string(),
            imported_at_ms: crate::now_ms(),
        })
        .map_err(std::io::Error::other)?;
        line.push(b'\n');
        if self.torn_tail {
            line.insert(0, b'\n');
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        file.write_all(&line)?;
        self.torn_tail = false;
        self.entries.insert(
            (source.to_string(), session_id.to_string()),
            chat_id.to_string(),
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_survive_reopen_and_a_torn_tail() {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = Ledger::open(dir.path());
        ledger.record("codex", "s1", "c1").unwrap();
        ledger.record("claude", "s1", "c2").unwrap();
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(dir.path().join(LEDGER_FILE))
            .unwrap();
        file.write_all(b"{\"source\":\"codex\",\"sess").unwrap();
        let mut ledger = Ledger::open(dir.path());
        ledger.record("codex", "s3", "c3").unwrap();
        let ledger = Ledger::open(dir.path());
        assert_eq!(ledger.chat_for("codex", "s3"), Some("c3"));
        assert_eq!(ledger.chat_for("codex", "s1"), Some("c1"));
        assert_eq!(ledger.chat_for("claude", "s1"), Some("c2"));
        assert_eq!(ledger.chat_for("codex", "s2"), None);
    }
}
