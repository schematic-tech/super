use std::{
    collections::{BTreeMap, HashMap},
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use directories::ProjectDirs;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const LOCAL_STATE_SCHEMA_VERSION: u32 = 1;

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
    PupApiKey,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalRepository {
    pub root: PathBuf,
    pub common_git_dir: PathBuf,
    pub association_id: String,
    pub workspace_id: Uuid,
    pub name: String,
    // Read older state files without applying or writing their saved prompt answer.
    #[serde(default, skip_serializing, rename = "dirty_preference")]
    pub _legacy_dirty_preference: Option<bool>,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalRevision {
    pub id: Uuid,
    pub tree_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingSubmission {
    pub request_sha256: String,
    pub idempotency_key: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct StateStore {
    directory: PathBuf,
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
        Ok(Self { directory })
    }

    pub fn load(&self) -> Result<LocalState> {
        let path = self.state_path();
        match fs::read(&path) {
            Ok(bytes) => {
                let value: serde_json::Value =
                    serde_json::from_slice(&bytes).with_context(|| format!("could not read {}", path.display()))?;
                let version = value.get("schema_version").and_then(serde_json::Value::as_u64);
                if version != Some(u64::from(LOCAL_STATE_SCHEMA_VERSION)) {
                    anyhow::bail!(
                        "{} uses an incompatible super state schema; expected version {LOCAL_STATE_SCHEMA_VERSION}",
                        path.display()
                    )
                }
                serde_json::from_value(value).with_context(|| format!("could not read {}", path.display()))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(LocalState::default()),
            Err(error) => Err(error).with_context(|| format!("could not read {}", path.display())),
        }
    }

    pub fn update<T>(&self, change: impl FnOnce(&mut LocalState) -> Result<T>) -> Result<T> {
        self.ensure_directory()?;
        let lock = self.open_lock()?;
        lock.lock_exclusive().context("could not lock super state")?;
        let mut state = self.load()?;
        let result = change(&mut state)?;
        self.save_unlocked(&state)?;
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

    /// Stores the Schematic API key outside the ordinary JSON state document.  The file is replaced
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
                        "stored Schematic API key {} is readable by another user; run `super login` again",
                        path.display()
                    )
                }
            }
        }
        match fs::read_to_string(&path) {
            Ok(value) => {
                let value = value.trim().to_owned();
                if value.is_empty() || value.chars().any(char::is_control) {
                    anyhow::bail!("the stored Schematic API key is invalid; run `super logout` then `super login`")
                }
                Ok(Some(value))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).with_context(|| "could not read the stored Schematic API key"),
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

    fn save_unlocked(&self, state: &LocalState) -> Result<()> {
        let path = self.state_path();
        let temporary = self.directory.join("state.json.tmp");
        let bytes = serde_json::to_vec_pretty(state)?;
        let mut file = open_private(&temporary)?;
        file.set_len(0)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, &path).with_context(|| format!("could not replace {}", path.display()))?;
        #[cfg(unix)]
        File::open(&self.directory)
            .context("could not open the super configuration directory for synchronization")?
            .sync_all()
            .context("could not synchronize the super configuration directory")?;
        Ok(())
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
        };
        fs::write(store.state_path(), br#"{"repositories":[]}"#).unwrap();
        assert!(
            store
                .load()
                .unwrap_err()
                .to_string()
                .contains("incompatible super state schema")
        );

        fs::write(store.state_path(), br#"{"schema_version":2,"repositories":[]}"#).unwrap();
        assert!(store.load().unwrap_err().to_string().contains("expected version 1"));
    }

    #[test]
    fn local_state_rejects_unknown_fields() {
        let directory = tempfile::tempdir().unwrap();
        let store = StateStore {
            directory: directory.path().to_owned(),
        };
        fs::write(store.state_path(), br#"{"schema_version":1,"mystery":true}"#).unwrap();
        assert!(store.load().unwrap_err().to_string().contains("could not read"));
    }

    #[cfg(unix)]
    #[test]
    fn stored_api_keys_must_remain_private() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let store = StateStore {
            directory: directory.path().to_owned(),
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
            _legacy_dirty_preference: None,
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
            _legacy_dirty_preference: None,
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
