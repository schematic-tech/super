//! Private, atomically replaced credentials and cross-process refresh coordination.
use anyhow::{Context, Result, ensure};
use chrono::Utc;
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use super_types::{
    api,
    login::{CLIENT_ID, RevokeRequest, TokenRequest, TokenResponse},
};
use uuid::Uuid;

use crate::config::{CredentialSource, StateStore};

// Never derive Debug: this file contains both bearer credentials.
#[derive(Serialize, Deserialize)]
pub struct Credential {
    pub api_url: String,
    pub session_id: Uuid,
    access_token: String,
    refresh_token: String,
    expires_at: i64,
    #[serde(default)]
    pending_refresh: Option<Uuid>,
}

impl Credential {
    pub fn from_response(api_url: &str, response: TokenResponse, requested_at: i64) -> Result<Self> {
        ensure!(
            response.token_type.eq_ignore_ascii_case("bearer")
                && response.expires_in > 0
                && response.expires_in <= 900
                && !response.session_id.is_nil()
                && valid_token(&response.access_token)
                && valid_token(&response.refresh_token),
            "Schematic returned an invalid sign-in response"
        );
        Ok(Self {
            api_url: api_url.into(),
            session_id: response.session_id,
            access_token: response.access_token,
            refresh_token: response.refresh_token,
            expires_at: requested_at + i64::try_from(response.expires_in)?,
            pending_refresh: None,
        })
    }

    pub fn load(store: &StateStore) -> Result<Self> {
        let value = store
            .load_api_key()?
            .context("your saved login is missing; run `super login` again")?;
        serde_json::from_str(&value)
            .map_err(|_| anyhow::anyhow!("your saved login is invalid; run `super login` again"))
    }

    pub fn save(&self, store: &StateStore) -> Result<()> {
        store.save_api_key(&serde_json::to_string(self)?)
    }

    pub async fn revoke(&self, http: &Client) -> Result<()> {
        let response = http
            .post(format!("{}{}", self.api_url, api::CLI_REVOKE))
            .form(&RevokeRequest {
                client_id: CLIENT_ID.into(),
                token: self.refresh_token.clone(),
            })
            .send()
            .await
            .map_err(reqwest::Error::without_url)?;
        ensure!(
            response.status().is_success(),
            "Schematic could not sign out this session (HTTP {})",
            response.status()
        );
        Ok(())
    }
}

fn valid_token(token: &str) -> bool {
    !token.is_empty() && token.len() <= 4096 && token.bytes().all(|b| b.is_ascii_graphic())
}

fn current(store: &StateStore, api_url: &str, id: Uuid) -> Result<Credential> {
    let state = store.load()?;
    ensure!(
        state.credential == Some(CredentialSource::OAuth),
        "your login changed; retry the command"
    );
    let credential = Credential::load(store)?;
    ensure!(
        credential.api_url == api_url && credential.session_id == id,
        "your login changed; retry the command"
    );
    Ok(credential)
}

pub async fn access_token(store: &StateStore, api_url: &str, id: Uuid, rejected: Option<&str>) -> Result<String> {
    let fresh = |credential: &Credential| {
        credential.pending_refresh.is_none()
            && credential.expires_at > Utc::now().timestamp() + 60
            && rejected != Some(credential.access_token.as_str())
    };
    let credential = current(store, api_url, id)?;
    if fresh(&credential) {
        return Ok(credential.access_token);
    }
    // Never block a Tokio worker on flock: another command or the observer may be renewing.
    let _lock = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            if let Some(lock) = store.try_credential_lock()? {
                break Ok::<_, anyhow::Error>(lock);
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await
    .context("another sign-in operation is still running; retry the command")??;
    let mut credential = current(store, api_url, id)?;
    if fresh(&credential) {
        return Ok(credential.access_token);
    }
    let request_id = *credential.pending_refresh.get_or_insert_with(Uuid::new_v4);
    credential.save(store)?;
    let requested_at = Utc::now().timestamp();
    let response = exchange(
        &crate::login::http_client()?,
        api_url,
        &TokenRequest::RefreshToken {
            client_id: CLIENT_ID.into(),
            refresh_token: credential.refresh_token.clone(),
        },
        Some(request_id),
    )
    .await?;
    ensure!(
        response.session_id == id,
        "Schematic returned a different session; run `super login` again"
    );
    let renewed = Credential::from_response(api_url, response, requested_at)?;
    renewed.save(store)?;
    Ok(renewed.access_token)
}

pub async fn exchange(
    http: &Client,
    api_url: &str,
    body: &TokenRequest,
    request_id: Option<Uuid>,
) -> Result<TokenResponse> {
    // Only renewal has a durable idempotency key. Retry one transient failure promptly,
    // within the server's recovery window, without replaying a one-use authorization code.
    let mut attempt = 0;
    loop {
        let result = exchange_once(http, api_url, body, request_id).await;
        match result {
            Err((_, true)) if request_id.is_some() && attempt == 0 => {
                attempt += 1;
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
            other => return other.map_err(|(error, _)| error),
        }
    }
}

async fn exchange_once(
    http: &Client,
    api_url: &str,
    body: &TokenRequest,
    request_id: Option<Uuid>,
) -> std::result::Result<TokenResponse, (anyhow::Error, bool)> {
    let mut request = http.post(format!("{api_url}{}", api::CLI_TOKEN)).form(body);
    if let Some(id) = request_id {
        request = request.header("Idempotency-Key", id.to_string());
    }
    let response = request
        .send()
        .await
        .map_err(reqwest::Error::without_url)
        .context("could not reach Schematic to renew or complete sign-in; retry the command")
        .map_err(|error| (error, true))?;
    let status = response.status();
    let bytes = crate::login::bounded_body(response)
        .await
        .map_err(|error| (error, true))?;
    if status == StatusCode::BAD_REQUEST || status == StatusCode::UNAUTHORIZED {
        return Err((
            anyhow::anyhow!("your Schematic login has expired or was signed out; run `super login` again"),
            false,
        ));
    }
    if !status.is_success() {
        return Err((
            anyhow::anyhow!("Schematic could not renew or complete sign-in (HTTP {status}); retry the command"),
            status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS,
        ));
    }
    serde_json::from_slice(&bytes)
        .context("Schematic returned an invalid sign-in response")
        .map_err(|error| (error, false))
}
