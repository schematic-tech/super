//! Environment routing carried by a Schematic API key. Decoding is not authentication.

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use uuid::Uuid;

/// A canonical API origin. Public Super routes are appended by the client.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiBase(String);

impl ApiBase {
    /// Validates an origin before it can receive credentials. HTTP is loopback-only.
    ///
    /// # Errors
    /// Returns a fixed message without including the input or any credentials.
    pub fn parse(value: &str) -> Result<Self, &'static str> {
        if value.len() > 512 || value.bytes().any(|b| b.is_ascii_whitespace() || b.is_ascii_control()) {
            return Err("the Super API URL is invalid");
        }
        let url = url::Url::parse(value).map_err(|_| "the Super API URL is invalid")?;
        if !matches!(url.scheme(), "http" | "https") || url.host().is_none() {
            return Err("the Super API URL must be an HTTP or HTTPS origin");
        }
        if !url.username().is_empty()
            || url.password().is_some()
            || url.path() != "/"
            || url.query().is_some()
            || url.fragment().is_some()
            || value.contains('\\')
        {
            return Err("the Super API URL must be an origin without credentials, path, query, or fragment");
        }
        let loopback = match url.host() {
            Some(url::Host::Domain("localhost")) => true,
            Some(url::Host::Ipv4(address)) => address.is_loopback(),
            Some(url::Host::Ipv6(address)) => address.is_loopback(),
            _ => false,
        };
        if url.scheme() == "http" && !loopback {
            return Err("the Super API URL must use HTTPS outside localhost development");
        }
        Ok(Self(url.origin().ascii_serialization()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Only non-secret metadata is returned. The original key remains the bearer credential.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoutedKey {
    pub id: Uuid,
    pub api_base: ApiBase,
}

impl RoutedKey {
    /// Recognizes versioned keys; legacy credentials return `None` for migration compatibility.
    ///
    /// # Errors
    /// Rejects malformed or unsupported versioned keys without echoing their contents.
    pub fn parse(value: &str) -> Result<Option<Self>, &'static str> {
        if !value.starts_with("super_v") && !value.starts_with("pup_v") {
            return Ok(None);
        }
        let invalid = "the Schematic API key is malformed or uses an unsupported version";
        if value.len() > 1024 {
            return Err(invalid);
        }
        let mut fields = value.split('.');
        if !matches!(fields.next(), Some("super_v2" | "pup_v2")) {
            return Err(invalid);
        }
        let id = fields.next().ok_or(invalid)?;
        let encoded = fields.next().ok_or(invalid)?;
        let secret = fields.next().ok_or(invalid)?;
        let lower_hex = |s: &str| s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if fields.next().is_some() || id.len() != 32 || !lower_hex(id) || secret.len() != 64 || !lower_hex(secret) {
            return Err(invalid);
        }
        let origin = URL_SAFE_NO_PAD.decode(encoded).map_err(|_| invalid)?;
        let origin = std::str::from_utf8(&origin).map_err(|_| invalid)?;
        let api_base = ApiBase::parse(origin).map_err(|_| invalid)?;
        if api_base.as_str() != origin {
            return Err(invalid);
        }
        Ok(Some(Self {
            id: Uuid::parse_str(id).map_err(|_| invalid)?,
            api_base,
        }))
    }

    /// Encodes a key using 256 independently generated random secret bits supplied by the issuer.
    #[must_use]
    pub fn encode(&self, secret: &[u8; 32]) -> String {
        format!(
            "super_v2.{}.{}.{}",
            self.id.simple(),
            URL_SAFE_NO_PAD.encode(self.api_base.as_str()),
            hex::encode(secret)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_round_trip_for_separate_environments() {
        for origin in [
            "https://api.schematic.tech",
            "https://api.staging.schematic.tech",
            "http://127.0.0.1:8080",
        ] {
            let metadata = RoutedKey {
                id: Uuid::new_v4(),
                api_base: ApiBase::parse(origin).unwrap(),
            };
            let key = metadata.encode(&[0xab; 32]);
            assert_eq!(RoutedKey::parse(&key).unwrap(), Some(metadata.clone()));
            let legacy = key.replacen("super_v2.", "pup_v2.", 1);
            assert_eq!(RoutedKey::parse(&legacy).unwrap(), Some(metadata));
            assert!(RoutedKey::parse(&format!("{key}.extra")).is_err());
            assert!(RoutedKey::parse(&key[..key.len() - 1]).is_err());
            assert!(!format!("{:?}", RoutedKey::parse(&key).unwrap()).contains(&hex::encode([0xab; 32])));
        }
        assert!(RoutedKey::parse("pup_live_legacy").unwrap().is_none());
        assert!(RoutedKey::parse("hydra_live_legacy").unwrap().is_none());
        assert!(RoutedKey::parse("pup_v3.unknown").is_err());
        assert!(RoutedKey::parse("super_v3.unknown").is_err());
    }

    #[test]
    fn origins_reject_credential_leaks_and_are_canonical() {
        for value in [
            "http://remote.test",
            "https://user:password@api.test",
            "https://api.test/path",
            "https://api.test?token=secret",
            "https://api.test/#fragment",
            " https://api.test",
            "https://api.test\\path",
            "file:///secret",
        ] {
            assert!(ApiBase::parse(value).is_err(), "{value}");
        }
        assert_eq!(
            ApiBase::parse("https://API.TEST:443/").unwrap().as_str(),
            "https://api.test"
        );
        assert!(ApiBase::parse("http://[::1]:8080").is_ok());
    }
}
