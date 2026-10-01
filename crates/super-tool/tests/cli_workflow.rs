use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
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
use super_types::{SourceFile, source_sha256, source_tree_sha256};
use tempfile::TempDir;

const WORKSPACE: &str = "00000000-0000-0000-0000-000000000001";
const CHECK: &str = "42";
const RUN: &str = "00000000-0000-0000-0000-000000000017";
const REVISION: &str = "00000000-0000-0000-0000-000000000099";
const SOURCE: &str = "from schematic import *\n\n@supertest\ndef law():\n    assert True\n";

const LANGUAGE_EXAMPLES: &[(&str, &str, &str, &str, u32)] = &[
    (
        "NormalizeSpaces.cs",
        include_str!("fixtures/languages/NormalizeSpaces.cs"),
        "csharp",
        "NormalizingTwiceChangesNothing",
        8,
    ),
    (
        "normalize_spaces.js",
        include_str!("fixtures/languages/normalize_spaces.js"),
        "javascript",
        "normalizingTwiceChangesNothing",
        5,
    ),
    (
        "NormalizeSpaces.java",
        include_str!("fixtures/languages/NormalizeSpaces.java"),
        "java",
        "normalizingTwiceChangesNothing",
        9,
    ),
    (
        "increment.vhd",
        include_str!("fixtures/languages/increment.vhd"),
        "vhdl",
        "increment_never_decreases",
        11,
    ),
];

#[test]
fn language_examples_support_discovery_submission_results_and_rechecking() {
    fn replace_declaration(value: &mut Value, declaration: &Value) {
        match value {
            Value::Object(object) => {
                if let Some(supertest) = object.get_mut("supertest") {
                    *supertest = declaration.clone();
                }
                for child in object.values_mut() {
                    replace_declaration(child, declaration);
                }
            }
            Value::Array(array) => {
                for child in array {
                    replace_declaration(child, declaration);
                }
            }
            _ => {}
        }
    }

    for &(file, source, language, name, line) in LANGUAGE_EXAMPLES {
        let path = format!("supertests/{file}");
        let declaration = json!({"path": path, "name": name, "language": language, "line": line});
        let fixture = Fixture::with_files(false, false, &[(&path, source)], |routes| {
            for response in routes.values_mut() {
                replace_declaration(response, &declaration);
            }
        });
        for selector in [".", "supertests", &path, &format!("{path}::{name}")] {
            let output = fixture.run(&["check", selector, "--detach", "--json"]);
            assert!(output.status.success(), "{language} {selector}: {output:?}");
            let submissions = fixture.service.submissions.lock().unwrap();
            assert_eq!(submissions.last().unwrap()["supertests"], json!([declaration]));
            assert_eq!(submissions.last().unwrap()["certify"], false);
        }
        let output = fixture.run(&["status", &path, "--json"]);
        assert!(output.status.success(), "{language}: {output:?}");
        let status: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(status["data"]["rows"][0]["current"]["supertest"], declaration);
        let output = fixture.run(&["check", "--problems", "--detach", "--json"]);
        assert!(output.status.success(), "{language}: {output:?}");
        let submissions = fixture.service.submissions.lock().unwrap();
        assert_eq!(submissions.last().unwrap()["supertests"], json!([declaration]));
    }
}

#[test]
fn new_language_discovery_respects_exclusions_and_rejects_ambiguous_names() {
    for &(file, source, _, name, _) in LANGUAGE_EXAMPLES {
        let path = format!("supertests/{file}");
        let ignored = Fixture::with_files(
            false,
            false,
            &[(&path, source), (".superignore", "supertests/\n"), ("test.py", SOURCE)],
            |_| {},
        );
        let output = ignored.run(&["check", ".", "--detach", "--json"]);
        assert!(output.status.success(), "{output:?}");
        assert_eq!(
            ignored.service.submissions.lock().unwrap()[0]["supertests"],
            json!([
                {"path": "test.py", "name": "law", "language": "python", "line": 3}
            ])
        );

        let duplicate = match file {
            "NormalizeSpaces.cs" => format!("{source}\nclass Other {{ [Schematic.Supertest] void {name}() {{}} }}"),
            "NormalizeSpaces.java" => {
                format!("{source}\nclass Other {{ @tech.schematic.Supertest void {name}() {{}} }}")
            }
            "normalize_spaces.js" => format!("{source}\nconst {name} = supertest(text => {{}});"),
            "increment.vhd" => source.replace(
                "end architecture;",
                &format!("--% supertest\n{name} : process begin wait; end process;\nend architecture;"),
            ),
            _ => unreachable!(),
        };
        let fixture = Fixture::with_files(false, false, &[(&path, &duplicate)], |_| {});
        let output = fixture.run(&["check", ".", "--detach", "--json"]);
        assert_eq!(output.status.code(), Some(2));
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("more than once"),
            "{output:?}"
        );
        assert!(fixture.service.submissions.lock().unwrap().is_empty());
    }
}

#[test]
fn pass_explanations_require_details_in_human_output_but_are_always_in_json() {
    let details = json!([
        {"text":"", "emphasized":false},
        {"text":"Conclusion", "emphasized":true},
        {"text":"  The checked behavior is explained below.", "emphasized":false},
        {"text":"  The function examines each input value.", "emphasized":false}
    ]);
    for outcome in ["pass", "fail", "conditional"] {
        let fixture = Fixture::configured(false, false, |routes| {
            let check = routes
                .get_mut(&format!("GET /v1/super/workspaces/{WORKSPACE}/checks/{CHECK}"))
                .unwrap();
            check["result"]["outcome"] = json!(outcome);
            check["problematic"] = json!(outcome != "pass");
            check["fix"] = Value::Null;
            check["presentation"]["details"] = details.clone();
        });
        for expanded in [false, true] {
            for json_output in [false, true] {
                let mut args = vec!["status", "--check", CHECK];
                if expanded {
                    args.push("--details");
                }
                if json_output {
                    args.push("--json");
                }
                let output = fixture.run(&args);
                assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
                let text = String::from_utf8_lossy(&output.stdout);
                assert_eq!(
                    text.contains("The function examines each input value."),
                    outcome != "pass" || expanded || json_output,
                    "{text}"
                );
                if json_output {
                    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
                    let check = &value["data"]["rows"][0]["current"];
                    assert_eq!(check["presentation"]["details"], details);
                    assert_eq!(check["result"]["outcome"], outcome);
                }
            }
        }
    }
}

#[test]
fn malformed_responses_do_not_echo_unexpected_server_fields_or_values() {
    for json_output in [false, true] {
        for unexpected_field in [false, true] {
            let fixture = Fixture::configured(false, false, |routes| {
                let check = routes
                    .get_mut(&format!("GET /v1/super/workspaces/{WORKSPACE}/checks/{CHECK}"))
                    .unwrap();
                if unexpected_field {
                    check["unpublished_field"] = json!("unpublished_value");
                } else {
                    check["result"]["outcome"] = json!("unpublished_value");
                }
            });
            let args = if json_output {
                vec!["status", "--check", CHECK, "--json"]
            } else {
                vec!["status", "--check", CHECK]
            };
            let output = fixture.run(&args);
            assert_eq!(output.status.code(), Some(2));
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(text.contains("the Super API returned invalid data"), "{text}");
            assert!(!text.contains("unpublished_"), "{text}");
        }
    }
}

struct Service {
    origin: String,
    seen: Arc<Mutex<Vec<String>>>,
    submission_keys: Arc<Mutex<Vec<String>>>,
    submissions: Arc<Mutex<Vec<Value>>>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}
impl Service {
    fn start(mut routes: BTreeMap<String, Value>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let submission_keys = Arc::new(Mutex::new(Vec::new()));
        let observed_keys = Arc::clone(&submission_keys);
        let submissions = Arc::new(Mutex::new(Vec::new()));
        let observed_submissions = Arc::clone(&submissions);
        let requests = Arc::clone(&seen);
        let stopping = Arc::clone(&stop);
        let worker = thread::spawn(move || {
            let mut stream_index = 0;
            while !stopping.load(Ordering::Relaxed) {
                let Ok((mut stream, _)) = listener.accept() else {
                    thread::sleep(Duration::from_millis(5));
                    continue;
                };
                // macOS can inherit the listener's nonblocking mode on accepted sockets.
                stream.set_nonblocking(false).unwrap();
                stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                stream.set_write_timeout(Some(Duration::from_secs(2))).unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                if !matches!(reader.read_line(&mut line), Ok(length) if length > 0) {
                    continue;
                }
                let target = line.split_whitespace().take(2).collect::<Vec<_>>().join(" ");
                requests.lock().unwrap().push(target.clone());
                let mut length = 0;
                let mut submission_key = None;
                loop {
                    line.clear();
                    if reader.read_line(&mut line).is_err() || line == "\r\n" || line.is_empty() {
                        break;
                    }
                    if let Some(value) = line.strip_prefix("idempotency-key:") {
                        submission_key = Some(value.trim().to_owned());
                    }
                    if let Some(value) = line.to_lowercase().strip_prefix("content-length:") {
                        length = value.trim().parse::<usize>().unwrap();
                    }
                }
                assert!(length <= 1024 * 1024);
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                let path = target.split('?').next().unwrap();
                if path.starts_with("POST ") && path.ends_with("/check-submissions") {
                    observed_submissions
                        .lock()
                        .unwrap()
                        .push(serde_json::from_slice(&body).unwrap());
                    observed_keys
                        .lock()
                        .unwrap()
                        .push(submission_key.expect("admission idempotency header"));
                    if routes.remove("__lose_first_admission_response").is_some() {
                        continue;
                    }
                }
                let route = if routes.contains_key(&target) {
                    target.as_str()
                } else {
                    path
                };
                let (status, mut body) = fixture_response(&mut routes, route);
                if let Some(delay) = body.get("_delay_ms").and_then(Value::as_u64) {
                    thread::sleep(Duration::from_millis(delay));
                    body.as_object_mut().unwrap().remove("_delay_ms");
                }
                if body.get("_lose_response").is_some() {
                    continue;
                }
                if status == "200 OK" && path.starts_with("POST ") && path.ends_with("/cancel") {
                    publish_cancellation(&mut routes, path, &body);
                }
                let (body, media_type) = if let Some(frames) = body.get("_sse").and_then(Value::as_array) {
                    let frame = &frames[stream_index.min(frames.len() - 1)];
                    stream_index += 1;
                    (frame.as_str().unwrap().to_owned(), "text/event-stream")
                } else {
                    (body.to_string(), "application/json")
                };
                write_response(&mut stream, status, media_type, &body);
            }
        });
        Self {
            origin,
            seen,
            submission_keys,
            submissions,
            stop,
            worker: Some(worker),
        }
    }
}
impl Drop for Service {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.worker.take().unwrap().join().unwrap();
    }
}

fn write_response(stream: &mut TcpStream, status: &str, media_type: &str, body: &str) {
    if let Err(error) = write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: {media_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    ) {
        assert!(
            matches!(
                error.kind(),
                std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
            ),
            "fixture response failed: {error}"
        );
    }
}

fn fixture_response(routes: &mut BTreeMap<String, Value>, path: &str) -> (&'static str, Value) {
    let Some(body) = routes.get_mut(path) else {
        return (
            "404 Not Found",
            json!({"error":{"code":"not_found","message":"fixture not found"}}),
        );
    };
    if let Some(responses) = body.get_mut("_responses").and_then(Value::as_array_mut) {
        let response = if responses.len() > 1 {
            responses.remove(0)
        } else {
            responses[0].clone()
        };
        let status = match response["status"].as_u64().unwrap() {
            200 => "200 OK",
            409 => "409 Conflict",
            503 => "503 Service Unavailable",
            status => panic!("unsupported fixture status {status}"),
        };
        return (status, response["body"].clone());
    }
    ("200 OK", body.clone())
}

fn publish_cancellation(routes: &mut BTreeMap<String, Value>, path: &str, body: &Value) {
    let resource = path.strip_prefix("POST ").unwrap().strip_suffix("/cancel").unwrap();
    routes.insert(format!("GET {resource}"), body.clone());
    for value in routes.values_mut() {
        if let Some(checks) = value.get_mut("checks").and_then(Value::as_array_mut) {
            for check in checks {
                if check["repository_id"] == body["repository_id"] && check["number"] == body["number"] {
                    *check = body.clone();
                }
            }
        }
    }
}

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git").args(args).current_dir(root).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        format_args!(
            "stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn prepare_repository(directory: &Path, files: &[(&str, &str)]) -> (std::path::PathBuf, String, String) {
    let root = directory.join("repo");
    fs::create_dir(&root).unwrap();
    fs::create_dir(directory.join("config")).unwrap();
    git(&root, &["init", "-q"]);
    git(&root, &["config", "user.email", "test@example.invalid"]);
    git(&root, &["config", "user.name", "CLI Fixture"]);
    let mut sources = Vec::new();
    for (path, source) in files {
        fs::create_dir_all(root.join(path).parent().unwrap()).unwrap();
        fs::write(root.join(path), source).unwrap();
        sources.push(SourceFile {
            path: (*path).into(),
            sha256: source_sha256(source.as_bytes()),
            bytes: source.len() as u64,
            executable: false,
        });
    }
    git(&root, &["add", "."]);
    git(&root, &["-c", "commit.gpgsign=false", "commit", "-qm", "fixture"]);
    let head = git(&root, &["rev-parse", "HEAD"]);
    let tree = source_tree_sha256(&sources).unwrap();
    (root, head, tree)
}

