//! The SportsDataIO NBA credential boundary: environment-only load,
//! header-only transport, and redaction on every outbound string.
//!
//! The subscription key enters the process solely through
//! [`ENVIRONMENT_VARIABLE`], lives in zeroizing memory, and is attached only via
//! the [`SUBSCRIPTION_KEY_HEADER`] request header — never as a `?key=` query
//! parameter, which would land in proxy logs, redirect targets and error strings
//! that echo a URL. The credential type has no `Display` and a `Debug` that
//! prints [`REDACTION_PLACEHOLDER`], so no format string can render it, and
//! [`redact_secret`] is the single choke point every SportsDataIO error, log and
//! report string passes through first. See the "Credential rule" in
//! `signals/sports/nba/SPORTSDATAIO-TRUE-TRIAL-GATE.md`; the block comment below
//! records why that rule is narrower than the provider's documentation.

use std::fmt;

use reqwest::RequestBuilder;
use reqwest::header::{HeaderName, HeaderValue};
use thiserror::Error;
use zeroize::Zeroizing;

// --- SportsDataIO NBA credential boundary.
//
// The gate rule (`signals/sports/nba/SPORTSDATAIO-TRUE-TRIAL-GATE.md`, "Credential
// rule") is narrower than the provider's own documentation. SportsDataIO permits
// EITHER a `?key=` query parameter OR the `Ocp-Apim-Subscription-Key` header; Tiny
// uses only the header, because a query parameter lands in proxy logs, redirect
// `Location` values, browser history and any error string that echoes a URL.
//
// The consequences that shape this module:
//
//   * the value enters the process ONLY through the environment (`from_env`), and
//     is never a CLI argument, never a file this crate writes, never a constant;
//   * it lives in `Zeroizing<String>` so the plaintext is wiped on drop;
//   * `Debug` prints `<redacted>` and there is deliberately NO `Display`, so no
//     format string anywhere can render it, even by accident;
//   * `redact_secret` is applied to every error/log string on the SportsDataIO
//     path before it can reach stderr, a journal record or a report.
//
// The header NAME is a public, non-secret literal and appears in this file once;
// the secret-scan script must not flag it (gate SDIO-1 file list).

/// The one environment variable the harness reads the credential from. The value
/// is loaded into the shell from the macOS Keychain immediately before an approved
/// run and unset afterwards; this crate never stores, hashes or prefixes it.
pub const ENVIRONMENT_VARIABLE: &str = "SPORTSDATAIO_NBA_API_KEY";

/// The only authentication transport Tiny uses for SportsDataIO. Never `?key=`.
pub const SUBSCRIPTION_KEY_HEADER: &str = "Ocp-Apim-Subscription-Key";

/// What a redacted occurrence of the credential is replaced with.
pub const REDACTION_PLACEHOLDER: &str = "<redacted>";

/// Accepted credential length band. SportsDataIO issues 32-character hexadecimal
/// subscription keys; the floor also guarantees that redaction can never match a
/// trivially short substring of unrelated text, and the ceiling bounds the work
/// `redact_secret` does per error string.
const MINIMUM_KEY_BYTES: usize = 8;
const MAXIMUM_KEY_BYTES: usize = 256;

/// Environment-injected SportsDataIO subscription key. Held only in zeroizing
/// memory, never formatted, never persisted, never placed in a URL.
pub struct SportsDataIoCredential {
    key: Zeroizing<String>,
}

impl SportsDataIoCredential {
    /// Read the credential from `SPORTSDATAIO_NBA_API_KEY`. The error path never
    /// carries the value: a missing variable names only the variable, and an
    /// unusable value reports only that it is unusable.
    pub fn from_env() -> Result<Self, SportsDataIoAuthError> {
        let value = std::env::var(ENVIRONMENT_VARIABLE)
            .map_err(|_| SportsDataIoAuthError::MissingEnvironment(ENVIRONMENT_VARIABLE))?;
        Self::from_value(value)
    }

    /// Build a credential from an already-held value. Tests inject a SENTINEL
    /// through this constructor; production always goes through `from_env`.
    ///
    /// Rejects an empty or whitespace-only value, any value containing whitespace
    /// (a shell-quoting accident that would corrupt the header), any non-ASCII or
    /// non-graphic byte (which cannot be a legal header value), and any value
    /// outside the accepted length band.
    pub fn from_value(value: String) -> Result<Self, SportsDataIoAuthError> {
        let key = Zeroizing::new(value);
        if key.len() < MINIMUM_KEY_BYTES
            || key.len() > MAXIMUM_KEY_BYTES
            || !key.bytes().all(|byte| byte.is_ascii_graphic())
        {
            return Err(SportsDataIoAuthError::InvalidCredential);
        }
        Ok(Self { key })
    }

