//! The shared outbound HTTP boundary for every provider REST call.
//!
//! One client builder enforces the crate's transport rules in a single place:
//! HTTPS only, no redirects followed, no referer, no proxy, a TLS version
//! floor, and bounded connect, request and idle-pool timeouts. Response bodies
//! are read through a byte-bounded reader so a provider cannot force unbounded
//! allocation, and every failure collapses into [`SecureHttpError`].

use std::time::Duration;

use reqwest::{Client, Response, redirect::Policy};
use sha2::{Digest, Sha256};
use thiserror::Error;

/// TCP + TLS connect deadline for a provider request.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// End-to-end deadline for a provider request, including body read.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);
/// Idle connection lifetime in the pool, and the TCP keepalive interval.
const IDLE_POOL_TIMEOUT: Duration = Duration::from_secs(30);

/// The default provider HTTP client: TLS 1.3 only. Every provider uses this
/// unless it has a documented, endpoint-forced exception.
pub(crate) fn client() -> Result<Client, SecureHttpError> {
    build_client(reqwest::tls::Version::TLS_1_3)
}

/// TLS 1.3-preferred, TLS 1.2-permitted HTTP client for providers whose
/// endpoints do not terminate TLS 1.3. A SEPARATE constructor, never the
/// default, so a provider selection bug cannot silently downgrade every path.
///
/// This is the HTTP analogue of `websocket_tls::tls12_compatible_client_config`
/// and is admitted on exactly the same reasoning. Two of ESPN's three daily-crawl
/// hosts terminate TLS at 1.2 and refuse a 1.3-only handshake outright, so the
/// 1.3 floor makes them permanently unreachable (measured 2026-08-31:
/// `site.api.espn.com` negotiates TLS 1.3, while `sports.core.api.espn.com` and
/// `site.web.api.espn.com` both cap at TLS 1.2 with ECDHE-RSA-AES128-GCM-SHA256;
/// under the 1.3-only client 145 of 177 crawl requests failed, and every failure
/// was on one of those two hosts). rustls's TLS 1.2 implementation offers only
/// ECDHE key exchange with AEAD suites (no CBC, no static RSA); these requests
/// are keyless public GETs that send no credential, no cookie and no user data,
/// so admitting 1.2 for them weakens no authenticated or private path. The
/// version floor is a MINIMUM, not a pin, so these hosts upgrade to 1.3
/// automatically if ESPN's edge ever supports it.
pub(crate) fn tls12_compatible_client() -> Result<Client, SecureHttpError> {
    build_client(reqwest::tls::Version::TLS_1_2)
}

/// Build a provider client with `minimum_tls` as the negotiated-version floor.
/// The floor is a minimum, not a pin, so an endpoint that later supports a
/// higher version upgrades automatically.
fn build_client(minimum_tls: reqwest::tls::Version) -> Result<Client, SecureHttpError> {
    Client::builder()
        .https_only(true)
        .redirect(Policy::none())
        .referer(false)
        .no_proxy()
        .min_tls_version(minimum_tls)
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .tcp_keepalive(Some(IDLE_POOL_TIMEOUT))
        .tcp_nodelay(true)
        .pool_idle_timeout(IDLE_POOL_TIMEOUT)
        .pool_max_idle_per_host(2)
        .user_agent("tiny-signals-prototype/0.1")
        .build()
        .map_err(|_| SecureHttpError::ClientBuild)
}

/// Read one bounded response body, requiring a 2xx status.
///
/// Returns `HttpStatus` for any other status and `ResponseTooLarge` once the
/// declared or streamed length passes `maximum_bytes`.
pub(crate) async fn bounded_body(
    response: Response,
    maximum_bytes: usize,
) -> Result<Vec<u8>, SecureHttpError> {
    let (status, body) = bounded_body_with_status(response, maximum_bytes).await?;
    if !(200..300).contains(&status) {
        return Err(SecureHttpError::HttpStatus(status));
    }
    Ok(body)
}

/// Read one bounded response even when its status is non-successful.
///
/// Discovery journals use this variant so a provider error response remains
/// self-contained evidence instead of being collapsed into a network failure.
pub(crate) async fn bounded_body_with_status(
    mut response: Response,
    maximum_bytes: usize,
) -> Result<(u16, Vec<u8>), SecureHttpError> {
    let status = response.status().as_u16();
    if response
        .content_length()
        .is_some_and(|length| length > maximum_bytes as u64)
    {
        return Err(SecureHttpError::ResponseTooLarge { maximum_bytes });
    }

    let mut body = Vec::with_capacity(
        response
            .content_length()
            .and_then(|length| usize::try_from(length).ok())
            .unwrap_or(0)
            .min(maximum_bytes),
    );
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| SecureHttpError::ResponseRead)?
    {
        if body.len().saturating_add(chunk.len()) > maximum_bytes {
            return Err(SecureHttpError::ResponseTooLarge { maximum_bytes });
        }
        body.extend_from_slice(&chunk);
    }
    Ok((status, body))
}

/// Lowercase hex SHA-256 of `bytes`, used to fingerprint response payloads in
/// discovery journals.
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[derive(Debug, Error)]
/// Failure modes of the shared HTTP boundary.
pub(crate) enum SecureHttpError {
    /// Returned when the reqwest client cannot be built, typically because the
    /// requested TLS version floor is unavailable in this build.
    #[error("secure HTTP client construction failed")]
    ClientBuild,
    /// Returned by [`bounded_body`] when the response status is outside 2xx.
    /// Carries the status code.
    #[error("provider HTTP request returned status {0}")]
    HttpStatus(u16),
    /// Returned when the declared `Content-Length` or the streamed body passes
    /// the caller-supplied byte cap; the body is abandoned mid-read.
    #[error("provider HTTP response exceeded {maximum_bytes} bytes")]
    ResponseTooLarge { maximum_bytes: usize },
    /// Returned when a body chunk fails to arrive: connection reset, timeout,
    /// or a transport-level decode error.
    #[error("provider HTTP response could not be read")]
    ResponseRead,
}