struct Fixture {
    directory: TempDir,
    service: Service,
    head: String,
}
impl Fixture {
    fn new(operational_error: bool) -> Self {
        Self::with_active_check(operational_error, false)
    }
    fn with_active_check(operational_error: bool, active: bool) -> Self {
        Self::with_lost_response(operational_error, active, false)
    }
    fn with_lost_response(operational_error: bool, active: bool, lose_response: bool) -> Self {
        Self::configured(operational_error, active, |routes| {
            if lose_response {
                routes.insert("__lose_first_admission_response".into(), json!(true));
            }
        })
    }
    fn configured(operational_error: bool, active: bool, configure: impl FnOnce(&mut BTreeMap<String, Value>)) -> Self {
        Self::with_files(operational_error, active, &[("test.py", SOURCE)], configure)
    }
    fn with_files(
        operational_error: bool,
        active: bool,
        files: &[(&str, &str)],
        configure: impl FnOnce(&mut BTreeMap<String, Value>),
    ) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let (root, head, tree) = prepare_repository(directory.path(), files);
        let check = json!({
            "number": 42, "repository_id": WORKSPACE,
            "supertest": {"path": "test.py", "name": "law", "language": "python", "line": 4},
            "revision": {"id": REVISION, "tree_sha256": tree, "reported_git_commit": head},
            "terminal": true, "problematic": !operational_error,
            "operational_error": if operational_error { Some("error") } else { None },
            "result": if operational_error { Value::Null } else { json!({"outcome":"fail","assurance":"uncertified"}) },
            "presentation": {
                "status": {"marker": "!", "label": "fail (certifying…)", "tone": "warning"},
                "details": [{"text": "Witness: -42\nExpected: 6\nObserved: 0", "emphasized": false}], "history": {}
            },
            "fix": {
                "id": "00000000-0000-0000-0000-000000000042", "state": "proposed", "base_revision_id": REVISION, "base_tree_sha256": tree,
                "summary": "Make the assertion explicit.", "instructions": "Apply the proposed text patch.",
                "diff": "diff --git a/test.py b/test.py\n--- a/test.py\n+++ b/test.py\n@@ -3,3 +3,3 @@\n @supertest\n def law():\n-    assert True\n+    assert 1 == 1\n",
                "files": [{"path": "test.py", "change": "modified"}], "validation": [],
                "created_at": "2026-01-01T00:00:00Z"
            },
            "created_at": "2026-01-01T00:00:00Z", "updated_at": "2026-01-01T00:01:00Z"
        });
        let mut finished_check = check.clone();
        finished_check["event_sequence"] = json!(2);
        let terminal_event = format!(
            "event: check\ndata: {}\n\n",
            json!({"sequence": 2, "check": finished_check})
        );
        let mut check = check;
        if active {
            check["result"] = Value::Null;
            check["terminal"] = json!(false);
            check["problematic"] = json!(false);
            check["event_sequence"] = json!(1);
            check["presentation"]["status"] = json!({"marker": "●", "label": "checking", "tone": "active"});
        }
        let submission = json!({
            "submission": {"id": RUN, "repository_id": WORKSPACE, "selector": ".", "check_numbers": [42], "created_at": "2026-01-01T00:00:00Z"},
            "checks": [check.clone()]
        });
        let mut other_submission = submission.clone();
        other_submission["submission"]["id"] = json!("00000000-0000-0000-0000-000000000018");
        let base = format!("/v1/super/workspaces/{WORKSPACE}");
        let mut canceled = check.clone();
        canceled["terminal"] = json!(true);
        canceled["operational_error"] = json!("canceled");
        let mut routes = BTreeMap::from([
            (format!("POST {base}/checks/{CHECK}/cancel"), canceled),
            (
                format!("GET {base}/check-events"),
                json!({"_sse": [": keepalive\n\n", terminal_event]}),
            ),
            (format!("GET {base}/checks/{CHECK}"), check.clone()),
            (
                format!("GET {base}/checks/history"),
                json!({"checks": [check], "next_before": null}),
            ),
            (format!("GET {base}/check-submissions/latest"), submission.clone()),
            (format!("GET {base}/check-submissions/{RUN}"), submission.clone()),
            (format!("POST {base}/check-submissions"), submission),
            (
                format!("GET {base}/check-submissions/00000000-0000-0000-0000-000000000018"),
                other_submission,
            ),
            (
                format!("GET /v1/super/workspace-revisions/{REVISION}"),
                json!({
                    "id": REVISION, "workspace_id": WORKSPACE, "parent_id": null, "reported_git_commit": head,
                    "tree_sha256": tree, "files": [], "state": "complete",
                    "created_at": "2026-01-01T00:00:00Z", "completed_at": "2026-01-01T00:00:00Z"
                }),
            ),
        ]);
        configure(&mut routes);
        let service = Service::start(routes);
        let state = json!({
            "schema_version": 1, "repositories": [{
                "root": root.canonicalize().unwrap(), "common_git_dir": root.join(".git").canonicalize().unwrap(),
                "association_id": "fixture", "workspace_id": WORKSPACE, "name": "text-tools",
                "revisions": {head.clone(): {"id": REVISION, "tree_sha256": tree}}
            }]
        });
        fs::write(directory.path().join("config/state.json"), state.to_string()).unwrap();
        Self {
            directory,
            service,
            head,
        }
    }
    fn command(&self, args: &[&str]) -> Command {
        let updates = self.directory.path().join("config").join("updates");
        std::fs::create_dir_all(&updates).unwrap();
        std::fs::write(updates.join("releases.json"), b"{}").unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_super"));
        command
            .args(args)
            .current_dir(self.directory.path().join("repo"))
            .env("SUPER_CONFIG_DIR", self.directory.path().join("config"))
            .env("SUPER_API_URL", &self.service.origin)
            .env("SUPER_ACCESS_TOKEN", "fixture-token")
            .env("SUPER_NO_DAEMON", "1")
            .env("NO_COLOR", "1");
        command
    }
    fn run(&self, args: &[&str]) -> Output {
        let mut child = self
            .command(args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while child.try_wait().unwrap().is_none() {
            if std::time::Instant::now() >= deadline {
                child.kill().unwrap();
                let output = child.wait_with_output().unwrap();
                panic!("CLI fixture timed out: {args:?} {output:?}");
            }
            thread::sleep(Duration::from_millis(5));
        }
        child.wait_with_output().unwrap()
    }
    fn source(&self) -> String {
        fs::read_to_string(self.directory.path().join("repo/test.py")).unwrap()
    }
}

#[test]
fn link_prints_a_compact_completion_and_preserves_the_json_contract() {
    for branch in [Some("main"), None] {
        let workspace = json!({
            "id": WORKSPACE, "name": "text-tools",
            "created_at": "2026-01-01T00:00:00Z", "updated_at": "2026-01-01T00:00:00Z"
        });
        let fixture = Fixture::configured(false, false, |routes| {
            routes.insert(format!("GET /v1/super/workspaces/{WORKSPACE}"), workspace.clone());
        });
        let root = fixture.directory.path().join("repo");
        if let Some(branch) = branch {
            git(&root, &["branch", "-M", branch]);
        } else {
            git(&root, &["checkout", "--detach"]);
        }
        let output = fixture.run(&["link", "."]);
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            format!(
                "  ✓ Linked text-tools · {} · {}\n  Next: super check\n",
                branch.unwrap_or("detached HEAD"),
                &fixture.head[..7]
            )
        );
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.starts_with("Linking repo · connecting\n"), "{stderr}");
        assert!(!stderr.contains('\x1b'), "piped progress must not contain animation");

        let output = fixture.run(&["link", ".", "--json"]);
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let mut source = json!({"oid": fixture.head, "temporary": false});
        if let Some(branch) = branch {
            source["branch"] = json!(branch);
        }
        assert_eq!(
            serde_json::from_slice::<Value>(&output.stdout).unwrap(),
            json!({
                "schema_version": 1, "type": "linked",
                "data": {"workspace": workspace, "source": source}
            })
        );
    }
}

#[test]
fn exact_status_is_read_only_and_findings_do_not_change_status_exit_code() {
    let fixture = Fixture::new(false);
    let before = fs::read(fixture.directory.path().join("config/state.json")).unwrap();
    let output = fixture.run(&["status", "--check", CHECK, "--json"]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["type"], "results");
    assert_eq!(value["data"]["rows"][0]["current"]["number"], 42);
    assert_eq!(value["data"]["problem_count"], 1);
    let check = &value["data"]["rows"][0]["current"];
    assert_eq!(check["result"], json!({"outcome":"fail","assurance":"uncertified"}));
    assert!(value["data"]["rows"][0]["current"].get("counterexamples").is_none());
    assert!(
        value["data"]["rows"][0]["current"]["presentation"]["details"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Witness: -42")
    );
    assert_eq!(fixture.source(), SOURCE);
    assert_eq!(
        before,
        fs::read(fixture.directory.path().join("config/state.json")).unwrap()
    );
    assert_eq!(
        *fixture.service.seen.lock().unwrap(),
        [format!("GET /v1/super/workspaces/{WORKSPACE}/checks/{CHECK}")]
    );
    let human = fixture.run(&["status", "--check", CHECK]);
    assert!(human.status.success());
    assert!(
        !human.stdout.contains(&0x1b),
        "redirected human output contains terminal escapes"
    );
    assert!(String::from_utf8_lossy(&human.stdout).contains("fail"));
    assert!(
        fixture
            .service
            .seen
            .lock()
            .unwrap()
            .iter()
            .all(|request| !request.contains("/checks/history"))
    );
    let history = fixture.run(&["status", "--check", CHECK, "--history"]);
    assert!(history.status.success());
    assert!(
        fixture
            .service
            .seen
            .lock()
            .unwrap()
            .iter()
            .any(|request| request.contains("/checks/history"))
    );
}

#[test]
fn selectors_choose_the_latest_request_across_commits_while_exact_targets_stay_pinned() {
    let fixture = Fixture::configured(false, false, |routes| {
        let base = format!("/v1/super/workspaces/{WORKSPACE}");
        let old = routes[&format!("GET {base}/checks/{CHECK}")].clone();
        let mut latest = old.clone();
        latest["number"] = json!(44);
        latest["revision"]["reported_git_commit"] = json!("b".repeat(40));
        latest["created_at"] = json!("2026-01-03T00:00:00Z");
        latest["updated_at"] = latest["created_at"].clone();
        routes.insert(
            format!("GET {base}/checks/history"),
            json!({"checks": [latest, old], "next_before": null}),
        );
    });
    let root = fixture.directory.path().join("repo");
    fs::write(root.join("uncommitted.txt"), "local edits").unwrap();
    for args in [
        vec!["status", "test.py::law", "--json"],
        vec!["status", "test.py", "--json"],
    ] {
        let output = fixture.run(&args);
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["data"]["rows"][0]["current"]["number"], 44);
        assert_eq!(value["data"]["rows"][0]["previous"]["number"], 42);
    }
    for args in [
        vec!["status", "--json"],
        vec!["status", "--check", CHECK, "--json"],
        vec!["status", "--run", RUN, "--json"],
    ] {
        let output = fixture.run(&args);
        assert!(output.status.success());
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["data"]["rows"][0]["current"]["number"], 42);
    }
    let output = fixture.run(&["fix", "test.py::law", "--dry-run", "--json"]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["data"]["check_number"], 44);
    assert_eq!(fixture.source(), SOURCE);
    // A removed local declaration must not make its saved history inaccessible.
    fs::remove_file(root.join("test.py")).unwrap();
    assert!(fixture.run(&["status", "test.py::law", "--json"]).status.success());
    assert!(
        fixture
            .service
            .seen
            .lock()
            .unwrap()
            .iter()
            .all(|request| request.starts_with("GET "))
    );
}

#[test]
fn status_fix_and_cancel_resolve_latest_checks_across_history_pages_and_broader_runs() {
    for command in ["status", "fix", "cancel"] {
        let fixture = Fixture::configured(false, false, |routes| {
            let base = format!("/v1/super/workspaces/{WORKSPACE}");
            let old = routes[&format!("GET {base}/checks/{CHECK}")].clone();
            let mut latest = old.clone();
            latest["number"] = json!(44);
            latest["created_at"] = json!("2026-01-03T00:00:00Z");
            latest["updated_at"] = latest["created_at"].clone();
            latest["result"]["outcome"] = json!("pass");
            latest["problematic"] = json!(false);
            latest["fix"] = Value::Null;
            let mut other = latest.clone();
            other["number"] = json!(43);
            other["supertest"]["name"] = json!("other_law");
            routes.insert(
                format!("GET {base}/checks/history"),
                json!({"checks": [latest], "next_before": 44}),
            );
            routes.insert(
                format!("GET {base}/checks/history?limit=500&selector=test.py&before=44"),
                json!({"checks": [other, old], "next_before": null}),
            );
        });
        let mut args = vec![command, "test.py", "--json"];
        if command == "fix" {
            args.push("--dry-run");
        }
        let output = fixture.run(&args);
        if command == "fix" {
            assert_eq!(output.status.code(), Some(2));
            let text = String::from_utf8_lossy(&output.stdout);
            assert!(text.contains("fix requires one supertest"), "{text}");
        } else {
            assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
            let value: Value = serde_json::from_slice(&output.stdout).unwrap();
            let rows = value["data"]["rows"].as_array().unwrap();
            assert_eq!(
                rows.iter()
                    .map(|row| row["current"]["number"].as_u64().unwrap())
                    .collect::<Vec<_>>(),
                [44, 43]
            );
        }
        let requests = fixture.service.seen.lock().unwrap();
        assert!(
            requests.iter().any(|request| request.contains("before=44")),
            "{requests:?}"
        );
        assert!(
            requests.iter().all(|request| request.starts_with("GET ")),
            "{requests:?}"
        );
        assert!(
            requests
                .iter()
                .all(|request| !request.contains("check-submissions/latest")),
            "{requests:?}"
        );
    }
}

#[test]
fn problems_rechecks_only_the_latest_runs_findings_and_paths_narrow_that_run() {
    let updated = format!(
        "# updated source\n\n{SOURCE}\n@supertest\ndef passing():\n    assert True\n\n@supertest\ndef added():\n    assert True\n"
    );
    for (directory, selector, expected) in [
        ("", None, vec!["test.py::law", "tests/nested.py::law"]),
        ("", Some("test.py"), vec!["test.py::law"]),
        ("", Some("test.py::law"), vec!["test.py::law"]),
        ("", Some("tests"), vec!["tests/nested.py::law"]),
        ("tests", None, vec!["test.py::law", "tests/nested.py::law"]),
        ("tests", Some("."), vec!["tests/nested.py::law"]),
    ] {
        let fixture = Fixture::with_files(
            false,
            false,
            &[("test.py", &updated), ("tests/nested.py", SOURCE)],
            |routes| {
                let base = format!("/v1/super/workspaces/{WORKSPACE}");
                let mut failed = routes[&format!("GET {base}/checks/{CHECK}")].clone();
                failed["revision"]["reported_git_commit"] = json!("f".repeat(40));
                let mut passed = failed.clone();
                passed["number"] = json!(43);
                passed["supertest"]["name"] = json!("passing");
                passed["result"]["outcome"] = json!("pass");
                passed["problematic"] = json!(false);
                passed["fix"] = Value::Null;
                let mut nested = failed.clone();
                nested["number"] = json!(44);
                nested["supertest"]["path"] = json!("tests/nested.py");
                // Selection follows the API's problematic flag, not just a fail label.
                nested["result"]["outcome"] = json!("conditional");
                let latest = routes.get_mut(&format!("GET {base}/check-submissions/latest")).unwrap();
                latest["submission"]["check_numbers"] = json!([42, 43, 44]);
                latest["checks"] = json!([failed, passed, nested]);

                // A per-declaration history lookup would wrongly omit law and include passing.
                let mut history = latest["checks"].as_array().unwrap().clone();
                history[0]["problematic"] = json!(false);
                history[1]["problematic"] = json!(true);
                routes.insert(
                    format!("GET {base}/checks/history"),
                    json!({"checks": history, "next_before": null}),
                );
            },
        );
        let mut args = vec!["check", "--problems", "--detach", "--json"];
        args.extend(selector);
        let output = fixture
            .command(&args)
            .current_dir(fixture.directory.path().join("repo").join(directory))
            .output()
            .unwrap();
        assert!(output.status.success(), "{args:?}: {output:?}");
        let submissions = fixture.service.submissions.lock().unwrap();
        assert_eq!(submissions.len(), 1);
        let submitted = submissions[0]["supertests"].as_array().unwrap();
        assert_eq!(
            submitted
                .iter()
                .map(|supertest| format!(
                    "{}::{}",
                    supertest["path"].as_str().unwrap(),
                    supertest["name"].as_str().unwrap()
                ))
                .collect::<Vec<_>>(),
            expected
        );
        assert_eq!(submissions[0]["revision_id"], REVISION);
        for supertest in submitted {
            if supertest["path"] == "test.py" {
                assert_eq!(supertest["line"], 5, "use the declaration from current source");
            }
        }
        let requests = fixture.service.seen.lock().unwrap();
        assert_eq!(
            requests[0],
            format!("GET /v1/super/workspaces/{WORKSPACE}/check-submissions/latest")
        );
        let post = requests
            .iter()
            .position(|request| request.starts_with("POST "))
            .unwrap();
        assert!(
            requests[..post]
                .iter()
                .all(|request| !request.contains("checks/history")),
            "{requests:?}"
        );
        assert!(
            requests
                .iter()
                .all(|request| !request.contains("check-submissions/latest?")),
            "a path must not select an older matching run: {requests:?}"
        );
    }
}

