//! Device-local worktree destinations and branch prefixes. Existing chats
//! retain their saved cwd and branch.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use zeron_proto::{DEFAULT_WORKTREE_BRANCH_PREFIX, WorktreeSettings, WorktreeSettingsStatus};

use crate::EngineError;

pub(crate) struct WorktreePreferences {
    path: PathBuf,
    default_root: PathBuf,
    environment_override: Option<PathBuf>,
    branch_prefix_override: Option<String>,
    settings: Mutex<WorktreeSettings>,
}

impl WorktreePreferences {
    pub fn open(
        data_dir: &Path,
        default_root: PathBuf,
        environment_override: Option<PathBuf>,
    ) -> Self {
        let path = data_dir.join("worktree-settings.json");
        let settings = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<WorktreeSettings>(&bytes).ok())
            .filter(|settings| {
                !settings.use_custom_directory
                    || settings
                        .custom_directory
                        .as_deref()
                        .is_some_and(|path| !path.is_empty() && Path::new(path).is_absolute())
            })
            .map(|mut settings| {
                settings.branch_prefix = settings
                    .branch_prefix
                    .and_then(|prefix| normalize_branch_prefix(&prefix).ok());
                settings
            })
            .unwrap_or_default();
        Self {
            path,
            default_root,
            environment_override,
            branch_prefix_override: None,
            settings: Mutex::new(settings),
        }
    }

    /// An explicit branch prefix that takes precedence over preferences.
    pub fn with_branch_prefix_override(mut self, prefix: Option<String>) -> Self {
        self.branch_prefix_override = prefix.and_then(|prefix| {
            normalize_branch_prefix(&prefix)
                .inspect_err(
                    |error| tracing::warn!(%error, "ignoring worktree branch prefix override"),
                )
                .ok()
        });
        self
    }

    fn branch_prefix_for(&self, settings: &WorktreeSettings) -> String {
        self.branch_prefix_override
            .clone()
            .or_else(|| settings.branch_prefix.clone())
            .unwrap_or_else(|| DEFAULT_WORKTREE_BRANCH_PREFIX.to_string())
    }

    pub fn branch_prefix(&self) -> String {
        self.branch_prefix_for(&self.settings.lock().unwrap_or_else(PoisonError::into_inner))
    }

    fn root_for(&self, settings: &WorktreeSettings) -> PathBuf {
        self.environment_override.clone().unwrap_or_else(|| {
            if settings.use_custom_directory {
                settings
                    .custom_directory
                    .as_ref()
                    .map(PathBuf::from)
                    .unwrap_or_else(|| self.default_root.clone())
            } else {
                self.default_root.clone()
            }
        })
    }

    pub fn root(&self) -> PathBuf {
        self.root_for(&self.settings.lock().unwrap_or_else(PoisonError::into_inner))
    }

    pub fn status(&self) -> WorktreeSettingsStatus {
        let settings = self
            .settings
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        WorktreeSettingsStatus {
            effective_directory: self.root_for(&settings).to_string_lossy().into_owned(),
            default_directory: self.default_root.to_string_lossy().into_owned(),
            environment_override: self
                .environment_override
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
            effective_branch_prefix: Some(self.branch_prefix_for(&settings)),
            branch_prefix_environment_override: self.branch_prefix_override.clone(),
            settings,
        }
    }

    pub async fn set(
        &self,
        mut next: WorktreeSettings,
    ) -> Result<WorktreeSettingsStatus, EngineError> {
        let current = self
            .settings
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        // A save that resends the current location (e.g. a branch prefix
        // change) needs neither the location override lifted nor its disk.
        let same_location = next.use_custom_directory == current.use_custom_directory
            && next.custom_directory == current.custom_directory;
        if self.environment_override.is_some() && !same_location {
            return Err(EngineError::Other(
                "Worktree location is controlled by ZERON_WORKTREES_DIR on this device".into(),
            ));
        }
        next.branch_prefix = match next.branch_prefix {
            None => current.branch_prefix,
            Some(_) if self.branch_prefix_override.is_some() => {
                return Err(EngineError::Other(
                    "Worktree branch prefix is controlled by ZERON_WORKTREE_BRANCH_PREFIX on this device"
                        .into(),
                ));
            }
            Some(prefix) => Some(normalize_branch_prefix(&prefix)?),
        };
        if next.use_custom_directory && !same_location {
            let path = next
                .custom_directory
                .clone()
                .ok_or_else(|| EngineError::Other("Choose a worktree folder".into()))?;
            // A disconnected disk must not block an engine worker or leave the
            // settings save pending forever. A late probe cannot publish settings.
            let (tx, rx) = tokio::sync::oneshot::channel();
            std::thread::Builder::new()
                .name("worktree-folder-check".into())
                .spawn(move || {
                    let _ = tx.send(validate_directory(&path));
                })?;
            let path = tokio::time::timeout(Duration::from_secs(6), rx)
                .await
                .map_err(|_| EngineError::Other("Worktree folder check timed out".into()))?
                .map_err(|_| EngineError::Other("Worktree folder check failed".into()))??;
            next.custom_directory = Some(path.to_string_lossy().into_owned());
        }
        {
            let mut current = self.settings.lock().unwrap_or_else(PoisonError::into_inner);
            // Disabling must work even if the previous disk is disconnected,
            // and preserve the last validated directory for the next enable.
            // Disabling without a folder is a reset and forgets it.
            if !next.use_custom_directory && next.custom_directory.is_some() {
                next.custom_directory = current.custom_directory.clone();
            }
            let parent = self.path.parent().expect("settings have a data directory");
            std::fs::create_dir_all(parent)?;
            let mut temp = tempfile::NamedTempFile::new_in(parent)?;
            serde_json::to_writer_pretty(&mut temp, &next)
                .map_err(|error| EngineError::Other(error.to_string()))?;
            temp.write_all(b"\n")?;
            temp.as_file().sync_all()?;
            temp.persist(&self.path)
                .map_err(|error| EngineError::Io(error.error))?;
            *current = next;
        }
        Ok(self.status())
    }
}

