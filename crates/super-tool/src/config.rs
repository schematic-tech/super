use std::{
    collections::{BTreeMap, HashMap},
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use directories::ProjectDirs;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

mod metadata;

const LOCAL_STATE_SCHEMA_VERSION: u32 = 1;

/// Command-facing view, also used to read the legacy profile format. Current persistence
/// separates profile settings, worktree links, and clone history in `metadata`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalState {
    pub schema_version: u32,
    #[serde(default)]
    pub api_url: Option<String>,
    #[serde(default)]
    pub credential: Option<CredentialSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_api_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser_login_key: Option<Uuid>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub other_api_repositories: BTreeMap<String, ApiRepositories>,
    #[serde(default)]
    pub repositories: Vec<LocalRepository>,
    #[serde(default)]
    pub unlinked_repositories: Vec<LocalRepository>,
}

impl Default for LocalState {
    fn default() -> Self {
        Self {
            schema_version: LOCAL_STATE_SCHEMA_VERSION,
            api_url: None,
            credential: None,
            credential_api_url: None,
            browser_login_key: None,
            other_api_repositories: BTreeMap::new(),
            repositories: Vec::new(),
            unlinked_repositories: Vec::new(),
        }
    }
}

/// Links and source caches belong to exactly one API environment.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApiRepositories {
    pub repositories: Vec<LocalRepository>,
    pub unlinked_repositories: Vec<LocalRepository>,
}

