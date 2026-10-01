use std::{
    collections::BTreeSet,
    fs::File,
    io::{Seek, Write},
    path::PathBuf,
    process::{Command, Stdio},
    time::Duration,
};

use crate::{
    api::{PupClient, RevisionCache},
    config::{CredentialSource, LocalRepository, LocalRevision, StateStore},
    git::GitRepository,
};
use anyhow::{Context, Result};
use fs2::FileExt;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};

const FALLBACK_RECONCILE_INTERVAL: Duration = Duration::from_secs(30);
const EVENT_COALESCE_DELAY: Duration = Duration::from_millis(75);

pub fn ensure_daemon(store: &StateStore) -> Result<()> {
    if std::env::var_os("SUPER_NO_DAEMON").is_some() || std::env::var_os("SUPER_ACCESS_TOKEN").is_some() {
        return Ok(());
    }
    let lock = store.daemon_lock()?;
    if lock.try_lock_exclusive().is_err() {
        return Ok(());
    }
    FileExt::unlock(&lock)?;
    let executable = std::env::current_exe().context("could not locate the super executable")?;
    Command::new(executable)
        .arg("daemon")
        .env_remove("SUPER_ACCESS_TOKEN")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("could not start the super repository observer")?;
    Ok(())
}

pub async fn run(store: StateStore) -> Result<()> {
    let mut lock = store.daemon_lock()?;
    if lock.try_lock_exclusive().is_err() {
        return Ok(());
    }
    record_process(&mut lock)?;
    loop {
        let state = store.load()?;
        if state.repositories.is_empty() || state.credential != Some(CredentialSource::PupApiKey) {
            return Ok(());
        }
        reconcile_all(&store).await?;
        let (watcher, mut events) = git_watcher(&store)?;
        tokio::select! {
            () = tokio::time::sleep(FALLBACK_RECONCILE_INTERVAL) => {}
            event = events.recv() => {
                if event.is_none() {
                    return Ok(());
                }
                tokio::time::sleep(EVENT_COALESCE_DELAY).await;
                while events.try_recv().is_ok() {}
            }
        }
        drop(watcher);
    }
}

fn git_watcher(store: &StateStore) -> Result<(RecommendedWatcher, tokio::sync::mpsc::Receiver<()>)> {
    let (sender, receiver) = tokio::sync::mpsc::channel(1);
    let mut watcher = notify::recommended_watcher(move |_event| {
        let _ = sender.try_send(());
    })
    .context("could not start the super Git observer")?;
    watcher
        .watch(store.profile_directory(), RecursiveMode::NonRecursive)
        .context("could not watch the super profile")?;
    let state = store.load()?;
    let roots = state
        .repositories
        .iter()
        .map(|repository| repository.common_git_dir.clone())
        .collect::<BTreeSet<PathBuf>>();
    for root in roots {
        if root.exists() {
            watcher
                .watch(&root, RecursiveMode::Recursive)
                .with_context(|| format!("could not watch Git metadata at {}", root.display()))?;
        }
    }
    Ok((watcher, receiver))
}

fn record_process(lock: &mut File) -> Result<()> {
    lock.set_len(0)?;
    lock.rewind()?;
    writeln!(lock, "pid={}\nprotocol=1", std::process::id())?;
    lock.flush()?;
    Ok(())
}

async fn reconcile_all(store: &StateStore) -> Result<()> {
    let state = store.load()?;
    let Some(api_url) = state.api_url.as_deref() else {
        return Ok(());
    };
    if state.credential != Some(CredentialSource::PupApiKey) {
        return Ok(());
    }
    let Some(token) = store.load_api_key()? else {
        return Ok(());
    };
    // A foreground command may temporarily use a key from another environment. Never
    // reconcile that environment's links using the independently saved login credential.
    let credential_api = crate::key_api_url(
        Some(&token),
        None,
        state.credential_api_url.as_deref().or(Some(api_url)),
    )?;
    if credential_api != api_url {
        return Ok(());
    }
    let client = PupClient::new(api_url, Some(&token))?;
    for repository in state.repositories {
        let git = from_local(&repository);
        let Ok(head) = git.head() else {
            continue;
        };
        if repository.last_seen_oid.as_deref() == Some(&head.oid) {
            continue;
        }
        let parent_id = head
            .parent_oid
            .as_ref()
            .and_then(|oid| repository.revisions.get(oid).map(|revision| revision.id));
        let Ok((revision, snapshot)) = client
            .ensure_revision(
                repository.workspace_id,
                &git,
                &head,
                RevisionCache {
                    parent_id,
                    source_hashes: &repository.source_hashes,
                    revision: repository.revisions.get(&head.oid),
                },
            )
            .await
        else {
            continue;
        };
        store.update(|state| {
            if let Some(local) = state
                .repositories
                .iter_mut()
                .find(|local| local.association_id == repository.association_id)
            {
                local.last_seen_oid = Some(head.oid.clone());
                if let Some(snapshot) = snapshot {
                    local.source_hashes = snapshot.source_hashes;
                }
                local.revisions.insert(
                    head.oid,
                    LocalRevision {
                        id: revision.id,
                        tree_sha256: revision.tree_sha256,
                    },
                );
            }
            Ok(())
        })?;
    }
    Ok(())
}

pub fn from_local(repository: &LocalRepository) -> GitRepository {
    GitRepository {
        root: repository.root.clone(),
        common_git_dir: repository.common_git_dir.clone(),
        name: repository.name.clone(),
    }
}
