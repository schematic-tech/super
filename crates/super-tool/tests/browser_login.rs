use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};
use super_types::credential::{ApiBase, RoutedKey};
use tempfile::TempDir;
use url::Url;
use uuid::Uuid;

fn command(profile: &TempDir, api: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_super"));
    command
        .args(["--api-url", api])
        .env("SUPER_CONFIG_DIR", profile.path())
        .env("SUPER_NO_DAEMON", "1")
        .env("SUPER_NO_UPDATE_CHECK", "1")
        .env_remove("SUPER_API_URL")
        .env_remove("SUPER_ACCESS_TOKEN");
    command
}

fn read_request(socket: &TcpStream) -> (String, String, Value) {
    socket.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut reader = BufReader::new(socket);
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let path = line.split_whitespace().nth(1).unwrap().to_owned();
    let mut authorization = String::new();
    let mut request_id = None;
    let mut length = 0;
    loop {
        line.clear();
        reader.read_line(&mut line).unwrap();
        if line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().unwrap();
            }
            if name.eq_ignore_ascii_case("idempotency-key") {
                request_id = Some(value.trim().to_owned());
            }
            if name.eq_ignore_ascii_case("authorization") {
                value.trim().clone_into(&mut authorization);
            }
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    let mut body = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&body).unwrap_or_else(|_| {
            Value::Object(
                url::form_urlencoded::parse(&body)
                    .map(|(key, value)| (key.into_owned(), Value::String(value.into_owned())))
                    .collect(),
            )
        })
    };
    if let Some(id) = request_id {
        body["_request_id"] = json!(id);
    }
    (path, authorization, body)
}

