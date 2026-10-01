use std::{
    collections::HashMap,
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    path::Path,
    process::{Command, Output},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use serde_json::{Value, json};
use tempfile::TempDir;
use uuid::Uuid;

const NOW: &str = "2026-09-14T00:00:00Z";

#[derive(Default)]
struct Remote {
    workspaces: HashMap<String, Value>,
    revisions: HashMap<String, Value>,
    checks: Vec<Value>,
    requests: Vec<(String, String, Value)>,
    workspace_error: Option<u16>,
    fail_finalize: bool,
}

impl Remote {
    fn respond(&mut self, method: &str, path: &str, body: &Value) -> (u16, Value) {
        self.requests.push((method.into(), path.into(), body.clone()));
        let parts: Vec<_> = path.trim_start_matches("/v1/super/").split('/').collect();
        match (method, parts.as_slice()) {
            ("POST", ["workspaces"]) => {
                // Deliberately return a new identity on every creation request, as an API
                // upgrade that changed the link idempotency mapping did for older links.
                let id = Uuid::new_v4().to_string();
                let workspace = json!({"id": id, "name": body["name"], "created_at": NOW, "updated_at": NOW});
                self.workspaces.insert(id, workspace.clone());
                (201, workspace)
            }
            ("GET", ["workspaces", id]) => {
                if let Some(status) = self.workspace_error {
                    return (status, json!({}));
                }
                (200, self.workspaces[*id].clone())
            }
            ("POST", ["workspaces", workspace, "revisions"]) => {
                assert!(self.workspaces.contains_key(*workspace));
                if let Some(parent) = body["parent_id"].as_str() {
                    assert_eq!(self.revisions[parent]["workspace_id"], *workspace);
                }
                let id = Uuid::new_v4().to_string();
                let revision = json!({
                    "id": id, "workspace_id": workspace, "parent_id": body["parent_id"],
                    "reported_git_commit": body["reported_git_commit"], "tree_sha256": body["tree_sha256"],
                    "files": body["files"], "state": "complete", "created_at": NOW, "completed_at": NOW,
                });
                self.revisions.insert(id.clone(), revision.clone());
                (
                    201,
                    json!({
                        "revision": revision, "missing_content": [],
                        "diff": {"revision_id": id, "parent_id": body["parent_id"], "added": [], "changed": [], "removed": []},
                    }),
                )
            }
            ("GET", ["workspace-revisions", id]) => (200, self.revisions[*id].clone()),
            ("POST", ["workspace-revisions", id, "finalize"]) => {
                if self.fail_finalize {
                    return (503, json!({}));
                }
                (200, self.revisions[*id].clone())
            }
            ("POST", ["workspaces", workspace, "check-submissions"]) => (201, self.submit(workspace, body)),
            _ => panic!("unexpected request: {method} {path}"),
        }
    }

    fn submit(&mut self, workspace: &str, body: &Value) -> Value {
        let revision = &self.revisions[body["revision_id"].as_str().unwrap()];
        assert_eq!(revision["workspace_id"], workspace);
        let check = self
            .checks
            .iter()
            .find(|check| check["revision"]["id"] == revision["id"])
            .cloned()
            .unwrap_or_else(|| {
                let check = json!({
                    "repository_id": workspace, "number": self.checks.len() + 1,
                    "supertest": body["supertests"][0],
                    "revision": {"id": revision["id"], "tree_sha256": revision["tree_sha256"],
                        "reported_git_commit": revision["reported_git_commit"]},
                    "terminal": false, "problematic": false, "result": null, "operational_error": null,
                    "presentation": {"status": {"marker": "●", "label": "checking", "tone": "active"}, "history": {}},
                    "created_at": NOW, "updated_at": NOW,
                });
                self.checks.push(check.clone());
                check
            });
        json!({
            "submission": {"id": Uuid::new_v4(), "repository_id": workspace, "selector": body["selector"],
                "check_numbers": [check["number"]], "created_at": NOW},
            "checks": [check],
        })
    }
}

struct Fixture {
    directory: TempDir,
    url: String,
    remote: Arc<Mutex<Remote>>,
    stopped: Arc<AtomicBool>,
    server: Option<thread::JoinHandle<()>>,
}

impl Fixture {
    fn new() -> Self {
        let directory = TempDir::new().unwrap();
        let root = directory.path().join("repository");
        std::fs::create_dir(&root).unwrap();
        git(&root, &["init", "-b", "main"]);
        std::fs::write(root.join("example.py"), "def example():\n    return 1\n").unwrap();
        std::fs::write(
            root.join("supertests.py"),
            "from schematic import *\n\n@supertest\ndef law():\n    assert True\n",
        )
        .unwrap();
        git(&root, &["add", "."]);
        git(
            &root,
            &[
                "-c",
                "user.name=Super Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-m",
                "Initial source",
            ],
        );

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let remote = Arc::new(Mutex::new(Remote::default()));
        let stopped = Arc::new(AtomicBool::new(false));
        let server_remote = remote.clone();
        let server_stopped = stopped.clone();
        let server = thread::spawn(move || {
            for connection in listener.incoming() {
                if server_stopped.load(Ordering::SeqCst) {
                    break;
                }
                let mut stream = connection.unwrap();
                stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                let mut reader = BufReader::new(&mut stream);
                let mut first = String::new();
                reader.read_line(&mut first).unwrap();
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    if let Some((name, value)) = line.split_once(':')
                        && name.eq_ignore_ascii_case("content-length")
                    {
                        length = value.trim().parse::<usize>().unwrap();
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                let mut request = first.split_whitespace();
                let method = request.next().unwrap();
                let path = request.next().unwrap();
                let (status, response) = server_remote.lock().unwrap().respond(
                    method,
                    path,
                    &serde_json::from_slice(&body).unwrap_or(Value::Null),
                );
                let response = serde_json::to_vec(&response).unwrap();
                write!(stream, "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", response.len()).unwrap();
                stream.write_all(&response).unwrap();
            }
        });
        Self {
            directory,
            url,
            remote,
            stopped,
            server: Some(server),
        }
    }

    fn pup(&self, args: &[&str]) -> Output {
        self.pup_in(&self.directory.path().join("repository"), args)
    }

    fn pup_in(&self, directory: &Path, args: &[&str]) -> Output {
        let updates = self.directory.path().join("profile").join("updates");
        std::fs::create_dir_all(&updates).unwrap();
        std::fs::write(updates.join("releases.json"), b"{}").unwrap();
        Command::new(env!("CARGO_BIN_EXE_super"))
            .args(args)
            .env("SUPER_CONFIG_DIR", self.directory.path().join("profile"))
            .env("SUPER_API_URL", &self.url)
            .env("SUPER_ACCESS_TOKEN", "test-key")
            .env("SUPER_NO_DAEMON", "1")
            .current_dir(directory)
            .output()
            .unwrap()
    }

    fn ok(&self, args: &[&str]) -> Value {
        self.ok_in(&self.directory.path().join("repository"), args)
    }

    fn ok_in(&self, directory: &Path, args: &[&str]) -> Value {
        let output = self.pup_in(directory, args);
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn state(&self) -> Value {
        serde_json::from_slice(&std::fs::read(self.directory.path().join("profile/state.json")).unwrap()).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.url.trim_start_matches("http://"));
        self.server.take().unwrap().join().unwrap();
    }
}

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git").args(args).current_dir(root).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout).unwrap().trim().into()
}

fn nested_repository(fixture: &Fixture, worktree: bool) -> std::path::PathBuf {
    let parent = fixture.directory.path().join("repository");
    std::fs::write(parent.join(".git/info/exclude"), "/nested/\n").unwrap();
    if worktree {
        git(&parent, &["worktree", "add", "--detach", "nested", "HEAD"]);
    } else {
        git(&parent, &["clone", "--no-hardlinks", ".", "nested"]);
    }
    parent.join("nested").canonicalize().unwrap()
}

#[cfg(unix)]
#[test]
fn link_reports_all_unsupported_entries_and_accepts_the_suggested_ignores() {
    use std::os::unix::fs::symlink;

    let fixture = Fixture::new();
    fixture.ok(&["link", "--json"]);
    let root = fixture.directory.path().join("repository");
    std::fs::create_dir(root.join("vendor")).unwrap();
    symlink("example.py", root.join("first-link")).unwrap();
    symlink("../example.py", root.join("vendor/second-link")).unwrap();
    git(&root, &["add", "."]);
    let oid = git(&root, &["rev-parse", "HEAD"]);
    git(
        &root,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{oid},vendor/module"),
        ],
    );
    git(
        &root,
        &[
            "-c",
            "user.name=Super Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "-m",
            "Unsupported entries",
        ],
    );

    let revisions_before = fixture.remote.lock().unwrap().revisions.len();
    let output = fixture.pup(&["link", "--json"]);
    assert_eq!(output.status.code(), Some(2));
    let error: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(error["type"], "error");
    assert_eq!(error["data"]["code"], "operation_failed");
    let message = error["data"]["message"].as_str().unwrap();
    let text = fixture.pup(&["link"]);
    assert_eq!(text.status.code(), Some(2));
    for path in ["first-link", "vendor/second-link", "vendor/module"] {
        assert!(message.contains(path), "{message}");
        assert!(String::from_utf8_lossy(&text.stderr).contains(path), "{text:?}");
    }
    assert_eq!(fixture.remote.lock().unwrap().revisions.len(), revisions_before);
    assert!(!root.join(".superignore").exists());

    let patterns = message
        .split_once("To exclude these entries, append these lines to .superignore.\n")
        .unwrap()
        .1
        .split("\n\n")
        .next()
        .unwrap();
    std::fs::write(root.join(".superignore"), format!("{patterns}\n")).unwrap();
    git(&root, &["add", ".superignore"]);
    git(
        &root,
        &[
            "-c",
            "user.name=Super Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "-m",
            "Ignore unsupported entries",
        ],
    );
    fixture.ok(&["link", "--json"]);
    assert_eq!(fixture.remote.lock().unwrap().revisions.len(), revisions_before + 1);
}

