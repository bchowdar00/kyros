//! API-key handling and request signing for Gemini's private API.

use std::time::{SystemTime, UNIX_EPOCH};

/// Errors from constructing or loading an [`ApiKey`]. Messages never contain
/// the secret.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AuthError {
    /// The key id was empty.
    #[error("api key id must not be empty")]
    EmptyKey,
    /// The secret was empty.
    #[error("api secret must not be empty")]
    EmptySecret,
    /// The key id is not account-scoped (must start with `account-`).
    #[error("api key must be account-scoped (start with \"account-\")")]
    NotAccountScoped,
    /// Exactly one of `GEMINI_API_KEY` / `GEMINI_API_SECRET` was set.
    #[error("both GEMINI_API_KEY and GEMINI_API_SECRET must be set, or neither")]
    PartialEnv,
}

/// An account-scoped Gemini API key pair. The secret is never exposed through
/// `Debug`, `Display`, `Serialize`, or any accessor.
pub struct ApiKey {
    key: String,
    secret: String,
}

impl std::fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the secret, and never the full key id: show at most the
        // `account-` prefix followed by an ellipsis.
        let shown: String = if self.key.len() > 8 {
            format!("{}…", &self.key[..8])
        } else {
            self.key.clone()
        };
        f.debug_struct("ApiKey")
            .field("key", &shown)
            .field("secret", &"<redacted>")
            .finish()
    }
}

impl ApiKey {
    /// Validate and construct a key pair. Both parts must be non-empty and the
    /// key id must start with `account-` (master/group keys are refused).
    pub fn new(key: impl Into<String>, secret: impl Into<String>) -> Result<ApiKey, AuthError> {
        let key = key.into();
        let secret = secret.into();
        if key.is_empty() {
            return Err(AuthError::EmptyKey);
        }
        if secret.is_empty() {
            return Err(AuthError::EmptySecret);
        }
        if !key.starts_with("account-") {
            return Err(AuthError::NotAccountScoped);
        }
        Ok(ApiKey { key, secret })
    }

    /// Read `GEMINI_API_KEY` / `GEMINI_API_SECRET` from the environment.
    pub fn from_env() -> Result<Option<ApiKey>, AuthError> {
        let key = std::env::var("GEMINI_API_KEY").ok();
        let secret = std::env::var("GEMINI_API_SECRET").ok();
        from_env_pair(key, secret)
    }

    /// The public key id.
    pub fn key(&self) -> &str {
        &self.key
    }

    /// REST authentication headers for a payload value.
    pub fn rest_headers(&self, payload: &serde_json::Value) -> RestHeaders {
        use base64::Engine;
        let json = serde_json::to_string(payload).unwrap_or_else(|_| "null".to_string());
        let b64 = base64::engine::general_purpose::STANDARD.encode(json.as_bytes());
        let signature = sign(self.secret.as_bytes(), &b64);
        RestHeaders {
            apikey: self.key.clone(),
            payload: b64,
            signature,
        }
    }

    /// WebSocket handshake authentication headers for a decimal nonce.
    pub fn ws_headers(&self, nonce: u64) -> WsHeaders {
        use base64::Engine;
        let nonce = nonce.to_string();
        let payload = base64::engine::general_purpose::STANDARD.encode(nonce.as_bytes());
        let signature = sign(self.secret.as_bytes(), &payload);
        WsHeaders {
            apikey: self.key.clone(),
            nonce,
            payload,
            signature,
        }
    }
}

/// REST authentication headers (`X-GEMINI-APIKEY`, `-PAYLOAD`, `-SIGNATURE`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestHeaders {
    /// `X-GEMINI-APIKEY`.
    pub apikey: String,
    /// `X-GEMINI-PAYLOAD`: standard base64 of the compact JSON payload.
    pub payload: String,
    /// `X-GEMINI-SIGNATURE`: lowercase-hex HMAC-SHA384 of `payload`.
    pub signature: String,
}