    /// Attach the subscription-key header to one outbound request. The header
    /// value is marked sensitive so reqwest's own debug/logging paths elide it,
    /// and it is built here (rather than deferred inside `RequestBuilder`) so a
    /// construction failure surfaces as our own valueless error.
    pub fn authorize(
        &self,
        request: RequestBuilder,
    ) -> Result<RequestBuilder, SportsDataIoAuthError> {
        let name = HeaderName::from_static("ocp-apim-subscription-key");
        let mut value = HeaderValue::from_str(self.key.as_str())
            .map_err(|_| SportsDataIoAuthError::InvalidCredential)?;
        value.set_sensitive(true);
        Ok(request.header(name, value))
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
    /// error page is entitled to echo the request headers it received -- which is
    /// exactly how the key would reach a mode-`0600` journal that is later retained
    /// as the evidence bundle. This is the body-side twin of `redact`, and the
    /// journal is the one sink the repository secret scanner cannot cover after the
    /// fact: its high-confidence marker needs the header NAME beside the value, and
    /// a journal line never carries the header name.
    ///
    /// Matching is over BYTES, not text. The credential is ASCII-graphic by
    /// construction, so an occurrence can never straddle a UTF-8 boundary and the
    /// ASCII placeholder can never create one: a UTF-8 body stays UTF-8, and a
    /// binary body stays byte-exact everywhere the key does not appear.
    ///
    /// `truncated` says the caller cut the body at a recording cap. A cut can
    /// bisect an occurrence and leave a PREFIX behind that exact-match replacement
    /// cannot see, and the gate's credential rule forbids storing even a prefix, so
    /// a truncated body additionally drops its last `key.len() - 1` bytes.
    ///
    /// Returns the bytes that may be recorded and whether a whole occurrence was
    /// replaced; the caller journals that as `body_redacted`.
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

/// First position of `needle` in `haystack`. An empty needle never matches: a
/// credential shorter than `MINIMUM_KEY_BYTES` is refused at construction, so this
/// guard exists only so the windowed scan below cannot be handed a zero width.
fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// `<redacted>` and nothing else. There is intentionally no `Display` impl.
impl fmt::Debug for SportsDataIoCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(REDACTION_PLACEHOLDER)
    }
}

/// Scrub the credential out of one error, log or report string. Applied on EVERY
/// SportsDataIO error path before the string can reach stderr, a journal record or
/// a report -- a provider or transport error is allowed to echo a request, and
/// this is the single choke point that guarantees it cannot echo the key.
pub fn redact_secret(text: &str, credential: &SportsDataIoCredential) -> String {
    credential.redact(text)
}

/// Why the SportsDataIO credential could not be established. Neither variant
/// carries the credential value.
#[derive(Debug, Error)]
pub enum SportsDataIoAuthError {
    /// `SPORTSDATAIO_NBA_API_KEY` is absent from the environment. Carries the
    /// variable name only, never its value.
    #[error("{0} is not set; load it from the approved credential alias before the run")]
    MissingEnvironment(&'static str),
    /// Deliberately valueless: the rejected credential is never echoed.
    #[error("the SportsDataIO credential is empty, malformed, or not a usable header value")]
    InvalidCredential,
}

#[cfg(test)]
mod tests {
    use super::{
        REDACTION_PLACEHOLDER, SUBSCRIPTION_KEY_HEADER, SportsDataIoCredential, redact_secret,
    };

    const SENTINEL: &str = "sentinel-sdio-key-0123456789abcdef";

