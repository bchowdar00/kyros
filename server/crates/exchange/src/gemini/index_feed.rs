//! The BTC price every chart, strike distance and fair value reads:
//! the index Gemini settles on, from Gemini's public socket.

use std::sync::Arc;

use tokio::sync::broadcast::{self, error::RecvError};

use super::types::WsEvent;
use crate::pricing::feed::PriceFeed;

/// Write every price of `index` (Gemini's index id, e.g. `LRRR_XBT`)
/// into `feed` until the socket is gone.
pub fn spawn_gemini_index_feed(
    mut events: broadcast::Receiver<WsEvent>,
    feed: Arc<PriceFeed>,
    index: &'static str,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match events.recv().await {
                Ok(WsEvent::Index { index: id, price, .. }) if id == index => feed.record(price),
                Ok(_) | Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => break,
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_named_index_reaches_the_feed_and_no_other_does() {
        let (tx, rx) = broadcast::channel(8);
        let feed = PriceFeed::new();
        let task = spawn_gemini_index_feed(rx, feed.clone(), "LRRR_XBT");
        tx.send(WsEvent::Index { index: "LRRR_ETH".into(), price: 3_000.0, ts_ms: 1_795_021_353_000 }).unwrap();
        tx.send(WsEvent::Index { index: "LRRR_XBT".into(), price: 62_766.298, ts_ms: 1_795_021_354_000 }).unwrap();
        tx.send(WsEvent::Connected).unwrap();
        drop(tx);
        task.await.unwrap();
        assert_eq!(feed.latest(), Some(62_766.298));
    }
}