#[test]
fn problems_without_findings_succeeds_before_prompting_or_syncing_even_with_older_failures() {
    for args in [
        vec!["check", "--problems"],
        vec!["check", "--problems", "--json"],
        vec!["check", "--problems", "--detach", "--json"],
        vec!["check", "--problems", "--stream", "--json"],
        vec!["check", "--problems", "test.py::law", "--json"],
    ] {
        let fixture = Fixture::configured(false, false, |routes| {
            let latest = routes
                .get_mut(&format!(
                    "GET /v1/super/workspaces/{WORKSPACE}/check-submissions/latest"
                ))
                .unwrap();
            latest["checks"][0]["result"]["outcome"] = json!("pass");
            latest["checks"][0]["problematic"] = json!(false);
            if args.contains(&"test.py::law") {
                let mut outside_selection = latest["checks"][0].clone();
                outside_selection["number"] = json!(43);
                outside_selection["supertest"]["name"] = json!("other_law");
                outside_selection["result"]["outcome"] = json!("fail");
                outside_selection["problematic"] = json!(true);
                latest["submission"]["check_numbers"] = json!([42, 43]);
                latest["checks"].as_array_mut().unwrap().push(outside_selection);
            }
        });
        fs::write(
            fixture.directory.path().join("repo/test.py"),
            format!("# dirty\n{SOURCE}"),
        )
        .unwrap();
        let state = fixture.directory.path().join("config/state.json");
        let before = fs::read(&state).unwrap();
        let output = fixture.run(&args);
        assert!(output.status.success(), "{args:?}: {output:?}");
        let message = if args.contains(&"test.py::law") {
            "No problems to recheck in `test.py::law` from the latest run."
        } else {
            "No problems to recheck in the latest run."
        };
        if args.contains(&"--json") {
            assert_eq!(
                serde_json::from_slice::<Value>(&output.stdout).unwrap(),
                json!({
                    "schema_version": 1, "type": "success", "data": {"message": message}
                })
            );
        } else {
            assert!(String::from_utf8_lossy(&output.stdout).contains(message));
        }
        assert_eq!(fs::read(state).unwrap(), before);
        assert_eq!(
            *fixture.service.seen.lock().unwrap(),
            [format!("GET /v1/super/workspaces/{WORKSPACE}/check-submissions/latest")]
        );
        assert!(fixture.service.submissions.lock().unwrap().is_empty());
    }
}

#[test]
fn problems_requires_a_matching_latest_run_without_falling_back_to_history() {
    for missing_run in [true, false] {
        let fixture = Fixture::configured(false, false, |routes| {
            let latest = format!("GET /v1/super/workspaces/{WORKSPACE}/check-submissions/latest");
            if missing_run {
                routes.remove(&latest);
            } else {
                routes.get_mut(&latest).unwrap()["checks"][0]["supertest"]["path"] = json!("other.py");
            }
        });
        let output = fixture.run(&["check", "--problems", "test.py", "--json"]);
        assert_eq!(output.status.code(), Some(2));
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        let message = value["data"]["message"].as_str().unwrap();
        assert!(
            message.contains(if missing_run {
                "no previous run to recheck"
            } else {
                "no supertests in the latest run match `test.py`"
            }),
            "{message}"
        );
        assert!(message.contains("without `--problems`"), "{message}");
        assert_eq!(
            *fixture.service.seen.lock().unwrap(),
            [format!("GET /v1/super/workspaces/{WORKSPACE}/check-submissions/latest")]
        );
    }
}

#[test]
fn problems_still_requires_a_dirty_source_choice_when_there_are_findings() {
    let fixture = Fixture::new(false);
    fs::write(
        fixture.directory.path().join("repo/test.py"),
        format!("# dirty\n{SOURCE}"),
    )
    .unwrap();
    let output = fixture.run(&["check", "--problems", "--detach", "--json"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stdout).contains("uncommitted changes require an explicit choice"));
    assert!(fixture.service.submissions.lock().unwrap().is_empty());
    let output = fixture.run(&["check", "--problems", "--commit", "HEAD", "--detach", "--json"]);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(fixture.service.submissions.lock().unwrap().len(), 1);
}

#[test]
fn reopened_status_recognizes_cached_uncommitted_sources_without_changing_json() {
    let temporary_oid = "f".repeat(40);
    let fixture = Fixture::configured(false, false, |routes| {
        for response in routes.values_mut() {
            if response.get("number").is_some() {
                response["revision"]["reported_git_commit"] = json!(temporary_oid);
            }
            if let Some(checks) = response.get_mut("checks").and_then(Value::as_array_mut) {
                for check in checks {
                    check["revision"]["reported_git_commit"] = json!(temporary_oid);
                }
            }
        }
    });
    let state_path = fixture.directory.path().join("config/state.json");
    let mut state: Value = serde_json::from_slice(&fs::read(&state_path).unwrap()).unwrap();
    state["repositories"][0]["temporary_commits"] = json!({
        format!("{}:{}", fixture.head, "a".repeat(40)): temporary_oid
    });
    fs::write(&state_path, state.to_string()).unwrap();
    let before = fs::read(&state_path).unwrap();
    let root = fixture.directory.path().join("repo");
    git(&root, &["commit", "--allow-empty", "-m", "Later checkout"]);
    let current_head = git(&root, &["rev-parse", "HEAD"]);
    assert_ne!(current_head, fixture.head);
    let source = format!("uncommitted changes · based on {}", &fixture.head[..7]);
    for args in [
        vec!["status"],
        vec!["status", "--run", RUN],
        vec!["status", "--check", CHECK],
        vec!["status", "--check", CHECK, "--history"],
        vec!["status", "test.py", "--history"],
    ] {
        let output = fixture.run(&args);
        assert!(output.status.success(), "{output:?}");
        let text = String::from_utf8_lossy(&output.stdout);
        assert_eq!(text.matches(&source).count(), 1, "{args:?}: {text}");
    }
    let output = fixture.run(&["status", "--check", CHECK, "--json"]);
    assert!(output.status.success(), "{output:?}");
    let data: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(data["data"]["source"]["oid"], current_head);
    assert_eq!(data["data"]["source"]["temporary"], false);
    assert_eq!(
        data["data"]["rows"][0]["current"]["revision"]["reported_git_commit"],
        temporary_oid
    );
    assert!(data["data"].get("temporary_commit_parents").is_none());
    assert_eq!(fs::read(&state_path).unwrap(), before, "status must stay read-only");

    state["repositories"][0]["temporary_commits"] = json!({});
    fs::write(state_path, state.to_string()).unwrap();
    let output = fixture.run(&["status", "--check", CHECK]);
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        text.contains("fffffff") && !text.contains("uncommitted changes"),
        "{text}"
    );
}

#[test]
fn discovery_explains_working_files_absent_from_the_selected_commit() {
    let source = "from schematic import supertest\n\n@supertest\ndef law() -> None:\n    assert True\n";
    let fixture = Fixture::with_files(false, false, &[("test.py", source), ("nested/n.py", source)], |_| {});
    let root = fixture.directory.path().join("repo");
    for path in ["supertests.py", "nested/[new].py"] {
        fs::write(root.join(path), source).unwrap();
        // Jujutsu uses intent-to-add entries for new working-copy files.
        git(&root, &["add", "--intent-to-add", "--", path]);
        let index = git(&root, &["ls-files", "--stage"]);
        for selector in [path.to_owned(), format!("{path}::law")] {
            let output = fixture.run(&["check", &selector, "--commit", "HEAD", "--detach", "--json"]);
            assert_eq!(output.status.code(), Some(2), "{output:?}");
            let value: Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(
                value["data"]["message"],
                format!(
                    "`{path}` exists in your working copy but is not present in selected commit {}.\n\
                     To check your working copy instead, use `--dirty` instead of `--commit`.",
                    &fixture.head[..7]
                )
            );
        }
        assert_eq!(git(&root, &["rev-parse", "HEAD"]), fixture.head);
        assert_eq!(git(&root, &["ls-files", "--stage"]), index);
        assert_eq!(fs::read_to_string(root.join(path)).unwrap(), source);
    }
    assert!(fixture.service.submissions.lock().unwrap().is_empty());
}

#[test]
fn discovery_does_not_confuse_excluded_or_unsupported_paths_with_absent_committed_files() {
    let fixture = Fixture::with_files(
        false,
        false,
        &[
            ("test.py", SOURCE),
            ("ignored.py", ""),
            (".superignore", "ignored.py\n"),
        ],
        |_| {},
    );
    let root = fixture.directory.path().join("repo");
    fs::write(root.join("notes.txt"), "").unwrap();
    fs::create_dir(root.join("empty.py")).unwrap();
    for path in ["ignored.py", "notes.txt", "missing.py", "empty.py"] {
        let output = fixture.run(&["check", path, "--commit", "HEAD", "--detach", "--json"]);
        assert_eq!(output.status.code(), Some(2), "{output:?}");
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(
            value["data"]["message"],
            format!(
                "path `{path}` does not match a supported Python, Rust, C, C#, JavaScript, Java, or VHDL source file"
            )
        );
    }
    assert!(fixture.service.submissions.lock().unwrap().is_empty());
}

#[test]
fn discovery_does_not_suggest_dirty_when_working_source_is_already_selected() {
    let fixture = Fixture::with_files(
        false,
        false,
        &[("test.py", SOURCE), (".gitignore", "ignored.py\n")],
        |_| {},
    );
    fs::write(fixture.directory.path().join("repo/ignored.py"), "").unwrap();
    let output = fixture.run(&["check", "ignored.py", "--dirty", "--detach", "--json"]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        value["data"]["message"],
        "path `ignored.py` does not match a supported Python, Rust, C, C#, JavaScript, Java, or VHDL source file"
    );
    assert!(fixture.service.submissions.lock().unwrap().is_empty());
}

#[test]
fn dirty_noninteractive_checks_require_a_flag_even_with_a_legacy_answer() {
    for previous in [Value::Null, json!(true), json!(false)] {
        let fixture = Fixture::new(false);
        let state_path = fixture.directory.path().join("config/state.json");
        let mut state: Value = serde_json::from_slice(&fs::read(&state_path).unwrap()).unwrap();
        state["repositories"][0]["dirty_preference"] = previous;
        fs::write(&state_path, state.to_string()).unwrap();
        fs::write(
            fixture.directory.path().join("repo/test.py"),
            format!("{SOURCE}# local edit\n"),
        )
        .unwrap();
        let before = fs::read(&state_path).unwrap();
        let output = fixture.run(&["check", "--detach", "--json"]);
        assert_eq!(output.status.code(), Some(2));
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(
            text.contains("explicit choice in a noninteractive terminal"),
            "{output:?}"
        );
        assert!(fixture.service.seen.lock().unwrap().is_empty());
        assert_eq!(fs::read(&state_path).unwrap(), before);

        let output = fixture.run(&["check", "--commit", "HEAD", "--detach", "--json"]);
        assert!(output.status.success(), "{output:?}");
        assert!(fixture.source().contains("# local edit"));
    }
}

#[cfg(unix)]
mod terminal {
    use super::*;
    use nix::{
        poll::{PollFd, PollFlags, poll},
        pty::{Winsize, openpty},
        sys::termios::tcgetattr,
    };
    use std::{
        fs::File,
        os::fd::AsFd,
        process::{Child, Stdio},
        time::Instant,
    };

    struct Process(Child);
    impl Drop for Process {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn read_until(master: &mut File, text: &mut String, needle: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !text.contains(needle) {
            assert!(Instant::now() < deadline, "terminal did not show {needle:?}: {text}");
            let ready = {
                let mut fds = [PollFd::new(master.as_fd(), PollFlags::POLLIN)];
                poll(&mut fds, 25_u16).unwrap() > 0
            };
            if ready {
                let mut bytes = [0; 8192];
                let count = master.read(&mut bytes).unwrap();
                assert!(count > 0, "terminal closed before {needle:?}: {text}");
                text.push_str(&String::from_utf8_lossy(&bytes[..count]));
                assert!(text.len() < 2 * 1024 * 1024, "unbounded terminal output");
            }
        }
    }

    #[test]
    fn watch_navigation_and_detach_restore_both_terminal_sizes() {
        for (cols, rows, quit, active) in [(80, 24, 27, true), (120, 36, 3, true), (120, 36, 27, false)] {
            let fixture = Fixture::configured(false, active, |routes| {
                let submission = routes
                    .get_mut(&format!("GET /v1/super/workspaces/{WORKSPACE}/check-submissions/{RUN}"))
                    .unwrap();
                if active {
                    submission["checks"][0]["presentation"]["live_line"] = json!("Roving... Analyzing the code.");
                }
                let mut second = submission["checks"][0].clone();
                second["number"] = json!(43);
                second["supertest"]["name"] = json!("another_law");
                submission["checks"].as_array_mut().unwrap().push(second);
                submission["submission"]["check_numbers"] = json!([42, 43]);
            });
            let pty = openpty(
                Some(&Winsize {
                    ws_col: cols,
                    ws_row: rows,
                    ws_xpixel: 0,
                    ws_ypixel: 0,
                }),
                None,
            )
            .unwrap();
            let before = tcgetattr(&pty.slave).unwrap();
            let slave = File::from(pty.slave);
            let mut master = File::from(pty.master);
            let mut child = Process(
                fixture
                    .command(&["status", "--run", RUN, "--watch"])
                    .env("TERM", "xterm-256color")
                    .stdin(Stdio::from(slave.try_clone().unwrap()))
                    .stdout(Stdio::from(slave.try_clone().unwrap()))
                    .stderr(Stdio::from(slave.try_clone().unwrap()))
                    .spawn()
                    .unwrap(),
            );
            let mut text = String::new();
            read_until(&mut master, &mut text, "Enter for more details");
            master.write_all(b"q\r").unwrap();
            read_until(&mut master, &mut text, "Esc back");
            master.write_all(b"q\x1b").unwrap();
            let mut returned = String::new();
            read_until(&mut master, &mut returned, "Enter for more details");
            assert!(
                child.0.try_wait().unwrap().is_none(),
                "Esc from details must return to the list"
            );
            text.push_str(&returned);
            master.write_all(&[quit]).unwrap();
            read_until(&mut master, &mut text, "\x1b[?1049l");
            read_until(
                &mut master,
                &mut text,
                if active {
                    "Watch: super status --watch"
                } else {
                    "Details: super status"
                },
            );
            let receipt = text.rsplit("\x1b[?1049l").next().unwrap();
            assert!(!receipt.contains(RUN));
            assert!(!receipt.contains("--run"));
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if let Some(status) = child.0.try_wait().unwrap() {
                    assert!(status.success(), "{text}");
                    break;
                }
                assert!(Instant::now() < deadline, "view did not exit");
                thread::sleep(Duration::from_millis(10));
            }
            assert_eq!(tcgetattr(&slave).unwrap(), before, "terminal mode was not restored");
            assert_eq!(text.contains("Roving"), active, "only active checks show activity");
            assert!(
                !text.contains('🐾'),
                "activity uses animated dots rather than paw prints"
            );
            for mode in [1000, 1002, 1003, 1006, 1015] {
                assert!(
                    !text.contains(&format!("\x1b[?{mode}h")),
                    "live views must leave mouse selection with the terminal"
                );
            }
            assert!(
                fixture
                    .service
                    .seen
                    .lock()
                    .unwrap()
                    .iter()
                    .all(|r| r.starts_with("GET ")),
                "unexpected requests: {:?}",
                fixture.service.seen.lock().unwrap()
            );
            assert_eq!(fixture.source(), SOURCE);
        }
    }

    fn start_terminal(fixture: &Fixture, args: &[&str], cols: u16, rows: u16) -> (Process, File, File) {
        let mut command = fixture.command(args);
        command.env_remove("NO_COLOR");
        start_terminal_command(command, cols, rows)
    }

