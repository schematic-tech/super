//! Browser sign-in contracts for the Schematic CLI.
use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartRequest {
    pub redirect_uri: String,
    pub code_challenge: String,
    pub state: String,
    pub device_name: String,
}

#[derive(Serialize, Deserialize)]
pub struct StartResponse {
    pub authorization_url: String,
}

pub const CLIENT_ID: &str = "schematic-cli";

#[derive(Serialize, Deserialize)]
pub struct OAuthStartRequest {
    pub client_id: String,
    pub response_type: String,
    pub code_challenge_method: String,
    #[serde(flatten)]
    pub request: StartRequest,
}

// OAuth token endpoint requests use application/x-www-form-urlencoded.
#[derive(Serialize, Deserialize)]
#[serde(tag = "grant_type")]
pub enum TokenRequest {
    #[serde(rename = "authorization_code")]
    AuthorizationCode {
        client_id: String,
        code: String,
        code_verifier: String,
        redirect_uri: String,
    },
    #[serde(rename = "refresh_token")]
    RefreshToken { client_id: String, refresh_token: String },
}

#[derive(Clone, Serialize, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub token_type: String,
    pub expires_in: u64,
    pub refresh_token: String,
    pub session_id: uuid::Uuid,
    pub email: String,
}

#[derive(Serialize, Deserialize)]
pub struct RevokeRequest {
    pub client_id: String,
    pub token: String,
}