#[test]
fn unlinked_nested_repositories_do_not_use_the_parent_association() {
    for worktree in [false, true] {
        let fixture = Fixture::new();
        fixture.ok(&["link", "--json"]);
        let nested = nested_repository(&fixture, worktree);
        let current = nested.join("examples/text-tools");
        std::fs::create_dir_all(&current).unwrap();
        let before = fixture.state();
        let request_count = fixture.remote.lock().unwrap().requests.len();

        for args in [
            &["check", "../../supertests.py", "--detach", "--json"][..],
            &["check", "missing/supertests.py::law", "--detach", "--json"],
            &["check", "--detach", "--json"],
            &["status", "--json"],
            &["status", "../../supertests.py", "--json"],
            &["fix", "--dry-run", "--json"],
            &["fix", "../../supertests.py", "--dry-run", "--json"],
            &["cancel", "--check", "1", "--json"],
            &["unlink", "--json"],
            &["unlink", ".", "--json"],
        ] {
            let output = fixture.pup_in(&current, args);
            assert_eq!(output.status.code(), Some(2), "{args:?}: {output:?}");
            let error: Value = serde_json::from_slice(&output.stdout).unwrap();
            let message = error.to_string();
            assert!(message.contains("repository is not linked"), "{args:?}: {message}");
            assert!(message.contains("super link ."), "{message}");
            assert!(message.contains(nested.to_str().unwrap()), "{message}");
            assert_eq!(fixture.state(), before);
            assert_eq!(fixture.remote.lock().unwrap().requests.len(), request_count);
        }

        let output = fixture.pup(&["check", "nested/supertests.py", "--detach", "--json"]);
        assert_eq!(output.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&output.stdout).contains("repository is not linked"));
        assert_eq!(fixture.state(), before);
        assert_eq!(fixture.remote.lock().unwrap().requests.len(), request_count);
    }
}