    fn start_terminal_command(mut command: Command, cols: u16, rows: u16) -> (Process, File, File) {
        let pty = openpty(
            Some(&Winsize {
                ws_col: cols,
                ws_row: rows,
                ws_xpixel: 0,
                ws_ypixel: 0,
            }),
            None,
        )
        .unwrap();
        let slave = File::from(pty.slave);
        let master = File::from(pty.master);
        let child = command
            .env("TERM", "xterm-256color")
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave.try_clone().unwrap()))
            .spawn()
            .unwrap();
        (Process(child), master, slave)
    }

    fn replace_checks(routes: &mut BTreeMap<String, Value>, checks: &[Value]) {
        for (route, body) in routes.iter_mut() {
            if body.get("checks").is_some() {
                body["checks"] = json!(checks);
                if body.get("submission").is_some() {
                    body["submission"]["check_numbers"] = checks.iter().map(|check| check["number"].clone()).collect();
                }
            } else if route == &format!("GET /v1/super/workspaces/{WORKSPACE}/checks/{CHECK}") {
                *body = checks[0].clone();
            }
        }
    }

    fn wait_for_exit(process: &mut Process) -> std::process::ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = process.0.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < deadline, "CLI did not exit");
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn completed_checks_stay_open_for_review_until_closed() {
        for (outcome, count, exit_code, quit) in [
            ("pass", 1, 0, b'\x1b'),
            ("pass", 3, 0, b'\x03'),
            ("fail", 1, 1, b'\x03'),
            ("blocked", 1, 2, b'\x1b'),
        ] {
            let fixture = Fixture::configured(false, false, |routes| {
                let mut check = routes[&format!("GET /v1/super/workspaces/{WORKSPACE}/checks/{CHECK}")].clone();
                check["fix"] = Value::Null;
                check["problematic"] = json!(outcome == "fail");
                if outcome == "blocked" {
                    check["result"] = Value::Null;
                    check["operational_error"] = json!("blocked");
                } else {
                    check["result"]["outcome"] = json!(outcome);
                }
                check["presentation"]["details"] =
                    json!([{ "text": "Saved result explanation.", "emphasized": false }]);
                let checks = (0..count)
                    .map(|index| {
                        let mut check = check.clone();
                        check["number"] = json!(42 + index);
                        check["supertest"]["name"] = json!(format!("law_{index}"));
                        check
                    })
                    .collect::<Vec<_>>();
                replace_checks(routes, &checks);
            });
            let (mut process, mut master, slave) = start_terminal(&fixture, &["check"], 100, 32);
            let mut text = String::new();
            read_until(&mut master, &mut text, "Esc/Ctrl+C close");
            assert!(process.0.try_wait().unwrap().is_none(), "{text}");
            // Completed and reused results must still be navigable.
            master.write_all(b"\r").unwrap();
            let mut details = String::new();
            read_until(&mut master, &mut details, "Esc back");
            read_until(&mut master, &mut details, "Saved result explanation.");
            master.write_all(b"\x1b").unwrap();
            let mut returned = String::new();
            read_until(&mut master, &mut returned, "Enter for more details");
            master.write_all(&[quit]).unwrap();
            read_until(&mut master, &mut text, "\x1b[?1049l");
            assert_eq!(wait_for_exit(&mut process).code(), Some(exit_code));
            assert!(
                tcgetattr(&slave)
                    .unwrap()
                    .local_flags
                    .contains(nix::sys::termios::LocalFlags::ICANON)
            );
            assert!(
                !fixture
                    .service
                    .seen
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|request| request.contains("/cancel"))
            );
            let receipt = console::strip_ansi_codes(text.rsplit("\x1b[?1049l").next().unwrap());
            assert!(!receipt.contains("Detached"));
        }
    }

    #[test]
    fn attached_checks_receive_late_fixes_and_remain_open_after_settlement() {
        for (final_state, args, exit_code) in [
            ("available", vec!["check"], 1),
            ("unavailable", vec!["check"], 1),
            ("pending", vec!["check"], 1),
            ("available", vec!["status", "--check", CHECK, "--watch"], 0),
            ("unavailable", vec!["status", "--check", CHECK, "--watch"], 0),
            ("pending", vec!["status", "--check", CHECK, "--watch"], 0),
        ] {
            let fixture = Fixture::configured(false, true, |routes| {
                let mut active = routes[&format!("GET /v1/super/workspaces/{WORKSPACE}/checks/{CHECK}")].clone();
                let proposal = active["fix"].take();
                active["presentation"]["details"] = json!([]);
                active["presentation"]["live_line"] = json!("Checking the source.");
                replace_checks(routes, &[active.clone()]);
                let mut verdict = active;
                verdict["terminal"] = json!(true);
                verdict["problematic"] = json!(true);
                verdict["result"] = json!({"outcome": "fail", "assurance": "uncertified"});
                verdict["fix_pending"] = json!(true);
                verdict["updates_pending"] = json!(true);
                verdict["event_sequence"] = json!(2);
                let mut settled = verdict.clone();
                settled["event_sequence"] = json!(3);
                settled["fix_pending"] = json!(false);
                settled["updates_pending"] = json!(false);
                settled["presentation"]["details"] =
                    json!([{ "text": "Explanation delivered after the verdict.", "emphasized": false }]);
                if final_state == "available" {
                    settled["fix"] = proposal;
                }
                let event = |check: &Value| {
                    format!(
                        "event: check\ndata: {}\n\n",
                        json!({"sequence": check["event_sequence"], "check": check})
                    )
                };
                routes.insert(
                    format!("GET /v1/super/workspaces/{WORKSPACE}/check-events"),
                    json!({"_sse": [": keepalive\n\n", event(&verdict), if final_state == "pending" { ": keepalive\n\n".into() } else { event(&settled) }]}),
                );
            });
            let (mut process, mut master, _) = start_terminal(&fixture, &args, 100, 32);
            let mut text = String::new();
            read_until(&mut master, &mut text, "Checking the source.");
            read_until(&mut master, &mut text, "A fix proposal may still arrive.");
            let notice = match final_state {
                "available" => "super fix --check 42",
                "unavailable" => "No fix proposal available.",
                _ => "A fix proposal may still arrive.",
            };
            read_until(&mut master, &mut text, notice);
            if final_state != "pending" {
                read_until(&mut master, &mut text, "Explanation delivered after the verdict.");
            }
            assert!(process.0.try_wait().unwrap().is_none(), "{text}");
            master.write_all(b"\x1b").unwrap();
            read_until(&mut master, &mut text, "\x1b[?1049l");
            let mut receipt = text.rsplit("\x1b[?1049l").next().unwrap().to_owned();
            read_until(&mut master, &mut receipt, notice);
            if final_state == "pending" {
                read_until(&mut master, &mut receipt, "--watch");
                assert!(
                    console::strip_ansi_codes(&receipt).contains("Watch: super status --check 42 --watch"),
                    "{receipt}"
                );
            }
            assert_eq!(wait_for_exit(&mut process).code(), Some(exit_code));
            assert!(!receipt.contains("Checking the source."));
            assert!(!receipt.contains("Detached"));
            assert!(
                !fixture
                    .service
                    .seen
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|request| request.contains("/cancel"))
            );
        }
    }

    #[test]
    fn small_terminals_and_json_still_exit_automatically_at_the_verdict() {
        for (args, cols, rows) in [(vec!["check"], 50, 12), (vec!["check", "--json"], 100, 32)] {
            let fixture = Fixture::new(false);
            let (mut process, mut master, _) = start_terminal(&fixture, &args, cols, rows);
            let mut text = String::new();
            read_until(
                &mut master,
                &mut text,
                if args.contains(&"--json") {
                    "\"type\":\"results\""
                } else {
                    "super fix --check 42"
                },
            );
            assert_eq!(wait_for_exit(&mut process).code(), Some(1));
            assert!(!text.contains("\x1b[?1049h"));
        }
    }

    #[test]
    fn escape_detaches_single_check_and_directly_opened_status_without_canceling() {
        for args in [vec!["check"], vec!["status", "--check", CHECK, "--watch"]] {
            let fixture = Fixture::configured(false, true, |routes| {
                routes.insert(
                    format!("GET /v1/super/workspaces/{WORKSPACE}/check-events"),
                    json!({"_sse": [": keepalive\n\n"]}),
                );
            });
            let (mut process, mut master, slave) = start_terminal(&fixture, &args, 80, 24);
            let mut text = String::new();
            read_until(&mut master, &mut text, "Esc/Ctrl+C detach");
            master.write_all(b"\x1b").unwrap();
            read_until(&mut master, &mut text, "Checks continue remotely.");
            assert!(process.0.wait().unwrap().success());
            assert!(text.contains("\x1b[?1049l"));
            assert!(
                tcgetattr(&slave)
                    .unwrap()
                    .local_flags
                    .contains(nix::sys::termios::LocalFlags::ICANON)
            );
            assert!(
                fixture
                    .service
                    .seen
                    .lock()
                    .unwrap()
                    .iter()
                    .all(|path| !path.ends_with("/cancel"))
            );
            assert_eq!(fixture.source(), SOURCE);
        }
    }

    #[test]
    fn watched_passes_start_with_headline_and_enter_reveals_explanation() {
        for multiple in [false, true] {
            let fixture = Fixture::configured(false, false, |routes| {
                let check = routes
                    .get_mut(&format!("GET /v1/super/workspaces/{WORKSPACE}/checks/{CHECK}"))
                    .unwrap();
                check["result"]["outcome"] = json!("pass");
                check["problematic"] = json!(false);
                check["fix"] = Value::Null;
                check["presentation"]["details"] = json!([
                    {"text":"", "emphasized":false},
                    {"text":"Conclusion", "emphasized":true},
                    {"text":"  Duplicate items are removed.", "emphasized":false},
                    {"text":"  The function tracks previously seen integers.", "emphasized":false}
                ]);
                let passed = check.clone();
                for response in routes.values_mut() {
                    if let Some(checks) = response.get_mut("checks").and_then(Value::as_array_mut) {
                        checks[0] = passed.clone();
                        if multiple {
                            let mut second = passed.clone();
                            second["number"] = json!(43);
                            second["supertest"]["name"] = json!("another_law");
                            checks.push(second);
                        }
                    }
                    if multiple && let Some(submission) = response.get_mut("submission") {
                        submission["check_numbers"] = json!([42, 43]);
                    }
                }
            });
            let args = if multiple {
                vec!["status", "--run", RUN, "--watch"]
            } else {
                vec!["status", "--check", CHECK, "--watch"]
            };
            let (mut process, mut master, _) = start_terminal(&fixture, &args, 80, 24);
            let mut text = String::new();
            read_until(&mut master, &mut text, "Esc/Ctrl+C close");
            assert!(text.contains("Duplicate items are removed."));
            assert!(!text.contains("The function tracks previously seen integers."));
            assert!(text.contains("Enter for more details"));
            master.write_all(b"\r").unwrap();
            read_until(&mut master, &mut text, "The function tracks previously seen integers.");
            read_until(&mut master, &mut text, "Esc back");
            master.write_all(b"\x03").unwrap();
            read_until(&mut master, &mut text, "\x1b[?1049l");
            assert!(process.0.wait().unwrap().success());
            assert!(
                fixture
                    .service
                    .seen
                    .lock()
                    .unwrap()
                    .iter()
                    .all(|request| request.starts_with("GET "))
            );
        }
    }

    #[test]
    fn monochrome_tables_bold_the_selected_row_without_color_codes() {
        let fixture = Fixture::configured(false, false, |routes| {
            let current = routes[&format!("GET /v1/super/workspaces/{WORKSPACE}/checks/{CHECK}")].clone();
            let mut second = current.clone();
            second["number"] = json!(43);
            second["supertest"]["name"] = json!("another_law");
            for response in routes.values_mut() {
                if let Some(checks) = response.get_mut("checks").and_then(Value::as_array_mut) {
                    checks.push(second.clone());
                }
                if let Some(submission) = response.get_mut("submission") {
                    submission["check_numbers"] = json!([42, 43]);
                }
            }
            let mut earlier = current.clone();
            earlier["number"] = json!(41);
            earlier["created_at"] = json!("2025-12-31T00:00:00Z");
            routes
                .get_mut(&format!("GET /v1/super/workspaces/{WORKSPACE}/checks/history"))
                .unwrap()["checks"] = json!([second, current, earlier]);
        });
        let command = fixture.command(&["status", "--run", RUN, "--watch"]);
        let (mut process, mut master, _) = start_terminal_command(command, 120, 36);
        let mut text = String::new();
        read_until(&mut master, &mut text, "Esc/Ctrl+C close");
        for cell in ["›", "×", "law", "fail", " · "] {
            assert!(
                text.contains(&format!("\x1b[1m{cell}")),
                "selected cell is not bold: {cell}; {text}"
            );
        }
        master.write_all(b"\r").unwrap();
        let mut history = String::new();
        read_until(&mut master, &mut history, "Check history");
        read_until(&mut master, &mut history, "↑/↓ attempts");
        read_until(&mut master, &mut history, "Ctrl+C close");
        assert!(history.contains("COMMIT"));
        for cell in ["›", "×", "fail", &fixture.head[..7], "#42"] {
            assert!(
                history.contains(&format!("\x1b[1m{cell}")),
                "selected history cell is not bold: {cell}; {history}"
            );
        }
        let sgr = regex::Regex::new("\u{1b}\\[([0-9;]*)m").unwrap();
        for output in [&text, &history] {
            assert!(
                sgr.captures_iter(output)
                    .all(|capture| matches!(&capture[1], "0" | "1"))
            );
        }
        master.write_all(b"\x03").unwrap();
        read_until(&mut master, &mut history, "\x1b[?1049l");
        assert!(process.0.wait().unwrap().success());
    }

    #[test]
    fn single_check_history_is_visible_and_selectable_without_enter() {
        for (args, width, height) in [
            (vec!["check"], 80, 24),
            (vec!["status", "--check", CHECK, "--watch"], 120, 36),
        ] {
            let fixture = Fixture::configured(false, true, |routes| {
                routes.insert(
                    format!("GET /v1/super/workspaces/{WORKSPACE}/check-events"),
                    json!({"_sse": [": keepalive\n\n"]}),
                );
                let history = routes
                    .get_mut(&format!("GET /v1/super/workspaces/{WORKSPACE}/checks/history"))
                    .unwrap();
                let mut earlier = history["checks"][0].clone();
                earlier["number"] = json!(41);
                earlier["created_at"] = json!("2025-12-31T00:00:00Z");
                earlier["terminal"] = json!(true);
                earlier["presentation"]["details"] = json!([
                    {"text": "Earlier attempt evidence.", "emphasized": false}
                ]);
                history["checks"].as_array_mut().unwrap().push(earlier);
            });
            let (mut process, mut master, slave) = start_terminal(&fixture, &args, width, height);
            let mut text = String::new();
            read_until(&mut master, &mut text, "↑/↓ attempts");
            assert!(text.contains("REQUESTED") && text.contains("#41"));
            assert!(!text.contains("Earlier attempt evidence."));
            assert!(text.contains("Esc/Ctrl+C detach"));
            master.write_all(b"\x1b[B").unwrap();
            read_until(&mut master, &mut text, "Earlier attempt evidence.");
            master.write_all(b"\x1b").unwrap();
            read_until(&mut master, &mut text, "Checks continue remotely.");
            assert!(process.0.wait().unwrap().success());
            assert!(
                tcgetattr(&slave)
                    .unwrap()
                    .local_flags
                    .contains(nix::sys::termios::LocalFlags::ICANON)
            );
            assert!(
                fixture
                    .service
                    .seen
                    .lock()
                    .unwrap()
                    .iter()
                    .all(|path| !path.ends_with("/cancel"))
            );
        }
    }

    #[test]
    fn accepted_checks_render_before_slow_history_and_detach_without_a_stale_table() {
        let fixture = Fixture::configured(false, true, |routes| {
            routes
                .get_mut(&format!("GET /v1/super/workspaces/{WORKSPACE}/checks/history"))
                .unwrap()["_delay_ms"] = json!(1500);
        });
        let (mut process, mut master, slave) = start_terminal(&fixture, &["check"], 80, 24);
        let mut text = String::new();
        read_until(&mut master, &mut text, "Starting checks");
        let start = Instant::now();
        read_until(&mut master, &mut text, "Elapsed");
        assert!(
            start.elapsed() < Duration::from_millis(1200),
            "history delayed the first accepted-check frame: {:?}; requests: {:?}; output: {text}",
            start.elapsed(),
            fixture.service.seen.lock().unwrap()
        );
        assert!(
            text.contains("38;2;209;177;245"),
            "real terminal has the purple active indicator"
        );
        assert!(
            text.contains("48;2;55;60;73"),
            "the browser highlights its detail panel"
        );
        master.write_all(b"\x03").unwrap();
        read_until(&mut master, &mut text, "--watch");
        assert!(process.0.wait().unwrap().success());
        let after_raw = text.split("\x1b[?1049l").last().unwrap();
        assert!(!after_raw.contains("48;2;"), "a highlight escaped the interactive view");
        let after = console::strip_ansi_codes(after_raw);
        assert!(
            after.contains("Detached") && after.contains("Checks continue remotely."),
            "{after}"
        );
        assert!(
            after.contains(&format!("super status --check {CHECK} --watch")),
            "{after}"
        );
        assert!(!after.contains(RUN));
        assert!(
            !after.contains("attempt history") && !after.contains("SUPERTEST"),
            "{after}"
        );
        assert!(
            tcgetattr(&slave)
                .unwrap()
                .local_flags
                .contains(nix::sys::termios::LocalFlags::ICANON)
        );
        assert!(
            fixture
                .service
                .seen
                .lock()
                .unwrap()
                .iter()
                .all(|path| !path.ends_with("/cancel"))
        );
    }

    #[test]
    fn failed_optional_history_keeps_the_view_and_current_results_available() {
        let fixture = Fixture::configured(false, true, |routes| {
            routes.insert(
                format!("GET /v1/super/workspaces/{WORKSPACE}/checks/history"),
                json!({"_responses": [{
                    "status": 503, "body": {"error": {"code": "unavailable", "message": "History unavailable"}}
                }]}),
            );
        });
        let (mut process, mut master, _) = start_terminal(&fixture, &["status", "--run", RUN, "--watch"], 120, 36);
        let mut text = String::new();
        read_until(&mut master, &mut text, "n retry history");
        assert!(text.contains("History unavailable"));
        assert!(process.0.try_wait().unwrap().is_none());
        assert!(!text.contains("\x1b[?1049l"), "a history error closed the view");
        assert!(text.contains("#42"));
        master.write_all(b"n").unwrap();
        master.write_all(b"\x03").unwrap();
        read_until(&mut master, &mut text, "\x1b[?1049l");
        assert!(process.0.wait().unwrap().success());
        assert_eq!(fixture.source(), SOURCE);
    }

    #[test]
    fn a_slow_failed_older_page_preserves_the_selected_check_and_detach_command() {
        let fixture = Fixture::configured(false, true, |routes| {
            routes.insert(
                format!("GET /v1/super/workspaces/{WORKSPACE}/check-events"),
                json!({"_sse": [": keepalive\n\n"]}),
            );
            let key = format!("GET /v1/super/workspaces/{WORKSPACE}/checks/history");
            let mut first = routes[&key].clone();
            first["next_before"] = json!(42);
            routes.insert(key, json!({"_responses": [
                {"status": 200, "body": first},
                {"status": 503, "body": {"_delay_ms": 500, "error": {"code": "unavailable", "message": "Older history unavailable"}}}
            ]}));
        });
        let (mut process, mut master, slave) = start_terminal(&fixture, &["status", "--run", RUN, "--watch"], 80, 24);
        let mut text = String::new();
        read_until(&mut master, &mut text, "n older");
        master.write_all(b"n").unwrap();
        read_until(&mut master, &mut text, "n retry history");
        // Reconstruct the visible rows: a spinner repaint at row 1 no longer rewrites
        // the unchanged check identity and controls below it.
        let cursor = regex::Regex::new(r"\x1b\[(\d+);1H").unwrap();
        let mut rows = BTreeMap::new();
        for (position, line) in cursor.captures_iter(&text).zip(cursor.split(&text).skip(1)) {
            rows.insert(position[1].parse::<usize>().unwrap(), console::strip_ansi_codes(line));
        }
        let last_frame = rows.values().map(AsRef::as_ref).collect::<Vec<_>>().join("\n");
        assert!(last_frame.contains("#42"));
        assert!(last_frame.contains("Esc/Ctrl+C detach"));
        assert!(process.0.try_wait().unwrap().is_none());
        master.write_all(b"\x03").unwrap();
        read_until(&mut master, &mut text, &format!("super status --check {CHECK} --watch"));
        assert!(process.0.wait().unwrap().success());
        assert!(
            tcgetattr(&slave)
                .unwrap()
                .local_flags
                .contains(nix::sys::termios::LocalFlags::ICANON)
        );
        assert_eq!(fixture.source(), SOURCE);
        assert!(
            fixture
                .service
                .seen
                .lock()
                .unwrap()
                .iter()
                .any(|request| request.contains("before=42"))
        );
    }

    #[test]
    fn confirmation_requires_enter_and_rejects_invalid_answers_before_applying() {
        let fixture = Fixture::new(false);
        let (mut process, mut master, slave) = start_terminal(&fixture, &["fix", "--check", CHECK], 80, 24);
        let mut text = String::new();
        read_until(&mut master, &mut text, "[Y/n]");
        master.write_all(b"y").unwrap();
        text.clear();
        read_until(&mut master, &mut text, "[Y/n]");
        assert!(process.0.try_wait().unwrap().is_none());
        assert_eq!(fixture.source(), SOURCE, "typing y alone must not apply");
        master.write_all(b"esterday\r").unwrap();
        read_until(&mut master, &mut text, "Enter y or n");
        assert_eq!(fixture.source(), SOURCE, "invalid words must not be accepted as y");
        master.write_all(b"\x15YES\r").unwrap();
        read_until(&mut master, &mut text, "super check test.py::law");
        assert!(process.0.wait().unwrap().success());
        assert!(fixture.source().contains("assert 1 == 1"));
        assert!(
            tcgetattr(&slave)
                .unwrap()
                .local_flags
                .contains(nix::sys::termios::LocalFlags::ICANON)
        );
    }

    #[test]
    fn dirty_confirmation_shares_the_prompt_and_does_not_remember_declining_or_canceling() {
        let sgr = regex::Regex::new("\u{1b}\\[[0-9;]*m").unwrap();
        for (answer, canceled, no_color) in [
            (b"no\r".as_slice(), false, false),
            (b"\x03".as_slice(), true, false),
            (b"\x1b".as_slice(), true, true),
        ] {
            let fixture = Fixture::new(false);
            let changed = format!("{SOURCE}# local edit\n");
            fs::write(fixture.directory.path().join("repo/test.py"), &changed).unwrap();
            let mut command = fixture.command(&["check"]);
            if !no_color {
                command.env_remove("NO_COLOR");
            }
            let (mut process, mut master, slave) = start_terminal_command(command, 80, 24);
            let mut text = String::new();
            read_until(&mut master, &mut text, "[Y/n]");
            assert!(
                sgr.replace_all(&text, "")
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
                    .contains("Uncommitted changes found. Proceed with the check, including these changes?"),
                "long prompt must not be clipped"
            );
            assert_eq!(text.contains("38;2;209;177;245"), !no_color);
            master.write_all(answer).unwrap();
            read_until(
                &mut master,
                &mut text,
                if canceled {
                    "Check canceled"
                } else {
                    "the worktree has uncommitted changes"
                },
            );
            assert_eq!(process.0.wait().unwrap().code(), Some(2));
            let state: Value =
                serde_json::from_slice(&fs::read(fixture.directory.path().join("config/state.json")).unwrap()).unwrap();
            assert!(state["repositories"][0].get("dirty_preference").is_none());
            assert_eq!(fixture.source(), changed);
            assert!(
                fixture.service.seen.lock().unwrap().is_empty(),
                "declined or canceled checks must not upload"
            );
            assert!(
                tcgetattr(&slave)
                    .unwrap()
                    .local_flags
                    .contains(nix::sys::termios::LocalFlags::ICANON)
            );
            if no_color {
                assert!(!sgr.is_match(&text));
            }
            let (mut again, mut master, _) = start_terminal(&fixture, &["check"], 80, 24);
            let mut text = String::new();
            read_until(&mut master, &mut text, "[Y/n]");
            master.write_all(b"\x1b").unwrap();
            read_until(&mut master, &mut text, "Check canceled");
            assert_eq!(again.0.wait().unwrap().code(), Some(2));
        }
    }

    #[test]
    fn dirty_confirmation_ignores_legacy_answers_and_does_not_remember_acceptance() {
        for previous in [Value::Null, json!(true), json!(false)] {
            let fixture = Fixture::new(false);
            let state_path = fixture.directory.path().join("config/state.json");
            let mut state: Value = serde_json::from_slice(&fs::read(&state_path).unwrap()).unwrap();
            state["repositories"][0]["dirty_preference"] = previous;
            fs::write(&state_path, state.to_string()).unwrap();
            let root = fixture.directory.path().join("repo");
            let changed = format!("{SOURCE}# local edit\n");
            fs::write(root.join("test.py"), &changed).unwrap();
            let before_status = git(&root, &["status", "--porcelain"]);
            let mut temporary_commits = Value::Null;
            for _ in 0..2 {
                let (mut process, mut master, _) = start_terminal(&fixture, &["check", "--detach"], 80, 24);
                let mut text = String::new();
                read_until(&mut master, &mut text, "[Y/n]");
                master.write_all(b"\r").unwrap();
                // This fixture intentionally has no source-upload route. Acceptance must
                // advance to syncing, and even a retry after that failure must ask again.
                read_until(&mut master, &mut text, "fixture not found");
                assert_eq!(process.0.wait().unwrap().code(), Some(2));
                let state: Value = serde_json::from_slice(&fs::read(&state_path).unwrap()).unwrap();
                assert!(state["repositories"][0].get("dirty_preference").is_none());
                let current = &state["repositories"][0]["temporary_commits"];
                assert_eq!(current.as_object().unwrap().len(), 1);
                if !temporary_commits.is_null() {
                    assert_eq!(
                        current, &temporary_commits,
                        "unchanged source can reuse its temporary commit"
                    );
                }
                temporary_commits = current.clone();
                assert_eq!(git(&root, &["rev-parse", "HEAD"]), fixture.head);
                assert_eq!(git(&root, &["status", "--porcelain"]), before_status);
                assert_eq!(fixture.source(), changed);
            }
        }
    }

    #[test]
    fn fix_preview_is_colored_and_confirmation_defaults_to_yes_or_applies_exactly_once() {
        for (cols, rows, answer, applies) in [
            (80, 24, b"\r".as_slice(), true),
            (120, 36, b"yes\r".as_slice(), true),
            (80, 24, b"NO\r".as_slice(), false),
            (80, 24, b"y\x7fn\r".as_slice(), false),
            (80, 24, b"\x03".as_slice(), false),
            (80, 24, b"\x1b".as_slice(), false),
        ] {
            let fixture = Fixture::new(false);
            let (mut process, mut master, slave) = start_terminal(&fixture, &["fix", "--check", CHECK], cols, rows);
            let mut text = String::new();
            read_until(&mut master, &mut text, "[Y/n]");
            assert_eq!(fixture.source(), SOURCE, "preview mutated source before approval");
            assert!(
                text.contains("48;2;72;42;43") && text.contains("48;2;36;60;45"),
                "diff backgrounds are missing"
            );
            assert!(text.contains("38;2;243;161;161"), "removed-line markers must be red");
            master.write_all(answer).unwrap();
            read_until(
                &mut master,
                &mut text,
                if applies {
                    "super check test.py::law"
                } else {
                    "No changes made."
                },
            );
            assert!(process.0.wait().unwrap().success());
            if applies {
                assert!(fixture.source().contains("assert 1 == 1"));
            } else {
                assert_eq!(fixture.source(), SOURCE);
            }
            assert_eq!(
                git(&fixture.directory.path().join("repo"), &["rev-parse", "HEAD"]),
                fixture.head
            );
            assert!(
                tcgetattr(&slave)
                    .unwrap()
                    .local_flags
                    .contains(nix::sys::termios::LocalFlags::ICANON)
            );
        }
    }

    #[test]
    fn changed_source_warning_precedes_confirmation_and_later_edits_stop_application() {
        for edit_during_prompt in [false, true] {
            let fixture = Fixture::new(false);
            let root = fixture.directory.path().join("repo");
            fs::write(root.join("notes.txt"), "before preview\n").unwrap();
            let (mut process, mut master, _slave) = start_terminal(&fixture, &["fix", "--check", CHECK], 80, 24);
            let mut text = String::new();
            read_until(&mut master, &mut text, "[Y/n]");
            assert!(text.find("Your source has changed").unwrap() < text.find("[Y/n]").unwrap());
            assert_eq!(fixture.source(), SOURCE);
            if edit_during_prompt {
                fs::write(root.join("notes.txt"), "edited while deciding\n").unwrap();
            }
            master.write_all(b"\r").unwrap();
            read_until(
                &mut master,
                &mut text,
                if edit_during_prompt {
                    "No fix was applied"
                } else {
                    "super check test.py::law"
                },
            );
            assert_eq!(
                process.0.wait().unwrap().code(),
                Some(if edit_during_prompt { 2 } else { 0 })
            );
            assert_eq!(
                fixture.source(),
                if edit_during_prompt {
                    SOURCE.to_owned()
                } else {
                    SOURCE.replace("assert True", "assert 1 == 1")
                }
            );
            assert_eq!(
                fs::read_to_string(root.join("notes.txt")).unwrap(),
                if edit_during_prompt {
                    "edited while deciding\n"
                } else {
                    "before preview\n"
                }
            );
        }
    }
}