impl LocalState {
    pub fn select_api(&mut self, api_url: &str) {
        if self.api_url.as_deref() == Some(api_url) {
            return;
        }
        // Old profiles may have links but no saved origin (for example CI using an explicit
        // endpoint). Their first resolved API establishes that existing scope.
        if self.api_url.is_none() {
            self.api_url = Some(api_url.to_owned());
            return;
        }
        if let Some(previous) = self.api_url.take() {
            if self.credential.is_some() && self.credential_api_url.is_none() {
                self.credential_api_url = Some(previous.clone());
            }
            self.other_api_repositories.insert(
                previous,
                ApiRepositories {
                    repositories: std::mem::take(&mut self.repositories),
                    unlinked_repositories: std::mem::take(&mut self.unlinked_repositories),
                },
            );
        }
        let incoming = self.other_api_repositories.remove(api_url).unwrap_or_default();
        self.repositories = incoming.repositories;
        self.unlinked_repositories = incoming.unlinked_repositories;
        self.api_url = Some(api_url.to_owned());
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CredentialSource {
    /// A Schematic API key whose secret is stored in `api-key` with mode 0600.
    #[serde(alias = "pup_api_key")]
    SuperApiKey,
    /// A browser login with privately stored access and refresh tokens.
    #[serde(rename = "oauth")]
    OAuth,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalRepository {
    pub root: PathBuf,
    pub common_git_dir: PathBuf,
    pub association_id: String,
    pub workspace_id: Uuid,
    pub name: String,
    #[serde(default)]
    pub last_seen_oid: Option<String>,
    #[serde(default)]
    pub temporary_commits: HashMap<String, String>,
    /// Verified mapping from Git blob identity to Super's portable SHA-256 content identity.
    #[serde(default)]
    pub source_hashes: HashMap<String, String>,
    /// Stable Super revision and authoritative source-tree identities for each locally selected Git
    /// commit already admitted to the workspace.
    #[serde(default)]
    pub revisions: HashMap<String, LocalRevision>,
    /// Crash-safe idempotency entries for submissions whose API response has not been journaled.
    #[serde(default)]
    pub pending_submissions: Vec<PendingSubmission>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LocalRevision {
    pub id: Uuid,
    pub tree_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PendingSubmission {
    pub request_sha256: String,
    pub idempotency_key: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct StateStore {
    directory: PathBuf,
    discovered_roots: Arc<Mutex<Vec<PathBuf>>>,
}

impl StateStore {
    pub fn discover() -> Result<Self> {
        let directory = if let Some(path) = std::env::var_os("SUPER_CONFIG_DIR") {
            PathBuf::from(path)
        } else {
            ProjectDirs::from("tech", "Schematic", "super")
                .context("could not determine the super configuration directory")?
                .config_dir()
                .to_owned()
        };
        Ok(Self {
            directory,
            discovered_roots: Arc::default(),
        })
    }

    pub fn load(&self) -> Result<LocalState> {
        // Existing readers participate in the writer lock without creating files on a
        // read-only command. Atomic files also allow a first reader during initialization.
        let lock = match File::open(self.lock_path()) {
            Ok(lock) => {
                FileExt::lock_shared(&lock).context("could not lock super state")?;
                Some(lock)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error).context("could not open super state lock"),
        };
        let result = metadata::load(self).map(|(_, state)| state);
        drop(lock);
        result
    }

    /// Discover links from Git itself, including after a checkout has moved. The profile's
    /// directory list is only a hint for the observer; reading a link does not rewrite state.
    pub fn load_for(&self, git: &crate::git::GitRepository) -> Result<LocalState> {
        let mut roots = self
            .discovered_roots
            .lock()
            .expect("repository discovery lock poisoned");
        if !roots.contains(&git.root) {
            roots.push(git.root.clone());
        }
        drop(roots);
        self.load()
    }

    pub fn update<T>(&self, change: impl FnOnce(&mut LocalState) -> Result<T>) -> Result<T> {
        self.ensure_directory()?;
        let lock = self.open_lock()?;
        lock.lock_exclusive().context("could not lock super state")?;
        let (profile, mut state) = metadata::load(self)?;
        let before = state.clone();
        let result = change(&mut state)?;
        metadata::save(self, profile, &before, &state)?;
        FileExt::unlock(&lock).context("could not unlock super state")?;
        Ok(result)
    }

    pub fn select_api(&self, api_url: &str) -> Result<()> {
        if self.load()?.api_url.as_deref().is_some_and(|saved| saved != api_url) {
            self.update(|state| {
                state.select_api(api_url);
                Ok(())
            })?;
        }
        Ok(())
    }

    pub fn daemon_lock(&self) -> Result<File> {
        self.ensure_directory()?;
        let path = self.directory.join("daemon.lock");
        open_private(&path)
    }

    pub fn credential_lock(&self) -> Result<File> {
        self.try_credential_lock()?
            .context("another super sign-in operation is already running")
    }

    pub fn try_credential_lock(&self) -> Result<Option<File>> {
        self.ensure_directory()?;
        let lock = open_private(&self.directory.join("credential.lock"))?;
        match lock.try_lock_exclusive() {
            Ok(()) => Ok(Some(lock)),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
            Err(error) => Err(error).context("could not lock your saved login"),
        }
    }

    /// Stores the Super API key outside the ordinary JSON state document.  The file is replaced
    /// atomically and is private to the current user.
    pub fn save_api_key(&self, value: &str) -> Result<()> {
        if value.trim().is_empty() || value.chars().any(char::is_control) {
            anyhow::bail!("the Schematic API key is empty or contains control characters")
        }
        self.ensure_directory()?;
        let path = self.api_key_path();
        let temporary = self.directory.join("api-key.tmp");
        let mut file = open_private(&temporary)?;
        file.set_len(0)?;
        file.write_all(value.trim().as_bytes())?;
        file.sync_all()?;
        fs::rename(&temporary, &path).with_context(|| format!("could not replace {}", path.display()))?;
        #[cfg(unix)]
        File::open(&self.directory)?.sync_all()?;
        Ok(())
    }

    /// Reads the private Schematic API key, if the user is logged in.
    pub fn load_api_key(&self) -> Result<Option<String>> {
        let path = self.api_key_path();
        if let Ok(metadata) = fs::symlink_metadata(&path) {
            if metadata.file_type().is_symlink() {
                anyhow::bail!(
                    "refusing to follow a symbolic link for private super state {}",
                    path.display()
                )
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if metadata.permissions().mode() & 0o077 != 0 {
                    anyhow::bail!(
                        "saved Schematic login {} is readable by another user; run `super login` again",
                        path.display()
                    )
                }
            }
        }
        match fs::read_to_string(&path) {
            Ok(value) => {
                let value = value.trim().to_owned();
                if value.is_empty() || value.chars().any(char::is_control) {
                    anyhow::bail!("the saved Schematic login is invalid; run `super login` again")
                }
                Ok(Some(value))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).with_context(|| "could not read your saved Schematic login"),
        }
    }

    /// Removes the private credential file.  Missing credentials are already logged out.
    pub fn clear_api_key(&self) -> Result<()> {
        match fs::remove_file(self.api_key_path()) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).with_context(|| "could not remove the stored Schematic API key"),
        }
    }

    pub fn profile_directory(&self) -> &Path {
        &self.directory
    }

    fn state_path(&self) -> PathBuf {
        self.directory.join("state.json")
    }

    fn api_key_path(&self) -> PathBuf {
        self.directory.join("api-key")
    }

    fn lock_path(&self) -> PathBuf {
        self.directory.join("state.lock")
    }

    fn ensure_directory(&self) -> Result<()> {
        fs::create_dir_all(&self.directory).with_context(|| {
            format!(
                "could not create super configuration directory {}",
                self.directory.display()
            )
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&self.directory, fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }

    fn open_lock(&self) -> Result<File> {
        open_private(&self.lock_path())
    }
}

fn open_private(path: &Path) -> Result<File> {
    if let Ok(metadata) = fs::symlink_metadata(path)
        && metadata.file_type().is_symlink()
    {
        anyhow::bail!(
            "refusing to follow a symbolic link for private super state {}",
            path.display()
        )
    }
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(path)
        .with_context(|| format!("could not open {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // The mode option affects only newly created files.  Tighten an existing credential or
        // temporary file as well, so a prior permissive file cannot survive a replacement.
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

pub fn find_repository<'a>(state: &'a LocalState, root: &Path) -> Option<&'a LocalRepository> {
    state.repositories.iter().find(|repository| repository.root == root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_state_requires_the_exact_schema() {
        let directory = tempfile::tempdir().unwrap();
        let store = StateStore {
            directory: directory.path().to_owned(),
            discovered_roots: Arc::default(),
        };
        fs::write(store.state_path(), br#"{"repositories":[]}"#).unwrap();
        assert!(
            store
                .load()
                .unwrap_err()
                .to_string()
                .contains("incompatible super state schema")
        );

        fs::write(store.state_path(), br#"{"schema_version":3,"repositories":[]}"#).unwrap();
        assert!(
            store
                .load()
                .unwrap_err()
                .to_string()
                .contains("incompatible super state schema")
        );
    }

    #[test]
    fn local_state_rejects_unknown_fields() {
        let directory = tempfile::tempdir().unwrap();
        let store = StateStore {
            directory: directory.path().to_owned(),
            discovered_roots: Arc::default(),
        };
        fs::write(store.state_path(), br#"{"schema_version":1,"mystery":true}"#).unwrap();
        assert!(store.load().unwrap_err().to_string().contains("could not read"));
    }

    #[test]
    fn local_state_migrates_the_former_saved_credential_name() {
        let state: LocalState = serde_json::from_str(
            r#"{
                "schema_version": 1,
                "api_url": "https://api.schematic.example",
                "credential": "pup_api_key",
                "credential_api_url": "https://api.schematic.example",
                "repositories": []
            }"#,
        )
        .unwrap();

        assert_eq!(state.credential, Some(CredentialSource::SuperApiKey));
        let saved = serde_json::to_string(&state).unwrap();
        assert!(saved.contains("super_api_key"));
        assert!(!saved.contains("pup_api_key"));
    }

    #[cfg(unix)]
    #[test]
    fn stored_api_keys_must_remain_private() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let store = StateStore {
            directory: directory.path().to_owned(),
            discovered_roots: Arc::default(),
        };
        store.save_api_key("pup_live_test").unwrap();
        fs::set_permissions(store.api_key_path(), fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            store
                .load_api_key()
                .unwrap_err()
                .to_string()
                .contains("readable by another user")
        );
    }

    #[test]
    fn associations_match_exact_repository_roots() {
        let repository = |root: &str| LocalRepository {
            root: PathBuf::from(root),
            common_git_dir: PathBuf::from(root).join(".git"),
            association_id: root.into(),
            workspace_id: Uuid::nil(),
            name: "repo".into(),
            last_seen_oid: None,
            temporary_commits: HashMap::new(),
            source_hashes: HashMap::new(),
            revisions: HashMap::new(),
            pending_submissions: Vec::new(),
        };
        let state = LocalState {
            repositories: vec![repository("/work"), repository("/work/nested")],
            ..LocalState::default()
        };
        assert_eq!(
            find_repository(&state, Path::new("/work/nested")).unwrap().root,
            PathBuf::from("/work/nested")
        );
        assert!(find_repository(&state, Path::new("/work/unlinked")).is_none());
    }

    #[test]
    fn unlinked_associations_are_not_active_repositories() {
        let repository = LocalRepository {
            root: PathBuf::from("/work"),
            common_git_dir: PathBuf::from("/work/.git"),
            association_id: "association".into(),
            workspace_id: Uuid::nil(),
            name: "repo".into(),
            last_seen_oid: None,
            temporary_commits: HashMap::new(),
            source_hashes: HashMap::new(),
            revisions: HashMap::new(),
            pending_submissions: Vec::new(),
        };
        let state = LocalState {
            unlinked_repositories: vec![repository],
            ..LocalState::default()
        };

        assert!(find_repository(&state, Path::new("/work")).is_none());
    }
}
