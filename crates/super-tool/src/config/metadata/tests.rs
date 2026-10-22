use chrono::Utc;
use std::{process::Command, sync::Arc};

use super::*;

const API: &str = "https://api.example.test";

struct Fixture {
    directory: tempfile::TempDir,
    git: GitRepository,
    store: StateStore,
}

fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git").arg("-C").arg(root).args(args).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("repo");
        fs::create_dir(&root).unwrap();
        git(&root, &["init", "--quiet"]);
        git(
            &root,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.test",
                "commit",
                "--allow-empty",
                "-m",
                "initial",
            ],
        );
        let store = StateStore {
            directory: directory.path().join("profile"),
            discovered_roots: Arc::default(),
        };
        store.ensure_directory().unwrap();
        Self {
            directory,
            git: GitRepository::discover(&root).unwrap(),
            store,
        }
    }

    fn repository(git: &GitRepository) -> LocalRepository {
        LocalRepository {
            root: git.root.clone(),
            common_git_dir: git.common_git_dir.clone(),
            association_id: Uuid::new_v4().to_string(),
            workspace_id: Uuid::new_v4(),
            name: git.name.clone(),
            _legacy_dirty_preference: None,
            last_seen_oid: Some("a".repeat(40)),
            temporary_commits: HashMap::from([("parent:tree".into(), "a".repeat(40))]),
            source_hashes: HashMap::from([("blob".into(), "b".repeat(64))]),
            revisions: HashMap::from([(
                "a".repeat(40),
                LocalRevision {
                    id: Uuid::new_v4(),
                    tree_sha256: "b".repeat(64),
                },
            )]),
            pending_submissions: vec![PendingSubmission {
                request_sha256: "c".repeat(64),
                idempotency_key: "retry-key".into(),
                created_at: Utc::now(),
            }],
        }
    }

    fn seed(&self, repositories: Vec<LocalRepository>) {
        let state = LocalState {
            api_url: Some(API.into()),
            credential: Some(CredentialSource::SuperApiKey),
            repositories,
            ..LocalState::default()
        };
        fs::write(self.store.state_path(), serde_json::to_vec(&state).unwrap()).unwrap();
    }

    fn profile(&self) -> Profile {
        read(&self.store.state_path()).unwrap().unwrap()
    }

    fn worktree(&self) -> GitRepository {
        let root = self.directory.path().join("repo-worktrees/second");
        git(&self.git.root, &["worktree", "add", "--detach", root.to_str().unwrap()]);
        GitRepository::discover(&root).unwrap()
    }
}

#[test]
fn migration_preserves_links_history_and_pending_submissions_and_reads_are_read_only() {
    let fixture = Fixture::new();
    let original = Fixture::repository(&fixture.git);
    fixture.seed(vec![original.clone()]);
    let legacy = fs::read(fixture.store.state_path()).unwrap();
    fixture.store.load_for(&fixture.git).unwrap();
    assert_eq!(fs::read(fixture.store.state_path()).unwrap(), legacy);
    fixture.store.update(|_| Ok(())).unwrap();
    let profile = fixture.profile();
    assert_eq!(profile.schema_version, 2);
    assert!(profile.unmigrated.is_empty());
    let saved = fixture.store.load().unwrap();
    assert_eq!(
        serde_json::to_value(&saved.repositories[0]).unwrap(),
        serde_json::to_value(&original).unwrap()
    );
    assert_eq!(saved.credential, Some(CredentialSource::SuperApiKey));
    let link_path = worktree_path(&fixture.git, profile.id).unwrap();
    let shared_path = shared_path(&fixture.git, profile.id);
    let links: serde_json::Value = read(&link_path).unwrap().unwrap();
    assert!(links["environments"][API].get("root").is_none());
    assert!(links.get("credential").is_none());
    let before = fs::metadata(&shared_path).unwrap().modified().unwrap();
    fixture.store.update(|_| Ok(())).unwrap();
    assert_eq!(fs::metadata(&shared_path).unwrap().modified().unwrap(), before);
    // An interrupted migration can leave old fallback data; the completed link is authoritative.
    let mut profile = fixture.profile();
    profile.unmigrated = scopes(&LocalState {
        api_url: Some(API.into()),
        repositories: vec![original],
        ..LocalState::default()
    });
    write(&fixture.store.state_path(), &profile).unwrap();
    fixture
        .store
        .update(|state| {
            state.repositories[0].pending_submissions.clear();
            Ok(())
        })
        .unwrap();
    assert!(
        fixture.store.load().unwrap().repositories[0]
            .pending_submissions
            .is_empty()
    );
    assert!(fixture.profile().unmigrated.is_empty());
}

