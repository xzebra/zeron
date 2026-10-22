//! Importing conversation history from other agent CLIs on this device
//! (Codex, Claude Code) as chats.
//!
//! Nothing here runs on its own: a person lists a source's sessions and asks
//! for some or all of them (`ListHistorySources` / `ListHistorySessions` /
//! `ImportHistory`). Each [`HistorySource`] knows where its CLI keeps
//! transcripts and replays one as events (the readers live beside each
//! driver's wire decoder in `zeron-harness`); everything after that is
//! shared:
//!
//!  - events fold into doc entries exactly as a live run's do ([`fold`]),
//!    and the doc lands in the born-chat2 shape `local_import` documents
//!    (`save_snapshot_with_cursor(id, bytes, 0, CHAT2_DOC_EPOCH)`);
//!  - the chat goes into this device's project for its working directory
//!    (reused, or created when the directory still exists), else it is
//!    project-less; it keeps its original dates, title and model, and the
//!    CLI's session id so continuing it resumes the same session;
//!  - the [`ledger`] records every imported session, so "import all" never
//!    revives a chat someone deleted, and listing skips imported sessions
//!    without parsing them;
//!  - one unreadable file or line is counted and skipped, never fatal.

mod claude;
mod codex;
mod fold;
mod ledger;

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use zeron_doc::SessionDoc;
use zeron_harness::history::{DiscoveredSession, TranscriptEvent, TranscriptInfo, summarize_title};
use zeron_proto::{ChatConfig, HarnessId, SandboxLevel};
use zeron_sync::DocsStore;

pub use claude::ClaudeHistory;
pub use codex::CodexHistory;

use crate::EngineError;
use crate::chat2_host::CHAT2_DOC_EPOCH;
use crate::workspace_host::{WorkspaceHost, linked_worktree_root};
use fold::EntryFold;
use ledger::Ledger;

/// Errors carried in a summary; the counts still cover every failure.
const MAX_REPORTED_ERRORS: usize = 20;
/// Default cap on a session listing.
const DEFAULT_LIST_LIMIT: usize = 500;

/// One CLI whose local history can be imported.
pub trait HistorySource: Send + Sync {
    /// Stable id used on the wire and in the ledger (`codex`, `claude`).
    fn id(&self) -> &'static str;
    fn label(&self) -> &'static str;
    /// The harness an imported chat continues on.
    fn harness(&self) -> HarnessId;
    /// Whether this device has the CLI's history store at all.
    fn available(&self) -> bool;
    /// Every importable session, newest first, from cheap head/tail probes.
    fn discover(&self) -> Vec<DiscoveredSession>;
    /// Replay one session, streaming its events into `emit`.
    fn load(
        &self,
        session: &DiscoveredSession,
        emit: &mut dyn FnMut(TranscriptEvent),
    ) -> std::io::Result<TranscriptInfo>;
    /// Whether resuming the CLI's session requires its original directory.
    fn resume_needs_cwd(&self) -> bool;
}

/// A source as the settings page shows it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistorySourceStatus {
    pub id: String,
    pub label: String,
    pub available: bool,
    pub total: usize,
    /// Sessions already here: imported (even if since deleted) or run by Zeron.
    pub imported: usize,
}

impl HistorySourceStatus {
    pub fn remaining(&self) -> usize {
        self.total.saturating_sub(self.imported)
    }
}

/// Where a listed session stands relative to this workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum HistorySessionState {
    /// Never imported.
    New,
    /// Imported, and its chat is still here.
    Imported,
    /// Imported, then its chat was deleted. "Import all" leaves it alone;
    /// picking it explicitly imports it again.
    Removed,
    /// Zeron itself ran this session: a chat already holds it.
    Native,
}

/// One session in a picker listing.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistorySessionRow {
    pub id: String,
    pub title: Option<String>,
    pub cwd: Option<String>,
    pub model: Option<String>,
    pub started_at: Option<i64>,
    pub updated_at: i64,
    pub size_bytes: u64,
    pub state: HistorySessionState,
    /// The chat holding this session, when one is here.
    pub chat_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistorySessionList {
    pub sessions: Vec<HistorySessionRow>,
    /// Every session the source has, before the listing cap.
    pub total: usize,
}

/// What an `ImportHistory` call asks for.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct HistoryImportRequest {
    pub source: String,
    /// Specific sessions; ignored when `all` is set.
    pub session_ids: Vec<String>,
    /// Every session not imported yet.
    pub all: bool,
}