#[test]
fn check_exit_codes_distinguish_acceptance_findings_and_operational_failure() {
    for (detach, operational, code) in [(true, false, 0), (false, false, 1), (false, true, 2)] {
        let fixture = Fixture::new(operational);
        let mut args = vec!["check", "--json"];
        if detach {
            args.push("--detach");
        }
        let output = fixture.run(&args);
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(output.status.code(), Some(code), "{value}");
        assert_eq!(value["data"]["run"]["id"], RUN, "{value}");
        assert!(
            fixture
                .service
                .seen
                .lock()
                .unwrap()
                .iter()
                .any(|request| request.starts_with("POST "))
        );
    }
}

#[test]
fn fix_default_preview_is_compact_and_details_and_json_preserve_the_proposal() {
    for summary in ["Review the proposed correction.", "Make the assertion explicit."] {
        let mut proposal = Value::Null;
        let fixture = Fixture::configured(false, false, |routes| {
            let check = routes
                .get_mut(&format!("GET /v1/super/workspaces/{WORKSPACE}/checks/{CHECK}"))
                .unwrap();
            check["fix"]["summary"] = json!(summary);
            check["fix"]["validation"] = json!(["Run the regression suite."]);
            proposal = check["fix"].clone();
        });
        for details in [false, true] {
            let mut args = vec!["fix", "--check", CHECK, "--dry-run"];
            if details {
                args.push("--details");
            }
            let output = fixture.run(&args);
            assert!(output.status.success(), "{output:?}");
            let text = String::from_utf8_lossy(&output.stdout);
            assert!(
                text.starts_with(&format!(
                    "  text-tools · {}\n  Fix proposal · law · #42\n",
                    &fixture.head[..7]
                )),
                "{text}"
            );
            assert_eq!(
                text.contains(summary),
                details || summary != "Review the proposed correction."
            );
            assert_eq!(text.contains("Instructions"), details);
            assert_eq!(text.contains("Apply the proposed text patch."), details);
            assert_eq!(text.contains("Suggested validation"), details);
            assert_eq!(text.contains("Run the regression suite."), details);
            assert!(text.contains("assert True") && text.contains("assert 1 == 1"));
            assert!(!text.contains("fail") && !text.contains("super ·"));
            let progress = String::from_utf8_lossy(&output.stderr);
            assert!(progress.contains("Fetching fix proposal"));
            assert!(!progress.contains("Waiting for") && !progress.contains("Preparing a fix"));
            args.push("--json");
            let output = fixture.run(&args);
            assert!(output.status.success(), "{output:?}");
            let data: Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(data["data"]["proposal"], proposal);
            assert_eq!(fixture.source(), SOURCE);
        }
    }
}

#[test]
fn fix_waiting_message_uses_pending_state_and_stops_when_no_proposal_is_expected() {
    for (terminal, pending, message) in [
        (true, true, Some("Waiting for fix proposal")),
        (false, false, Some("Waiting for check updates")),
        (true, false, None),
    ] {
        let fixture = Fixture::configured(false, false, |routes| {
            let check = routes
                .get_mut(&format!("GET /v1/super/workspaces/{WORKSPACE}/checks/{CHECK}"))
                .unwrap();
            check["fix"] = Value::Null;
            check["fix_pending"] = json!(pending);
            check["terminal"] = json!(terminal);
        });
        let output = fixture.run(&["fix", "--check", CHECK, "--dry-run"]);
        let text = String::from_utf8_lossy(&output.stderr);
        assert!(text.contains("Fetching fix proposal"), "{text}");
        assert!(!text.contains("Preparing a fix"), "{text}");
        if let Some(message) = message {
            assert!(output.status.success(), "{output:?}");
            assert!(text.contains(message), "{text}");
        } else {
            assert_eq!(output.status.code(), Some(2));
            assert!(text.contains("finished without a fix proposal"), "{text}");
            assert!(!text.contains("Waiting for"), "{text}");
            assert!(
                !fixture
                    .service
                    .seen
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|route| route.contains("check-events"))
            );
        }
        assert_eq!(fixture.source(), SOURCE);
    }
}

