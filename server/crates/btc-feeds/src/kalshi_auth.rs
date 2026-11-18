//! Kalshi WebSocket handshake authentication: RSA-PSS request signing from an
//! environment-configured key id and on-disk private key.
//!
//! The key id comes from the environment and the RSA key is read from an
//! absolute, symlink-refusing, owner-only path bounded at 64 KiB, accepted in
//! either PKCS#8 or PKCS#1 PEM. Both, plus every intermediate PEM/DER buffer and
//! each produced signature, live only in zeroizing memory. A fresh
//! `timestamp + "GET" + path` message is signed per connection attempt and the
//! three `kalshi-access-*` headers are marked sensitive, so they are redacted
//! from request debug output and never enter the raw journal. Every failure path
//! fails closed to [`KalshiAuthError`] before the socket is opened.

use std::fs::OpenOptions;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use ring::rand::SystemRandom;
use ring::signature::{RSA_PSS_SHA256, RsaKeyPair};
use thiserror::Error;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderName, HeaderValue, Request};
use zeroize::Zeroizing;

const KALSHI_WEBSOCKET_URL: &str = "wss://external-api-ws.kalshi.com/trade-api/ws/v2";
const KALSHI_WEBSOCKET_PATH: &str = "/trade-api/ws/v2";
/// WebSocket host the index socket dials. The path is configuration, because
/// the index endpoint's shape is settled by the R1 capture rather than by
/// documentation; the host is the one Kalshi publishes for every channel.
pub(crate) const KALSHI_WEBSOCKET_HOST: &str = "external-api-ws.kalshi.com";
const MAX_KEY_ID_BYTES: usize = 512;
const MAX_PRIVATE_KEY_BYTES: u64 = 64 * 1024;

/// Environment-only Kalshi RSA signer. Header values are never printable or captured.
pub(crate) struct KalshiAuthClient {
    key_id: Zeroizing<String>,
    signing_key: RsaKeyPair,
    random: SystemRandom,
}

impl KalshiAuthClient {
    pub(crate) fn from_env() -> Result<Self, KalshiAuthError> {
        let key_id = Zeroizing::new(
            std::env::var("KALSHI_API_KEY_ID")
                .map_err(|_| KalshiAuthError::MissingEnvironment("KALSHI_API_KEY_ID"))?,
        );
        let key_path = PathBuf::from(std::env::var_os("KALSHI_PRIVATE_KEY_PATH").ok_or(
            KalshiAuthError::MissingEnvironment("KALSHI_PRIVATE_KEY_PATH"),
        )?);
        if key_id.is_empty()
            || key_id.len() > MAX_KEY_ID_BYTES
            || key_id.trim() != key_id.as_str()
            || !key_id.bytes().all(|byte| byte.is_ascii_graphic())
        {
            return Err(KalshiAuthError::InvalidCredentials);
        }
        let pem = read_private_key(&key_path)?;
        let (encoding, der) = decode_private_key_pem(&pem)?;
        let signing_key = match encoding {
            PrivateKeyEncoding::Pkcs8 => RsaKeyPair::from_pkcs8(der.as_slice()),
            PrivateKeyEncoding::Pkcs1 => RsaKeyPair::from_der(der.as_slice()),
        }
        .map_err(|_| KalshiAuthError::InvalidCredentials)?;

        Ok(Self {
            key_id,
            signing_key,
            random: SystemRandom::new(),
        })
    }

    /// Build a fresh signed request for every connection attempt. The request is
    /// consumed directly by tungstenite and never enters the raw journal.
    pub(crate) fn websocket_request(&self) -> Result<Request<()>, KalshiAuthError> {
        let timestamp = unix_millis()?;
        let message = signing_message(timestamp);
        let mut signature_bytes = Zeroizing::new(vec![0; self.signing_key.public().modulus_len()]);
        self.signing_key
            .sign(
                &RSA_PSS_SHA256,
                &self.random,
                message.as_bytes(),
                signature_bytes.as_mut_slice(),
            )
            .map_err(|_| KalshiAuthError::Signing)?;
        let signature = Zeroizing::new(STANDARD.encode(signature_bytes.as_slice()));
        build_websocket_request(self.key_id.as_str(), signature.as_str(), timestamp)
    }

    /// Build a fresh signed request for a host and path other than the adopted
    /// market socket's.
    ///
    /// The market-socket helper above signs a compiled constant, which is what
    /// makes it impossible to point at another channel by accident. The index
    /// socket needs a second endpoint, so its host and path are explicit
    /// arguments and the signing message is built from the given `path` — the
    /// signature and the request can therefore never describe different paths.
    /// Both are bounded and must be a plain absolute path and a bare host.
    pub(crate) fn websocket_request_for(
        &self,
        host: &str,
        path: &str,
    ) -> Result<Request<()>, KalshiAuthError> {
        validate_host(host)?;
        validate_path(path)?;
        let timestamp = unix_millis()?;
        let signature = self.sign_message(&signing_message_for(path, timestamp))?;
        let url = format!("wss://{host}{path}");
        let mut request = url
            .into_client_request()
            .map_err(|_| KalshiAuthError::Request)?;
        insert_access_headers(
            &mut request,
            self.key_id.as_str(),
            signature.as_str(),
            timestamp,
        )?;
        Ok(request)
    }

