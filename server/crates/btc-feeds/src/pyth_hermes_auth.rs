//! The Pyth Hermes credential boundary: environment-only load, header-only
//! transport, and redaction on every outbound string.
//!
//! Both Hermes update endpoints (`/v2/updates/price/stream` and
//! `/v2/updates/price/latest`) require `Authorization: Bearer $PYTH_API_KEY`
//! and answer 401 without it (verified 2026-09-03). The key enters the process
//! solely through [`ENVIRONMENT_VARIABLE`], lives in zeroizing memory, and is
//! attached only via the [`AUTHORIZATION_HEADER`] request header — never as a
//! `?token=` query parameter, which would land in proxy logs, redirect targets,
//! the recorded outbound request descriptor, and error strings that echo a URL.
//! The credential type has no `Display` and a `Debug` that prints
//! [`REDACTION_PLACEHOLDER`], so no format string can render it, and
//! [`redact_secret`] is the single choke point every Hermes error, log and
//! report string passes through first.
//!
//! This is the byte-for-byte analogue of `sportsdataio_auth`, on the same
//! reasoning: the only differences are the environment variable, the header
//! (`Authorization: Bearer …` rather than `Ocp-Apim-Subscription-Key`), and the
//! accepted length band. The capture writer additionally redacts the header out
//! of any recorded response body, exactly as the SportsDataIO harness does, so a
//! provider error page that echoes its request headers cannot leave the key in a
//! retained journal (test T-N9).

use std::fmt;

use reqwest::RequestBuilder;
use reqwest::header::{AUTHORIZATION, HeaderValue};
use thiserror::Error;
use zeroize::Zeroizing;

/// The one environment variable the harness reads the credential from. The value
/// is loaded into the shell immediately before an approved run and unset
/// afterwards; this crate never stores, hashes, prefixes or logs it.
pub const ENVIRONMENT_VARIABLE: &str = "PYTH_API_KEY";

/// The only authentication transport Tiny uses for Hermes: the standard
/// `Authorization` header carrying a `Bearer` token. Never `?token=`.
pub const AUTHORIZATION_HEADER: &str = "Authorization";

/// What a redacted occurrence of the credential is replaced with.
pub const REDACTION_PLACEHOLDER: &str = "<redacted>";

/// Accepted credential length band. Pyth does not publish its key format, so the
/// band is deliberately wide; the floor guarantees redaction can never match a
/// trivially short substring of unrelated text, and the ceiling bounds the work
/// [`redact_secret`] does per error string.
const MINIMUM_KEY_BYTES: usize = 8;
const MAXIMUM_KEY_BYTES: usize = 512;

/// Environment-injected Pyth Hermes API key. Held only in zeroizing memory,
/// never formatted, never persisted, never placed in a URL. The `Bearer ` prefix
/// is applied only at header-construction time and is never stored.
pub struct PythHermesCredential {
    key: Zeroizing<String>,
}

impl PythHermesCredential {
    /// Read the credential from `PYTH_API_KEY`. The error path never carries the
    /// value: a missing variable names only the variable, and an unusable value
    /// reports only that it is unusable.
    pub fn from_env() -> Result<Self, PythHermesAuthError> {
        let value = std::env::var(ENVIRONMENT_VARIABLE)
            .map_err(|_| PythHermesAuthError::MissingEnvironment(ENVIRONMENT_VARIABLE))?;
        Self::from_value(value)
    }

    /// Build a credential from an already-held value. Tests inject a SENTINEL
    /// through this constructor; production always goes through `from_env`.
    ///
    /// Rejects an empty or whitespace-only value, any value containing
    /// whitespace (a shell-quoting accident that would corrupt the header), any
    /// non-ASCII or non-graphic byte (which cannot be a legal header value), and
    /// any value outside the accepted length band.
    pub fn from_value(value: String) -> Result<Self, PythHermesAuthError> {
        let key = Zeroizing::new(value);
        if key.len() < MINIMUM_KEY_BYTES
            || key.len() > MAXIMUM_KEY_BYTES
            || !key.bytes().all(|byte| byte.is_ascii_graphic())
        {
            return Err(PythHermesAuthError::InvalidCredential);
        }
        Ok(Self { key })
    }