fn respond(socket: &mut TcpStream, status: &str, body: &Value) {
    let body = body.to_string();
    write!(
        socket,
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
}

fn accept(listener: &TcpListener) -> TcpStream {
    listener.set_nonblocking(true).unwrap();
    let until = Instant::now() + Duration::from_secs(20);
    loop {
        match listener.accept() {
            Ok((socket, _)) => {
                socket.set_nonblocking(false).unwrap();
                return socket;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < until => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("missing API request: {error}"),
        }
    }
}

fn callback(start: &Value, denied: bool) -> thread::JoinHandle<String> {
    let mut url = Url::parse(start["redirect_uri"].as_str().unwrap()).unwrap();
    url.query_pairs_mut()
        .append_pair("state", start["state"].as_str().unwrap());
    if denied {
        url.query_pairs_mut().append_pair("error", "access_denied");
    } else {
        url.query_pairs_mut().append_pair("code", "one-time-code");
    }
    thread::spawn(move || {
        let host = format!("127.0.0.1:{}", url.port().unwrap());
        let mut socket = TcpStream::connect(&host).unwrap();
        socket.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        write!(
            socket,
            "GET {}?{} HTTP/1.1\r\nHost: {host}\r\n\r\n",
            url.path(),
            url.query().unwrap()
        )
        .unwrap();
        let mut response = String::new();
        socket.read_to_string(&mut response).unwrap();
        response
    })
}

fn login_server(listener: &TcpListener, key: &str, denied: bool) {
    let mut socket = accept(listener);
    let (path, authorization, start) = read_request(&socket);
    assert_eq!(path, "/v1/super/auth/cli/oauth/start");
    assert!(authorization.is_empty());
    respond(
        &mut socket,
        "200 OK",
        &json!({"authorization_url":"https://platform.example.test/cli/connect?request=example"}),
    );
    drop(socket);
    let browser = callback(&start, denied);
    if !denied {
        let mut socket = accept(listener);
        let (path, authorization, exchange) = read_request(&socket);
        assert_eq!(path, "/v1/super/auth/cli/token");
        assert!(authorization.is_empty());
        assert_eq!(exchange["code"], "one-time-code");
        assert_eq!(exchange["redirect_uri"], start["redirect_uri"]);
        assert_eq!(
            URL_SAFE_NO_PAD.encode(Sha256::digest(exchange["code_verifier"].as_str().unwrap().as_bytes())),
            start["code_challenge"]
        );
        respond(
            &mut socket,
            "200 OK",
            &json!({"access_token":key,"refresh_token":format!("refresh-{key}"),"token_type":"Bearer","expires_in":900,"session_id":Uuid::new_v4(),"email":"you@example.com"}),
        );
    }
    let page = browser.join().unwrap();
    let outcome = if denied { "canceled" } else { "connected" };
    assert!(page.starts_with("HTTP/1.1 303 See Other\r\n"));
    assert!(page.contains(&format!(
        "Location: https://platform.example.test/cli/result/?outcome={outcome}\r\n"
    )));
    assert!(!page.contains("code=") && !page.contains("state=") && !page.contains("request="));
    assert!(!page.contains(key));
    assert!(page.contains("Cache-Control: no-store"));
}

fn key(base: &str) -> String {
    RoutedKey {
        id: Uuid::new_v4(),
        api_base: ApiBase::parse(base).unwrap(),
    }
    .encode(&[0x23; 32])
}

#[test]
fn failed_exchange_returns_to_platform_and_preserves_the_saved_login() {
    let profile = TempDir::new().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        login_server(&listener, "existing-access", false);
        let mut socket = accept(&listener);
        let (_, _, start) = read_request(&socket);
        respond(
            &mut socket,
            "200 OK",
            &json!({"authorization_url":"https://platform.example.test/cli/connect?request=private-request"}),
        );
        drop(socket);
        let browser = callback(&start, false);
        let mut socket = accept(&listener);
        let (path, _, _) = read_request(&socket);
        assert_eq!(path, "/v1/super/auth/cli/token");
        respond(
            &mut socket,
            "503 Service Unavailable",
            &json!({"error":"temporarily_unavailable"}),
        );
        let response = browser.join().unwrap();
        assert!(response.contains("Location: https://platform.example.test/cli/result/?outcome=failed\r\n"));
        for private in ["private-request", "one-time-code", "existing-access"] {
            assert!(!response.contains(private));
        }
    });
    assert!(
        command(&profile, &base)
            .args(["login", "--no-browser"])
            .output()
            .unwrap()
            .status
            .success()
    );
    let path = profile.path().join("api-key");
    let saved = std::fs::read(&path).unwrap();
    let output = command(&profile, &base)
        .args(["login", "--no-browser"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(std::fs::read(path).unwrap(), saved);
    server.join().unwrap();
}

#[test]
fn browser_login_saves_private_tokens_replaces_sessions_and_revokes_on_logout() {
    let profile = TempDir::new().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let first = key(&base);
    let second = key(&base);
    let expected = second.clone();
    let server = thread::spawn(move || {
        login_server(&listener, &first, false);
        login_server(&listener, &second, false);
        for token in [&first, &second] {
            let mut socket = accept(&listener);
            let (path, authorization, body) = read_request(&socket);
            assert_eq!(path, "/v1/super/auth/cli/revoke");
            assert!(authorization.is_empty());
            assert_eq!(body["token"], format!("refresh-{token}"));
            respond(&mut socket, "204 No Content", &Value::Null);
        }
    });
    for _ in 0..2 {
        let output = command(&profile, &base)
            .args(["login", "--no-browser"])
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert!(String::from_utf8_lossy(&output.stdout).contains("Logged in as you@example.com"));
        assert!(!String::from_utf8_lossy(&output.stdout).contains(&expected));
        assert!(!String::from_utf8_lossy(&output.stderr).contains(&expected));
    }
    assert_eq!(
        serde_json::from_str::<Value>(&std::fs::read_to_string(profile.path().join("api-key")).unwrap()).unwrap()["access_token"],
        expected
    );
    let state: Value = serde_json::from_slice(&std::fs::read(profile.path().join("state.json")).unwrap()).unwrap();
    assert_eq!(state["credential"], "oauth");
    assert!(state["browser_login_key"].is_null());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(profile.path().join("api-key"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    let output = command(&profile, &base)
        .arg("logout")
        .env("SUPER_ACCESS_TOKEN", "unrelated-environment-key")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(!profile.path().join("api-key").exists());
    server.join().unwrap();
}

#[test]
fn denied_browser_login_preserves_the_existing_credential() {
    let profile = TempDir::new().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let existing = key(&base);
    std::fs::write(profile.path().join("api-key"), &existing).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(profile.path().join("api-key"), std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let server = thread::spawn(move || login_server(&listener, "unused-secret", true));
    let output = command(&profile, &base)
        .args(["login", "--no-browser"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("sign-in canceled in your browser"));
    assert_eq!(
        std::fs::read_to_string(profile.path().join("api-key")).unwrap(),
        existing
    );
    server.join().unwrap();
}

#[test]
fn manual_key_can_be_piped_and_logout_only_removes_it_locally() {
    let profile = TempDir::new().unwrap();
    // Signing in also repairs an unreadable or invalid credential left by an older build.
    std::fs::write(profile.path().join("api-key"), "invalid\ncredential").unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let token = key(&base);
    let expected = token.clone();
    let server = thread::spawn(move || {
        let mut socket = accept(&listener);
        let (path, authorization, _) = read_request(&socket);
        assert_eq!(path, "/v1/super/auth/whoami");
        assert_eq!(authorization, format!("Bearer {expected}"));
        respond(&mut socket, "200 OK", &json!({"email":"you@example.com"}));
    });
    let mut child = command(&profile, &base)
        .args(["login", "--api-key"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    writeln!(child.stdin.take().unwrap(), "{token}").unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{output:?}");
    server.join().unwrap();
    let output = command(&profile, &base).arg("logout").output().unwrap();
    assert!(output.status.success(), "{output:?}");
}

#[test]
fn browser_login_can_replace_an_invalid_saved_cli_credential() {
    let profile = TempDir::new().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let token = key(&base);
    let expected = token.clone();
    std::fs::write(profile.path().join("api-key"), "invalid\ncredential").unwrap();
    std::fs::write(
        profile.path().join("state.json"),
        json!({"schema_version": 1, "browser_login_key": Uuid::new_v4()}).to_string(),
    )
    .unwrap();
    let server = thread::spawn(move || login_server(&listener, &token, false));
    let output = command(&profile, &base)
        .args(["login", "--no-browser"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        serde_json::from_str::<Value>(&std::fs::read_to_string(profile.path().join("api-key")).unwrap()).unwrap()["access_token"],
        expected
    );
    server.join().unwrap();
}

#[test]
fn logout_retains_tokens_on_server_failure_and_clears_an_already_revoked_session() {
    let profile = TempDir::new().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let token = key(&base);
    let expected = token.clone();
    let server = thread::spawn(move || {
        login_server(&listener, &token, false);
        for status in ["503 Service Unavailable", "200 OK"] {
            let mut socket = accept(&listener);
            let (path, authorization, body) = read_request(&socket);
            assert_eq!(path, "/v1/super/auth/cli/revoke");
            assert!(authorization.is_empty());
            assert_eq!(body["token"], format!("refresh-{token}"));
            respond(
                &mut socket,
                status,
                &json!({"error":{"code":"unavailable","message":"Try again."}}),
            );
        }
    });
    let output = command(&profile, &base)
        .args(["login", "--no-browser"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let output = command(&profile, &base).arg("logout").output().unwrap();
    assert!(!output.status.success());
    assert_eq!(
        serde_json::from_str::<Value>(&std::fs::read_to_string(profile.path().join("api-key")).unwrap()).unwrap()["access_token"],
        expected
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("retry `super logout`"));
    let output = command(&profile, &base).arg("logout").output().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(!profile.path().join("api-key").exists());
    server.join().unwrap();
}

#[test]
fn a_lost_refresh_response_retains_a_durable_request_id_and_retries_the_same_grant() {
    lost_refresh_response(false);
}

#[test]
fn a_transient_refresh_failure_is_recovered_without_another_command() {
    lost_refresh_response(true);
}

fn lost_refresh_response(recover_automatically: bool) {
    let (session_sender, session_receiver) = std::sync::mpsc::channel();
    let profile = TempDir::new().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        login_server(&listener, "initial-access", false);
        let socket = accept(&listener);
        let (path, auth, first) = read_request(&socket);
        assert_eq!(path, "/v1/super/auth/cli/token");
        assert!(auth.is_empty());
        assert_eq!(first["grant_type"], "refresh_token");
        assert!(first["_request_id"].is_string());
        drop(socket); // The server committed a rotation, but its response never reached the CLI.
        let mut socket = accept(&listener);
        let (_, _, retry) = read_request(&socket);
        assert_eq!(first, retry);
        if !recover_automatically {
            drop(socket); // The automatic retry is also lost; the next command must reuse the ID.
            socket = accept(&listener);
            let (_, _, retry) = read_request(&socket);
            assert_eq!(first, retry);
        }
        // The existing session ID is passed through a test channel below.
        let id = session_receiver.recv().unwrap();
        respond(
            &mut socket,
            "200 OK",
            &json!({"access_token":"renewed-access", "refresh_token":"renewed-refresh",
            "token_type":"Bearer", "expires_in":900, "session_id":id,"email":"you@example.com"}),
        );
        let mut socket = accept(&listener);
        let (path, authorization, _) = read_request(&socket);
        assert!(path.starts_with("/v1/super/usage"));
        assert_eq!(authorization, "Bearer renewed-access");
        respond(
            &mut socket,
            "503 Service Unavailable",
            &json!({"error":{"code":"unavailable","message":"Test usage unavailable"}}),
        );
    });
    let output = command(&profile, &base)
        .args(["login", "--no-browser"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let path = profile.path().join("api-key");
    let mut saved: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    session_sender
        .send(saved["session_id"].as_str().unwrap().to_owned())
        .unwrap();
    saved["expires_at"] = json!(0);
    std::fs::write(&path, saved.to_string()).unwrap();
    let output = command(&profile, &base).arg("usage").output().unwrap();
    assert!(!output.status.success());
    let output = if recover_automatically {
        output
    } else {
        let pending: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(pending["pending_refresh"].is_string());
        assert_eq!(pending["refresh_token"], "refresh-initial-access");
        command(&profile, &base).arg("usage").output().unwrap()
    };
    assert!(!output.status.success()); // The usage fixture is deliberately unavailable.
    assert!(String::from_utf8_lossy(&output.stderr).contains("Test usage unavailable"));
    let renewed: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(renewed["refresh_token"], "renewed-refresh");
    assert!(renewed["pending_refresh"].is_null());
    server.join().unwrap();
}
