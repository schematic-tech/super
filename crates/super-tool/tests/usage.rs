use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader, Write},
    net::TcpListener,
    process::{Command, Output},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};
use super_types::credential::{ApiBase, RoutedKey};
use tempfile::TempDir;
use uuid::Uuid;

const REPOSITORY: &str = "00000000-0000-0000-0000-000000000001";

struct Fixture {
    profile: TempDir,
    directory: TempDir,
    base: String,
    key: String,
    requests: Arc<Mutex<Vec<BTreeMap<String, String>>>>,
    stop: Arc<AtomicBool>,
    server: Option<thread::JoinHandle<()>>,
}

impl Fixture {
    fn new(mut respond: impl FnMut(&BTreeMap<String, String>) -> (u16, Value) + Send + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let key = RoutedKey {
            id: Uuid::new_v4(),
            api_base: ApiBase::parse(&base).unwrap(),
        }
        .encode(&[0x42; 32]);
        let expected = key.clone();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let observed = requests.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let server = thread::spawn(move || {
            while !stopped.load(Ordering::Relaxed) {
                let (mut socket, _) = match listener.accept() {
                    Ok(value) => value,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(error) => panic!("{error}"),
                };
                socket.set_nonblocking(false).unwrap();
                socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                let mut reader = BufReader::new(&socket);
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let target = line.split_whitespace().nth(1).unwrap();
                assert!(
                    line.starts_with("GET /v1/super/usage?") || target == "/v1/super/workspaces",
                    "{line}"
                );
                let mut query: BTreeMap<_, _> = reqwest::Url::parse(&format!("http://localhost{target}"))
                    .unwrap()
                    .query_pairs()
                    .into_owned()
                    .collect();
                if target == "/v1/super/workspaces" {
                    query.insert("resource".into(), "workspaces".into());
                }
                let mut authorization = None;
                loop {
                    let mut header = String::new();
                    reader.read_line(&mut header).unwrap();
                    if header == "\r\n" || header.is_empty() {
                        break;
                    }
                    if let Some((name, value)) = header.split_once(':') {
                        if name.eq_ignore_ascii_case("authorization") {
                            authorization = Some(value.trim().to_owned());
                        }
                        assert!(!name.eq_ignore_ascii_case("cookie"));
                    }
                }
                assert_eq!(authorization.as_deref(), Some(format!("Bearer {expected}").as_str()));
                observed.lock().unwrap().push(query.clone());
                let (status, mut value) = respond(&query);
                if target == "/v1/super/workspaces" && status == 200 {
                    value = value["repositories"].take();
                }
                let body = value.to_string();
                write!(socket, "HTTP/1.1 {status} Response\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
        });
        Self {
            profile: TempDir::new().unwrap(),
            directory: TempDir::new().unwrap(),
            base,
            key,
            requests,
            stop,
            server: Some(server),
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        let updates = self.profile.path().join("updates");
        std::fs::create_dir_all(&updates).unwrap();
        std::fs::write(updates.join("releases.json"), b"{}").unwrap();
        Command::new(env!("CARGO_BIN_EXE_super"))
            .args(args)
            .current_dir(self.directory.path())
            .env("SUPER_CONFIG_DIR", self.profile.path())
            .env("SUPER_ACCESS_TOKEN", &self.key)
            .env_remove("SUPER_API_URL")
            .env("SUPER_NO_DAEMON", "1")
            .env("TZ", "UTC")
            .env("NO_COLOR", "1")
            .env("TERM", "dumb")
            .output()
            .unwrap()
    }

    fn link_locally(&self) {
        assert!(
            Command::new("git")
                .args(["init", "--quiet"])
                .arg(self.directory.path())
                .status()
                .unwrap()
                .success()
        );
        let root = self.directory.path().canonicalize().unwrap();
        std::fs::write(self.profile.path().join("state.json"), json!({
            "schema_version": 1, "api_url": self.base,
            "repositories": [{"root":root,"common_git_dir":root.join(".git"),"association_id":"test", "workspace_id":REPOSITORY,"name":"payments","last_seen_oid":null,"temporary_commits":{},"source_hashes":{},"revisions":{},"pending_submissions":[]}]
        }).to_string()).unwrap();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let result = self.server.take().unwrap().join();
        if !thread::panicking() {
            result.unwrap();
        }
    }
}

fn report() -> Value {
    serde_json::from_str(include_str!("fixtures/usage.json")).unwrap()
}

#[test]
fn default_report_works_outside_git_and_json_preserves_the_wire_report() {
    let fixture = Fixture::new(|_| (200, report()));
    let output = fixture.run(&["usage"]);
    assert!(output.status.success(), "{output:?}");
    let text = String::from_utf8(output.stdout).unwrap();
    for expected in [
        "Super usage",
        "75% remaining",
        "750M of 1B tokens",
        "228.7K",
        "128.4K",
        "8.2K+",
        "All repositories",
        "Showing 3 of 3 checks",
    ] {
        assert!(text.contains(expected), "missing {expected}: {text}");
    }
    assert!(!text.contains('\x1b'));
    assert!(!text.contains(&fixture.key));
    let output = fixture.run(&["usage", "--json"]);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        json!({"schema_version":1,"type":"usage","data":report()})
    );
    let requests = fixture.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    for request in requests.iter() {
        assert_eq!(request["period"], "30d");
        assert_eq!(request["timezone"], "UTC");
        assert!(!request.contains_key("repositoryId"));
    }
}

#[test]
fn repository_names_use_the_server_filter_and_preserve_account_quota() {
    let fixture = Fixture::new(|query| {
        let mut value = report();
        if let Some(repository) = query.get("repositoryId") {
            assert_eq!(repository, REPOSITORY);
            value["checks"]
                .as_array_mut()
                .unwrap()
                .retain(|check| check["repositoryId"] == REPOSITORY);
            value["totalChecks"] = json!(2);
            value["totalTokens"] = json!(220_500);
        }
        (200, value)
    });
    let output = fixture.run(&["usage", "--repo", "payments", "--period", "7d"]);
    assert!(output.status.success(), "{output:?}");
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("Last 7 days · payments"));
    assert!(text.contains("220.5K"));
    assert!(text.contains("750M of 1B tokens"));
    assert!(!text.contains("REPOSITORY"));
    assert!(!text.contains("roundtrip"));
    let requests = fixture.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0]["resource"], "workspaces");
    assert_eq!(requests[1]["period"], "7d");
    assert_eq!(requests[1]["repositoryId"], REPOSITORY);
}