    /// Sign one authenticated REST GET over `path`, **without** its query
    /// string, and return the three `kalshi-access-*` header values.
    ///
    /// This is the only signed REST helper in the crate: `kalshi::discover_market`
    /// is a plain unauthenticated GET. The values are zeroizing and are inserted
    /// as sensitive headers by the caller, so they never reach a log, a notice or
    /// the raw journal.
    pub(crate) fn sign_get(&self, path: &str) -> Result<SignedHeaders, KalshiAuthError> {
        validate_path(path)?;
        let timestamp = unix_millis()?;
        let signature = self.sign_message(&signing_message_for(path, timestamp))?;
        Ok(SignedHeaders {
            key_id: self.key_id.clone(),
            signature,
            timestamp,
        })
    }

    fn sign_message(&self, message: &str) -> Result<Zeroizing<String>, KalshiAuthError> {
        let mut signature_bytes = Zeroizing::new(vec![0; self.signing_key.public().modulus_len()]);
        self.signing_key
            .sign(
                &RSA_PSS_SHA256,
                &self.random,
                message.as_bytes(),
                signature_bytes.as_mut_slice(),
            )
            .map_err(|_| KalshiAuthError::Signing)?;
        Ok(Zeroizing::new(STANDARD.encode(signature_bytes.as_slice())))
    }
}

/// The three `kalshi-access-*` values for one signed REST request.
///
/// The key id and signature stay in zeroizing memory for the value's whole
/// life; nothing here implements `Debug` or `Display`, so neither can be
/// printed by accident.
pub(crate) struct SignedHeaders {
    pub(crate) key_id: Zeroizing<String>,
    pub(crate) signature: Zeroizing<String>,
    pub(crate) timestamp: u64,
}

/// Longest accepted signing path, in bytes.
const MAX_SIGNING_PATH_BYTES: usize = 256;
/// Longest accepted host, in bytes.
const MAX_HOST_BYTES: usize = 128;

/// A signing path must be a bare absolute path: no scheme, no host, no query
/// string (Kalshi signs the path only), no whitespace, printable ASCII.
fn validate_path(path: &str) -> Result<(), KalshiAuthError> {
    if path.is_empty()
        || path.len() > MAX_SIGNING_PATH_BYTES
        || !path.starts_with('/')
        || path.contains('?')
        || path.contains('#')
        || path.contains("//")
        || !path.bytes().all(|byte| byte.is_ascii_graphic())
    {
        return Err(KalshiAuthError::Request);
    }
    Ok(())
}