#[test]
fn fix_completion_rechecks_the_specific_supertest_from_the_current_directory() {
    let fixture = Fixture::new(false);
    let root = fixture.directory.path().join("repo");
    fs::create_dir(root.join("nested")).unwrap();
    let output = fixture
        .command(&["fix", "--check", CHECK, "--yes"])
        .current_dir(root.join("nested"))
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        text.ends_with(
            "  ✓ Applied · 1 file changed\n  Changes are uncommitted.\n\n  Next: super check ../test.py::law\n"
        ),
        "{text}"
    );
    assert!(!text.contains("--dirty"));
    assert_eq!(fixture.source(), SOURCE.replace("assert True", "assert 1 == 1"));
    assert_eq!(git(&root, &["rev-parse", "HEAD"]), fixture.head);
}

#[test]
fn fix_preview_is_read_only_and_application_requires_approval_without_committing() {
    let fixture = Fixture::new(false);
    let rejected = fixture.run(&["fix", "--check", CHECK, "--json"]);
    assert_eq!(rejected.status.code(), Some(2));
    assert!(fixture.service.seen.lock().unwrap().is_empty());
    let preview = fixture.run(&["fix", "--check", CHECK, "--dry-run", "--json"]);
    assert!(
        preview.status.success(),
        "{}",
        format_args!(
            "stdout={} stderr={}",
            String::from_utf8_lossy(&preview.stdout),
            String::from_utf8_lossy(&preview.stderr)
        )
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&preview.stdout).unwrap()["type"],
        "fix_proposal"
    );
    assert_eq!(fixture.source(), SOURCE);
    let preview_data = serde_json::from_slice::<Value>(&preview.stdout).unwrap();
    assert_eq!(preview_data["data"]["source_changed"], false);
    assert_eq!(preview_data["data"]["applies_cleanly"], true);
    let applied = fixture.run(&["fix", "--check", CHECK, "--yes", "--json"]);
    assert!(
        applied.status.success(),
        "{}",
        format_args!(
            "stdout={} stderr={}",
            String::from_utf8_lossy(&applied.stdout),
            String::from_utf8_lossy(&applied.stderr)
        )
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&applied.stdout).unwrap()["type"],
        "success"
    );
    assert!(fixture.source().contains("assert 1 == 1"));
    assert_eq!(
        git(&fixture.directory.path().join("repo"), &["rev-parse", "HEAD"]),
        fixture.head
    );
}

#[test]
fn fix_uses_dirty_checked_contents_without_warning_or_changing_staging() {
    let checked = format!("# checked edit\n{SOURCE}");
    let tree = source_tree_sha256(&[SourceFile {
        path: "test.py".into(),
        sha256: source_sha256(checked.as_bytes()),
        bytes: checked.len() as u64,
        executable: false,
    }])
    .unwrap();
    let fixture = Fixture::configured(false, false, |routes| {
        let check = routes
            .get_mut(&format!("GET /v1/super/workspaces/{WORKSPACE}/checks/{CHECK}"))
            .unwrap();
        check["revision"]["tree_sha256"] = json!(tree);
        check["fix"]["base_tree_sha256"] = json!(tree);
    });
    let root = fixture.directory.path().join("repo");
    fs::write(root.join("test.py"), format!("# staged edit\n{SOURCE}")).unwrap();
    git(&root, &["add", "test.py"]);
    fs::write(root.join("test.py"), &checked).unwrap();
    let index = fs::read(root.join(".git/index")).unwrap();
    let output = fixture.run(&["fix", "--check", CHECK, "--yes", "--json"]);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap()["data"]["source_changed"],
        false
    );
    assert!(!String::from_utf8_lossy(&output.stderr).contains("source has changed"));
    assert_eq!(fixture.source(), checked.replace("assert True", "assert 1 == 1"));
    assert_eq!(fs::read(root.join(".git/index")).unwrap(), index);
    assert_eq!(git(&root, &["rev-parse", "HEAD"]), fixture.head);
}

#[test]
fn changed_source_warns_and_applies_with_line_offsets_and_existing_edits() {
    for committed in [false, true] {
        let fixture = Fixture::new(false);
        let root = fixture.directory.path().join("repo");
        let shifted = format!("{}{SOURCE}", "# added earlier\n".repeat(20));
        fs::write(root.join("test.py"), &shifted).unwrap();
        fs::write(root.join("notes.txt"), "keep my notes\n").unwrap();
        if committed {
            git(&root, &["add", "."]);
            git(&root, &["-c", "commit.gpgsign=false", "commit", "-qm", "Later source"]);
        }
        let head = git(&root, &["rev-parse", "HEAD"]);
        let index = fs::read(root.join(".git/index")).unwrap();
        let preview = fixture.run(&["fix", "--check", CHECK, "--dry-run", "--json"]);
        assert!(preview.status.success(), "{preview:?}");
        let data = serde_json::from_slice::<Value>(&preview.stdout).unwrap();
        assert_eq!(data["data"]["source_changed"], true);
        assert_eq!(data["data"]["applies_cleanly"], true);
        assert_eq!(fixture.source(), shifted);
        let output = fixture.run(&["fix", "--check", CHECK, "--yes", "--json"]);
        assert!(output.status.success(), "{output:?}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("Your source has changed since check #42"));
        assert_eq!(
            serde_json::from_slice::<Value>(&output.stdout).unwrap()["data"]["source_changed"],
            true
        );
        assert_eq!(fixture.source(), shifted.replace("assert True", "assert 1 == 1"));
        assert_eq!(fs::read(root.join(".git/index")).unwrap(), index);
        assert_eq!(git(&root, &["rev-parse", "HEAD"]), head);
        assert_eq!(fs::read_to_string(root.join("notes.txt")).unwrap(), "keep my notes\n");
    }
}

#[test]
fn unchanged_source_with_a_different_commit_does_not_warn() {
    let fixture = Fixture::new(false);
    let root = fixture.directory.path().join("repo");
    git(
        &root,
        &[
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--allow-empty",
            "-qm",
            "Same source",
        ],
    );
    let output = fixture.run(&["fix", "--check", CHECK, "--yes", "--json"]);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap()["data"]["source_changed"],
        false
    );
    assert!(!String::from_utf8_lossy(&output.stderr).contains("source has changed"));
}

#[test]
fn conflicting_fix_remains_previewable_and_application_preserves_local_edits() {
    let fixture = Fixture::new(false);
    let changed = SOURCE.replace("assert True", "assert False");
    fs::write(fixture.directory.path().join("repo/test.py"), &changed).unwrap();
    let preview = fixture.run(&["fix", "--check", CHECK, "--dry-run", "--json"]);
    assert!(preview.status.success(), "{preview:?}");
    let data = serde_json::from_slice::<Value>(&preview.stdout).unwrap();
    assert_eq!(data["data"]["applies_cleanly"], false);
    assert!(data["data"]["apply_error"].as_str().unwrap().contains("test.py"));
    let output = fixture.run(&["fix", "--check", CHECK, "--yes", "--json"]);
    assert_eq!(output.status.code(), Some(2));
    let data = serde_json::from_slice::<Value>(&output.stdout).unwrap();
    assert!(data["data"]["message"].as_str().unwrap().contains("test.py"));
    assert_eq!(fixture.source(), changed);
}

#[test]
fn invalid_check_numbers_and_missing_cancel_target_are_structured_argument_errors() {
    let fixture = Fixture::new(false);
    for args in [
        vec!["status", "--check", "0", "--json"],
        vec!["status", "--check", RUN, "--json"],
        vec!["status", "--check", "9223372036854775808", "--json"],
        vec!["status", "--history", "--before", "0", "--json"],
        vec!["cancel", "--json"],
        vec!["fix", "--agent", "codex", "--yes", "--json"],
        vec!["fix", "--agent", "claude", "--yes", "--json"],
    ] {
        let output = fixture.run(&args);
        assert_eq!(output.status.code(), Some(2));
        assert_eq!(
            serde_json::from_slice::<Value>(&output.stdout).unwrap()["data"]["code"],
            "invalid_arguments"
        );
    }
    assert!(fixture.service.seen.lock().unwrap().is_empty());
}

#[test]
fn attached_checks_reconnect_from_snapshots_and_cancel_is_explicit() {
    let fixture = Fixture::with_active_check(false, true);
    let output = fixture.run(&["check", "--json"]);
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(1), "{value}");
    assert_eq!(value["data"]["rows"][0]["current"]["event_sequence"], 2);
    assert_eq!(value["data"]["rows"][0]["current"]["result"]["outcome"], "fail");
    assert!(value["data"]["rows"][0]["current"].get("counterexamples").is_none());
    assert!(
        value["data"]["rows"][0]["current"]["presentation"]["details"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Witness: -42")
    );
    let requests = fixture.service.seen.lock().unwrap().clone();
    let streams: Vec<_> = requests.iter().filter(|r| r.contains("/check-events")).collect();
    assert!(streams.len() >= 2, "{requests:?}");
    assert!(
        streams.iter().all(|r| !r.contains("after=")),
        "reconnect uses authoritative snapshots"
    );
    assert!(requests.iter().all(|r| !r.contains("/cancel")));
    let output = fixture.run(&["cancel", "--check", CHECK, "--json"]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(
        fixture
            .service
            .seen
            .lock()
            .unwrap()
            .iter()
            .any(|r| r.ends_with("/cancel"))
    );
}

#[test]
fn different_runs_observe_one_number_and_explicit_cancellation_changes_the_shared_check() {
    let fixture = Fixture::with_active_check(false, true);
    let other_run = "00000000-0000-0000-0000-000000000018";
    for run in [RUN, other_run] {
        let output = fixture.run(&["status", "--run", run, "--json"]);
        assert!(output.status.success());
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["data"]["run"]["id"], run);
        assert_eq!(value["data"]["run"]["check_numbers"], json!([42]));
        assert_eq!(value["data"]["rows"][0]["current"]["number"], 42);
        assert_eq!(value["data"]["rows"][0]["current"]["terminal"], false);
    }
    assert!(
        fixture
            .service
            .seen
            .lock()
            .unwrap()
            .iter()
            .all(|request| request.starts_with("GET "))
    );
    let output = fixture.run(&["cancel", "--run", RUN]);
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("○ canceled · law · #42"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("every run and observer"));
    let output = fixture.run(&["status", "--run", other_run, "--json"]);
    assert!(output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["data"]["rows"][0]["current"]["number"], 42);
    assert_eq!(value["data"]["rows"][0]["current"]["operational_error"], "canceled");
    assert_eq!(
        fixture
            .service
            .seen
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request.starts_with("POST "))
            .count(),
        1
    );
}

fn cancellation_fixture(lose_acceptance: bool) -> Fixture {
    Fixture::configured(false, false, |routes| {
        let base = format!("/v1/super/workspaces/{WORKSPACE}");
        let canceled = routes[&format!("POST {base}/checks/{CHECK}/cancel")].clone();
        publish_cancellation(routes, &format!("POST {base}/checks/{CHECK}/cancel"), &canceled);
        let mut replacement = routes[&format!("POST {base}/check-submissions")].clone();
        replacement["submission"]["id"] = json!("00000000-0000-0000-0000-000000000019");
        replacement["submission"]["check_numbers"] = json!([43]);
        replacement["checks"][0]["number"] = json!(43);
        replacement["checks"][0]["terminal"] = json!(false);
        replacement["checks"][0]["problematic"] = json!(false);
        replacement["checks"][0]["operational_error"] = Value::Null;
        replacement["checks"][0]["result"] = Value::Null;
        replacement["checks"][0]["fix"] = Value::Null;
        replacement["checks"][0]["presentation"]["details"] = json!([]);
        replacement["checks"][0]["presentation"]["status"] = json!({"marker":"●","label":"checking","tone":"active"});
        routes.insert(format!("GET {base}/checks/43"), replacement["checks"][0].clone());
        routes.insert(
            format!("GET {base}/checks/history"),
            json!({"checks":[replacement["checks"][0],canceled],"next_before":null}),
        );
        let mut responses = vec![json!({"status":409,"body":{"error":{
            "code":"check_cancellation_pending","message":"The previous check is still stopping."
        }}})];
        if lose_acceptance {
            responses.push(json!({"status":200,"body":{"_lose_response":true}}));
        }
        responses.push(json!({"status":200,"body":replacement}));
        routes.insert(
            format!("POST {base}/check-submissions"),
            json!({"_responses":responses}),
        );
    })
}

#[test]
fn canceled_checks_get_a_new_attempt_after_waiting_without_rewriting_old_runs() {
    let fixture = cancellation_fixture(false);
    let output = fixture.run(&["check", "--detach", "--json"]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stdout));
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["data"]["run"]["check_numbers"], json!([43]));
    assert_eq!(value["data"]["rows"][0]["current"]["number"], 43);
    assert!(String::from_utf8_lossy(&output.stderr).contains("finish canceling"));
    let keys = fixture.service.submission_keys.lock().unwrap().clone();
    assert_eq!(keys.len(), 2);
    assert_eq!(keys[0], keys[1], "waiting never invents a new submission");
    let old = fixture.run(&["status", "--run", RUN, "--json"]);
    assert!(old.status.success());
    let old: Value = serde_json::from_slice(&old.stdout).unwrap();
    assert_eq!(old["data"]["rows"][0]["current"]["number"], 42);
    assert_eq!(old["data"]["rows"][0]["current"]["operational_error"], "canceled");
    let history = fixture.run(&["status", "--check", "43", "--history", "--json"]);
    assert!(history.status.success(), "{}", String::from_utf8_lossy(&history.stdout));
    let history: Value = serde_json::from_slice(&history.stdout).unwrap();
    let entries = history["data"]["history"].as_array().unwrap();
    assert_eq!(entries.len(), 2);
    assert!(
        entries
            .iter()
            .any(|check| check["number"] == 42 && check["operational_error"] == "canceled")
    );
}

#[test]
fn lost_replacement_acceptance_recovers_the_same_submission() {
    let fixture = cancellation_fixture(true);
    let first = fixture.run(&["check", "--detach", "--json"]);
    assert_eq!(first.status.code(), Some(2));
    let retry = fixture.run(&["check", "--detach", "--json"]);
    assert!(retry.status.success(), "{}", String::from_utf8_lossy(&retry.stdout));
    let value: Value = serde_json::from_slice(&retry.stdout).unwrap();
    assert_eq!(value["data"]["run"]["check_numbers"], json!([43]));
    let keys = fixture.service.submission_keys.lock().unwrap();
    assert_eq!(keys.len(), 3);
    assert!(keys.iter().all(|key| key == &keys[0]));
}

#[cfg(unix)]
#[test]
fn interrupting_cancellation_wait_preserves_the_submission_key_and_does_not_cancel_work() {
    use nix::{
        sys::signal::{Signal, kill},
        unistd::Pid,
    };
    let fixture = cancellation_fixture(false);
    let mut child = fixture
        .command(&["check", "--detach", "--json"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while fixture.service.submission_keys.lock().unwrap().is_empty() {
        assert!(std::time::Instant::now() < deadline, "CLI did not submit");
        thread::sleep(Duration::from_millis(5));
    }
    kill(Pid::from_raw(i32::try_from(child.id()).unwrap()), Signal::SIGINT).unwrap();
    while child.try_wait().unwrap().is_none() {
        if std::time::Instant::now() >= deadline {
            child.kill().unwrap();
            panic!("CLI did not interrupt cancellation wait");
        }
        thread::sleep(Duration::from_millis(5));
    }
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stdout).contains("recover this submission"));
    let retry = fixture.run(&["check", "--detach", "--json"]);
    assert!(retry.status.success());
    let keys = fixture.service.submission_keys.lock().unwrap();
    assert!(keys.len() >= 2 && keys.iter().all(|key| key == &keys[0]));
    assert!(
        fixture
            .service
            .seen
            .lock()
            .unwrap()
            .iter()
            .all(|request| !request.ends_with("/cancel"))
    );
}