#[test]
fn local_repository_and_ids_filter_directly_without_syncing_source() {
    let fixture = Fixture::new(|_| (200, report()));
    fixture.link_locally();
    for selected in [".", REPOSITORY] {
        let output = fixture.run(&["usage", "--repo", selected, "--period", "today", "--json"]);
        assert!(output.status.success(), "{output:?}");
    }
    let requests = fixture.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(
        requests
            .iter()
            .all(|query| query["repositoryId"] == REPOSITORY && query["period"] == "today")
    );
    assert!(!fixture.profile.path().join("daemon.json").exists());
}

#[test]
fn invalid_and_ambiguous_repositories_do_not_fall_back_to_an_account_report() {
    let fixture = Fixture::new(|_| {
        let mut value = report();
        value["repositories"][1]["name"] = json!("payments");
        (200, value)
    });
    for (name, message) in [
        ("missing", "no repository named"),
        ("payments", "more than one repository"),
    ] {
        let output = fixture.run(&["usage", "--repo", name, "--json"]);
        assert_eq!(output.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&output.stdout).contains(message));
    }
    assert_eq!(fixture.requests.lock().unwrap().len(), 2);
    let output = fixture.run(&["usage", "--period", "90d"]);
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(fixture.requests.lock().unwrap().len(), 2);
}

#[test]
fn exhausted_reports_succeed_and_authentication_errors_are_actionable() {
    let fixture = Fixture::new(|_| {
        let mut value = report();
        value["quota"]["remaining"] = json!(0);
        value["quota"]["used"] = value["quota"]["limit"].clone();
        value["quota"]["exhausted"] = json!(true);
        (200, value)
    });
    let output = fixture.run(&["usage"]);
    assert!(output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stdout).contains("New checks are paused. Running checks will finish."));
    let denied = Fixture::new(|_| {
        (
            401,
            json!({"error":{"code":"unauthorized","message":"Your session has expired."}}),
        )
    });
    let output = denied.run(&["usage", "--json"]);
    assert_eq!(output.status.code(), Some(2));
    let error = String::from_utf8_lossy(&output.stdout);
    assert!(error.contains("super login"));
    assert!(!error.contains(&denied.key));
}
