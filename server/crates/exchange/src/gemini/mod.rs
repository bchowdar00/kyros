//! Gemini's prediction markets, read from its public API.
//!
//! Nothing in this module holds a key or names an account: [`rest`]
//! refuses any path that is not public market data, and [`ws`] refuses
//! any stream that is.

pub mod audit;
pub mod driver;
pub mod env;
pub mod index_feed;
pub mod mirror;
pub mod mirror_task;
pub mod rest;
pub mod state;
pub mod strikes;
pub mod types;
pub mod venue;
pub mod ws;

/// The socket stream of the index Gemini settles BTC contracts on.
pub const BTC_INDEX_STREAM: &str = "lukka:LS-LRRR-XBTUSD@indexPrice";
/// That index's id in the stream's messages.
pub const BTC_INDEX_ID: &str = "LRRR_XBT";
/// The socket stream that carries every contract's strike and status.
pub const CONTRACT_STATUS_STREAM: &str = "contractStatus";

pub use env::GeminiEnv;
pub use rest::list_up_windows;
pub use state::{GeminiSeriesView, GeminiState, gemini_series_for};
pub use types::{LocalBook, UpWindow, WsEvent};
pub use ws::PublicSocket;

#[cfg(test)]
mod live {
    use super::*;
    use std::time::Duration;

    /// Against Gemini itself, with no key: `cargo test -p exchange --lib
    /// gemini::live -- --ignored`. Not part of the normal run, which
    /// never leaves this machine.
    #[tokio::test]
    #[ignore = "talks to Gemini's public API"]
    async fn the_public_client_reads_a_live_window_its_book_and_the_index() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let env = env::PRODUCTION;
        let windows = list_up_windows(&env, "BTC05M").await.expect("event list");
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let live = windows
            .iter()
            .find(|w| w.starts_at_ms <= now_ms && now_ms < w.expires_at_ms)
            .unwrap_or_else(|| panic!("no window is live among {} listed", windows.len()));
        eprintln!("live window: {live:?}");
        assert_eq!(live.expires_at_ms - live.starts_at_ms, 300_000);

        let (socket, mut events) = PublicSocket::spawn(env);
        socket.subscribe(vec![
            format!("{}@depth@100ms", live.symbol),
            "contractStatus".into(),
            "lukka:LS-LRRR-XBTUSD@indexPrice".into(),
        ]);
        let (mut connected, mut snapshot, mut index) = (false, false, false);
        let all = async {
            while !(connected && snapshot && index) {
                match events.recv().await.expect("event channel") {
                    WsEvent::Connected => connected = true,
                    WsEvent::Depth { symbol, snapshot: true, bids, asks, .. } => {
                        assert_eq!(symbol, live.symbol);
                        eprintln!("snapshot: {} bids, {} asks", bids.len(), asks.len());
                        snapshot = true;
                    }
                    WsEvent::Index { index: name, price, .. } => {
                        eprintln!("index {name} {price}");
                        index = true;
                    }
                    WsEvent::Disconnected => panic!("dropped during the check"),
                    _ => {}
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(20), all).await.expect("live events in 20 s");
    }
}