/// Progress for one import run; the last item is always `Summary`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    tag = "kind"
)]
pub enum HistoryImportEvent {
    /// Emitted once, before any transcript is read.
    Start { total: usize },
    /// One session, emitted before it is read.
    Session {
        index: usize,
        total: usize,
        title: Option<String>,
    },
    /// Terminal summary, and the RPC stream's final item.
    Summary {
        imported: usize,
        skipped: usize,
        failed: usize,
        errors: Vec<String>,
        /// The chat written, when exactly one session was imported.
        chat_id: Option<String>,
    },
}

#[derive(Clone)]
pub struct HistoryImporter {
    inner: Arc<Inner>,
}

struct Inner {
    sources: Vec<Box<dyn HistorySource>>,
    device_id: String,
    workspace: WorkspaceHost,
    store: Arc<DocsStore>,
    ledger: Arc<Mutex<Ledger>>,
    /// One run at a time: two overlapping runs could import a session twice.
    running: Arc<Mutex<()>>,
}

/// What this workspace already holds, read once per call.
struct Known {
    live_chats: std::collections::HashSet<String>,
    /// `(harness, harness session id)` of this device's chats → chat id.
    native: HashMap<(HarnessId, String), String>,
}

impl HistoryImporter {
    pub fn new(
        sources: Vec<Box<dyn HistorySource>>,
        device_id: &str,
        workspace: WorkspaceHost,
        store: Arc<DocsStore>,
        store_root: &Path,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                sources,
                device_id: device_id.to_string(),
                workspace,
                store,
                ledger: Arc::new(Mutex::new(Ledger::open(store_root))),
                running: Arc::new(Mutex::new(())),
            }),
        }
    }

    /// The same importer (workspace, store, ledger) over other sources —
    /// for tests and tooling that point at a fixture store.
    #[doc(hidden)]
    pub fn with_sources(&self, sources: Vec<Box<dyn HistorySource>>) -> Self {
        Self {
            inner: Arc::new(Inner {
                sources,
                device_id: self.inner.device_id.clone(),
                workspace: self.inner.workspace.clone(),
                store: self.inner.store.clone(),
                ledger: self.inner.ledger.clone(),
                running: self.inner.running.clone(),
            }),
        }
    }

    /// The Codex and Claude Code stores this device's CLIs use
    /// (`$CODEX_HOME`, `$CLAUDE_CONFIG_DIR`, with their home defaults).
    pub fn default_sources(
        codex_home: &Path,
        claude_config_dir: &Path,
    ) -> Vec<Box<dyn HistorySource>> {
        vec![
            Box::new(CodexHistory::new(codex_home)),
            Box::new(ClaudeHistory::new(claude_config_dir)),
        ]
    }

    fn source(&self, id: &str) -> Result<&dyn HistorySource, EngineError> {
        self.inner
            .sources
            .iter()
            .find(|s| s.id() == id)
            .map(|s| s.as_ref())
            .ok_or_else(|| EngineError::Other(format!("unknown history source: {id}")))
    }

    fn known(&self) -> Result<Known, EngineError> {
        let mut known = Known {
            live_chats: Default::default(),
            native: HashMap::new(),
        };
        for chat in self.inner.workspace.read_chats()? {
            if chat.device_id == self.inner.device_id
                && let (Some(config), Some(session)) = (&chat.config, &chat.harness_session_id)
                && !session.is_empty()
            {
                known
                    .native
                    .insert((config.harness, session.clone()), chat.id.clone());
            }
            known.live_chats.insert(chat.id);
        }
        Ok(known)
    }

    fn state(
        &self,
        source: &dyn HistorySource,
        known: &Known,
        ledger: &Ledger,
        session_id: &str,
    ) -> (HistorySessionState, Option<String>) {
        if let Some(chat_id) = ledger.chat_for(source.id(), session_id) {
            return if known.live_chats.contains(chat_id) {
                (HistorySessionState::Imported, Some(chat_id.to_string()))
            } else {
                (HistorySessionState::Removed, None)
            };
        }
        match known
            .native
            .get(&(source.harness(), session_id.to_string()))
        {
            Some(chat_id) => (HistorySessionState::Native, Some(chat_id.clone())),
            None => (HistorySessionState::New, None),
        }
    }

    fn ledger(&self) -> std::sync::MutexGuard<'_, Ledger> {
        self.inner
            .ledger
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Every source with its counts. Blocking (fs) — run off the async path.
    pub fn sources(&self) -> Result<Vec<HistorySourceStatus>, EngineError> {
        let known = self.known()?;
        let mut out = Vec::new();
        for source in &self.inner.sources {
            let sessions = source.discover();
            let ledger = self.ledger();
            let imported = sessions
                .iter()
                .filter(|s| {
                    self.state(source.as_ref(), &known, &ledger, &s.id).0
                        != HistorySessionState::New
                })
                .count();
            out.push(HistorySourceStatus {
                id: source.id().to_string(),
                label: source.label().to_string(),
                available: source.available(),
                total: sessions.len(),
                imported,
            });
        }
        Ok(out)
    }

    /// A source's sessions, newest first, capped at `limit`.
    pub fn sessions(
        &self,
        source_id: &str,
        limit: Option<usize>,
    ) -> Result<HistorySessionList, EngineError> {
        let source = self.source(source_id)?;
        let known = self.known()?;
        let sessions = source.discover();
        let ledger = self.ledger();
        let rows = sessions
            .iter()
            .take(limit.unwrap_or(DEFAULT_LIST_LIMIT))
            .map(|s| {
                let (state, chat_id) = self.state(source, &known, &ledger, &s.id);
                HistorySessionRow {
                    id: s.id.clone(),
                    title: s.display_title(),
                    cwd: s.cwd.clone(),
                    model: s.model.clone(),
                    started_at: s.started_ms,
                    updated_at: s.updated_ms,
                    size_bytes: s.size_bytes,
                    state,
                    chat_id,
                }
            })
            .collect();
        Ok(HistorySessionList {
            sessions: rows,
            total: sessions.len(),
        })
    }

    /// Run one import, emitting [`HistoryImportEvent`]s (the last is always
    /// `Summary`). Blocking (fs + sqlite) — callers run it off the async path.
    pub fn run(
        &self,
        request: &HistoryImportRequest,
        mut emit: impl FnMut(HistoryImportEvent),
    ) -> Result<(), EngineError> {
        let source = self.source(&request.source)?;
        let Ok(_running) = self.inner.running.try_lock() else {
            return Err(EngineError::Other("An import is already running".into()));
        };
        let known = self.known()?;
        let sessions = source.discover();
        let mut skipped = 0;
        let mut failed = 0;
        let mut errors = Vec::new();
        let targets: Vec<&DiscoveredSession> = {
            let ledger = self.ledger();
            let state = |id: &str| self.state(source, &known, &ledger, id).0;
            if request.all {
                sessions
                    .iter()
                    .filter(|s| state(&s.id) == HistorySessionState::New)
                    .collect()
            } else {
                let mut picked = Vec::new();
                for id in &request.session_ids {
                    match sessions.iter().find(|s| &s.id == id) {
                        Some(s)
                            if matches!(
                                state(id),
                                HistorySessionState::New | HistorySessionState::Removed
                            ) =>
                        {
                            picked.push(s)
                        }
                        Some(_) => skipped += 1,
                        None => {
                            failed += 1;
                            errors.push(format!("{id}: no such session"));
                        }
                    }
                }
                picked
            }
        };

        let total = targets.len();
        emit(HistoryImportEvent::Start { total });
        let mut placement = Placement::new(&self.inner.workspace, &self.inner.device_id)?;
        let mut imported = Vec::new();
        for (index, session) in targets.into_iter().enumerate() {
            let title = session.display_title();
            emit(HistoryImportEvent::Session {
                index,
                total,
                title: title.clone(),
            });
            match self.import_one(source, session, &mut placement) {
                Ok(Some(chat_id)) => imported.push(chat_id),
                Ok(None) => skipped += 1,
                Err(err) => {
                    failed += 1;
                    let name = title.unwrap_or_else(|| session.id.clone());
                    tracing::warn!(source = source.id(), session = %session.id, error = %err,
                        "history import: session failed");
                    errors.push(format!("{name}: {err}"));
                }
            }
        }
        self.inner.workspace.flush();
        errors.truncate(MAX_REPORTED_ERRORS);
        emit(HistoryImportEvent::Summary {
            imported: imported.len(),
            skipped,
            failed,
            errors,
            chat_id: (imported.len() == 1).then(|| imported[0].clone()),
        });
        Ok(())
    }

    /// Import one session. `None` when it held no conversation to import.
    fn import_one(
        &self,
        source: &dyn HistorySource,
        session: &DiscoveredSession,
        placement: &mut Placement<'_>,
    ) -> Result<Option<String>, EngineError> {
        let mut fold = EntryFold::new(&self.inner.device_id);
        let info = source.load(session, &mut |event| fold.push(event))?;
        if info.malformed_lines > 0 {
            tracing::info!(source = source.id(), session = %session.id,
                malformed = info.malformed_lines, "history import: skipped unreadable lines");
        }
        let entries = fold.finish();
        if entries.is_empty() {
            return Ok(None);
        }

        let chat_id = crate::new_id();
        let doc = SessionDoc::init(&chat_id)?;
        for entry in &entries {
            doc.push_message(entry)?;
        }
        self.inner.store.save_snapshot_with_cursor(
            &chat_id,
            &doc.export_snapshot()?,
            0,
            CHAT2_DOC_EPOCH,
        )?;

        // Placement: the project for this directory on this device, when the
        // directory is still there; otherwise a project-less chat.
        let cwd = info
            .cwd
            .clone()
            .or_else(|| session.cwd.clone())
            .filter(|cwd| Path::new(cwd).is_dir());
        let space_id = cwd
            .as_deref()
            .map(|cwd| placement.space_for(cwd))
            .transpose()?;
        let config = ChatConfig {
            harness: source.harness(),
            model: info.model.clone().or_else(|| session.model.clone()),
            reasoning: None,
            model_options: Default::default(),
            sandbox: SandboxLevel::WorkspaceWrite,
        };
        let workspace = &self.inner.workspace;
        workspace.create_chat(
            &chat_id,
            space_id.as_deref(),
            Some(&self.inner.device_id),
            Some(config),
            cwd.clone(),
        )?;
        let mut row = workspace
            .chat(&chat_id)?
            .ok_or_else(|| EngineError::Other("imported chat row vanished".into()))?;
        let first_prompt = entries.iter().find_map(|e| {
            (e.role == zeron_doc::MessageRole::User)
                .then(|| fold::last_text(std::slice::from_ref(e)))
                .flatten()
        });
        row.title = info
            .title
            .clone()
            .or_else(|| session.title.clone())
            .or_else(|| first_prompt.as_deref().map(summarize_title))
            .or_else(|| Some(format!("{} session", source.label())));
        let first_at = entries.first().map(|e| e.created_at);
        if let Some(at) = info
            .started_ms
            .or(session.started_ms)
            .or(first_at)
            .and_then(chrono::DateTime::from_timestamp_millis)
        {
            row.created_at = at;
        }
        row.last_message_at =
            chrono::DateTime::from_timestamp_millis(info.last_ms.unwrap_or(session.updated_ms));
        row.last_message_preview =
            fold::last_text(&entries).map(|text| text.chars().take(120).collect());
        if space_id.is_some() {
            row.branch = info.branch.clone().or_else(|| session.branch.clone());
        }
        if space_id.is_some() || !source.resume_needs_cwd() {
            row.harness_session_id = Some(session.id.clone());
            row.harness_session_cwd = row.cwd.clone();
        }
        workspace.import_chat_row(&row)?;
        self.ledger().record(source.id(), &session.id, &chat_id)?;
        Ok(Some(chat_id))
    }
}

