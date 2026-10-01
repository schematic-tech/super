use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Write},
    net::TcpListener,
    process::{Command, Output},
    thread,
};
use super_types::credential::{ApiBase, RoutedKey};
use tempfile::TempDir;
use uuid::Uuid;

fn login(profile: &TempDir, key: &str, extra: &[&str]) -> Output {
    std::fs::create_dir_all(profile.path().join("updates")).unwrap();
    std::fs::write(profile.path().join("updates/releases.json"), b"{}").unwrap();
    Command::new(env!("CARGO_BIN_EXE_super"))
        .args(extra)
        .arg("login")
        .env("SUPER_CONFIG_DIR", profile.path())
        .env("SUPER_NO_DAEMON", "1")
        .env("SUPER_ACCESS_TOKEN", key)
        .env_remove("SUPER_API_URL")
        .output()
        .unwrap()
}

fn endpoint_with_prefix(count: usize, prefix: &str) -> (String, String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let key = RoutedKey {
        id: Uuid::new_v4(),
        api_base: ApiBase::parse(&base).unwrap(),
    }
    .encode(&[0x23; 32])
    .replacen("super_v2", prefix, 1);
    let expected = key.clone();
    let server = thread::spawn(move || {
        for _ in 0..count {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut request = String::new();
            let mut reader = BufReader::new(&socket);
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                    break;
                }
                request.push_str(&line);
            }
            assert!(request.starts_with("GET /v1/super/auth/whoami "));
            assert!(request.lines().any(|line| {
                line.split_once(':').is_some_and(|(name, value)| {
                    name.eq_ignore_ascii_case("authorization") && value.trim() == format!("Bearer {expected}")
                })
            }));
            let body = r#"{"email":"test@example.test"}"#;
            write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
        }
    });
    (base, key, server)
}

fn endpoint(count: usize) -> (String, String, thread::JoinHandle<()>) {
    endpoint_with_prefix(count, "super_v2")
}

#[test]
fn legacy_routed_key_keeps_its_origin_and_exact_bearer_value() {
    let profile = TempDir::new().unwrap();
    let (_, key, server) = endpoint_with_prefix(1, "pup_v2");
    let output = login(&profile, &key, &[]);
    assert!(output.status.success(), "{output:?}");
    server.join().unwrap();
    assert_eq!(std::fs::read_to_string(profile.path().join("api-key")).unwrap(), key);
}

#[test]
fn login_routes_from_key_and_restores_links_when_switching_back() {
    let profile = TempDir::new().unwrap();
    let (first, first_key, first_server) = endpoint(2);
    let output = login(&profile, &first_key, &[]);
    assert!(output.status.success(), "{output:?}");
    let path = profile.path().join("state.json");
    let mut state: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(state["api_url"], first);
    let repository = json!({"root":"/work/repo","common_git_dir":"/work/repo/.git","association_id":"preserved","workspace_id":Uuid::new_v4(),"name":"repo","last_seen_oid":null,"temporary_commits":{},"source_hashes":{},"revisions":{},"pending_submissions":[]});
    state["repositories"] = json!([repository]);
    std::fs::write(&path, serde_json::to_vec(&state).unwrap()).unwrap();
    let (second, key, server) = endpoint(1);
    let output = login(&profile, &key, &[]);
    assert!(output.status.success(), "{output:?}");
    server.join().unwrap();
    let mut state: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(state["api_url"], second);
    assert_eq!(state["repositories"], json!([]));
    assert_eq!(
        state["other_api_repositories"][&first]["repositories"],
        json!([repository])
    );

    let output = login(&profile, &first_key, &[]);
    assert!(output.status.success(), "{output:?}");
    first_server.join().unwrap();
    state = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(state["repositories"], json!([repository]));
    assert_eq!(
        std::fs::read_to_string(profile.path().join("api-key")).unwrap(),
        first_key
    );
}

#[test]
fn conflicting_override_and_malformed_keys_fail_before_network_or_storage() {
    let profile = TempDir::new().unwrap();
    let key = RoutedKey {
        id: Uuid::new_v4(),
        api_base: ApiBase::parse("https://api.staging.schematic.tech").unwrap(),
    }
    .encode(&[0xab; 32]);
    let output = login(&profile, &key, &["--api-url", "https://api.schematic.tech"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("this key selects a different API"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains(&key));
    let output = login(&profile, "pup_v2.secret-invalid", &[]);
    assert!(!output.status.success());
    assert!(!profile.path().join("api-key").exists());
}