    fn sentinel() -> SportsDataIoCredential {
        SportsDataIoCredential::from_value(SENTINEL.to_owned()).expect("sentinel is well formed")
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
                SportsDataIoCredential::from_value(rejected.to_owned()).is_err(),
                "value {rejected:?} must be refused"
            );
        }
        assert!(SportsDataIoCredential::from_value(SENTINEL.to_owned()).is_ok());
        assert!(SportsDataIoCredential::from_value("a".repeat(257)).is_err());
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
            "GET https://api.sportsdata.io/v3/nba/scores/json/teams failed with {SENTINEL} \
             (retry sent {SENTINEL})"
        );
        let redacted = redact_secret(&error, &credential);
        assert!(!redacted.contains(SENTINEL));
        assert_eq!(redacted.matches(REDACTION_PLACEHOLDER).count(), 2);
        assert!(redacted.starts_with("GET https://api.sportsdata.io/v3/nba/scores/json/teams"));
    }

    #[test]
    fn body_redaction_removes_the_credential_from_recorded_bytes() {
        let credential = sentinel();

        // The named threat: a provider error page that echoes the request headers.
        let echoed = format!(
            "{{\"Message\":\"Access denied\",\"Request\":{{\
             \"Ocp-Apim-Subscription-Key\":\"{SENTINEL}\",\"Retry\":\"{SENTINEL}\"}}}}"
        );
        let (recorded, replaced) = credential.redact_body(echoed.as_bytes(), false);
        assert!(replaced);
        let rendered = String::from_utf8(recorded).expect("an ASCII replacement keeps UTF-8");
        assert!(!rendered.contains(SENTINEL));
        assert_eq!(rendered.matches(REDACTION_PLACEHOLDER).count(), 2);
        assert!(rendered.starts_with(r#"{"Message":"Access denied""#));

        // A clean body is passed through byte for byte and is not flagged.
        let clean = br#"[{"TeamID":1,"Key":"DET"}]"#;
        let (recorded, replaced) = credential.redact_body(clean, false);
        assert!(!replaced);
        assert_eq!(recorded, clean);

        // A non-UTF-8 body keeps every byte that is not the credential.
        let mut binary = vec![0xff, 0xfe];
        binary.extend_from_slice(SENTINEL.as_bytes());
        binary.push(0x00);
        let (recorded, replaced) = credential.redact_body(&binary, false);
        assert!(replaced);
        assert_eq!(recorded[..2], [0xff, 0xfe]);
        assert_eq!(recorded[recorded.len() - 1], 0x00);
        assert_eq!(recorded.len(), 3 + REDACTION_PLACEHOLDER.len());

        // A TRUNCATED body can end in a bisected occurrence that exact matching
        // cannot see. The tail guard drops it; not even a prefix is recorded.
        let cut = SENTINEL.len() / 2;
        let lead = "provider error page: the request presented ";
        let mut bisected = lead.as_bytes().to_vec();
        bisected.extend_from_slice(&SENTINEL.as_bytes()[..cut]);
        let (recorded, replaced) = credential.redact_body(&bisected, true);
        assert!(!replaced, "no whole occurrence was present");
        let rendered = String::from_utf8(recorded).expect("ASCII");
        assert!(!rendered.contains(&SENTINEL[..cut]));
        assert!(rendered.starts_with("provider error page"));
        assert_eq!(rendered.len(), bisected.len() - (SENTINEL.len() - 1));

        // The guard costs at most key.len() - 1 bytes and never underflows.
        let (recorded, _) = credential.redact_body(b"short", true);
        assert!(recorded.is_empty());
    }

    #[test]
    fn missing_environment_error_names_only_the_variable() {
        // Construct the error directly: reading the process environment in a test
        // would race every other test in the binary.
        let error = super::SportsDataIoAuthError::MissingEnvironment(super::ENVIRONMENT_VARIABLE);
        let rendered = error.to_string();
        assert!(rendered.contains("SPORTSDATAIO_NBA_API_KEY"));
        assert!(!rendered.contains(SENTINEL));
        let invalid = super::SportsDataIoAuthError::InvalidCredential.to_string();
        assert!(!invalid.contains(SENTINEL));
    }

    #[test]
    fn authorized_request_carries_only_the_subscription_key_header() {
        let credential = sentinel();
        let client = reqwest::Client::builder()
            .build()
            .expect("client construction");
        let request = credential
            .authorize(client.get("https://api.sportsdata.io/v3/nba/scores/json/teams"))
            .expect("sentinel is a legal header value")
            .build()
            .expect("request builds");
        assert_eq!(request.url().query(), None);
        assert!(!request.url().as_str().contains("key="));
        let header = request
            .headers()
            .get(SUBSCRIPTION_KEY_HEADER)
            .expect("subscription header is attached");
        assert!(header.is_sensitive());
        assert_eq!(header.to_str().expect("ascii header"), SENTINEL);
        // The rendered request must not leak the key outside that one header.
        assert!(!format!("{request:?}").contains(SENTINEL));
    }
}