    /// Attach the `Authorization: Bearer <key>` header to one outbound request.
    /// The header value is marked sensitive so reqwest's own debug/logging paths
    /// elide it, and it is built here (rather than deferred inside
    /// `RequestBuilder`) so a construction failure surfaces as our own valueless
    /// error.
    pub fn authorize(
        &self,
        request: RequestBuilder,
    ) -> Result<RequestBuilder, PythHermesAuthError> {
        let mut value = HeaderValue::from_str(&format!("Bearer {}", self.key.as_str()))
            .map_err(|_| PythHermesAuthError::InvalidCredential)?;
        value.set_sensitive(true);
        Ok(request.header(AUTHORIZATION, value))
    }

    /// Replace every occurrence of the credential in `text` with the placeholder.
    pub fn redact(&self, text: &str) -> String {
        if self.key.is_empty() {
            return text.to_owned();
        }
        text.replace(self.key.as_str(), REDACTION_PLACEHOLDER)
    }

    /// Redact the credential out of one recorded RESPONSE BODY.
    ///
    /// The capture journals provider bodies VERBATIM, and a provider or upstream
    /// error page is entitled to echo the request headers it received — which is
    /// exactly how the key would reach a mode-`0600` journal that is later
    /// retained as the evidence bundle. This is the body-side twin of `redact`.
    ///
    /// Matching is over BYTES, not text. The credential is ASCII-graphic by
    /// construction, so an occurrence can never straddle a UTF-8 boundary and the
    /// ASCII placeholder can never create one.
    ///
    /// `truncated` says the caller cut the body at a recording cap. A cut can
    /// bisect an occurrence and leave a PREFIX behind that exact-match
    /// replacement cannot see, so a truncated body additionally drops its last
    /// `key.len() - 1` bytes.
    pub fn redact_body(&self, body: &[u8], truncated: bool) -> (Vec<u8>, bool) {
        let needle = self.key.as_bytes();
        let mut recorded = Vec::with_capacity(body.len());
        let mut rest = body;
        let mut replaced = false;
        while let Some(position) = find_bytes(rest, needle) {
            recorded.extend_from_slice(&rest[..position]);
            recorded.extend_from_slice(REDACTION_PLACEHOLDER.as_bytes());
            rest = &rest[position + needle.len()..];
            replaced = true;
        }
        recorded.extend_from_slice(rest);
        if truncated {
            let keep = recorded
                .len()
                .saturating_sub(needle.len().saturating_sub(1));
            recorded.truncate(keep);
        }
        (recorded, replaced)
    }
}

/// First position of `needle` in `haystack`. An empty needle never matches.
fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// `<redacted>` and nothing else. There is intentionally no `Display` impl.
impl fmt::Debug for PythHermesCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(REDACTION_PLACEHOLDER)
    }
}

/// Scrub the credential out of one error, log or report string. Applied on EVERY
/// Hermes error path before the string can reach stderr, a journal record or a
/// report.
pub fn redact_secret(text: &str, credential: &PythHermesCredential) -> String {
    credential.redact(text)
}

/// Why the Pyth Hermes credential could not be established. Neither variant
/// carries the credential value.
#[derive(Debug, Error)]
pub enum PythHermesAuthError {
    /// `PYTH_API_KEY` is absent from the environment. Carries the variable name
    /// only, never its value.
    #[error(
        "{0} is not set; the Pyth Hermes feed is refuse-closed without it (load it from the approved credential alias before the run)"
    )]
    MissingEnvironment(&'static str),
    /// Deliberately valueless: the rejected credential is never echoed.
    #[error("the Pyth Hermes credential is empty, malformed, or not a usable header value")]
    InvalidCredential,
}

#[cfg(test)]
mod tests {
    use super::{PythHermesCredential, REDACTION_PLACEHOLDER, redact_secret};

    const SENTINEL: &str = "sentinel-pyth-key-0123456789abcdef";

    fn sentinel() -> PythHermesCredential {
        PythHermesCredential::from_value(SENTINEL.to_owned()).expect("sentinel is well formed")
    }