/// This device's projects by path, read once per run and extended as the
/// run creates new ones.
struct Placement<'a> {
    workspace: &'a WorkspaceHost,
    device_id: &'a str,
    by_path: HashMap<String, String>,
}

impl<'a> Placement<'a> {
    fn new(workspace: &'a WorkspaceHost, device_id: &'a str) -> Result<Self, EngineError> {
        let by_path = workspace
            .read_spaces()?
            .into_iter()
            .filter(|s| s.device_id == device_id)
            .map(|s| (s.path, s.id))
            .collect();
        Ok(Self {
            workspace,
            device_id,
            by_path,
        })
    }

    /// The project at `cwd`, else at its checkout root when `cwd` is a linked
    /// worktree, else a new one there (the same resolution a live run's
    /// first command uses).
    fn space_for(&mut self, cwd: &str) -> Result<String, EngineError> {
        if let Some(id) = self.by_path.get(cwd) {
            return Ok(id.clone());
        }
        let root = linked_worktree_root(Path::new(cwd)).unwrap_or_else(|| cwd.to_string());
        let id = match self.by_path.get(&root) {
            Some(id) => id.clone(),
            None => {
                let id = crate::new_id();
                self.workspace
                    .create_space(&id, self.device_id, &root, None, false)?;
                // A project for this path may have landed since the run began.
                let id = self
                    .workspace
                    .read_spaces()?
                    .into_iter()
                    .find(|s| s.device_id == self.device_id && s.path == root)
                    .map(|s| s.id)
                    .unwrap_or(id);
                self.by_path.insert(root, id.clone());
                id
            }
        };
        self.by_path.insert(cwd.to_string(), id.clone());
        Ok(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_serialize_camel_case() {
        let summary = serde_json::to_value(HistoryImportEvent::Summary {
            imported: 1,
            skipped: 0,
            failed: 0,
            errors: Vec::new(),
            chat_id: Some("c1".into()),
        })
        .unwrap();
        assert_eq!(summary["kind"], "summary");
        assert_eq!(summary["chatId"], "c1");
        let row = serde_json::to_value(HistorySessionRow {
            id: "s".into(),
            title: None,
            cwd: None,
            model: None,
            started_at: None,
            updated_at: 1,
            size_bytes: 2,
            state: HistorySessionState::Removed,
            chat_id: None,
        })
        .unwrap();
        assert_eq!(row["state"], "removed");
        assert_eq!(row["sizeBytes"], 2);
    }
}