#[test]
fn linked_nested_repositories_resolve_relative_external_and_historical_selectors() {
    for worktree in [false, true] {
        let fixture = Fixture::new();
        let parent = fixture.ok(&["link", "--json"]);
        let nested = nested_repository(&fixture, worktree);
        let linked = fixture.ok_in(&nested, &["link", ".", "--json"]);
        assert_ne!(linked["data"]["workspace"]["id"], parent["data"]["workspace"]["id"]);
        let current = nested.join("examples");
        std::fs::create_dir(&current).unwrap();

        let relative = fixture.ok_in(&current, &["check", "../supertests.py::law", "--detach", "--json"]);
        let implicit = fixture.ok_in(&current, &["check", "--detach", "--json"]);
        let external = fixture.ok(&["check", "nested/supertests.py::law", "--detach", "--json"]);
        let absolute = fixture.ok_in(
            fixture.directory.path(),
            &[
                "check",
                nested.join("supertests.py").to_str().unwrap(),
                "--detach",
                "--json",
            ],
        );
        std::fs::remove_file(nested.join("supertests.py")).unwrap();
        let historical = fixture.ok_in(
            &nested,
            &["check", "supertests.py::law", "--commit", "HEAD", "--detach", "--json"],
        );
        for output in [relative, implicit, external, absolute, historical] {
            assert_eq!(
                output["data"]["run"]["repository_id"],
                linked["data"]["workspace"]["id"]
            );
        }
    }
}