#[test]
fn lost_admission_response_reuses_the_durable_key_and_does_not_restart_existing_work() {
    let fixture = Fixture::with_lost_response(false, true, true);
    let first = fixture.run(&["check", "--detach", "--json"]);
    assert_eq!(first.status.code(), Some(2));
    let retry = fixture.run(&["check", "--detach", "--json"]);
    assert!(retry.status.success(), "{}", String::from_utf8_lossy(&retry.stdout));
    let result: Value = serde_json::from_slice(&retry.stdout).unwrap();
    assert_eq!(result["data"]["run"]["check_numbers"], json!([42]));
    let next = fixture.run(&["check", "--detach", "--json"]);
    assert!(next.status.success());
    let keys = fixture.service.submission_keys.lock().unwrap();
    assert_eq!(keys.len(), 3);
    assert_eq!(keys[0], keys[1], "uncertain admission retries the same request");
    assert_ne!(
        keys[1], keys[2],
        "a later command can create a new run, still containing check 42"
    );
}

#[test]
fn retrying_an_original_run_after_cancellation_does_not_create_a_replacement() {
    let fixture = Fixture::with_lost_response(false, true, true);
    let lost = fixture.run(&["check", "--detach", "--json"]);
    assert_eq!(lost.status.code(), Some(2));
    let canceled = fixture.run(&["cancel", "--check", CHECK, "--json"]);
    assert!(canceled.status.success());
    let retry = fixture.run(&["check", "--detach", "--json"]);
    assert!(retry.status.success());
    let value: Value = serde_json::from_slice(&retry.stdout).unwrap();
    assert_eq!(value["data"]["run"]["check_numbers"], json!([42]));
    assert_eq!(value["data"]["rows"][0]["current"]["operational_error"], "canceled");
    let keys = fixture.service.submission_keys.lock().unwrap();
    assert_eq!(keys.len(), 2);
    assert_eq!(keys[0], keys[1]);
}

#[test]
fn noninteractive_checks_finish_at_verdict_even_when_a_fix_is_pending() {
    for args in [
        vec!["check"],
        vec!["check", "--json"],
        vec!["check", "--json", "--stream"],
    ] {
        let fixture = Fixture::configured(false, true, |routes| {
            let mut verdict = routes[&format!("GET /v1/super/workspaces/{WORKSPACE}/checks/{CHECK}")].clone();
            verdict["terminal"] = json!(true);
            verdict["problematic"] = json!(true);
            verdict["result"] = json!({"outcome": "fail", "assurance": "uncertified"});
            verdict["fix"] = Value::Null;
            verdict["fix_pending"] = json!(true);
            verdict["updates_pending"] = json!(true);
            verdict["event_sequence"] = json!(2);
            routes.insert(
                format!("GET /v1/super/workspaces/{WORKSPACE}/check-events"),
                json!({"_sse": [format!("event: check\ndata: {}\n\n", json!({"sequence": 2, "check": verdict}))]}),
            );
        });
        let output = fixture.run(&args);
        assert_eq!(output.status.code(), Some(1));
        let text = String::from_utf8(output.stdout).unwrap();
        if args.contains(&"--json") {
            let result: Value = serde_json::from_str(text.lines().last().unwrap()).unwrap();
            assert_eq!(result["type"], "results");
            assert_eq!(result["data"]["finished"], true);
            assert_eq!(result["data"]["rows"][0]["current"]["fix_pending"], true);
            assert_eq!(result["data"]["rows"][0]["current"]["updates_pending"], true);
        } else {
            assert!(text.contains("A fix proposal may still arrive."));
            assert!(text.contains("Watch: super status --check 42 --watch"));
        }
    }
}

#[test]
fn attached_json_stream_has_admission_updates_and_final_results_with_normal_exit_codes() {
    for active in [false, true] {
        for operational in [false, true] {
            let fixture = Fixture::with_active_check(operational, active);
            let args = if active {
                ["--json", "check", "--stream"]
            } else {
                ["check", "--json", "--stream"]
            };
            let output = fixture.run(&args);
            assert_eq!(
                output.status.code(),
                Some(if operational { 2 } else { 1 }),
                "{output:?}"
            );
            assert!(!output.stderr.contains(&0x1b), "{output:?}");
            assert!(!output.stdout.contains(&0x1b));
            let events: Vec<Value> = String::from_utf8(output.stdout)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert!(events.iter().all(|event| event["schema_version"] == 1));
            let admission = &events[0];
            let final_result = events.last().unwrap();
            assert_eq!(admission["type"], "results");
            assert_eq!(admission["data"]["finished"], !active);
            assert_eq!(final_result["type"], "results");
            assert_eq!(final_result["data"]["finished"], true);
            assert_eq!(final_result["data"]["run"], admission["data"]["run"]);
            assert_eq!(events.iter().filter(|event| event["type"] == "results").count(), 2);
            assert_eq!(events.iter().any(|event| event["type"] == "check_updated"), active);
            assert!(!events.iter().any(|event| event["type"] == "closed"));
        }
    }
}

#[test]
fn attached_stream_pass_and_argument_errors_keep_machine_exit_contract() {
    let fixture = Fixture::configured(false, false, |routes| {
        for body in routes.values_mut() {
            if let Some(check) = body
                .get_mut("checks")
                .and_then(Value::as_array_mut)
                .and_then(|checks| checks.first_mut())
            {
                check["result"]["outcome"] = json!("pass");
                check["problematic"] = json!(false);
            }
        }
    });
    let output = fixture.run(&["check", "--json", "--stream"]);
    assert!(output.status.success(), "{output:?}");
    let output = fixture.run(&["check", "--stream"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("--stream requires --json"));
    for args in [
        vec!["check", "--json", "--stream", "--detach"],
        vec!["status", "--json", "--stream"],
    ] {
        let output = fixture.run(&args);
        assert_eq!(output.status.code(), Some(2));
        let error: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(error["type"], "error");
        assert_eq!(error["data"]["code"], "invalid_arguments");
    }
}

#[test]
fn json_status_watch_finishes_selected_checks_without_starting_work() {
    for selector in ["test.py", ".", "test.py::law"] {
        let fixture = Fixture::with_active_check(false, true);
        let output = fixture.run(&["status", selector, "--watch", "--json"]);
        assert!(output.status.success(), "{selector}: {output:?}");
        let events: Vec<Value> = String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let initial = &events[0];
        assert_eq!(initial["type"], "results");
        assert_eq!(initial["data"]["rows"][0]["current"]["number"], 42);
        assert_eq!(initial["data"]["finished"], false);
        assert!(events.iter().any(|event| {
            event["type"] == "check_updated"
                && event["data"]["check_number"] == 42
                && event["data"]["check"]["result"]["outcome"] == "fail"
        }));
        assert_eq!(events.last().unwrap()["type"], "closed");
        assert_eq!(events.last().unwrap()["data"]["detached"], false);
        let requests = fixture.service.seen.lock().unwrap();
        assert!(
            requests.iter().any(|path| path.contains("/checks/history?")),
            "{requests:?}"
        );
        assert!(
            requests.iter().any(|path| path.contains("/check-events?")),
            "{requests:?}"
        );
        assert!(requests.iter().all(|path| path.starts_with("GET ")), "{requests:?}");
    }
}

#[test]
fn json_status_watch_waits_for_all_selected_updates_but_not_older_history() {
    for proposal_available in [false, true] {
        let fixture = Fixture::configured(false, false, |routes| {
            let base = format!("GET /v1/super/workspaces/{WORKSPACE}");
            let mut first = routes[&format!("{base}/checks/{CHECK}")].clone();
            let proposal = first["fix"].take();
            first["updates_pending"] = json!(true);
            first["fix_pending"] = json!(true);
            first["event_sequence"] = json!(1);
            let mut second = first.clone();
            second["number"] = json!(43);
            second["supertest"]["name"] = json!("another_law");
            second["fix_pending"] = json!(false);
            let mut historical = first.clone();
            historical["number"] = json!(41);
            historical["terminal"] = json!(false);
            routes.insert(
                format!("{base}/checks/history"),
                json!({"checks": [second, first, historical], "next_before": null}),
            );
            let submission = routes.get_mut(&format!("{base}/check-submissions/{RUN}")).unwrap();
            submission["checks"] = json!([first, second]);
            submission["submission"]["check_numbers"] = json!([42, 43]);

            first["event_sequence"] = json!(2);
            first["updates_pending"] = json!(false);
            first["fix_pending"] = json!(false);
            if proposal_available {
                first["fix"] = proposal;
            }
            second["event_sequence"] = json!(2);
            second["updates_pending"] = json!(false);
            second["presentation"]["details"] =
                json!([{"text": "Explanation delivered after the verdict.", "emphasized": false}]);
            let event = |check: &Value| format!("event: check\ndata: {}\n\n", json!({"sequence": 2, "check": check}));
            // Separate connections exercise reconnecting after one check settles while another
            // still has an explanation pending. The older attempt never settles.
            routes.insert(
                format!("{base}/check-events"),
                json!({"_sse": [event(&first), event(&second)]}),
            );
        });
        let output = fixture.run(&["status", "--run", RUN, "--history", "--watch", "--json"]);
        assert!(output.status.success(), "{output:?}");
        let events: Vec<Value> = String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(events[0]["type"], "results");
        assert_eq!(events[0]["data"]["finished"], true);
        assert_eq!(events[0]["data"]["rows"].as_array().unwrap().len(), 2);
        assert_eq!(events[0]["data"]["history"].as_array().unwrap().len(), 3);
        let updates: Vec<_> = events.iter().filter(|event| event["type"] == "check_updated").collect();
        assert_eq!(updates.len(), 2, "{events:?}");
        assert_eq!(updates[0]["data"]["check_number"], 42);
        assert_eq!(updates[0]["data"]["check"]["fix_pending"], false);
        assert_eq!(updates[0]["data"]["check"]["fix"].is_object(), proposal_available);
        assert_eq!(updates[1]["data"]["check_number"], 43);
        assert_eq!(
            updates[1]["data"]["check"]["presentation"]["details"][0]["text"],
            "Explanation delivered after the verdict."
        );
        assert_eq!(events.last().unwrap()["type"], "closed");
        assert_eq!(events.last().unwrap()["data"]["detached"], false);
        assert!(
            fixture
                .service
                .seen
                .lock()
                .unwrap()
                .iter()
                .all(|path| path.starts_with("GET "))
        );
        assert_eq!(fixture.source(), SOURCE);
    }
}

#[test]
fn json_status_watch_reports_settled_or_empty_runs_without_opening_a_stream() {
    for (operational_error, empty) in [(false, false), (true, false), (false, true)] {
        let fixture = Fixture::configured(operational_error, false, |routes| {
            if empty {
                let submission = routes
                    .get_mut(&format!("GET /v1/super/workspaces/{WORKSPACE}/check-submissions/{RUN}"))
                    .unwrap();
                submission["checks"] = json!([]);
                submission["submission"]["check_numbers"] = json!([]);
            }
        });
        let output = fixture.run(&["status", "--run", RUN, "--watch", "--json"]);
        assert!(output.status.success(), "{output:?}");
        let events: Vec<Value> = String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["type"], "results");
        assert_eq!(events[0]["data"]["finished"], true);
        assert_eq!(events[0]["data"]["rows"].as_array().unwrap().is_empty(), empty);
        assert_eq!(events[1]["type"], "closed");
        assert_eq!(
            events[1]["data"],
            json!({"target": format!("--run {RUN}"), "detached": false})
        );
        assert!(
            !fixture
                .service
                .seen
                .lock()
                .unwrap()
                .iter()
                .any(|path| path.contains("/check-events"))
        );
    }
}

#[cfg(unix)]
#[test]
fn json_observation_publishes_initial_results_and_interrupts_without_canceling() {
    use nix::{
        sys::signal::{Signal, kill},
        unistd::Pid,
    };
    use std::{process::Stdio, sync::mpsc};
    for args in [
        vec!["check", "--json", "--stream"],
        vec!["status", "--run", RUN, "--watch", "--json"],
    ] {
        let watching = args.contains(&"--watch");
        let fixture = Fixture::configured(false, true, |routes| {
            routes.insert(
                format!("GET /v1/super/workspaces/{WORKSPACE}/check-events"),
                json!({"_sse": [": keepalive\n\n"]}),
            );
        });
        let mut child = fixture
            .command(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (sender, receiver) = mpsc::channel();
        let reader = thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                if sender.send(line.unwrap()).is_err() {
                    break;
                }
            }
        });
        let admission = receiver.recv_timeout(Duration::from_secs(5));
        let alive = child.try_wait().unwrap().is_none();
        // Always clean up the test process even if the assertion below is going to fail.
        let _ = kill(Pid::from_raw(i32::try_from(child.id()).unwrap()), Signal::SIGINT);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while child.try_wait().unwrap().is_none() && std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        if child.try_wait().unwrap().is_none() {
            child.kill().unwrap();
        }
        let status = child.wait().unwrap();
        reader.join().unwrap();
        let admission: Value = serde_json::from_str(&admission.expect("no live admission snapshot")).unwrap();
        assert!(alive);
        assert_eq!(admission["type"], "results");
        assert_eq!(admission["data"]["finished"], false);
        assert!(status.success(), "Ctrl-C detaches without canceling: {status:?}");
        let final_event: Value =
            serde_json::from_str(&receiver.try_iter().last().expect("missing final partial snapshot")).unwrap();
        if watching {
            assert_eq!(final_event["type"], "closed");
            assert_eq!(final_event["data"]["detached"], true);
            assert_eq!(final_event["data"]["target"], format!("--run {RUN}"));
        } else {
            assert_eq!(final_event["type"], "results");
            assert_eq!(final_event["data"]["finished"], false);
            assert_eq!(final_event["data"]["run"], admission["data"]["run"]);
        }
        assert!(
            !fixture
                .service
                .seen
                .lock()
                .unwrap()
                .iter()
                .any(|path| path.contains("/cancel"))
        );
    }
}

#[test]
fn pending_fix_hint_uses_existing_server_state_and_preserves_json() {
    for pending in [false, true] {
        let fixture = Fixture::configured(false, false, |routes| {
            let key = format!("GET /v1/super/workspaces/{WORKSPACE}/checks/{CHECK}");
            let check = routes.get_mut(&key).unwrap();
            check["fix_pending"] = json!(pending);
            check["updates_pending"] = json!(true);
            check["fix"] = Value::Null;
            let check = check.clone();
            routes
                .get_mut(&format!("POST /v1/super/workspaces/{WORKSPACE}/check-submissions"))
                .unwrap()["checks"] = json!([check]);
        });
        for args in [
            vec!["status", "--check", CHECK],
            vec!["check"],
            vec!["check", "--detach"],
        ] {
            let output = fixture.run(&args);
            assert_ne!(output.status.code(), Some(2), "{output:?}");
            let text = String::from_utf8_lossy(&output.stdout);
            assert_eq!(text.contains("A fix proposal may still arrive."), pending, "{text}");
            assert!(!text.contains("being prepared"));
        }
        let output = fixture.run(&["status", "--check", CHECK, "--json"]);
        assert!(output.status.success(), "{output:?}");
        let data: Value = serde_json::from_slice(&output.stdout).unwrap();
        let check = &data["data"]["rows"][0]["current"];
        assert_eq!(check["fix_pending"], pending);
        assert!(check.get("fix_preparing_until").is_none());
    }
}