    #[test]
    fn credential_rejects_empty_whitespace_and_non_ascii_values() {
        for rejected in [
            "",
            "   ",
            "\t",
            "short",
            "has space inside key",
            "trailing-whitespace-key ",
            "\nleading-newline-key",
            "n\u{f6}n-ascii-credential-value-here",
        ] {
            assert!(
                PythHermesCredential::from_value(rejected.to_owned()).is_err(),
                "value {rejected:?} must be refused"
            );
        }
        assert!(PythHermesCredential::from_value(SENTINEL.to_owned()).is_ok());
        assert!(PythHermesCredential::from_value("a".repeat(513)).is_err());
    }

    #[test]
    fn debug_prints_only_the_placeholder() {
        let credential = sentinel();
        let rendered = format!("{credential:?}");
        assert_eq!(rendered, REDACTION_PLACEHOLDER);
        assert!(!rendered.contains(SENTINEL));
    }

    #[test]
    fn redaction_replaces_every_occurrence_in_an_error_string() {
        let credential = sentinel();
        let error = format!(
            "GET https://hermes.pyth.network/v2/updates/price/stream failed with {SENTINEL} \
             (retry sent {SENTINEL})"
        );
        let redacted = redact_secret(&error, &credential);
        assert!(!redacted.contains(SENTINEL));
        assert_eq!(redacted.matches(REDACTION_PLACEHOLDER).count(), 2);
    }

    #[test]
    fn body_redaction_removes_the_credential_from_recorded_bytes() {
        let credential = sentinel();
        // The named threat: a provider error page that echoes the request header.
        let echoed = format!(
            "{{\"error\":\"unauthorized\",\"request\":{{\"Authorization\":\"Bearer {SENTINEL}\"}}}}"
        );
        let (recorded, replaced) = credential.redact_body(echoed.as_bytes(), false);
        assert!(replaced);
        let rendered = String::from_utf8(recorded).expect("an ASCII replacement keeps UTF-8");
        assert!(!rendered.contains(SENTINEL));

        // A clean body is passed through byte for byte and is not flagged.
        let clean = br#"{"parsed":[]}"#;
        let (recorded, replaced) = credential.redact_body(clean, false);
        assert!(!replaced);
        assert_eq!(recorded, clean);

        // A TRUNCATED body ending in a bisected occurrence drops even the prefix.
        let cut = SENTINEL.len() / 2;
        let lead = "provider error page: the request presented ";
        let mut bisected = lead.as_bytes().to_vec();
        bisected.extend_from_slice(&SENTINEL.as_bytes()[..cut]);
        let (recorded, replaced) = credential.redact_body(&bisected, true);
        assert!(!replaced, "no whole occurrence was present");
        let rendered = String::from_utf8(recorded).expect("ASCII");
        assert!(!rendered.contains(&SENTINEL[..cut]));
        assert!(rendered.starts_with("provider error page"));
    }

    #[test]
    fn missing_environment_error_names_only_the_variable() {
        let error = super::PythHermesAuthError::MissingEnvironment(super::ENVIRONMENT_VARIABLE);
        let rendered = error.to_string();
        assert!(rendered.contains("PYTH_API_KEY"));
        assert!(!rendered.contains(SENTINEL));
        let invalid = super::PythHermesAuthError::InvalidCredential.to_string();
        assert!(!invalid.contains(SENTINEL));
    }

    #[test]
    fn authorized_request_carries_only_the_bearer_header() {
        let credential = sentinel();
        let client = reqwest::Client::builder()
            .build()
            .expect("client construction");
        let request = credential
            .authorize(client.get("https://hermes.pyth.network/v2/updates/price/stream"))
            .expect("sentinel is a legal header value")
            .build()
            .expect("request builds");
        assert_eq!(request.url().query(), None);
        assert!(!request.url().as_str().contains("token="));
        let header = request
            .headers()
            .get(reqwest::header::AUTHORIZATION)
            .expect("authorization header is attached");
        assert!(header.is_sensitive());
        assert_eq!(
            header.to_str().expect("ascii header"),
            format!("Bearer {SENTINEL}")
        );
        // The rendered request must not leak the key outside that one header.
        assert!(!format!("{request:?}").contains(SENTINEL));
    }
}
