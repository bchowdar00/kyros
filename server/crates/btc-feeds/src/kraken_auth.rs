//! Kraken private-REST authentication for the one credentialed call the L3
//! book needs: minting a short-lived WebSocket token.
//!
//! Credentials come from the environment, are bounds-checked before use, and
//! live only in zeroizing memory; neither they, the request signature, nor the
//! minted token is ever formatted, logged or exposed by `Debug`. Requests are
//! signed with Kraken's HMAC-SHA512 scheme over a strictly increasing
//! process-wide nonce, so concurrent calls can never reuse one. Every failure
//! path fails closed to [`KrakenAuthError`], and the only provider text
//! retained is a bounded, printable-ASCII error message.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use hmac::{Hmac, Mac};
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderName, HeaderValue};
use serde::Deserialize;
use sha2::{Digest, Sha256, Sha512};
use thiserror::Error;
use zeroize::Zeroizing;

use crate::secure_http;

const TOKEN_PATH: &str = "/0/private/GetWebSocketsToken";
const TOKEN_URL: &str = "https://api.kraken.com/0/private/GetWebSocketsToken";
const MAX_TOKEN_RESPONSE_BYTES: usize = 64 * 1024;
const MAX_API_KEY_BYTES: usize = 512;
const MAX_API_SECRET_BYTES: usize = 2_048;
const MAX_TOKEN_BYTES: usize = 4_096;
static LAST_NONCE: AtomicU64 = AtomicU64::new(0);

/// Kraken credentials held only in zeroizing memory and never formatted.
struct KrakenCredentials {
    api_key: Zeroizing<String>,
    api_secret: Zeroizing<Vec<u8>>,
}

impl KrakenCredentials {
    fn from_env() -> Result<Self, KrakenAuthError> {
        let api_key = std::env::var("KRAKEN_API_KEY")
            .map_err(|_| KrakenAuthError::MissingEnvironment("KRAKEN_API_KEY"))?;
        let encoded_secret = Zeroizing::new(
            std::env::var("KRAKEN_API_SECRET")
                .map_err(|_| KrakenAuthError::MissingEnvironment("KRAKEN_API_SECRET"))?,
        );
        Self::from_values(api_key, &encoded_secret)
    }

    fn from_values(api_key: String, encoded_secret: &str) -> Result<Self, KrakenAuthError> {
        if api_key.is_empty()
            || api_key.len() > MAX_API_KEY_BYTES
            || api_key.trim() != api_key
            || encoded_secret.is_empty()
            || encoded_secret.len() > MAX_API_SECRET_BYTES
            || encoded_secret.trim() != encoded_secret
        {
            return Err(KrakenAuthError::InvalidCredentials);
        }
        let api_secret = STANDARD
            .decode(encoded_secret)
            .map_err(|_| KrakenAuthError::InvalidCredentials)?;
        if api_secret.is_empty() || api_secret.len() > MAX_API_SECRET_BYTES {
            return Err(KrakenAuthError::InvalidCredentials);
        }
        Ok(Self {
            api_key: Zeroizing::new(api_key),
            api_secret: Zeroizing::new(api_secret),
        })
    }
}

/// Fixed-host client for the one authenticated operation needed by Kraken L3.
pub struct KrakenAuthClient {
    client: reqwest::Client,
    credentials: KrakenCredentials,
}

impl KrakenAuthClient {
    /// Build the client from `KRAKEN_API_KEY` and `KRAKEN_API_SECRET`.
    ///
    /// Fails closed if either variable is absent or outside its bounds, so a
    /// misconfigured run never reaches the wire. The decoded secret is held in
    /// zeroizing memory and never formatted.
    pub fn from_env() -> Result<Self, KrakenAuthError> {
        Ok(Self {
            client: secure_http::client().map_err(|_| KrakenAuthError::Http)?,
            credentials: KrakenCredentials::from_env()?,
        })
    }