#[test]
fn details_flag_expands_human_results_without_changing_json_or_fetching_history() {
    let fixture = Fixture::configured(false, false, |routes| {
        let check = routes
            .get_mut(&format!("GET /v1/super/workspaces/{WORKSPACE}/checks/{CHECK}"))
            .unwrap();
        check["presentation"]["details"] = json!([
            {"text":"", "emphasized":false},
            {"text":"Conclusion", "emphasized":true},
            {"text":"  The input exposes a problem.", "emphasized":false},
            {"text":"", "emphasized":false},
            {"text":"Problem 1", "emphasized":true},
            {"text":"  Location: test.py:4", "emphasized":false},
            {"text":"", "emphasized":false},
            {"text":"Counterexample", "emphasized":true},
            {"text":"  witness   = value = -42", "emphasized":false},
            {"text":"  expected  = 6", "emphasized":false},
            {"text":"  observed  = 0", "emphasized":false},
            {"text":"  reproduce = call the function with -42", "emphasized":false}
        ]);
    });
    let compact = fixture.run(&["status", "--check", CHECK]);
    let details = fixture.run(&["status", "--check", CHECK, "--details"]);
    assert!(compact.status.success() && details.status.success());
    let compact = String::from_utf8_lossy(&compact.stdout);
    let details = String::from_utf8_lossy(&details.stdout);
    assert!(compact.contains("The input exposes a problem.") && compact.contains("value = -42"));
    assert!(compact.contains("Counterexample") && !compact.contains("Problem 1"));
    assert!(!compact.contains("reproduce") && !compact.contains("Conclusion"));
    assert!(
        details.contains("The input exposes a problem.") && details.contains("reproduce = call the function with -42")
    );
    assert!(!details.contains("Conclusion"));
    let ordinary_json = fixture.run(&["status", "--check", CHECK, "--json"]);
    let detailed_json = fixture.run(&["status", "--check", CHECK, "--details", "--json"]);
    let ordinary: Value = serde_json::from_slice(&ordinary_json.stdout).unwrap();
    let detailed: Value = serde_json::from_slice(&detailed_json.stdout).unwrap();
    assert_eq!(ordinary, detailed);
    assert!(
        ordinary["data"]["rows"][0]["current"]["presentation"]["details"]
            .as_array()
            .unwrap()
            .iter()
            .any(|line| line["text"].as_str().unwrap().contains("reproduce"))
    );
    assert!(
        fixture
            .service
            .seen
            .lock()
            .unwrap()
            .iter()
            .all(|request| !request.contains("history"))
    );
    assert_eq!(fixture.source(), SOURCE);
}

#[test]
fn pass_explanations_are_opt_in_for_check_detach_and_status_but_always_in_json() {
    let fixture = Fixture::configured(false, false, |routes| {
        let check = routes
            .get_mut(&format!("GET /v1/super/workspaces/{WORKSPACE}/checks/{CHECK}"))
            .unwrap();
        check["result"]["outcome"] = json!("pass");
        check["problematic"] = json!(false);
        check["fix"] = Value::Null;
        check["presentation"]["details"] = json!([
            {"text":"", "emphasized":false},
            {"text":"Conclusion", "emphasized":true},
            {"text":"  Deduplication preserves unique items and removes repeats.", "emphasized":false},
            {"text":"  The function tracks previously seen integers.", "emphasized":false}
        ]);
        let passed = check.clone();
        for response in routes.values_mut() {
            if let Some(checks) = response.get_mut("checks").and_then(Value::as_array_mut) {
                checks[0] = passed.clone();
            }
        }
    });
    for mut args in [
        vec!["check"],
        vec!["check", "--detach"],
        vec!["status", "--check", CHECK],
    ] {
        let compact = fixture.run(&args);
        assert!(compact.status.success());
        let compact = String::from_utf8_lossy(&compact.stdout);
        assert!(compact.contains("✓ pass · law"));
        assert!(!compact.contains("Deduplication") && !compact.contains("Conclusion"));
        assert!(!compact.contains("Details:"));
        args.push("--details");
        let detailed = fixture.run(&args);
        assert!(detailed.status.success());
        let detailed = String::from_utf8_lossy(&detailed.stdout);
        assert!(
            detailed.contains("Deduplication") && detailed.contains("The function tracks previously seen integers.")
        );
        assert!(!detailed.contains("Conclusion"));
    }
    let ordinary = fixture.run(&["status", "--check", CHECK, "--json"]);
    let detailed = fixture.run(&["status", "--check", CHECK, "--details", "--json"]);
    assert!(ordinary.status.success() && detailed.status.success());
    let ordinary: Value = serde_json::from_slice(&ordinary.stdout).unwrap();
    let detailed: Value = serde_json::from_slice(&detailed.stdout).unwrap();
    assert_eq!(ordinary, detailed);
    assert_eq!(
        ordinary["data"]["rows"][0]["current"]["presentation"]["details"][2]["text"],
        "  Deduplication preserves unique items and removes repeats."
    );
}

#[test]
fn detached_receipts_preserve_commands_and_cached_results_keep_acceptance_exit_code() {
    let live = Fixture::with_active_check(false, true);
    let receipt = live.run(&["check", "--detach", "--details"]);
    assert!(receipt.status.success());
    let receipt = String::from_utf8_lossy(&receipt.stdout);
    assert_eq!(receipt.matches("Accepted").count(), 1);
    assert!(receipt.contains(&format!("Watch: super status --check {CHECK} --watch --details\n")));
    for id in [RUN, WORKSPACE, REVISION] {
        assert!(!receipt.contains(id));
    }
    assert!(!receipt.contains("Detached"));
    let json_output = live.run(&["check", "--detach", "--json"]);
    assert!(json_output.status.success());
    let json: Value = serde_json::from_slice(&json_output.stdout).unwrap();
    assert_eq!(json["data"]["run"]["id"], RUN);
    assert_eq!(
        json["data"]["resume_command"],
        format!("super status --run {RUN} --watch")
    );
    let reopened = live.run(&["status", "--check", CHECK]);
    assert!(reopened.status.success());
    assert!(String::from_utf8_lossy(&reopened.stdout).contains("#42"));
    let completed = Fixture::new(false);
    let result = completed.run(&["check", "--detach"]);
    assert!(result.status.success());
    let text = String::from_utf8_lossy(&result.stdout);
    assert!(text.contains("× fail"));
    assert!(!text.contains("Accepted") && !text.contains("continue remotely"));
}

fn check_event(check: &Value) -> String {
    format!(
        "event: check\ndata: {}\n\n",
        json!({"sequence":check["event_sequence"],"check":check})
    )
}

#[test]
fn cancel_waits_for_confirmation_and_matches_the_watching_terminal() {
    let fixture = Fixture::configured(false, true, |routes| {
        let base = format!("/v1/super/workspaces/{WORKSPACE}");
        let mut active = routes[&format!("GET {base}/checks/{CHECK}")].clone();
        active["fix"] = Value::Null;
        active["presentation"]["details"] = json!([]);
        let mut canceled = active.clone();
        canceled["terminal"] = json!(true);
        canceled["operational_error"] = json!("canceled");
        canceled["event_sequence"] = json!(2);
        // Ignore both an older cancellation snapshot and another check's event.
        let mut stale = canceled.clone();
        stale["event_sequence"] = json!(0);
        let mut unrelated = canceled.clone();
        unrelated["number"] = json!(99);
        routes.insert(format!("POST {base}/checks/{CHECK}/cancel"), active.clone());
        routes.insert(format!("GET {base}/check-events"), json!({"_sse":[
            format!("{}{}{}", check_event(&stale), check_event(&unrelated), check_event(&active)), check_event(&canceled)
        ]}));
    });
    let output = fixture.run(&["cancel", "test.py::law"]);
    assert!(output.status.success(), "{output:?}");
    let text = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        text,
        "  ○ canceled · law · #42\n\n  To check again: super check test.py::law\n"
    );
    assert!(!text.contains("checking"));
    let watcher = fixture.run(&["check", "test.py::law"]);
    assert_eq!(watcher.status.code(), Some(2));
    assert_eq!(watcher.stdout, output.stdout);
}

#[test]
fn cancel_of_stopped_checks_is_an_explicit_no_op_and_never_posts() {
    for canceled in [false, true] {
        let fixture = Fixture::configured(false, false, |routes| {
            if canceled {
                routes
                    .get_mut(&format!("GET /v1/super/workspaces/{WORKSPACE}/checks/{CHECK}"))
                    .unwrap()["operational_error"] = json!("canceled");
            }
        });
        let output = fixture.run(&["cancel", "--check", CHECK]);
        assert!(output.status.success());
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(
            text.contains(if canceled {
                "already canceled"
            } else {
                "already finished"
            }),
            "{text}"
        );
        assert!(text.contains("Nothing to cancel"));
        assert!(!text.contains("Witness"));
        let output = fixture.run(&["cancel", "--check", CHECK, "--json"]);
        assert!(output.status.success());
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(
            value["data"]["cancellation"][0]["outcome"],
            if canceled {
                "already_canceled"
            } else {
                "already_finished"
            }
        );
        assert_eq!(value["data"]["rows"][0]["current"]["result"]["outcome"], "fail");
        assert!(
            fixture
                .service
                .seen
                .lock()
                .unwrap()
                .iter()
                .all(|request| request.starts_with("GET "))
        );
    }
}

#[test]
fn cancel_includes_background_work_and_preserves_the_reported_result() {
    for json_output in [false, true] {
        let fixture = Fixture::configured(false, false, |routes| {
            let base = format!("/v1/super/workspaces/{WORKSPACE}");
            let check = routes.get_mut(&format!("GET {base}/checks/{CHECK}")).unwrap();
            check["updates_pending"] = json!(true);
            check["fix_pending"] = json!(true);
            check["fix"] = Value::Null;
            let check = check.clone();
            routes.insert(format!("POST {base}/checks/{CHECK}/cancel"), check.clone());
            let mut stopped = check;
            stopped["updates_pending"] = json!(false);
            stopped["fix_pending"] = json!(false);
            stopped["operational_error"] = json!("canceled");
            stopped["event_sequence"] = json!(3);
            routes.insert(
                format!("GET {base}/check-events"),
                json!({"_sse":[check_event(&stopped)]}),
            );
        });
        let args = if json_output {
            vec!["cancel", "--check", CHECK, "--json"]
        } else {
            vec!["cancel", "--check", CHECK]
        };
        let output = fixture.run(&args);
        assert!(output.status.success(), "{output:?}");
        if json_output {
            let value: Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(value["data"]["cancellation"][0]["outcome"], "background_stopped");
            let check = &value["data"]["rows"][0]["current"];
            assert_eq!(check["result"]["outcome"], "fail");
            assert_eq!(
                check["presentation"]["details"][0]["text"],
                "Witness: -42\nExpected: 6\nObserved: 0"
            );
        } else {
            let text = String::from_utf8_lossy(&output.stdout);
            assert!(text.contains("Background work stopped · law · #42"), "{text}");
            assert!(text.contains("Result retained · fail"), "{text}");
            assert!(!text.contains("○ canceled"), "{text}");
        }
        assert!(
            fixture
                .service
                .seen
                .lock()
                .unwrap()
                .iter()
                .any(|request| request.ends_with("/cancel"))
        );
    }
}

#[test]
fn cancel_reports_natural_completion_without_claiming_cancellation() {
    let fixture = Fixture::configured(false, true, |routes| {
        let base = format!("/v1/super/workspaces/{WORKSPACE}");
        let check = routes.get_mut(&format!("POST {base}/checks/{CHECK}/cancel")).unwrap();
        check["operational_error"] = Value::Null;
        check["result"] = json!({"outcome":"pass","assurance":"uncertified"});
        check["event_sequence"] = json!(2);
    });
    let output = fixture.run(&["cancel", "--check", CHECK]);
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("Finished before cancellation"), "{text}");
    assert!(text.contains("✓ pass · law"), "{text}");
    assert!(!text.contains("○ canceled"), "{text}");
}

#[test]
fn cancel_confirmation_is_bounded_and_does_not_invent_a_canceled_state() {
    let fixture = Fixture::configured(false, true, |routes| {
        let base = format!("/v1/super/workspaces/{WORKSPACE}");
        let active = routes[&format!("GET {base}/checks/{CHECK}")].clone();
        routes.insert(format!("POST {base}/checks/{CHECK}/cancel"), active.clone());
        routes.insert(
            format!("GET {base}/check-events"),
            json!({"_sse":[check_event(&active)]}),
        );
    });
    let output = fixture.run(&["cancel", "--check", CHECK, "--json"]);
    assert!(output.status.success(), "{output:?}");
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["data"]["cancellation"][0]["outcome"], "requested");
    assert_eq!(value["data"]["rows"][0]["current"]["terminal"], false);
    assert!(value["data"]["rows"][0]["current"]["operational_error"].is_null());
}

#[test]
fn cancel_partial_failure_reports_each_target_and_leaves_completed_checks_alone() {
    let fixture = Fixture::configured(false, true, |routes| {
        let base = format!("/v1/super/workspaces/{WORKSPACE}");
        let mut second = routes[&format!("GET {base}/checks/{CHECK}")].clone();
        second["number"] = json!(43);
        second["supertest"]["name"] = json!("other_law");
        let mut third = second.clone();
        third["number"] = json!(44);
        third["supertest"]["name"] = json!("finished_law");
        third["terminal"] = json!(true);
        let run = routes.get_mut(&format!("GET {base}/check-submissions/latest")).unwrap();
        run["checks"].as_array_mut().unwrap().extend([second.clone(), third]);
        run["submission"]["check_numbers"] = json!([42, 43, 44]);
        let mut checks = run["checks"].as_array().unwrap().clone();
        checks.reverse();
        routes.insert(
            format!("GET {base}/checks/history"),
            json!({"checks": checks, "next_before": null}),
        );
        second["terminal"] = json!(true);
        second["operational_error"] = json!("canceled");
        routes.insert(format!("POST {base}/checks/43/cancel"), second);
        routes.insert(format!("POST {base}/checks/{CHECK}/cancel"), json!({"_responses":[{"status":503,"body":{"error":{"code":"unavailable","message":"Service unavailable"}}}]}));
    });
    let output = fixture.run(&["cancel", ".", "--json"]);
    assert_eq!(output.status.code(), Some(2));
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    let report = value["data"]["cancellation"].as_array().unwrap();
    let find = |number| report.iter().find(|entry| entry["check_number"] == number).unwrap();
    assert_eq!(find(42)["outcome"], "unconfirmed");
    assert!(find(42)["error"].as_str().unwrap().contains("Service unavailable"));
    assert_eq!(find(43)["outcome"], "canceled");
    assert_eq!(find(44)["outcome"], "already_finished");
    assert!(
        !fixture
            .service
            .seen
            .lock()
            .unwrap()
            .iter()
            .any(|request| request.ends_with("/44/cancel"))
    );
}

#[cfg(unix)]
#[test]
fn interrupting_cancel_keeps_acknowledged_requests_in_the_report() {
    use nix::{
        sys::signal::{Signal, kill},
        unistd::Pid,
    };
    use std::process::Stdio;
    let fixture = Fixture::configured(false, true, |routes| {
        let base = format!("/v1/super/workspaces/{WORKSPACE}");
        let active = routes[&format!("GET {base}/checks/{CHECK}")].clone();
        routes.insert(format!("POST {base}/checks/{CHECK}/cancel"), active);
        routes.insert(format!("GET {base}/check-events"), json!({"_sse":[": keepalive\n\n"]}));
    });
    let mut child = fixture
        .command(&["cancel", "--check", CHECK, "--json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut observing = false;
    while std::time::Instant::now() < deadline {
        observing = fixture
            .service
            .seen
            .lock()
            .unwrap()
            .iter()
            .any(|request| request.contains("/check-events"));
        if observing {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    let _ = kill(Pid::from_raw(i32::try_from(child.id()).unwrap()), Signal::SIGINT);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while child.try_wait().unwrap().is_none() && std::time::Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    if child.try_wait().unwrap().is_none() {
        child.kill().unwrap();
    }
    let output = child.wait_with_output().unwrap();
    assert!(observing, "cancellation never reached confirmation");
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["data"]["interrupted"], true);
    assert_eq!(value["data"]["cancellation"][0]["outcome"], "requested");
    assert_eq!(value["data"]["rows"][0]["current"]["terminal"], false);
}
