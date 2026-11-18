//! Process-wide rustls client configurations for outbound WebSocket connects.
//!
//! Two configurations exist and no more: a TLS 1.3-only default, and a
//! 1.2-permitted variant admitted for the single named provider whose edge
//! cannot terminate 1.3. Each is a distinct process-wide `Arc`, so selecting
//! the wrong one is visible rather than a silent downgrade of every path, and
//! sharing the `Arc` keeps rustls's session cache warm across reconnects.

use std::sync::{Arc, OnceLock};

use rustls::ClientConfig;

use crate::CaptureStream;

/// Select the process-wide client configuration for `stream`'s endpoint.
///
/// This is the single owner of the version mapping: every outbound connect path
/// (capture, capture rotation, the live lane, the prediction live lane) selects
/// through here, so no path can silently pick the wrong TLS version for a
/// provider. `data-stream.binance.vision` terminates TLS at 1.2, so
/// [`CaptureStream::BinanceSpot`] gets the 1.2-permitted configuration and every
/// other endpoint keeps the TLS 1.3-only floor. Version preference is ordered,
/// so the spot mirror upgrades to 1.3 automatically if its edge ever supports
/// it.
///
/// DOCUMENTED, UNOBSERVED (probe P4): `fstream.binance.com`
/// ([`CaptureStream::BinanceUsdmPublic`]) has an unmeasured negotiated version,
/// so it keeps the 1.3-only floor rather than inheriting the spot mirror's
/// exception; a 1.2-only probe result would move it beside `BinanceSpot`.
pub(crate) fn client_config_for(stream: CaptureStream) -> Arc<ClientConfig> {
    match stream {
        CaptureStream::BinanceSpot => tls12_compatible_client_config(),
        _ => shared_client_config(),
    }
}

/// Process-wide TLS 1.3-only client configuration shared by every WebSocket
/// connect.
///
/// Pinning the protocol versions to TLS 1.3 enforces the negotiated-version
/// floor at client configuration: tokio-tungstenite's default connector would
/// otherwise admit TLS 1.2, which the unified rustls feature set still compiles
/// in. Sharing one `Arc` across reconnects keeps rustls's default in-memory
/// client session cache alive, so a reconnect can resume a prior TLS session.
pub(crate) fn shared_client_config() -> Arc<ClientConfig> {
    static CONFIG: OnceLock<Arc<ClientConfig>> = OnceLock::new();
    CONFIG
        .get_or_init(|| build_client_config(&[&rustls::version::TLS13]))
        .clone()
}

/// TLS 1.3-preferred, TLS 1.2-permitted configuration for providers whose
/// endpoints do not terminate TLS 1.3.
///
/// Binance's market-data-only distribution host (`data-stream.binance.vision`)
/// terminates TLS at 1.2: a 1.3-only handshake is refused outright, so the
/// process-wide 1.3 floor would make the endpoint permanently unreachable.
/// rustls's TLS 1.2 implementation offers only ECDHE key exchange with AEAD
/// suites (no CBC, no static RSA), and these connections carry public market
/// data with no credentials, so admitting 1.2 for this provider does not
/// weaken any authenticated or private path. Version preference is ordered, so
/// the endpoint upgrades to 1.3 automatically if its edge ever supports it.
pub(crate) fn tls12_compatible_client_config() -> Arc<ClientConfig> {
    static CONFIG: OnceLock<Arc<ClientConfig>> = OnceLock::new();
    CONFIG
        .get_or_init(|| build_client_config(&[&rustls::version::TLS13, &rustls::version::TLS12]))
        .clone()
}