/// A host must be a bare host name: no scheme, no path, no credentials, no port.
fn validate_host(host: &str) -> Result<(), KalshiAuthError> {
    if host.is_empty()
        || host.len() > MAX_HOST_BYTES
        || !host
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'.' || byte == b'-')
    {
        return Err(KalshiAuthError::Request);
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PrivateKeyEncoding {
    Pkcs8,
    Pkcs1,
}

fn decode_private_key_pem(
    pem: &str,
) -> Result<(PrivateKeyEncoding, Zeroizing<Vec<u8>>), KalshiAuthError> {
    const FORMATS: [(&str, &str, PrivateKeyEncoding); 2] = [
        (
            "-----BEGIN PRIVATE KEY-----",
            "-----END PRIVATE KEY-----",
            PrivateKeyEncoding::Pkcs8,
        ),
        (
            "-----BEGIN RSA PRIVATE KEY-----",
            "-----END RSA PRIVATE KEY-----",
            PrivateKeyEncoding::Pkcs1,
        ),
    ];

    let pem = pem.trim();
    for (begin, end, encoding) in FORMATS {
        let Some(body) = pem
            .strip_prefix(begin)
            .and_then(|body| body.strip_suffix(end))
        else {
            continue;
        };
        let mut base64_body = Zeroizing::new(String::with_capacity(body.len()));
        for character in body.chars() {
            if character.is_ascii_whitespace() {
                continue;
            }
            if !character.is_ascii() {
                return Err(KalshiAuthError::InvalidCredentials);
            }
            base64_body.push(character);
        }
        let der = Zeroizing::new(
            STANDARD
                .decode(base64_body.as_bytes())
                .map_err(|_| KalshiAuthError::InvalidCredentials)?,
        );
        if der.is_empty() || der.len() as u64 > MAX_PRIVATE_KEY_BYTES {
            return Err(KalshiAuthError::InvalidCredentials);
        }
        return Ok((encoding, der));
    }
    Err(KalshiAuthError::InvalidCredentials)
}

fn signing_message(timestamp: u64) -> String {
    signing_message_for(KALSHI_WEBSOCKET_PATH, timestamp)
}

/// The one signing-message shape Kalshi documents: `{timestamp}GET{path}`.
fn signing_message_for(path: &str, timestamp: u64) -> String {
    format!("{timestamp}GET{path}")
}

fn build_websocket_request(
    key_id: &str,
    signature: &str,
    timestamp: u64,
) -> Result<Request<()>, KalshiAuthError> {
    let mut request = KALSHI_WEBSOCKET_URL
        .into_client_request()
        .map_err(|_| KalshiAuthError::Request)?;
    insert_access_headers(&mut request, key_id, signature, timestamp)?;
    Ok(request)
}

fn insert_access_headers(
    request: &mut Request<()>,
    key_id: &str,
    signature: &str,
    timestamp: u64,
) -> Result<(), KalshiAuthError> {
    insert_sensitive_header(request, "kalshi-access-key", key_id)?;
    insert_sensitive_header(request, "kalshi-access-signature", signature)?;
    insert_sensitive_header(request, "kalshi-access-timestamp", &timestamp.to_string())?;
    Ok(())
}

fn read_private_key(path: &Path) -> Result<Zeroizing<String>, KalshiAuthError> {
    if !path.is_absolute() {
        return Err(KalshiAuthError::InvalidCredentialFile);
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        options.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    }
    let mut file = options
        .open(path)
        .map_err(|_| KalshiAuthError::InvalidCredentialFile)?;
    let metadata = file
        .metadata()
        .map_err(|_| KalshiAuthError::InvalidCredentialFile)?;
    if !metadata.file_type().is_file()
        || metadata.len() == 0
        || metadata.len() > MAX_PRIVATE_KEY_BYTES
    {
        return Err(KalshiAuthError::InvalidCredentialFile);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(KalshiAuthError::InsecureCredentialFile);
        }
    }
    let mut pem = String::new();
    (&mut file)
        .take(MAX_PRIVATE_KEY_BYTES + 1)
        .read_to_string(&mut pem)
        .map_err(|_| KalshiAuthError::InvalidCredentialFile)?;
    if pem.is_empty() || pem.len() as u64 > MAX_PRIVATE_KEY_BYTES {
        return Err(KalshiAuthError::InvalidCredentialFile);
    }
    Ok(Zeroizing::new(pem))
}

pub(crate) const fn websocket_endpoint() -> &'static str {
    KALSHI_WEBSOCKET_URL
}

fn insert_sensitive_header(
    request: &mut Request<()>,
    name: &'static str,
    value: &str,
) -> Result<(), KalshiAuthError> {
    let name = HeaderName::from_static(name);
    let mut value =
        HeaderValue::from_str(value).map_err(|_| KalshiAuthError::InvalidCredentials)?;
    value.set_sensitive(true);
    request.headers_mut().insert(name, value);
    Ok(())
}

fn unix_millis() -> Result<u64, KalshiAuthError> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| KalshiAuthError::Clock)?
        .as_millis();
    u64::try_from(millis).map_err(|_| KalshiAuthError::Clock)
}

/// Why Kalshi authentication failed. No variant carries key-id, PEM, DER or
/// signature material, and none names the private-key path.
#[derive(Debug, Error)]
pub enum KalshiAuthError {
    /// `KALSHI_API_KEY_ID` or `KALSHI_PRIVATE_KEY_PATH` is absent from the
    /// environment. Carries the variable name only, never its value.
    #[error("required environment variable {0} is not available")]
    MissingEnvironment(&'static str),
    /// The key id was empty, whitespace-padded, over 512 bytes or not printable
    /// ASCII; or the PEM carried no supported header pair, non-ASCII body,
    /// invalid base64, or DER that was empty or over 64 KiB; or the DER did not
    /// parse as a PKCS#8 or PKCS#1 RSA key; or a header value was unusable.
    #[error("Kalshi credentials are empty, malformed, or outside configured bounds")]
    InvalidCredentials,
    /// The private-key path was relative, could not be opened without following
    /// a symlink, was not a regular file, or was empty or over 64 KiB.
    #[error("Kalshi private-key file is missing, not regular, or outside configured bounds")]
    InvalidCredentialFile,
    /// On Unix, the private-key file grants any group or other permission bit.
    #[error(
        "Kalshi private-key file must deny group and other permissions (mode 0600 or stricter)"
    )]
    InsecureCredentialFile,
    /// The fixed WebSocket URL could not be turned into a client request.
    #[error("Kalshi authenticated WebSocket request construction failed")]
    Request,
    /// The RSA-PSS-SHA256 signature over the handshake message could not be
    /// produced.
    #[error("Kalshi RSA-PSS handshake signing failed")]
    Signing,
    /// The system clock was before the Unix epoch or past `u64::MAX`
    /// milliseconds, so no signing timestamp could be formed.
    #[error("a Kalshi signing timestamp could not be generated")]
    Clock,
}

