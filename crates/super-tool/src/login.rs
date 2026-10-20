use std::{
    io::{IsTerminal, Read},
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use reqwest::Client;
use serde::{Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use super_types::{
    api,
    credential::ApiBase,
    login::{CLIENT_ID, OAuthStartRequest, StartRequest, StartResponse, TokenRequest},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use url::Url;

use crate::{
    api::SuperClient,
    cli::LoginArgs,
    config::{CredentialSource, StateStore},
    daemon,
    output::Ui,
};

const LOGIN_TIMEOUT: Duration = Duration::from_mins(10);

pub async fn login(store: &StateStore, explicit_api: Option<&str>, args: &LoginArgs, ui: &Ui) -> Result<()> {
    let _lock = store.credential_lock()?;
    let previous = store.load()?;
    let saved_api = previous.credential_api_url.as_deref().or(previous.api_url.as_deref());
    let manual = match std::env::var("SUPER_ACCESS_TOKEN") {
        Ok(token) => Some(token),
        Err(std::env::VarError::NotPresent) if args.api_key => Some(read_key(ui)?),
        Err(std::env::VarError::NotPresent) => None,
        Err(_) => bail!("SUPER_ACCESS_TOKEN must contain a valid API key"),
    };
    let previous_session = if previous.credential == Some(CredentialSource::OAuth) {
        if let Ok(credential) = crate::oauth::Credential::load(store) {
            Some(credential)
        } else {
            ui.notice("The previous login could not be read. Sign out its session in Schematic Platform.");
            None
        }
    } else {
        None
    };
    let email = if let Some(token) = manual {
        ensure!(
            !token.trim().is_empty() && !token.chars().any(char::is_control),
            "the Super API key is empty or invalid"
        );
        let api_url = crate::key_api_url(Some(&token), explicit_api, saved_api)?;
        let identity = SuperClient::new(&api_url, Some(&token))?
            .whoami()
            .await
            .context("the API key was rejected; create a new key in Schematic Platform or contact support")?;
        save_manual_key(store, &api_url, &token)?;
        identity.email
    } else {
        let api_url = crate::key_api_url(None, explicit_api, saved_api)?;
        browser_login(store, &api_url, args, ui).await?
    };
    if let Some(previous) = previous_session
        && previous.revoke(&http_client()?).await.is_err()
    {
        ui.notice("The previous session could not be signed out. Sign it out under Access in Schematic Platform.");
    }
    daemon::ensure_daemon(store)?;
    ui.success(format!("Logged in as {email}"));
    Ok(())
}

async fn browser_login(store: &StateStore, api_url: &str, args: &LoginArgs, ui: &Ui) -> Result<String> {
    ensure!(
        ui.interactive || args.no_browser,
        "noninteractive login requires --api-key, --no-browser, or SUPER_ACCESS_TOKEN"
    );
    let http = http_client()?;
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .context("could not start the browser sign-in callback; use super login --api-key")?;
    let address = listener.local_addr()?;
    let redirect_uri = format!("http://{address}/callback");
    let verifier = random_secret()?;
    let state = random_secret()?;
    let start: StartResponse = post(
        &http,
        api_url,
        api::CLI_OAUTH_START,
        &OAuthStartRequest {
            client_id: CLIENT_ID.into(),
            response_type: "code".into(),
            code_challenge_method: "S256".into(),
            request: StartRequest {
                redirect_uri: redirect_uri.clone(),
                code_challenge: URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())),
                state: state.clone(),
                device_name: device_name(),
            },
        },
    )
    .await
    .context("could not start browser sign-in; you can use super login --api-key")?;
    let authorization = Url::parse(&start.authorization_url).context("the API returned an invalid sign-in URL")?;
    ApiBase::parse(&authorization.origin().ascii_serialization()).map_err(anyhow::Error::msg)?;
    ensure!(
        authorization.username().is_empty()
            && authorization.password().is_none()
            && authorization.path() == "/cli/connect"
            && authorization.fragment().is_none(),
        "the API returned an invalid sign-in URL"
    );
    let result_url = authorization.join("/cli/result/")?;
    let opened = if args.no_browser {
        false
    } else {
        ui.notice("Opening your browser…");
        webbrowser::open(authorization.as_str()).is_ok()
    };
    if !opened {
        eprintln!("Open this URL in a browser on this computer:\n{authorization}\n");
    }
    ui.notice("Waiting for sign-in…");
    let callback = tokio::select! {
        result = tokio::time::timeout(LOGIN_TIMEOUT, receive_callback(&listener, &state, &result_url)) => {
            result.context("sign-in timed out; run super login again")??
        }
        result = tokio::signal::ctrl_c() => {
            result.context("could not listen for sign-in cancellation")?;
            bail!("sign-in canceled");
        }
    };
    let (code, mut socket) = callback;
    let result = complete(
        &http,
        store,
        api_url,
        TokenRequest::AuthorizationCode {
            client_id: CLIENT_ID.into(),
            code,
            code_verifier: verifier,
            redirect_uri,
        },
    )
    .await;
    let outcome = if result.is_ok() {
        LoginOutcome::Connected
    } else {
        LoginOutcome::Failed
    };
    let _ = reply(&mut socket, &result_url, outcome).await;
    result
}

async fn complete(http: &Client, store: &StateStore, api_url: &str, body: TokenRequest) -> Result<String> {
    let requested_at = chrono::Utc::now().timestamp();
    let issued = crate::oauth::exchange(http, api_url, &body, None).await?;
    let email = issued.email.clone();
    let credential = crate::oauth::Credential::from_response(api_url, issued, requested_at)?;
    if let Err(error) = update_credential(store, |state| {
        credential.save(store)?;
        state.select_api(api_url);
        state.credential_api_url = Some(api_url.to_owned());
        state.credential = Some(CredentialSource::OAuth);
        state.browser_login_key = None;
        Ok(())
    }) {
        let _ = credential.revoke(http).await;
        return Err(error);
    }
    Ok(email)
}

fn save_manual_key(store: &StateStore, api_url: &str, token: &str) -> Result<()> {
    update_credential(store, |state| {
        store.save_api_key(token)?;
        state.select_api(api_url);
        state.credential_api_url = Some(api_url.to_owned());
        state.credential = Some(CredentialSource::SuperApiKey);
        state.browser_login_key = None;
        Ok(())
    })
}

fn update_credential(
    store: &StateStore,
    change: impl FnOnce(&mut crate::config::LocalState) -> Result<()>,
) -> Result<()> {
    let previous = store.load_api_key().ok().flatten();
    if let Err(error) = store.update(change) {
        // Restore the working credential if publishing the matching state failed.
        if let Some(previous) = previous {
            store.save_api_key(&previous)?;
        } else {
            store.clear_api_key()?;
        }
        return Err(error);
    }
    Ok(())
}

pub async fn logout(store: &StateStore, ui: &Ui) -> Result<()> {
    let _lock = store.credential_lock()?;
    let state = store.load()?;
    if state.credential == Some(CredentialSource::OAuth) {
        crate::oauth::Credential::load(store)?.revoke(&http_client()?).await
            .context("could not sign out this session; retry `super logout`, or sign it out under Access in Schematic Platform")?;
    }
    store.update(|state| {
        store.clear_api_key()?;
        state.credential = None;
        state.browser_login_key = None;
        Ok(())
    })?;
    ui.success("Logged out");
    Ok(())
}

pub(crate) fn http_client() -> Result<Client> {
    Ok(Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!("super/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(25))
        .build()?)
}

async fn post<T: DeserializeOwned>(http: &Client, api_url: &str, path: &str, body: &impl Serialize) -> Result<T> {
    let response = http
        .post(format!("{api_url}{path}"))
        .json(body)
        .send()
        .await
        .map_err(reqwest::Error::without_url)
        .context("could not reach Schematic")?;
    let status = response.status();
    let bytes = bounded_body(response).await?;
    ensure!(
        status.is_success(),
        "Schematic could not complete sign-in (HTTP {status}); run super login again or use --api-key"
    );
    serde_json::from_slice(&bytes).context("Schematic returned an invalid sign-in response")
}

pub(crate) async fn bounded_body(mut response: reqwest::Response) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(reqwest::Error::without_url)? {
        ensure!(
            bytes.len() + chunk.len() <= 16384,
            "Schematic returned an oversized sign-in response"
        );
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn read_key(ui: &Ui) -> Result<String> {
    if std::io::stdin().is_terminal() {
        return ui.api_key();
    }
    let mut bytes = Vec::new();
    std::io::stdin()
        .take(4097)
        .read_to_end(&mut bytes)
        .context("could not read the API key from stdin")?;
    ensure!(bytes.len() <= 4096, "the API key is too long");
    Ok(String::from_utf8(bytes)
        .context("the API key must be UTF-8")?
        .trim_end_matches(['\r', '\n'])
        .to_owned())
}

fn random_secret() -> Result<String> {
    let mut bytes = [0; 32];
    getrandom::fill(&mut bytes).map_err(|_| anyhow::anyhow!("could not generate a sign-in secret"))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn device_name() -> String {
    // This label is informational. It is never used as proof of device identity.
    let name = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .or_else(|| {
            std::process::Command::new("hostname")
                .output()
                .ok()
                .filter(|output| output.status.success())
                .and_then(|output| String::from_utf8(output.stdout).ok())
        });
    let name: String = name
        .unwrap_or_else(|| "this computer".into())
        .trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(36)
        .collect();
    if name.is_empty() { "this computer".into() } else { name }
}

async fn receive_callback(listener: &TcpListener, state: &str, result_url: &Url) -> Result<(String, TcpStream)> {
    let host = listener.local_addr()?.to_string();
    loop {
        let (mut socket, _) = listener.accept().await?;
        let request = tokio::time::timeout(Duration::from_secs(5), read_request(&mut socket)).await;
        let Ok(Ok(request)) = request else {
            let _ = reply(&mut socket, result_url, LoginOutcome::Invalid).await;
            continue;
        };
        match callback_code(&request, &host, state) {
            Ok(Some(code)) => return Ok((code, socket)),
            Ok(None) => {
                let _ = reply(&mut socket, result_url, LoginOutcome::Canceled).await;
                bail!("sign-in canceled in your browser");
            }
            Err(_) => {
                let _ = reply(&mut socket, result_url, LoginOutcome::Invalid).await;
            }
        }
    }
}

async fn read_request(socket: &mut TcpStream) -> Result<String> {
    let mut bytes = Vec::new();
    while !bytes.windows(4).any(|window| window == b"\r\n\r\n") {
        let mut buffer = [0; 1024];
        let count = socket.read(&mut buffer).await?;
        ensure!(count > 0 && bytes.len() + count <= 8192, "invalid callback request");
        bytes.extend_from_slice(&buffer[..count]);
    }
    String::from_utf8(bytes).context("invalid callback request")
}

fn callback_code(request: &str, host: &str, state: &str) -> Result<Option<String>> {
    let mut headers = [httparse::EMPTY_HEADER; 32];
    let mut parsed = httparse::Request::new(&mut headers);
    ensure!(
        parsed.parse(request.as_bytes())?.is_complete(),
        "incomplete callback request"
    );
    let path = parsed.path.context("missing callback path")?;
    ensure!(
        parsed.method == Some("GET") && parsed.version == Some(1) && path.starts_with("/callback?"),
        "invalid callback request"
    );
    let hosts: Vec<_> = parsed
        .headers
        .iter()
        .filter(|h| h.name.eq_ignore_ascii_case("host"))
        .collect();
    ensure!(
        hosts.len() == 1 && hosts[0].value == host.as_bytes(),
        "invalid callback host"
    );
    ensure!(
        !parsed
            .headers
            .iter()
            .any(|h| h.name.eq_ignore_ascii_case("transfer-encoding")
                || (h.name.eq_ignore_ascii_case("content-length") && h.value != b"0")),
        "unexpected callback body"
    );
    let url = Url::parse(&format!("http://{host}{path}"))?;
    ensure!(
        url.path() == "/callback" && url.fragment().is_none(),
        "invalid callback path"
    );
    let pairs: Vec<_> = url.query_pairs().collect();
    let field = |name: &str| -> Result<Option<String>> {
        let values: Vec<_> = pairs.iter().filter(|(key, _)| key == name).collect();
        ensure!(values.len() <= 1, "duplicate callback parameter");
        Ok(values.first().map(|(_, value)| value.to_string()))
    };
    ensure!(field("state")?.as_deref() == Some(state), "invalid callback state");
    let code = field("code")?;
    let error = field("error")?;
    if error.as_deref() == Some("access_denied") && code.is_none() {
        return Ok(None);
    }
    ensure!(error.is_none(), "invalid callback error");
    let code = code.context("missing callback code")?;
    ensure!(
        !code.is_empty() && code.len() <= 128 && code.bytes().all(|b| b.is_ascii_alphanumeric() || b".-_".contains(&b)),
        "invalid callback code"
    );
    Ok(Some(code))
}

#[derive(Clone, Copy)]
enum LoginOutcome {
    Connected,
    Canceled,
    Failed,
    Invalid,
}

async fn reply(socket: &mut TcpStream, result_url: &Url, outcome: LoginOutcome) -> Result<()> {
    // Platform owns the complete visual shell. Redirect only after the CLI knows the result;
    // never forward the callback's code/state, credential, or arbitrary error text.
    let mut destination = result_url.clone();
    destination.query_pairs_mut().append_pair(
        "outcome",
        match outcome {
            LoginOutcome::Connected => "connected",
            LoginOutcome::Canceled => "canceled",
            LoginOutcome::Failed => "failed",
            LoginOutcome::Invalid => "invalid",
        },
    );
    let response = format!(
        "HTTP/1.1 303 See Other\r\nLocation: {destination}\r\nContent-Length: 0\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nContent-Security-Policy: default-src 'none'; frame-ancestors 'none'\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n"
    );
    tokio::time::timeout(Duration::from_secs(5), socket.write_all(response.as_bytes())).await??;
    Ok(())
}

#[cfg(test)]
mod tests;