/// Build a client configuration pinned to `versions`, trusting the webpki root
/// set and offering no client certificate. The ring crypto provider is
/// installed idempotently here so construction never depends on connect-path
/// ordering. Version preference is ordered, not pinned to a single version.
fn build_client_config(
    versions: &[&'static rustls::SupportedProtocolVersion],
) -> Arc<ClientConfig> {
    // The builder reads the process default crypto provider; install the ring
    // provider here (idempotent) so construction mirrors `install_crypto_provider`
    // and never depends on connect-path ordering.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut root_store = rustls::RootCertStore::empty();
    // The same webpki root set tokio-tungstenite's default connector trusts.
    root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = ClientConfig::builder_with_protocol_versions(versions)
        .with_root_certificates(root_store)
        .with_no_client_auth();
    Arc::new(config)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{client_config_for, shared_client_config, tls12_compatible_client_config};
    use crate::CaptureStream;

    #[test]
    fn client_config_for_maps_binance_spot_to_tls12_and_all_others_to_the_floor() {
        // The single mapping pinned: Binance spot's distribution host terminates
        // at 1.2 and gets the 1.2-permitted Arc; every other stream keeps the
        // 1.3-only floor. A regression that dropped the exception (or widened it)
        // is caught here rather than only at a live handshake.
        assert!(Arc::ptr_eq(
            &client_config_for(CaptureStream::BinanceSpot),
            &tls12_compatible_client_config()
        ));
        // Spot-check a representative set held on the floor, including the
        // sibling Binance futures host (`fstream.binance.com`, P4) that does NOT
        // inherit the spot exception.
        for stream in [
            CaptureStream::BinanceUsdmPublic,
            CaptureStream::BinanceUsdmRest,
            CaptureStream::BybitSpot,
            CaptureStream::BybitLinear,
            CaptureStream::CoinbaseCore,
            CaptureStream::KrakenCore,
            CaptureStream::KrakenUsdtReference,
            CaptureStream::ChainlinkAggregator,
            CaptureStream::PythHermesStream,
        ] {
            assert!(
                Arc::ptr_eq(&client_config_for(stream), &shared_client_config()),
                "{stream:?} must keep the TLS 1.3-only floor"
            );
            assert!(
                !Arc::ptr_eq(
                    &client_config_for(stream),
                    &tls12_compatible_client_config()
                ),
                "{stream:?} must not select the 1.2-permitted config"
            );
        }
    }

    /// Preflight (network): perform a REAL TLS handshake to Binance's spot
    /// distribution host with the configuration `client_config_for` selects,
    /// proving the 1.2 exception actually completes a handshake on a new host —
    /// the guarantee the pure-`Arc` unit tests cannot give. Ignored by default
    /// because it needs outbound network and the venue reachable.
    ///
    /// Run it as the network preflight on a new deployment host with:
    ///   `SIGNALS_PREFLIGHT_NETWORK=1 cargo test -p btc-feeds --release -- \
    ///        --ignored binance_spot_tls_handshake_succeeds_with_selected_config`
    /// (see `docs/deployment/production-btc/preflight.sh`). It is a no-op unless
    /// `SIGNALS_PREFLIGHT_NETWORK=1` so an accidental `--ignored` run off-network
    /// does not fail.
    #[test]
    #[ignore = "network: real TLS 1.2 handshake to data-stream.binance.vision (deployment preflight)"]
    fn binance_spot_tls_handshake_succeeds_with_selected_config() {
        if std::env::var("SIGNALS_PREFLIGHT_NETWORK").as_deref() != Ok("1") {
            eprintln!(
                "skipping: set SIGNALS_PREFLIGHT_NETWORK=1 to run the live Binance spot handshake"
            );
            return;
        }
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        use tokio_tungstenite::{Connector, connect_async_tls_with_config};

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime builds");
        runtime.block_on(async {
            let request = crate::capture::endpoint(CaptureStream::BinanceSpot)
                .into_client_request()
                .expect("binance spot endpoint is a valid request");
            let connector = Connector::Rustls(client_config_for(CaptureStream::BinanceSpot));
            let outcome = tokio::time::timeout(
                std::time::Duration::from_secs(15),
                connect_async_tls_with_config(
                    request,
                    Some(crate::capture::websocket_config()),
                    true,
                    Some(connector),
                ),
            )
            .await
            .expect("handshake completes within 15s");
            let (_socket, _response) = outcome.expect(
                "TLS handshake to data-stream.binance.vision succeeds with the selected config",
            );
        });
    }

    #[test]
    fn shared_client_config_reuses_one_arc_across_calls() {
        // A config is constructed and the same process-wide Arc is returned on
        // every call, so reconnects share one rustls session cache. The TLS 1.3
        // negotiated-version floor is enforced by construction (protocol versions
        // pinned to TLS13) and is verified in review, not by a live handshake.
        let first = shared_client_config();
        let second = shared_client_config();
        assert!(std::sync::Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn tls12_compatible_config_is_distinct_and_also_shared() {
        // The 1.2-permitted configuration is its own process-wide Arc: reused
        // across reconnects, and never the same object as the 1.3-only floor,
        // so a provider selection bug cannot silently downgrade the default.
        let compatible_first = tls12_compatible_client_config();
        let compatible_second = tls12_compatible_client_config();
        assert!(std::sync::Arc::ptr_eq(
            &compatible_first,
            &compatible_second
        ));
        assert!(!std::sync::Arc::ptr_eq(
            &compatible_first,
            &shared_client_config()
        ));
    }
}