#[cfg(test)]
mod tests {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;

    use std::path::Path;

    use super::{
        KALSHI_WEBSOCKET_PATH, KalshiAuthError, PrivateKeyEncoding, build_websocket_request,
        decode_private_key_pem, read_private_key, signing_message, signing_message_for,
        validate_host, validate_path,
    };

    #[test]
    fn private_key_path_must_be_absolute() {
        assert!(matches!(
            read_private_key(Path::new("relative-key.pem")),
            Err(KalshiAuthError::InvalidCredentialFile)
        ));
    }

    #[test]
    fn private_key_pem_labels_decode_only_the_bounded_body() {
        let expected = b"synthetic DER, not a key";
        for (begin, end, encoding) in [
            (
                "-----BEGIN PRIVATE KEY-----",
                "-----END PRIVATE KEY-----",
                PrivateKeyEncoding::Pkcs8,
            ),
            (
                "-----BEGIN RSA PRIVATE KEY-----",
                "-----END RSA PRIVATE KEY-----",
                PrivateKeyEncoding::Pkcs1,
            ),
        ] {
            let pem = format!("{begin}\n{}\n{end}\n", STANDARD.encode(expected));
            let (actual_encoding, actual) =
                decode_private_key_pem(&pem).expect("bounded PEM body decodes");
            assert_eq!(actual_encoding, encoding);
            assert_eq!(actual.as_slice(), expected);
        }
        assert!(matches!(
            decode_private_key_pem("-----BEGIN PRIVATE KEY-----\n%%%\n-----END PRIVATE KEY-----"),
            Err(KalshiAuthError::InvalidCredentials)
        ));
    }

    #[test]
    fn handshake_message_and_headers_are_exact_and_sensitive() {
        let timestamp = 1_786_729_610_123;
        assert_eq!(
            signing_message(timestamp),
            format!("{timestamp}GET{KALSHI_WEBSOCKET_PATH}")
        );
        let request = build_websocket_request("test-key-id", "test-signature", timestamp)
            .expect("WebSocket request is constructed");
        for name in [
            "kalshi-access-key",
            "kalshi-access-signature",
            "kalshi-access-timestamp",
        ] {
            assert!(
                request.headers()[name].is_sensitive(),
                "{name} must be marked sensitive"
            );
        }
        assert_eq!(request.headers()["kalshi-access-key"], "test-key-id");
        assert_eq!(
            request.headers()["kalshi-access-signature"],
            "test-signature"
        );
        assert_eq!(
            request.headers()["kalshi-access-timestamp"],
            timestamp.to_string()
        );
    }

    /// The index socket signs the path it dials. A signing message built from
    /// another path than the request's would sign one channel and open another,
    /// so the two are derived from the same argument and the shape is pinned.
    #[test]
    fn index_signing_message_uses_the_given_path() {
        let timestamp = 1_786_729_610_123;
        assert_eq!(
            signing_message_for("/cfbenchmarks_value", timestamp),
            format!("{timestamp}GET/cfbenchmarks_value")
        );
        assert_eq!(
            signing_message(timestamp),
            signing_message_for(KALSHI_WEBSOCKET_PATH, timestamp)
        );
    }

    /// Kalshi signs the path WITHOUT its query string, and the helper is the
    /// only place that could smuggle a scheme, a host or a query into a signed
    /// message. Each of those is refused rather than normalized.
    #[test]
    fn signing_paths_and_hosts_are_bare() {
        for path in [
            "",
            "cfbenchmarks_value",
            "/cfbenchmarks/history/values?id=BRTI",
            "/cfbenchmarks#fragment",
            "//cfbenchmarks_value",
            "/cfbenchmarks value",
            "/cfbenchmarks\u{00e9}",
        ] {
            assert!(
                matches!(validate_path(path), Err(KalshiAuthError::Request)),
                "{path:?} must be refused"
            );
        }
        assert!(validate_path("/trade-api/v2/cfbenchmarks/values").is_ok());

        for host in [
            "",
            "wss://external-api-ws.kalshi.com",
            "external-api-ws.kalshi.com/path",
            "external-api-ws.kalshi.com:443",
            "user@external-api-ws.kalshi.com",
        ] {
            assert!(
                matches!(validate_host(host), Err(KalshiAuthError::Request)),
                "{host:?} must be refused"
            );
        }
        assert!(validate_host("external-api-ws.kalshi.com").is_ok());
    }
}