#[test]
fn moves_recover_links_from_git_and_reused_paths_do_not_inherit_links() {
    let fixture = Fixture::new();
    fixture.seed(vec![Fixture::repository(&fixture.git)]);
    fixture.store.update(|_| Ok(())).unwrap();
    let moved = fixture.directory.path().join("moved");
    fs::rename(&fixture.git.root, &moved).unwrap();
    fs::create_dir(&fixture.git.root).unwrap();
    git(&fixture.git.root, &["init", "--quiet"]);
    assert!(fixture.store.load_for(&fixture.git).unwrap().repositories.is_empty());
    let moved = GitRepository::discover(&moved).unwrap();
    let state = fixture.store.load_for(&moved).unwrap();
    assert_eq!(state.repositories.len(), 1);
    assert_eq!(state.repositories[0].root, moved.root);
    assert_eq!(state.repositories[0].common_git_dir, moved.common_git_dir);
    fixture.store.update(|_| Ok(())).unwrap();
    assert!(fixture.profile().worktrees.contains(&moved.root));
}

#[test]
fn worktree_move_and_removal_keep_shared_history_but_remove_the_link() {
    let fixture = Fixture::new();
    let worktree = fixture.worktree();
    fixture.seed(vec![Fixture::repository(&fixture.git), Fixture::repository(&worktree)]);
    fixture.store.update(|_| Ok(())).unwrap();
    let id = fixture.profile().id;
    let path = worktree_path(&worktree, id).unwrap();
    assert!(path.starts_with(fixture.git.common_git_dir.join("worktrees")));
    let moved = fixture.directory.path().join("repo-worktrees/moved");
    git(
        &fixture.git.root,
        &[
            "worktree",
            "move",
            worktree.root.to_str().unwrap(),
            moved.to_str().unwrap(),
        ],
    );
    let moved = GitRepository::discover(&moved).unwrap();
    assert_eq!(fixture.store.load_for(&moved).unwrap().repositories.len(), 2);
    fixture.store.update(|_| Ok(())).unwrap();
    git(&fixture.git.root, &["worktree", "remove", moved.root.to_str().unwrap()]);
    assert!(!path.exists());
    assert!(shared_path(&fixture.git, id).exists());
    assert_eq!(fixture.store.load().unwrap().repositories.len(), 1);
}

#[test]
fn shared_history_merges_worktree_updates_without_mixing_workspaces_or_apis() {
    let fixture = Fixture::new();
    let worktree = fixture.worktree();
    let first = Fixture::repository(&fixture.git);
    let mut second = Fixture::repository(&worktree);
    second.workspace_id = first.workspace_id;
    second.revisions.clone_from(&first.revisions);
    fixture.seed(vec![first.clone(), second]);
    fixture.store.update(|_| Ok(())).unwrap();
    let revision = LocalRevision {
        id: Uuid::new_v4(),
        tree_sha256: "d".repeat(64),
    };
    fixture
        .store
        .update(|state| {
            let first = state
                .repositories
                .iter_mut()
                .find(|repo| repo.root == fixture.git.root)
                .unwrap();
            first.revisions.insert("b".repeat(40), revision.clone());
            first.pending_submissions.clear();
            first.temporary_commits.insert("new:tree".into(), "b".repeat(40));
            Ok(())
        })
        .unwrap();
    let state = fixture.store.load().unwrap();
    for repo in &state.repositories {
        assert_eq!(repo.revisions[&"b".repeat(40)], revision);
        assert_eq!(repo.temporary_commits["new:tree"], "b".repeat(40));
    }
    assert_eq!(
        state
            .repositories
            .iter()
            .filter(|repo| repo.pending_submissions.is_empty())
            .count(),
        1
    );
    fixture
        .store
        .update(|state| {
            state.select_api("https://other.example.test");
            let mut other = first.clone();
            other.revisions.clear();
            other.workspace_id = Uuid::new_v4();
            state.repositories.push(other);
            Ok(())
        })
        .unwrap();
    let other = fixture.store.load().unwrap();
    assert!(other.repositories[0].revisions.is_empty());
    assert_eq!(other.other_api_repositories[API].repositories.len(), 2);
    fixture.store.select_api(API).unwrap();
    assert_eq!(fixture.store.load().unwrap().repositories[0].revisions.len(), 2);
}

