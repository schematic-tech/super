use std::{fs, process::Command};

fn command(profile: &std::path::Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_super"));
    command
        .env("SUPER_CONFIG_DIR", profile)
        .env("NO_COLOR", "1")
        .env_remove("SUPER_NO_UPDATE_CHECK");
    command
}

fn cache_alert(profile: &std::path::Path, version: &str, alert: &str) {
    let cache = profile.join("updates");
    fs::create_dir_all(&cache).unwrap();
    fs::write(
        cache.join("releases.json"),
        serde_json::json!({"super-tool": {"version": version, "alert": alert}}).to_string(),
    )
    .unwrap();
}

#[test]
fn alerts_appear_once_per_invocation_without_changing_stdout_or_exit_status() {
    let profile = tempfile::tempdir().unwrap();
    let message = "Installation is changing.\nSee https://example.test/install for instructions.";
    let expected = format!("\n  Super CLI notice\n  {}\n\n", message.replace('\n', "\n  "));
    // Even an up-to-date installation must show the alert, on every invocation.
    cache_alert(profile.path(), env!("CARGO_PKG_VERSION"), message);
    for args in [
        vec!["--version"],
        vec!["--help"],
        vec!["logout", "--json"],
        vec!["--unknown", "--json"],
    ] {
        for _ in 0..2 {
            let output = command(profile.path())
                .env("SUPER_NO_UPDATE_CHECK", "1")
                .args(&args)
                .output()
                .unwrap();
            assert_eq!(String::from_utf8(output.stderr).unwrap(), expected);
            match args[0] {
                "--version" => assert_eq!(
                    output.stdout,
                    concat!("super ", env!("CARGO_PKG_VERSION"), "\n").as_bytes()
                ),
                "--help" => assert!(String::from_utf8(output.stdout).unwrap().contains("Usage:")),
                _ => {
                    let response: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
                    assert_eq!(response["type"], if args[0] == "logout" { "success" } else { "error" });
                }
            }
            assert_eq!(output.status.code(), Some(if args[0] == "--unknown" { 2 } else { 0 }));
        }
    }
    let output = command(profile.path()).arg("--unknown").output().unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.starts_with(&expected));
    assert_eq!(stderr.matches("Super CLI notice").count(), 1);
    assert!(stderr.contains("unexpected argument"));
}

#[test]
fn active_alert_replaces_the_generic_update_hint_and_cleans_terminal_controls() {
    let profile = tempfile::tempdir().unwrap();
    cache_alert(
        profile.path(),
        "999.0.0",
        "\u{1b}[31mInstall instructions\u{1b}[0m\u{7}\r\nhttps://example.test/install",
    );
    let output = command(profile.path()).arg("--version").output().unwrap();
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "\n  Super CLI notice\n  Install instructions\n  https://example.test/install\n\n"
    );

    for alert in ["", " \t\n ", "\u{1b}[31m\u{7}\u{1b}[0m"] {
        cache_alert(profile.path(), "999.0.0", alert);
        let output = command(profile.path()).arg("--version").output().unwrap();
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(!stderr.contains("Super CLI notice"));
        assert!(stderr.contains("Super CLI 999.0.0 is available"));
    }
}

#[test]
fn detached_refresh_does_not_display_alerts_or_spawn_another_worker() {
    let profile = tempfile::tempdir().unwrap();
    cache_alert(profile.path(), "999.0.0", "Installation is changing.");
    let cache = profile.path().join("updates");
    let output = command(profile.path())
        .arg("refresh-releases")
        .arg(&cache)
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
    assert!(!cache.join("last-attempt").exists());
}

