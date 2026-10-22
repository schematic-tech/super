use std::{fs, path::Path, process::Command};

use serde_json::{Value, json};

/// Inspect persisted state without asking the CLI to mutate or migrate it.
pub fn read(profile: &Path) -> Value {
    let saved: Value = serde_json::from_slice(&fs::read(profile.join("state.json")).unwrap()).unwrap();
    if saved["schema_version"] == 1 {
        return saved;
    }
    let mut state = json!({
        "schema_version": 1, "api_url": saved["api_url"], "credential": saved["credential"],
        "credential_api_url": saved["credential_api_url"], "repositories": [], "unlinked_repositories": [],
        "other_api_repositories": saved.get("unmigrated").cloned().unwrap_or(json!({})),
    });
    let id = saved["id"].as_str().unwrap();
    for root in saved["worktrees"].as_array().unwrap() {
        let root = root.as_str().unwrap();
        let git = |args: &[&str]| {
            let output = Command::new("git").arg("-C").arg(root).args(args).output().unwrap();
            assert!(output.status.success());
            String::from_utf8(output.stdout).unwrap().trim().to_owned()
        };
        let directory = git(&["rev-parse", "--path-format=absolute", "--git-path", "super/worktree"]);
        let common = git(&["rev-parse", "--path-format=absolute", "--git-common-dir"]);
        let links: Value =
            serde_json::from_slice(&fs::read(Path::new(&directory).join(format!("{id}.json"))).unwrap()).unwrap();
        let shared: Value =
            serde_json::from_slice(&fs::read(Path::new(&common).join(format!("super/shared/{id}.json"))).unwrap())
                .unwrap();
        for (api, link) in links["environments"].as_object().unwrap() {
            let mut repository = link.clone();
            repository.as_object_mut().unwrap().remove("linked");
            repository["root"] = json!(root);
            repository["common_git_dir"] = json!(common);
            repository["temporary_commits"] = shared["temporary_commits"].clone();
            repository["source_hashes"] = shared["source_hashes"].clone();
            repository["revisions"] =
                shared["environments"][api]["revisions"][link["workspace_id"].as_str().unwrap()].clone();
            repository["pending_submissions"] =
                shared["environments"][api]["pending_submissions"][link["association_id"].as_str().unwrap()].clone();
            let scopes = state["other_api_repositories"].as_object_mut().unwrap();
            let scope = scopes
                .entry(api)
                .or_insert_with(|| json!({"repositories": [], "unlinked_repositories": []}));
            let key = if link["linked"] == true {
                "repositories"
            } else {
                "unlinked_repositories"
            };
            scope[key].as_array_mut().unwrap().push(repository);
        }
    }
    let api = saved["api_url"].as_str().unwrap_or_default();
    if let Some(active) = state["other_api_repositories"].as_object_mut().unwrap().remove(api) {
        state["repositories"] = active["repositories"].clone();
        state["unlinked_repositories"] = active["unlinked_repositories"].clone();
    }
    state
}
