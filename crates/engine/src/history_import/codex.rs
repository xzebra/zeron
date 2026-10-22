//! Codex history: `$CODEX_HOME/sessions` (default `~/.codex`).

use std::path::{Path, PathBuf};

use zeron_harness::codex::transcript;
use zeron_harness::history::{DiscoveredSession, TranscriptEvent, TranscriptInfo};
use zeron_proto::HarnessId;

use super::HistorySource;

pub struct CodexHistory {
    home: PathBuf,
}

impl CodexHistory {
    pub fn new(home: &Path) -> Self {
        Self {
            home: home.to_path_buf(),
        }
    }
}

impl HistorySource for CodexHistory {
    fn id(&self) -> &'static str {
        "codex"
    }

    fn label(&self) -> &'static str {
        "Codex"
    }

    fn harness(&self) -> HarnessId {
        HarnessId::Codex
    }

    fn available(&self) -> bool {
        self.home.join("sessions").is_dir()
    }

    fn discover(&self) -> Vec<DiscoveredSession> {
        transcript::discover(&self.home)
    }

    fn load(
        &self,
        session: &DiscoveredSession,
        emit: &mut dyn FnMut(TranscriptEvent),
    ) -> std::io::Result<TranscriptInfo> {
        transcript::read(&session.path, emit)
    }

    /// Codex threads resume by id from any directory.
    fn resume_needs_cwd(&self) -> bool {
        false
    }
}