#[test]
fn missing_legacy_checkouts_are_retained_and_missing_or_corrupt_metadata_fails_closed() {
    let fixture = Fixture::new();
    let mut missing = Fixture::repository(&fixture.git);
    missing.root = fixture.directory.path().join("offline");
    fixture.seed(vec![Fixture::repository(&fixture.git), missing]);
    fixture.store.update(|_| Ok(())).unwrap();
    assert_eq!(fixture.profile().unmigrated[API].repositories.len(), 1);
    let path = shared_path(&fixture.git, fixture.profile().id);
    let original = fs::read(&path).unwrap();
    fs::remove_file(&path).unwrap();
    assert!(
        fixture
            .store
            .load()
            .unwrap_err()
            .to_string()
            .contains("missing Super revision history")
    );
    fs::write(&path, b"{broken").unwrap();
    assert!(fixture.store.load().is_err());
    fs::write(&path, original).unwrap();
    assert_eq!(fixture.store.load().unwrap().repositories.len(), 2);
}

#[test]
fn profiles_are_isolated_and_concurrent_writers_preserve_every_revision() {
    let fixture = Fixture::new();
    fixture.seed(vec![Fixture::repository(&fixture.git)]);
    fixture.store.update(|_| Ok(())).unwrap();
    let other = StateStore {
        directory: fixture.directory.path().join("other-profile"),
        discovered_roots: Arc::default(),
    };
    other
        .update(|state| {
            state.api_url = Some(API.into());
            Ok(())
        })
        .unwrap();
    assert!(other.load_for(&fixture.git).unwrap().repositories.is_empty());
    std::thread::scope(|scope| {
        for index in 0..8 {
            let store = &fixture.store;
            scope.spawn(move || {
                store
                    .update(|state| {
                        state.repositories[0].revisions.insert(
                            index.to_string(),
                            LocalRevision {
                                id: Uuid::new_v4(),
                                tree_sha256: "e".repeat(64),
                            },
                        );
                        Ok(())
                    })
                    .unwrap();
            });
        }
    });
    assert_eq!(fixture.store.load().unwrap().repositories[0].revisions.len(), 9);
}

#[test]
fn offline_worktrees_resume_discovery_after_an_unrelated_profile_update() {
    let fixture = Fixture::new();
    fixture.seed(vec![Fixture::repository(&fixture.git)]);
    fixture.store.update(|_| Ok(())).unwrap();
    let hidden = fixture.directory.path().join("offline");
    fs::rename(&fixture.git.root, &hidden).unwrap();
    fixture
        .store
        .update(|state| {
            state.credential = None;
            Ok(())
        })
        .unwrap();
    assert!(fixture.store.load().unwrap().repositories.is_empty());
    fs::rename(&hidden, &fixture.git.root).unwrap();
    assert_eq!(fixture.store.load().unwrap().repositories.len(), 1);
}

#[cfg(unix)]
#[test]
fn unchanged_metadata_does_not_need_write_access_during_a_profile_update() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new();
    fixture.seed(vec![Fixture::repository(&fixture.git)]);
    fixture.store.update(|_| Ok(())).unwrap();
    let directory = fixture.git.common_git_dir.join("super/shared");
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o500)).unwrap();
    let result = fixture.store.update(|state| {
        state.credential = None;
        Ok(())
    });
    let mode = fs::metadata(&directory).unwrap().permissions().mode() & 0o777;
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
    result.unwrap();
    assert_eq!(
        mode, 0o500,
        "an unchanged clone must not have its permissions rewritten"
    );
}