#[test]
fn relinking_recovers_the_saved_workspace_across_creation_identity_changes() {
    let fixture = Fixture::new();
    let original = fixture.ok(&["link", "--json"]);
    let original_state = fixture.state();
    fixture.ok(&["link", ".", "--json"]);
    assert_eq!(fixture.state(), original_state);
    fixture.ok(&["unlink", ".", "--json"]);
    assert!(fixture.state()["repositories"].as_array().unwrap().is_empty());
    let relinked = fixture.ok(&["link", ".", "--json"]);
    assert_eq!(relinked["data"]["workspace"], original["data"]["workspace"]);
    assert_eq!(fixture.state(), original_state);
    let remote = fixture.remote.lock().unwrap();
    assert_eq!(remote.workspaces.len(), 1);
    assert_eq!(remote.revisions.len(), 1);
}

#[test]
fn new_flag_is_rejected_without_changing_links_or_contacting_the_service() {
    let fixture = Fixture::new();
    fixture.ok(&["link", "--json"]);
    let before = fixture.state();
    let request_count = fixture.remote.lock().unwrap().requests.len();
    let help = fixture.pup(&["link", "--help"]);
    assert!(help.status.success());
    assert!(!String::from_utf8_lossy(&help.stdout).contains("--new"));
    for args in [&["link", "--new", "--json"][..], &["link", ".", "--new", "--json"]] {
        let output = fixture.pup(args);
        assert_eq!(output.status.code(), Some(2));
        let error: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(error["data"]["code"], "invalid_arguments");
        assert_eq!(fixture.state(), before);
        assert_eq!(fixture.remote.lock().unwrap().requests.len(), request_count);
    }
}

fn second_commit(fixture: &Fixture) -> String {
    let root = fixture.directory.path().join("repository");
    std::fs::write(root.join("example.py"), "def example():\n    return 2\n").unwrap();
    git(
        &root,
        &[
            "-c",
            "user.name=Super Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "-am",
            "Change source",
        ],
    );
    git(&root, &["rev-parse", "HEAD"])
}