fn validate_directory(value: &str) -> Result<PathBuf, EngineError> {
    let value = value.trim();
    let home = crate::repos::home_dir();
    let path = zeron_proto::device_paths::expand_home(value, &home.to_string_lossy())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(value));
    if !path.is_absolute() {
        return Err(EngineError::Other(
            "Choose an absolute folder path on the selected device".into(),
        ));
    }
    if path.components().any(|part| {
        part.as_os_str()
            .as_encoded_bytes()
            .eq_ignore_ascii_case(b".git")
    }) {
        return Err(EngineError::Other(
            "Choose a folder outside Git's .git directory".into(),
        ));
    }
    #[cfg(windows)]
    crate::repos::validate_windows_worktree_path(&path.to_string_lossy())?;
    std::fs::create_dir_all(&path)?;
    let path = std::fs::canonicalize(path)?;
    #[cfg(windows)]
    crate::repos::validate_windows_worktree_path(&path.to_string_lossy())?;
    if path.components().any(|part| {
        part.as_os_str()
            .as_encoded_bytes()
            .eq_ignore_ascii_case(b".git")
    }) {
        return Err(EngineError::Other(
            "Choose a folder outside Git's .git directory".into(),
        ));
    }
    // Test actual write access instead of relying on permission bits/ACLs.
    let _probe = tempfile::Builder::new()
        .prefix(".zeron-write-check-")
        .tempfile_in(&path)?;
    Ok(path)
}