#[test]
fn cached_notice_is_styled_as_text_on_stderr_and_keeps_json_and_version_output_clean() {
    let profile = tempfile::tempdir().unwrap();
    let cache = profile.path().join("updates");
    fs::create_dir(&cache).unwrap();
    fs::write(
        cache.join("releases.json"),
        br#"{"super-tool":{"version":"999.0.0"},"another-tool":{"version":"1.0.0"}}"#,
    )
    .unwrap();
    let output = command(profile.path()).arg("--version").output().unwrap();
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        concat!("super ", env!("CARGO_PKG_VERSION"), "\n")
    );
    let notice = String::from_utf8(output.stderr).unwrap();
    assert!(notice.contains("Super CLI 999.0.0 is available"));
    assert!(notice.contains(concat!("(installed: ", env!("CARGO_PKG_VERSION"), ")")));
    assert!(notice.contains("curl -fsSL https://get.schematic.tech/cli.sh | sh\n"));
    assert!(!notice.contains('\x1b'));

    let output = command(profile.path()).args(["logout", "--json"]).output().unwrap();
    assert!(output.status.success());
    serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap();
    assert!(output.stderr.is_empty());

    let output = command(profile.path())
        .env("SUPER_NO_UPDATE_CHECK", "1")
        .arg("--version")
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    assert!(!cache.join("last-attempt").exists());
}

#[test]
fn missing_or_corrupt_cache_is_silent_and_a_failed_refresh_does_not_spawn_again_this_hour() {
    let profile = tempfile::tempdir().unwrap();
    let cache = profile.path().join("updates");
    fs::create_dir(&cache).unwrap();
    fs::write(cache.join("last-attempt"), b"").unwrap();
    for contents in [
        None,
        Some("<error>offline</error>"),
        Some(r#"{"super-tool":{"version":"0.0.1"}}"#),
        Some(r#"{"super-tool":"999.0.0"}"#),
    ] {
        if let Some(contents) = contents {
            fs::write(cache.join("releases.json"), contents).unwrap();
        }
        let output = command(profile.path()).arg("--version").output().unwrap();
        assert!(output.status.success());
        assert!(output.stderr.is_empty());
    }
    assert!(!cache.join("refresh.lock").exists());
}

#[test]
fn foreground_exits_while_the_detached_worker_is_waiting_on_the_network() {
    use fs2::FileExt;
    use std::{
        fs::OpenOptions,
        io::{Read, Write},
        net::TcpListener,
        thread,
        time::{Duration, Instant},
    };

    let profile = tempfile::tempdir().unwrap();
    // Stall a local HTTPS proxy. This exercises the real child and its production
    // URL without contacting the internet or making the URL configurable for users.
    let proxy = TcpListener::bind("127.0.0.1:0").unwrap();
    proxy.set_nonblocking(true).unwrap();
    let proxy_url = format!("http://{}", proxy.local_addr().unwrap());
    let start = Instant::now();
    let output = command(profile.path())
        .env("SUPER_NO_UPDATE_CHECK", "1")
        .env("HTTPS_PROXY", &proxy_url)
        .env("https_proxy", &proxy_url)
        .env("ALL_PROXY", &proxy_url)
        .env("all_proxy", &proxy_url)
        .env("NO_PROXY", "")
        .env("no_proxy", "")
        .arg("--version")
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    assert!(start.elapsed() < Duration::from_secs(5));
    let deadline = Instant::now() + Duration::from_secs(5);
    let (mut socket, _) = loop {
        match proxy.accept() {
            Ok(connection) => break connection,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "detached worker never connected");
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("{error}"),
        }
    };
    socket.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let mut request = Vec::new();
    while !request.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        socket.read_exact(&mut byte).unwrap();
        request.extend_from_slice(&byte);
    }
    assert!(request.starts_with(b"CONNECT get.schematic.tech:443 "));
    let cache = profile.path().join("updates");
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .open(cache.join("refresh.lock"))
        .unwrap();
    assert!(
        lock.try_lock_exclusive().is_err(),
        "worker should still be fetching after the foreground exits"
    );
    let output = command(profile.path()).arg("--version").output().unwrap();
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    assert!(
        proxy.accept().is_err(),
        "another startup must not create a second fetch"
    );

    socket
        .write_all(b"HTTP/1.1 503 Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
        .unwrap();
    drop(socket);
    let deadline = Instant::now() + Duration::from_secs(5);
    while lock.try_lock_exclusive().is_err() {
        assert!(Instant::now() < deadline, "failed worker did not release its lock");
        thread::sleep(Duration::from_millis(10));
    }
    assert!(!cache.join("releases.json").exists());
}