#[test]
fn earlier_commits_can_be_selected_with_or_without_a_cached_revision() {
    for cached in [false, true] {
        let fixture = Fixture::new();
        let root = fixture.directory.path().join("repository");
        let earlier = git(&root, &["rev-parse", "HEAD"]);
        if cached {
            fixture.ok(&["link", "--json"]);
        }
        let later = second_commit(&fixture);
        let linked = fixture.ok(&["link", "--json"]);
        fixture.ok(&["check", "--detach", "--json"]);
        let later_check = fixture.remote.lock().unwrap().checks[0].clone();
        let later_state = fixture.state();

        // An explicit revision does not move HEAD, staging, or the observed branch.
        fixture.ok(&["check", "--commit", &earlier, "--detach", "--json"]);
        assert_eq!(git(&root, &["rev-parse", "HEAD"]), later);
        assert_eq!(fixture.state()["repositories"][0]["last_seen_oid"], later);
        assert_eq!(
            fixture.state()["repositories"][0]["association_id"],
            later_state["repositories"][0]["association_id"]
        );
        let earlier_revision = fixture.state()["repositories"][0]["revisions"][&earlier].clone();

        // A checkout followed by unlink/relink selects the same earlier revision.
        fixture.ok(&["unlink", "--json"]);
        git(&root, &["checkout", "--detach", &earlier]);
        let relinked = fixture.ok(&["link", "--json"]);
        assert_eq!(relinked["data"]["workspace"], linked["data"]["workspace"]);
        assert_eq!(relinked["data"]["source"]["oid"], earlier);
        assert!(relinked["data"]["source"].get("branch").is_none());
        assert_eq!(
            fixture.state()["repositories"][0]["revisions"][&earlier],
            earlier_revision
        );
        fixture.ok(&["check", "--detach", "--json"]);
        git(&root, &["switch", "main"]);
        fixture.ok(&["check", "--detach", "--json"]);
        let remote = fixture.remote.lock().unwrap();
        assert_eq!(remote.workspaces.len(), 1);
        assert_eq!(remote.revisions.len(), 2);
        assert_eq!(remote.checks.len(), 2);
        assert_eq!(remote.checks[0], later_check);
        assert_eq!(remote.checks[1]["revision"]["reported_git_commit"], earlier);
    }
}

#[test]
fn relinking_to_an_unseen_ancestor_syncs_it_into_the_same_workspace() {
    let fixture = Fixture::new();
    let root = fixture.directory.path().join("repository");
    let earlier = git(&root, &["rev-parse", "HEAD"]);
    let later = second_commit(&fixture);
    let linked = fixture.ok(&["link", "--json"]);
    fixture.ok(&["unlink", "--json"]);
    git(&root, &["checkout", "--detach", &earlier]);
    let relinked = fixture.ok(&["link", "--json"]);
    assert_eq!(relinked["data"]["workspace"], linked["data"]["workspace"]);
    assert_eq!(relinked["data"]["source"]["oid"], earlier);
    let state = fixture.state();
    let revisions = state["repositories"][0]["revisions"].as_object().unwrap();
    assert!(revisions.contains_key(&earlier));
    assert!(revisions.contains_key(&later));
    let remote = fixture.remote.lock().unwrap();
    assert_eq!(remote.workspaces.len(), 1);
    assert_eq!(remote.revisions.len(), 2);
}