/// Trimmed `value`, if `<value><name>` passes `git check-ref-format --branch`
/// for the lowercase `-`-joined names Zeron appends. No `/` is implied, so
/// `feat` yields `featswift-otter`.
fn normalize_branch_prefix(value: &str) -> Result<String, EngineError> {
    let prefix = value.trim();
    let name = format!("{prefix}name");
    let valid = !name.starts_with(['-', '/'])
        && !name.contains("..")
        && !name.contains("//")
        && !name.contains("@{")
        && !name
            .chars()
            .any(|c| c.is_ascii_control() || " ~^:?*[\\".contains(c))
        && name
            .split('/')
            .all(|part| !part.starts_with('.') && !part.ends_with(".lock"));
    if valid {
        Ok(prefix.to_string())
    } else {
        Err(EngineError::Other(format!(
            "\"{prefix}\" can't start a Git branch name"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn custom(path: &Path) -> WorktreeSettings {
        WorktreeSettings {
            use_custom_directory: true,
            custom_directory: Some(path.to_string_lossy().into_owned()),
            branch_prefix: None,
        }
    }

    fn prefix(prefix: &str) -> WorktreeSettings {
        WorktreeSettings {
            branch_prefix: Some(prefix.into()),
            ..WorktreeSettings::default()
        }
    }

    #[test]
    fn branch_prefixes_follow_git_ref_rules() {
        for (value, normalized) in [
            ("zeron/", "zeron/"),
            ("", ""),
            ("  feat/ ", "feat/"),
            ("feat", "feat"),
            ("jorge-", "jorge-"),
            ("team/jorge.", "team/jorge."),
            ("a.lockb/", "a.lockb/"),
        ] {
            assert_eq!(normalize_branch_prefix(value).unwrap(), normalized);
        }
        for value in [
            "-x", "/x", "a//", "a..b", "a b", "a~", "a^", "a:", "a?", "a*", "a[", "a\\", "a\tb",
            "a@{", "a/.", ".a", "a.lock/", "a\u{7f}",
        ] {
            assert!(normalize_branch_prefix(value).is_err(), "reject {value:?}");
        }
    }

    #[tokio::test]
    async fn branch_prefix_persists_and_location_saves_keep_it() {
        let tmp = tempfile::tempdir().unwrap();
        let default = tmp.path().join("default");
        let prefs = WorktreePreferences::open(tmp.path(), default.clone(), None);
        assert_eq!(prefs.branch_prefix(), DEFAULT_WORKTREE_BRANCH_PREFIX);
        assert!(prefs.set(prefix("bad prefix")).await.is_err());
        let saved = prefs.set(prefix("")).await.unwrap();
        assert_eq!(saved.effective_branch_prefix.as_deref(), Some(""));
        // Clients that predate the prefix save locations without one.
        let located = prefs.set(custom(&tmp.path().join("custom"))).await.unwrap();
        assert_eq!(located.settings.branch_prefix.as_deref(), Some(""));
        assert_eq!(
            WorktreePreferences::open(tmp.path(), default.clone(), None).branch_prefix(),
            ""
        );
        // A prefix save resends the location without revalidating it, so a
        // disconnected folder doesn't block it.
        std::fs::remove_dir_all(tmp.path().join("custom")).unwrap();
        let renamed = prefs
            .set(WorktreeSettings {
                branch_prefix: Some("jorge/".into()),
                ..located.settings
            })
            .await
            .unwrap();
        assert_eq!(prefs.branch_prefix(), "jorge/");
        assert!(!tmp.path().join("custom").exists());
        assert_eq!(renamed.effective_directory, located.effective_directory);
        // ZERON_WORKTREES_DIR fixes the location, not the prefix.
        let forced = WorktreePreferences::open(tmp.path(), default.clone(), Some(default.clone()));
        let status = forced
            .set(WorktreeSettings {
                branch_prefix: Some("zeron/".into()),
                ..renamed.settings
            })
            .await
            .unwrap();
        assert_eq!(status.effective_branch_prefix.as_deref(), Some("zeron/"));
        // An explicit prefix override wins and can't be changed.
        let overridden = WorktreePreferences::open(tmp.path(), default, None)
            .with_branch_prefix_override(Some("ci-".into()));
        assert_eq!(overridden.branch_prefix(), "ci-");
        assert!(overridden.set(prefix("other/")).await.is_err());
    }

    #[tokio::test]
    async fn location_persists_and_disabling_preserves_the_last_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let default = tmp.path().join("default");
        let chosen = tmp.path().join("another disk").join("checkouts");
        let prefs = WorktreePreferences::open(tmp.path(), default.clone(), None);
        assert_eq!(prefs.root(), default);
        let saved = prefs.set(custom(&chosen)).await.unwrap();
        assert_eq!(
            PathBuf::from(saved.effective_directory),
            chosen.canonicalize().unwrap()
        );
        assert_eq!(
            std::fs::read_dir(&chosen).unwrap().count(),
            0,
            "write probe is removed"
        );
        let reloaded = WorktreePreferences::open(tmp.path(), default.clone(), None);
        assert_eq!(prefs.status(), reloaded.status());
        // A disconnected location must not prevent reverting to the default.
        std::fs::remove_dir_all(&chosen).unwrap();
        let disabled = reloaded
            .set(WorktreeSettings {
                use_custom_directory: false,
                ..custom(&tmp.path().join("unvalidated"))
            })
            .await
            .unwrap();
        assert!(!disabled.settings.use_custom_directory);
        assert_eq!(
            disabled.settings.custom_directory,
            saved.settings.custom_directory
        );
        assert_eq!(reloaded.root(), default);
        assert!(!chosen.exists());
        let restarted = WorktreePreferences::open(tmp.path(), default.clone(), None);
        assert_eq!(restarted.status(), disabled);
        // Reset: disabling without a folder forgets the remembered one.
        let reset = restarted.set(WorktreeSettings::default()).await.unwrap();
        assert_eq!(reset.settings, WorktreeSettings::default());
        assert_eq!(
            WorktreePreferences::open(tmp.path(), default, None).status(),
            reset
        );
    }

    #[tokio::test]
    async fn invalid_paths_and_failed_persistence_leave_the_previous_settings_intact() {
        let tmp = tempfile::tempdir().unwrap();
        let prefs = WorktreePreferences::open(tmp.path(), tmp.path().join("default"), None);
        let original = prefs.status();
        let file = tmp.path().join("file");
        std::fs::write(&file, "not a directory").unwrap();
        for path in [
            PathBuf::from("relative"),
            PathBuf::new(),
            file,
            tmp.path().join(".git/worktrees"),
        ] {
            assert!(
                prefs.set(custom(&path)).await.is_err(),
                "reject {}",
                path.display()
            );
            assert_eq!(prefs.status(), original);
        }
        // A failed atomic replacement must not publish an in-memory change.
        std::fs::create_dir(tmp.path().join("worktree-settings.json")).unwrap();
        assert!(prefs.set(custom(&tmp.path().join("valid"))).await.is_err());
        assert_eq!(prefs.status(), original);
    }

    #[tokio::test]
    async fn explicit_override_wins_over_saved_preferences() {
        let tmp = tempfile::tempdir().unwrap();
        let default = tmp.path().join("default");
        let prefs = WorktreePreferences::open(tmp.path(), default.clone(), None);
        prefs.set(custom(&tmp.path().join("custom"))).await.unwrap();
        let forced = tmp.path().join("forced");
        let overridden = WorktreePreferences::open(tmp.path(), default, Some(forced.clone()));
        assert_eq!(overridden.root(), forced);
        assert_eq!(
            overridden.status().environment_override,
            Some(forced.to_string_lossy().into_owned())
        );
        assert!(overridden.set(WorktreeSettings::default()).await.is_err());
        assert_eq!(prefs.status().settings, overridden.status().settings);
    }

    #[test]
    fn corrupt_or_incomplete_preferences_use_the_default() {
        let tmp = tempfile::tempdir().unwrap();
        let default = tmp.path().join("default");
        for data in [
            "broken",
            "{}",
            r#"{"useCustomDirectory":true}"#,
            r#"{"useCustomDirectory":true,"customDirectory":"relative"}"#,
        ] {
            std::fs::write(tmp.path().join("worktree-settings.json"), data).unwrap();
            assert_eq!(
                WorktreePreferences::open(tmp.path(), default.clone(), None).root(),
                default
            );
        }
    }
}