/// WebSocket handshake authentication headers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WsHeaders {
    /// `X-GEMINI-APIKEY`.
    pub apikey: String,
    /// `X-GEMINI-NONCE`: the nonce as decimal text.
    pub nonce: String,
    /// `X-GEMINI-PAYLOAD`: base64 of the decimal nonce text.
    pub payload: String,
    /// `X-GEMINI-SIGNATURE`: lowercase-hex HMAC-SHA384 of `payload`.
    pub signature: String,
}

/// Core of [`ApiKey::from_env`], split out so the set/unset/trim logic is
/// testable without touching the process environment.
fn from_env_pair(
    key: Option<String>,
    secret: Option<String>,
) -> Result<Option<ApiKey>, AuthError> {
    match (key, secret) {
        (None, None) => Ok(None),
        (Some(key), Some(secret)) => Ok(Some(ApiKey::new(key.trim(), secret.trim())?)),
        _ => Err(AuthError::PartialEnv),
    }
}

/// Lowercase-hex HMAC-SHA384 of `message` under `secret`.
pub fn sign(secret: &[u8], message: &str) -> String {
    use hmac::{Hmac, Mac};
    use sha2::Sha384;
    use std::fmt::Write;

    let mut mac = <Hmac<Sha384>>::new_from_slice(secret)
        .expect("HMAC-SHA384 accepts a key of any length");
    mac.update(message.as_bytes());
    let digest = mac.finalize().into_bytes();
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// The nonces for one API key. Gemini wants each request's nonce to be new
/// and larger than the last, and near the clock; the clock in seconds
/// repeats when two requests share a second. Clone it to share: every
/// request signed with one key must draw from one `Nonce`.
#[derive(Debug, Clone, Default)]
pub struct Nonce(std::sync::Arc<std::sync::atomic::AtomicU64>);

impl Nonce {
    pub fn new() -> Nonce {
        Nonce::default()
    }

    /// The next nonce: the clock in seconds, or one more than the last
    /// when the clock has not moved past it.
    pub fn next(&self) -> u64 {
        use std::sync::atomic::Ordering;
        let now = unix_seconds();
        let mut last = self.0.load(Ordering::SeqCst);
        loop {
            let next = now.max(last + 1);
            match self.0.compare_exchange(last, next, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => return next,
                Err(seen) => last = seen,
            }
        }
    }
}

/// How many seconds ahead of the clock a nonce may run before the request
/// that carries it waits. Gemini takes a time-based nonce only near its
/// own clock; a nonce that ran far ahead would lock the key out.
pub const MAX_NONCE_LEAD: u64 = 5;

/// How long a request carrying `nonce` waits so that it leaves no more
/// than [`MAX_NONCE_LEAD`] seconds ahead of `now`.
pub fn nonce_wait(nonce: u64, now: u64) -> std::time::Duration {
    std::time::Duration::from_secs(nonce.saturating_sub(now).saturating_sub(MAX_NONCE_LEAD))
}

/// Current Unix time in whole seconds.
pub fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_nonce_far_ahead_of_the_clock_waits_for_the_clock() {
        use std::time::Duration;
        assert_eq!(nonce_wait(1_000, 1_000), Duration::ZERO);
        assert_eq!(nonce_wait(1_000 + MAX_NONCE_LEAD, 1_000), Duration::ZERO, "a short burst is not slowed");
        assert_eq!(nonce_wait(1_000 + MAX_NONCE_LEAD + 3, 1_000), Duration::from_secs(3));
        assert_eq!(nonce_wait(990, 1_000), Duration::ZERO, "a clock that jumped forward costs nothing");
    }

    #[test]
    fn a_nonce_is_never_given_out_twice_and_stays_near_the_clock() {
        let nonce = Nonce::new();
        let first = nonce.next();
        assert!(unix_seconds().abs_diff(first) <= 2, "{first}");
        let shared = nonce.clone();
        let mut last = first;
        for _ in 0..5 {
            // Far more often than once a second, and from two holders.
            let a = nonce.next();
            let b = shared.next();
            assert!(a > last && b > a, "{last} {a} {b}");
            last = b;
        }
    }

    // Independent reference vector, computed with the system openssl:
    //   printf '%s' "greetings-from-kyros-384" \
    //     | openssl dgst -sha384 -hmac "kyros-test-secret"
    //   SHA2-384(stdin)= afcb92c6536e2275c74e654fafb5ba2165943cf8071ee0777c3b63082cbfde25a1c8eb16cb076292a825a0406c654fbd
    #[test]
    fn sign_matches_openssl_vector() {
        let got = sign(b"kyros-test-secret", "greetings-from-kyros-384");
        assert_eq!(
            got,
            "afcb92c6536e2275c74e654fafb5ba2165943cf8071ee0777c3b63082cbfde25a1c8eb16cb076292a825a0406c654fbd"
        );
    }

    #[test]
    fn rest_headers_payload_and_signature() {
        let key = ApiKey::new("account-kyros-demo", "kyros-test-secret").unwrap();
        let payload = serde_json::json!({
            "request": "/v1/prediction-markets/positions",
            "nonce": 1776294447u64
        });
        let headers = key.rest_headers(&payload);
        assert_eq!(headers.apikey, "account-kyros-demo");

        use base64::Engine;
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&headers.payload)
            .expect("payload is valid base64");
        // Decodes to exactly the compact JSON text we signed.
        assert_eq!(decoded, serde_json::to_vec(&payload).unwrap());
        assert_eq!(headers.signature, sign(b"kyros-test-secret", &headers.payload));
    }

    #[test]
    fn ws_headers_for_known_nonce() {
        let key = ApiKey::new("account-kyros-demo", "kyros-test-secret").unwrap();
        let headers = key.ws_headers(1776294447);
        assert_eq!(headers.apikey, "account-kyros-demo");
        assert_eq!(headers.nonce, "1776294447");
        // base64("1776294447") == "MTc3NjI5NDQ0Nw==" (verified with `base64`).
        assert_eq!(headers.payload, "MTc3NjI5NDQ0Nw==");
        // sign of that payload, verified with openssl (see module comment style):
        //   printf '%s' "MTc3NjI5NDQ0Nw==" | openssl dgst -sha384 -hmac "kyros-test-secret"
        assert_eq!(
            headers.signature,
            "e938b5898893aaa60151f79670d23fd83a0624624d57afba003e00b483eb9b30704180b3bdf3720485f9b219aae00ef6"
        );
        assert_eq!(headers.signature, sign(b"kyros-test-secret", &headers.payload));
    }

    #[test]
    fn new_rejects_bad_inputs_and_debug_redacts() {
        assert_eq!(ApiKey::new("", "secret").unwrap_err(), AuthError::EmptyKey);
        assert_eq!(
            ApiKey::new("account-x", "").unwrap_err(),
            AuthError::EmptySecret
        );
        assert_eq!(
            ApiKey::new("master-kyros-demo", "kyros-test-secret").unwrap_err(),
            AuthError::NotAccountScoped
        );

        let key = ApiKey::new("account-kyros-demo-secret-id", "kyros-super-secret").unwrap();
        let dbg = format!("{key:?}");
        assert!(!dbg.contains("kyros-super-secret"), "debug leaked secret: {dbg}");
        assert!(
            !dbg.contains("account-kyros-demo-secret-id"),
            "debug leaked full key id: {dbg}"
        );
        assert!(dbg.contains("redacted"), "debug should mark secret redacted: {dbg}");
    }

    #[test]
    fn from_env_pair_logic() {
        // neither set -> Ok(None)
        assert!(matches!(from_env_pair(None, None), Ok(None)));
        // exactly one set -> Err
        assert_eq!(
            from_env_pair(Some("account-x".into()), None).unwrap_err(),
            AuthError::PartialEnv
        );
        assert_eq!(
            from_env_pair(None, Some("secret".into())).unwrap_err(),
            AuthError::PartialEnv
        );
        // both set -> trimmed and validated
        let key = from_env_pair(
            Some("  account-kyros-demo  ".into()),
            Some("  kyros-test-secret  ".into()),
        )
        .unwrap()
        .unwrap();
        assert_eq!(key.key(), "account-kyros-demo");
        assert_eq!(key.rest_headers(&serde_json::json!({})).signature, sign(b"kyros-test-secret", &key.rest_headers(&serde_json::json!({})).payload));
    }
}