    /// Mint a short-lived WebSocket token over the authenticated REST endpoint.
    ///
    /// Signs `nonce=<nonce>` with the API secret (HMAC-SHA512 over the path and
    /// the SHA-256 of nonce plus body, base64-encoded) using a
    /// strictly-increasing process-wide nonce. The key and signature headers are
    /// marked sensitive so they are redacted from header debug output, and the
    /// signature, the response body and the token itself are held in zeroizing
    /// memory. A provider `error` array becomes
    /// [`KrakenAuthError::ProviderDenied`] rather than a token.
    pub async fn websocket_token(&self) -> Result<KrakenWebSocketToken, KrakenAuthError> {
        let nonce = next_nonce()?;
        let body = format!("nonce={nonce}");
        let signature = Zeroizing::new(sign(
            &self.credentials.api_secret,
            TOKEN_PATH,
            &nonce.to_string(),
            &body,
        )?);

        let mut api_key = HeaderValue::from_str(&self.credentials.api_key)
            .map_err(|_| KrakenAuthError::InvalidCredentials)?;
        api_key.set_sensitive(true);
        let mut api_signature =
            HeaderValue::from_str(&signature).map_err(|_| KrakenAuthError::InvalidCredentials)?;
        api_signature.set_sensitive(true);

        let response = self
            .client
            .post(TOKEN_URL)
            .header(HeaderName::from_static("api-key"), api_key)
            .header(HeaderName::from_static("api-sign"), api_signature)
            .header(
                CONTENT_TYPE,
                HeaderValue::from_static("application/x-www-form-urlencoded"),
            )
            .header(ACCEPT, HeaderValue::from_static("application/json"))
            .body(body)
            .send()
            .await
            .map_err(|_| KrakenAuthError::Http)?;
        let body = Zeroizing::new(
            secure_http::bounded_body(response, MAX_TOKEN_RESPONSE_BYTES)
                .await
                .map_err(|_| KrakenAuthError::Http)?,
        );
        parse_token_response(&body)
    }
}

/// Short-lived Kraken WebSocket token. Its value is intentionally not printable.
pub struct KrakenWebSocketToken(Zeroizing<String>);

impl KrakenWebSocketToken {
    pub(crate) fn expose(&self) -> &str {
        &self.0
    }

    #[cfg(test)]
    pub(crate) fn test_token(value: &str) -> Self {
        Self(Zeroizing::new(value.to_owned()))
    }
}

fn next_nonce() -> Result<u64, KrakenAuthError> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| KrakenAuthError::Clock)?
        .as_nanos();
    let now = u64::try_from(now).map_err(|_| KrakenAuthError::Clock)?;
    let mut observed = LAST_NONCE.load(Ordering::Relaxed);
    loop {
        let candidate = now.max(observed.checked_add(1).ok_or(KrakenAuthError::Clock)?);
        match LAST_NONCE.compare_exchange_weak(
            observed,
            candidate,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => return Ok(candidate),
            Err(actual) => observed = actual,
        }
    }
}

fn sign(
    secret: &[u8],
    path: &str,
    nonce: &str,
    encoded_body: &str,
) -> Result<String, KrakenAuthError> {
    let mut digest = Sha256::new();
    digest.update(nonce.as_bytes());
    digest.update(encoded_body.as_bytes());
    let digest = digest.finalize();

    let mut mac =
        Hmac::<Sha512>::new_from_slice(secret).map_err(|_| KrakenAuthError::InvalidCredentials)?;
    mac.update(path.as_bytes());
    mac.update(&digest);
    Ok(STANDARD.encode(mac.finalize().into_bytes()))
}

