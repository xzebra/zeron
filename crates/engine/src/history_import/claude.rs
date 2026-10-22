//! Claude Code history: `$CLAUDE_CONFIG_DIR/projects` (default `~/.claude`).

use std::path::{Path, PathBuf};

use zeron_harness::claude::transcript;
use zeron_harness::history::{DiscoveredSession, TranscriptEvent, TranscriptInfo};
use zeron_proto::HarnessId;

use super::HistorySource;

pub struct ClaudeHistory {
    config_dir: PathBuf,
}

impl ClaudeHistory {
    pub fn new(config_dir: &Path) -> Self {
        Self {
            config_dir: config_dir.to_path_buf(),
        }
    }
}

impl HistorySource for ClaudeHistory {
    fn id(&self) -> &'static str {
        "claude"
    }

    fn label(&self) -> &'static str {
        "Claude Code"
    }

    fn harness(&self) -> HarnessId {
        HarnessId::ClaudeCode
    }

    fn available(&self) -> bool {
        self.config_dir.join("projects").is_dir()
    }

    fn discover(&self) -> Vec<DiscoveredSession> {
        transcript::discover(&self.config_dir)
    }

    fn load(
        &self,
        session: &DiscoveredSession,
        emit: &mut dyn FnMut(TranscriptEvent),
    ) -> std::io::Result<TranscriptInfo> {
        transcript::read(&session.path, emit)
    }

    /// Claude keys its session store by working directory: `--resume` only
    /// finds the session when launched from the directory it ran in.
    fn resume_needs_cwd(&self) -> bool {
        true
    }
}
