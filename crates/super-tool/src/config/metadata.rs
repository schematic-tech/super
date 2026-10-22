//! The profile owns credentials and discovery hints, Git owns link placement. Files are
//! scoped by a stable profile UUID so separate `SUPER_CONFIG_DIR` profiles stay independent.
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{
    ApiRepositories, CredentialSource, LocalRepository, LocalRevision, LocalState, PendingSubmission, StateStore,
    open_private,
};
use crate::git::GitRepository;

#[cfg(test)]
mod tests;

const PROFILE_SCHEMA: u32 = 2;
const METADATA_SCHEMA: u32 = 1;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Profile {
    schema_version: u32,
    id: Uuid,
    api_url: Option<String>,
    credential: Option<CredentialSource>,
    credential_api_url: Option<String>,
    /// Discovery hints only: a directory without its own link is never linked by this index.
    worktrees: BTreeSet<PathBuf>,
    /// Preserve offline or missing legacy checkouts until they can be migrated safely.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    unmigrated: BTreeMap<String, ApiRepositories>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorktreeState {
    schema_version: u32,
    environments: BTreeMap<String, Link>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Link {
    association_id: String,
    workspace_id: Uuid,
    name: String,
    linked: bool,
    last_seen_oid: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SharedState {
    schema_version: u32,
    temporary_commits: HashMap<String, String>,
    source_hashes: HashMap<String, String>,
    environments: BTreeMap<String, SharedEnvironment>,
}

impl Default for SharedState {
    fn default() -> Self {
        Self {
            schema_version: METADATA_SCHEMA,
            temporary_commits: HashMap::new(),
            source_hashes: HashMap::new(),
            environments: BTreeMap::new(),
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SharedEnvironment {
    // Revision IDs are scoped to their remote workspace, even within the same Git clone.
    revisions: BTreeMap<Uuid, HashMap<String, LocalRevision>>,
    pending_submissions: BTreeMap<String, Vec<PendingSubmission>>,
}

fn scopes(state: &LocalState) -> BTreeMap<String, ApiRepositories> {
    let mut scopes = state.other_api_repositories.clone();
    scopes.insert(
        state.api_url.clone().unwrap_or_default(),
        ApiRepositories {
            repositories: state.repositories.clone(),
            unlinked_repositories: state.unlinked_repositories.clone(),
        },
    );
    scopes
}

fn repositories(scope: &ApiRepositories) -> impl Iterator<Item = (&LocalRepository, bool)> {
    scope
        .repositories
        .iter()
        .map(|repo| (repo, true))
        .chain(scope.unlinked_repositories.iter().map(|repo| (repo, false)))
}

pub(super) fn load(store: &StateStore) -> Result<(Option<Profile>, LocalState)> {
    let path = store.state_path();
    let Some(value) = read::<serde_json::Value>(&path)? else {
        return Ok((None, LocalState::default()));
    };
    match value.get("schema_version").and_then(serde_json::Value::as_u64) {
        Some(1) => {
            return Ok((
                None,
                serde_json::from_value(value).with_context(|| format!("could not read {}", path.display()))?,
            ));
        }
        Some(2) => {}
        _ => anyhow::bail!(
            "{} uses an incompatible super state schema; expected version 1 or 2",
            path.display()
        ),
    }
    let profile: Profile =
        serde_json::from_value(value).with_context(|| format!("could not read {}", path.display()))?;
    let mut scopes = profile.unmigrated.clone();
    let roots: BTreeSet<_> = profile
        .worktrees
        .iter()
        .cloned()
        .chain(
            store
                .discovered_roots
                .lock()
                .expect("repository discovery lock poisoned")
                .iter()
                .cloned(),
        )
        .collect();
    for root in roots {
        let Some(git) = discover_exact(&root) else { continue };
        let path = worktree_path(&git, profile.id)?;
        let Some(worktree) = read_metadata::<WorktreeState>(&path)? else {
            continue;
        };
        let shared_path = shared_path(&git, profile.id);
        let shared = read_metadata::<SharedState>(&shared_path)?
            .with_context(|| format!("missing Super revision history at {}", shared_path.display()))?;
        for (api, link) in worktree.environments {
            let history = shared
                .environments
                .get(&api)
                .with_context(|| format!("missing Super API history at {}", shared_path.display()))?;
            let revisions = history
                .revisions
                .get(&link.workspace_id)
                .with_context(|| format!("missing Super workspace history at {}", shared_path.display()))?;
            let pending = history
                .pending_submissions
                .get(&link.association_id)
                .with_context(|| format!("missing Super submission journal at {}", shared_path.display()))?;
            let repository = LocalRepository {
                root: git.root.clone(),
                common_git_dir: git.common_git_dir.clone(),
                association_id: link.association_id,
                workspace_id: link.workspace_id,
                name: link.name,
                _legacy_dirty_preference: None,
                last_seen_oid: link.last_seen_oid,
                temporary_commits: shared.temporary_commits.clone(),
                source_hashes: shared.source_hashes.clone(),
                revisions: revisions.clone(),
                pending_submissions: pending.clone(),
            };
            let scope = scopes.entry(api).or_default();
            // A completed metadata write supersedes its migration fallback. Also replace a
            // stale path for the same association when a moved worktree is rediscovered.
            for entries in [&mut scope.repositories, &mut scope.unlinked_repositories] {
                entries.retain(|old| old.root != git.root && old.association_id != repository.association_id);
            }
            if link.linked {
                scope.repositories.push(repository);
            } else {
                scope.unlinked_repositories.push(repository);
            }
        }
    }
    let active = scopes
        .remove(profile.api_url.as_deref().unwrap_or_default())
        .unwrap_or_default();
    let state = LocalState {
        api_url: profile.api_url.clone(),
        credential: profile.credential,
        credential_api_url: profile.credential_api_url.clone(),
        repositories: active.repositories,
        unlinked_repositories: active.unlinked_repositories,
        other_api_repositories: scopes,
        ..LocalState::default()
    };
    Ok((Some(profile), state))
}

pub(super) fn save(
    store: &StateStore,
    profile: Option<Profile>,
    before: &LocalState,
    state: &LocalState,
) -> Result<()> {
    let mut profile = if let Some(profile) = profile {
        profile
    } else {
        let profile = Profile {
            schema_version: PROFILE_SCHEMA,
            id: Uuid::new_v4(),
            api_url: before.api_url.clone(),
            credential: before.credential,
            credential_api_url: before.credential_api_url.clone(),
            worktrees: BTreeSet::new(),
            unmigrated: scopes(before),
        };
        // Persist the profile identity and recovery data before writing any Git metadata.
        // A crash can leave redundant legacy entries, but cannot orphan existing links.
        write(&store.state_path(), &profile)?;
        profile
    };
    let previous = scopes(before);
    let mut remaining = scopes(state);
    let mut worktrees: BTreeMap<PathBuf, (GitRepository, WorktreeState)> = BTreeMap::new();
    let mut clones: BTreeMap<PathBuf, SharedState> = BTreeMap::new();
    for (api, scope) in &remaining {
        for (repository, linked) in repositories(scope) {
            let Some(git) = discover_exact(&repository.root) else {
                continue;
            };
            let shared_path = shared_path(&git, profile.id);
            if !clones.contains_key(&shared_path) {
                clones.insert(shared_path.clone(), read_metadata(&shared_path)?.unwrap_or_default());
            }
            let shared = clones.get_mut(&shared_path).expect("loaded shared state");
            let old = previous.get(api).and_then(|scope| {
                repositories(scope)
                    .map(|(repo, _)| repo)
                    .find(|repo| repo.association_id == repository.association_id)
            });
            merge_changes(
                &mut shared.temporary_commits,
                &repository.temporary_commits,
                old.map(|repo| &repo.temporary_commits),
            );
            merge_changes(
                &mut shared.source_hashes,
                &repository.source_hashes,
                old.map(|repo| &repo.source_hashes),
            );
            let history = shared.environments.entry(api.clone()).or_default();
            merge_changes(
                history.revisions.entry(repository.workspace_id).or_default(),
                &repository.revisions,
                old.filter(|repo| repo.workspace_id == repository.workspace_id)
                    .map(|repo| &repo.revisions),
            );
            history.pending_submissions.insert(
                repository.association_id.clone(),
                repository.pending_submissions.clone(),
            );
            let path = worktree_path(&git, profile.id)?;
            let (_, worktree) = worktrees.entry(path).or_insert_with(|| {
                (
                    git,
                    WorktreeState {
                        schema_version: METADATA_SCHEMA,
                        environments: BTreeMap::new(),
                    },
                )
            });
            worktree.environments.insert(
                api.clone(),
                Link {
                    association_id: repository.association_id.clone(),
                    workspace_id: repository.workspace_id,
                    name: repository.name.clone(),
                    linked,
                    last_seen_oid: repository.last_seen_oid.clone(),
                },
            );
        }
    }
    // Histories precede their pointers. Removed worktrees leave history available to the
    // clone; Git removes only the per-worktree link along with that worktree's admin dir.
    for (path, shared) in clones {
        write_metadata(&path, &shared)?;
    }
    // Keep unavailable discovery hints: an offline disk may return later. They cannot
    // restore a link themselves, and foreground commands rediscover moved worktrees.
    for (path, (git, worktree)) in worktrees {
        write_metadata(&path, &worktree)?;
        profile.worktrees.insert(git.root.clone());
        for scope in remaining.values_mut() {
            scope.repositories.retain(|repo| repo.root != git.root);
            scope.unlinked_repositories.retain(|repo| repo.root != git.root);
        }
    }
    remaining.retain(|_, scope| !scope.repositories.is_empty() || !scope.unlinked_repositories.is_empty());
    profile.unmigrated = remaining;
    profile.api_url.clone_from(&state.api_url);
    profile.credential = state.credential;
    profile.credential_api_url.clone_from(&state.credential_api_url);
    write(&store.state_path(), &profile)
}

// Multiple worktrees may see the same history. Merge only newly observed/changed entries
// so an unchanged view from a second worktree cannot undo the first worktree's update.
fn merge_changes<T: Clone + Eq>(
    target: &mut HashMap<String, T>,
    incoming: &HashMap<String, T>,
    before: Option<&HashMap<String, T>>,
) {
    for (key, value) in incoming {
        if !target.contains_key(key) || before.and_then(|before| before.get(key)) != Some(value) {
            target.insert(key.clone(), value.clone());
        }
    }
}

fn discover_exact(root: &Path) -> Option<GitRepository> {
    let git = GitRepository::discover(root).ok()?;
    // Git searches ancestors: a removed nested worktree must not become its parent's link.
    (root.canonicalize().ok()? == git.root).then_some(git)
}

fn worktree_path(git: &GitRepository, profile: Uuid) -> Result<PathBuf> {
    Ok(git.super_worktree_directory()?.join(format!("{profile}.json")))
}

fn shared_path(git: &GitRepository, profile: Uuid) -> PathBuf {
    git.common_git_dir.join("super/shared").join(format!("{profile}.json"))
}

fn read<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .with_context(|| format!("could not read {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("could not read {}", path.display())),
    }
}

fn read_metadata<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    let Some(value) = read::<serde_json::Value>(path)? else {
        return Ok(None);
    };
    if value.get("schema_version").and_then(serde_json::Value::as_u64) != Some(u64::from(METADATA_SCHEMA)) {
        anyhow::bail!("{} uses an incompatible Super Git metadata schema", path.display());
    }
    serde_json::from_value(value)
        .map(Some)
        .with_context(|| format!("could not read {}", path.display()))
}

fn write_metadata(path: &Path, value: &impl Serialize) -> Result<()> {
    let bytes = canonical_bytes(value)?;
    if fs::read(path).ok().as_deref() == Some(&bytes) {
        return Ok(());
    }
    let directory = path.parent().context("Super metadata has no parent directory")?;
    for directory in [
        directory
            .parent()
            .context("Super metadata has no namespace directory")?,
        directory,
    ] {
        if let Ok(metadata) = fs::symlink_metadata(directory)
            && metadata.file_type().is_symlink()
        {
            anyhow::bail!(
                "refusing to follow a symbolic link for Super metadata {}",
                directory.display()
            );
        }
        // The Git directory must already exist. Do not recreate a worktree's admin
        // directory if Git removes it between discovery and this write.
        let created = match fs::create_dir(directory) {
            Ok(()) => true,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && directory.is_dir() => false,
            Err(error) => return Err(error).with_context(|| format!("could not create {}", directory.display())),
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
            if created {
                File::open(directory.parent().context("metadata directory has no parent")?)?.sync_all()?;
            }
        }
        #[cfg(not(unix))]
        let _ = created;
    }
    write_bytes(path, &bytes)
}

fn canonical_bytes(value: &impl Serialize) -> Result<Vec<u8>> {
    // Canonical map order keeps a freshly deserialized HashMap from causing a rewrite.
    Ok(serde_json::to_vec_pretty(&serde_json::to_value(value)?)?)
}

fn write(path: &Path, value: &impl Serialize) -> Result<()> {
    let bytes = canonical_bytes(value)?;
    // Avoid observer feedback loops and unrelated writes when only one checkout changed.
    if fs::read(path).ok().as_deref() == Some(&bytes) {
        return Ok(());
    }
    write_bytes(path, &bytes)
}

fn write_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    let temporary = path.with_extension("json.tmp");
    let mut file = open_private(&temporary)?;
    file.set_len(0)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(&temporary, path).with_context(|| format!("could not replace {}", path.display()))?;
    #[cfg(unix)]
    File::open(path.parent().context("state file has no directory")?)?.sync_all()?;
    Ok(())
}