fn parse_token_response(body: &[u8]) -> Result<KrakenWebSocketToken, KrakenAuthError> {
    let response: TokenResponse =
        serde_json::from_slice(body).map_err(|_| KrakenAuthError::InvalidResponse)?;
    if response.error.len() > 16
        || response.error.iter().any(|error| {
            error.is_empty()
                || error.len() > 256
                || !error
                    .bytes()
                    .all(|byte| byte.is_ascii_graphic() || byte == b' ')
        })
    {
        return Err(KrakenAuthError::InvalidResponse);
    }
    if !response.error.is_empty() {
        return Err(KrakenAuthError::ProviderDenied(response.error.join("; ")));
    }
    let result = response.result.ok_or(KrakenAuthError::InvalidResponse)?;
    if result.token.is_empty() || result.token.len() > MAX_TOKEN_BYTES || !result.token.is_ascii() {
        return Err(KrakenAuthError::InvalidResponse);
    }
    Ok(KrakenWebSocketToken(Zeroizing::new(result.token)))
}

#[derive(Deserialize)]
struct TokenResponse {
    error: Vec<String>,
    result: Option<TokenResult>,
}

#[derive(Deserialize)]
struct TokenResult {
    token: String,
}

/// Why Kraken authentication failed. No variant carries credential, signature
/// or token material; the only provider text retained is the validated
/// `ProviderDenied` message.
#[derive(Debug, Error)]
pub enum KrakenAuthError {
    /// `KRAKEN_API_KEY` or `KRAKEN_API_SECRET` is absent from the environment.
    /// Carries the variable name only, never its value.
    #[error("required environment variable {0} is not available")]
    MissingEnvironment(&'static str),
    /// A credential was empty, surrounded by whitespace, over its byte bound
    /// (512 for the key, 2048 for the base64 secret and its decoded bytes), not
    /// valid base64, or unusable as an HMAC key or HTTP header value.
    #[error("Kraken credentials are empty, malformed, or outside configured bounds")]
    InvalidCredentials,
    /// The secure-HTTP client could not be built, the request failed, or the
    /// body exceeded the 64 KiB response cap.
    #[error("secure Kraken authentication request failed")]
    Http,
    /// Kraken answered with a non-empty `error` array. Carries those messages
    /// joined by `; `, admitted only after each is bounded to 256 printable
    /// ASCII bytes and the array to 16 entries.
    #[error("Kraken rejected WebSocket token creation: {0}")]
    ProviderDenied(String),
    /// The body did not deserialize, carried an implausible `error` array, had
    /// no `result`, or held a token that was empty, non-ASCII, or over 4096
    /// bytes.
    #[error("Kraken returned an invalid bounded token response")]
    InvalidResponse,
    /// The system clock was before the Unix epoch, past `u64::MAX` nanoseconds,
    /// or the strictly increasing nonce counter would overflow.
    #[error("a monotonic Kraken nonce could not be generated")]
    Clock,
}

#[cfg(test)]
mod tests {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;

    use super::{parse_token_response, sign};

    #[test]
    fn official_kraken_signature_vector_matches() {
        // Published Kraken documentation fixture; this is not an account credential.
        let secret = STANDARD
            .decode("kQH5HW/8p1uGOVjbgWA7FunAmGO8lsSUXNsu3eow76sz84Q18fWxnyRzBHCd3pd5nE9qa99HAZtuZuj6F1huXg==")
            .expect("official example secret decodes");
        let body =
            "nonce=1616492376594&ordertype=limit&pair=XBTUSD&price=37500&type=buy&volume=1.25";
        assert_eq!(
            sign(&secret, "/0/private/AddOrder", "1616492376594", body)
                .expect("signature succeeds"),
            "4/dpxb3iT4tp/ZCVEwSnEsLxx0bqyhLpdfOpc6fn7OR8+UClSV5n9E6aSS8MPtnRfp32bAb0nmbRn6H8ndwLUQ=="
        );
    }

    #[test]
    fn token_response_rejects_oversized_secret_material() {
        let oversized = "x".repeat(4_097);
        let body = serde_json::to_vec(&serde_json::json!({
            "error": [],
            "result": {"token": oversized, "expires": 900}
        }))
        .expect("fixture serializes");
        assert!(parse_token_response(&body).is_err());
    }

    #[test]
    fn token_response_rejects_log_control_characters() {
        let body = br#"{"error":["rejected\nforged-log-line"],"result":null}"#;
        assert!(parse_token_response(body).is_err());
    }
}
