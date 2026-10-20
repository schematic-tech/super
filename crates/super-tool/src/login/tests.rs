use super::*;

#[test]
fn callback_requires_the_exact_host_state_path_and_unambiguous_parameters() {
    let request = |target: &str, host: &str| format!("GET {target} HTTP/1.1\r\nHost: {host}\r\n\r\n");
    let host = "127.0.0.1:12345";
    assert_eq!(
        callback_code(&request("/callback?state=expected&code=abc", host), host, "expected").unwrap(),
        Some("abc".into())
    );
    assert_eq!(
        callback_code(
            &request("/callback?state=expected&error=access_denied", host),
            host,
            "expected"
        )
        .unwrap(),
        None
    );
    for target in [
        "/favicon.ico",
        "/callback?state=wrong&code=abc",
        "/callback?code=abc",
        "/callback?state=expected&state=expected&code=abc",
        "/callback?state=expected&code=a&code=b",
        "/callback?state=expected&code=a&error=access_denied",
        "/callback?state=expected&code=",
        "/callback?state=expected&code=%0Asecret",
        "/callback?state=expected&error=unknown",
    ] {
        assert!(
            callback_code(&request(target, host), host, "expected").is_err(),
            "{target}"
        );
    }
    assert!(
        callback_code(
            &request("/callback?state=expected&code=abc", "attacker.test"),
            host,
            "expected"
        )
        .is_err()
    );
    assert!(
        callback_code(
            "GET /callback?state=expected&code=abc HTTP/1.1\r\nHost: 127.0.0.1:12345\r\nHost: attacker.test\r\n\r\n",
            host,
            "expected"
        )
        .is_err()
    );
}

#[tokio::test]
async fn invalid_callback_does_not_consume_the_waiting_login() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        receive_callback(
            &listener,
            "expected",
            &Url::parse("https://platform.example.test/cli/result/").unwrap(),
        )
        .await
        .unwrap()
    });
    let mut socket = TcpStream::connect(address).await.unwrap();
    socket
        .write_all(format!("GET /callback?state=wrong&code=abc HTTP/1.1\r\nHost: {address}\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut body = String::new();
    socket.read_to_string(&mut body).await.unwrap();
    assert!(body.starts_with("HTTP/1.1 303"));
    assert!(body.contains("Location: https://platform.example.test/cli/result/?outcome=invalid\r\n"));
    assert!(!body.contains("state=") && !body.contains("code="));
    let mut socket = TcpStream::connect(address).await.unwrap();
    socket
        .write_all(format!("GET /callback?state=expected&code=abc HTTP/1.1\r\nHost: {address}\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let (code, _) = task.await.unwrap();
    assert_eq!(code, "abc");
}

#[test]
fn pkce_matches_the_rfc7636_test_vector() {
    let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    assert_eq!(
        URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())),
        "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
    );
    assert_eq!(random_secret().unwrap().len(), 43);
}