#[test]
fn dirty_checks_and_restoring_files_preserve_the_original_link_and_history() {
    let fixture = Fixture::new();
    let linked = fixture.ok(&["link", "--json"]);
    fixture.ok(&["check", "--detach", "--json"]);
    let root = fixture.directory.path().join("repository");
    std::fs::write(root.join("example.py"), "def example():\n    return 2\n").unwrap();
    fixture.ok(&["check", "--dirty", "--detach", "--json"]);
    let state = fixture.state();
    let temporary = state["repositories"][0]["temporary_commits"].clone();
    assert_eq!(temporary.as_object().unwrap().len(), 1);
    let refs = git(&root, &["show-ref"]);
    let checks = fixture.remote.lock().unwrap().checks.clone();
    git(&root, &["restore", "."]);
    fixture.ok(&["unlink", "--json"]);
    let relinked = fixture.ok(&["link", "--json"]);
    fixture.ok(&["check", "--detach", "--json"]);
    assert_eq!(relinked["data"]["workspace"], linked["data"]["workspace"]);
    assert_eq!(relinked["data"]["source"], linked["data"]["source"]);
    assert_eq!(fixture.state()["repositories"][0]["temporary_commits"], temporary);
    assert_eq!(git(&root, &["show-ref"]), refs);
    assert!(git(&root, &["status", "--porcelain"]).is_empty());
    let remote = fixture.remote.lock().unwrap();
    assert_eq!(remote.workspaces.len(), 1);
    assert_eq!(remote.revisions.len(), 2);
    assert_eq!(remote.checks, checks);
}

#[test]
fn unavailable_workspace_keeps_the_existing_association_and_never_creates_a_replacement() {
    for status in [404, 401, 503] {
        let fixture = Fixture::new();
        fixture.ok(&["link", "--json"]);
        fixture.ok(&["unlink", "--json"]);
        let before = fixture.state();
        fixture.remote.lock().unwrap().workspace_error = Some(status);
        let output = fixture.pup(&["link", "--json"]);
        assert_eq!(output.status.code(), Some(2));
        assert_eq!(fixture.state(), before);
        if status == 404 {
            assert!(String::from_utf8_lossy(&output.stdout).contains("contact Schematic support"));
            assert!(!String::from_utf8_lossy(&output.stdout).contains("--new"));
        }
        assert_eq!(fixture.remote.lock().unwrap().workspaces.len(), 1);
    }
}

#[test]
fn failed_relink_sync_preserves_the_existing_association_until_retry_succeeds() {
    for unlinked in [false, true] {
        let fixture = Fixture::new();
        let original = fixture.ok(&["link", "--json"]);
        if unlinked {
            fixture.ok(&["unlink", "--json"]);
        }
        let before = fixture.state();
        second_commit(&fixture);
        fixture.remote.lock().unwrap().fail_finalize = true;
        let output = fixture.pup(&["link", "--json"]);
        assert_eq!(output.status.code(), Some(2));
        assert_eq!(fixture.state(), before);
        fixture.remote.lock().unwrap().fail_finalize = false;
        let recovered = fixture.ok(&["link", "--json"]);
        assert_eq!(recovered["data"]["workspace"], original["data"]["workspace"]);
        assert_eq!(fixture.remote.lock().unwrap().workspaces.len(), 1);
    }
}

#[test]
fn workspace_and_source_metadata_mismatches_report_the_actual_problem() {
    for (field, replacement, expected) in [
        (
            "workspace_id",
            json!(Uuid::new_v4()),
            "belongs to a different workspace",
        ),
        (
            "tree_sha256",
            json!("f".repeat(64)),
            "does not match the recorded source tree",
        ),
    ] {
        let fixture = Fixture::new();
        fixture.ok(&["link", "--json"]);
        let before = fixture.state();
        fixture.remote.lock().unwrap().revisions.values_mut().next().unwrap()[field] = replacement;
        let output = fixture.pup(&["link", "--json"]);
        assert_eq!(output.status.code(), Some(2));
        let message = String::from_utf8_lossy(&output.stdout);
        assert!(message.contains(expected), "{message}");
        if field == "workspace_id" {
            assert!(!message.contains("source tree"));
        }
        assert_eq!(fixture.state(), before);
        assert_eq!(fixture.remote.lock().unwrap().workspaces.len(), 1);
    }
}
