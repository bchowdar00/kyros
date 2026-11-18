//! Live BTC/USD price feed from Pyth Pro (Lazer) over WebSocket.
//!
//! This is the primary price source. It drives the same [`PriceFeed`] as
//! the Coinbase connector in [`super::feed`], so nothing downstream (the
//! schedulers, market makers, trader bots, chart windows, browser fanout)
//! knows which source is live. `main` picks Pyth whenever
//! `PYTH_PRO_API_KEY` is set and falls back to Coinbase otherwise.
//!
//! Design notes, mirroring the integration brief:
//!
//! * The official `pyth-lazer-client` maintains three redundant sockets
//!   against the three Lazer nodes, dedupes messages across them, and
//!   reconnects with exponential backoff. We do not reimplement any of it.
//! * We subscribe on [`Channel::RealTime`]: an update whenever the
//!   aggregate moves, throttled to at most one per millisecond. If the
//!   plan does not include real-time for the feed the server answers with
//!   a `subscriptionError`, and we fall back to the fastest fixed rate
//!   (50 ms) automatically.
//! * Only [`Format::LeUnsigned`] is requested; we consume off-chain and
//!   have no use for signed Solana/EVM payloads.
//! * `parsed: true` JSON delivery. The client's inbound channel is
//!   bounded, and [`PriceFeed::record`] is non-blocking (an atomic store,
//!   two short mutex pushes, a broadcast `send`), so handling a tick
//!   inline in the receive loop never stalls the socket reader.
//! * Prices arrive as an `i64` mantissa plus exponent (BTC/USD is `-8`);
//!   the conversion to `f64` happens at the very last step, when handing
//!   the tick to [`PriceFeed`], which is `f64` throughout.
//! * Latency (Pyth's `timestampUs` vs. our clock) and staleness (the
//!   feed's own `feedUpdateTimestamp` lagging the message timestamp) are
//!   logged on a sample so the operator can see the feed's health.

use std::sync::Arc;
use std::time::Duration;

use pyth_lazer_client::stream_client::PythLazerStreamClientBuilder;
use pyth_lazer_client::ws_connection::AnyResponse;
use pyth_lazer_protocol::api::{
    Channel, DeliveryFormat, Format, JsonBinaryEncoding, StreamUpdatedResponse, SubscribeRequest,
    SubscriptionId, SubscriptionParams, SubscriptionParamsRepr, WsResponse,
};
use pyth_lazer_protocol::time::FixedRate;
use pyth_lazer_protocol::{PriceFeedId, PriceFeedProperty};

use super::feed::{PriceFeed, unix_now_ms};

/// Name of the env var holding the Pyth Pro access token.
pub const API_KEY_ENV: &str = "PYTH_PRO_API_KEY";

/// Pyth Lazer streaming nodes. The docs require connecting to all of
/// them: single nodes drop during deploys.
pub const ENDPOINTS: [&str; 3] = [
    "wss://pyth-lazer-0.dourolabs.app/v1/stream",
    "wss://pyth-lazer-1.dourolabs.app/v1/stream",
    "wss://pyth-lazer-2.dourolabs.app/v1/stream",
];

/// `Crypto.BTC/USD` in Pyth's symbology.
pub const BTC_USD_FEED_ID: PriceFeedId = PriceFeedId(1);

/// Subscription id used for the real-time channel.
const SUB_REAL_TIME: SubscriptionId = SubscriptionId(1);
/// Subscription id used for the 50 ms fixed-rate fallback.
const SUB_FIXED_50MS: SubscriptionId = SubscriptionId(2);

/// Bounded capacity of the client's inbound message channel. Real-time
/// BTC runs at hundreds to low thousands of messages per second; this is
/// several seconds of headroom.
const CHANNEL_CAPACITY: usize = 10_000;

/// Interval between feed-health log lines (price, latency, tick rate).
const HEALTH_LOG_INTERVAL: Duration = Duration::from_secs(30);

/// Delay before rebuilding the client after its stream ends or fails to
/// start. The client already backs off on individual socket drops; this
/// only covers the whole-client failure path.
const RESTART_DELAY: Duration = Duration::from_secs(2);

/// Spawn a background task that keeps a Pyth Pro subscription for BTC/USD
/// alive and records every price update into `feed`. Runs forever.
pub fn spawn_pyth_feed(feed: Arc<PriceFeed>, access_token: String) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match run_pyth(&feed, access_token.clone()).await {
                Ok(()) => tracing::warn!("pyth feed stream ended; restarting"),
                Err(e) => tracing::warn!(error = %e, "pyth feed errored; restarting in 2s"),
            }
            tokio::time::sleep(RESTART_DELAY).await;
        }
    })
}

fn subscribe_request(id: SubscriptionId, channel: Channel) -> anyhow::Result<SubscribeRequest> {
    let params = SubscriptionParams::new(SubscriptionParamsRepr {
        price_feed_ids: Some(vec![BTC_USD_FEED_ID]),
        symbols: None,
        properties: vec![
            PriceFeedProperty::Price,
            PriceFeedProperty::Exponent,
            PriceFeedProperty::Confidence,
            PriceFeedProperty::FeedUpdateTimestamp,
        ],
        formats: vec![Format::LeUnsigned],
        delivery_format: DeliveryFormat::Json,
        json_binary_encoding: JsonBinaryEncoding::Base64,
        parsed: true,
        channel,
        ignore_invalid_feeds: true,
    })
    .map_err(|e| anyhow::anyhow!("invalid pyth subscription params: {e}"))?;
    Ok(SubscribeRequest {
        subscription_id: id,
        params,
    })
}

async fn run_pyth(feed: &PriceFeed, access_token: String) -> anyhow::Result<()> {
    let endpoints = ENDPOINTS
        .iter()
        .map(|e| e.parse())
        .collect::<Result<Vec<_>, _>>()?;

    let mut client = PythLazerStreamClientBuilder::new(access_token)
        .with_endpoints(endpoints)
        .with_num_connections(ENDPOINTS.len())
        .with_channel_capacity(CHANNEL_CAPACITY)
        .build()?;

    let mut rx = client.start().await?;
    client
        .subscribe(subscribe_request(SUB_REAL_TIME, Channel::RealTime)?)
        .await?;
    tracing::info!(feed_id = BTC_USD_FEED_ID.0, "pyth: subscribed BTC/USD on real_time");

    let mut active = SUB_REAL_TIME;
    let mut ticks_in_window: u64 = 0;
    let mut window_started = std::time::Instant::now();
    let mut first_logged = false;

    while let Some(msg) = rx.recv().await {
        let AnyResponse::Json(resp) = msg else {
            // We only requested JSON delivery; a binary frame means a
            // misconfiguration upstream. Ignore rather than crash.
            continue;
        };
        match resp {
            WsResponse::StreamUpdated(update) if update.subscription_id == active => {
                if let Some(tick) = extract_tick(&update) {
                    if let Some(conf) = tick.confidence {
                        feed.record_confidence(conf);
                    }
                    feed.record(tick.price);
                    ticks_in_window += 1;
                    let elapsed = window_started.elapsed();
                    if !first_logged || elapsed >= HEALTH_LOG_INTERVAL {
                        let secs = elapsed.as_secs_f64().max(f64::EPSILON);
                        tracing::info!(
                            price = tick.price,
                            latency_ms = tick.latency_ms,
                            carried_forward_ms = tick.carried_forward_ms,
                            conf = tick.confidence,
                            ticks_per_sec = format!("{:.1}", ticks_in_window as f64 / secs),
                            "pyth: feed health"
                        );
                        first_logged = true;
                        ticks_in_window = 0;
                        window_started = std::time::Instant::now();
                    }
                }
            }
            WsResponse::StreamUpdated(_) => {} // a subscription we already abandoned
            WsResponse::Subscribed(s) => {
                tracing::info!(sub = s.subscription_id.0, "pyth: subscription confirmed");
            }
            WsResponse::SubscribedWithInvalidFeedIdsIgnored(s) => {
                tracing::warn!(
                    sub = s.subscription_id.0,
                    ignored = ?s.ignored_invalid_feed_ids,
                    "pyth: subscription confirmed with ignored feeds"
                );
            }
            WsResponse::SubscriptionError(e) if e.subscription_id == SUB_REAL_TIME && active == SUB_REAL_TIME => {
                // The plan does not cover real_time for this feed. Fall back
                // to the fastest typed fixed rate.
                tracing::warn!(error = %e.error, "pyth: real_time rejected; falling back to fixed_rate@50ms");
                let _ = client.unsubscribe(SUB_REAL_TIME).await;
                client
                    .subscribe(subscribe_request(
                        SUB_FIXED_50MS,
                        Channel::FixedRate(FixedRate::RATE_50_MS),
                    )?)
                    .await?;
                active = SUB_FIXED_50MS;
            }
            WsResponse::SubscriptionError(e) => {
                anyhow::bail!("pyth subscription {} failed: {}", e.subscription_id.0, e.error);
            }
            WsResponse::Error(e) => {
                tracing::warn!(error = %e.error, "pyth: server error");
            }
            WsResponse::Unsubscribed(_) => {}
        }
    }
    Ok(())
}

/// A BTC/USD update decoded from a parsed Pyth payload.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PythTick {
    /// Dollar price, converted from mantissa/exponent at the last step.
    pub price: f64,
    /// Confidence interval in dollars, if reported.
    pub confidence: Option<f64>,
    /// Our clock minus Pyth's `timestampUs`, in ms. Negative means our
    /// clock is behind Pyth's.
    pub latency_ms: i64,
    /// How long the feed's own `feedUpdateTimestamp` lags the message
    /// timestamp, in ms. Non-zero means the price is being carried
    /// forward rather than freshly aggregated.
    pub carried_forward_ms: Option<u64>,
}

/// Pull the BTC/USD price out of a parsed stream update. Returns `None`
/// for messages without a parsed payload, without our feed, or without a
/// usable price/exponent pair.
pub fn extract_tick(update: &StreamUpdatedResponse) -> Option<PythTick> {
    let parsed = update.payload.parsed.as_ref()?;
    let feed = parsed
        .price_feeds
        .iter()
        .find(|f| f.price_feed_id == BTC_USD_FEED_ID)?;
    let exponent = feed.exponent?;
    let price = feed.price?.to_f64(exponent).ok()?;
    if !(price.is_finite() && price > 0.0) {
        return None;
    }
    let confidence = feed.confidence.and_then(|c| c.to_f64(exponent).ok());
    let msg_ms = parsed.timestamp_us.as_millis();
    let latency_ms = unix_now_ms() as i64 - msg_ms as i64;
    let carried_forward_ms = feed
        .feed_update_timestamp
        .map(|t| msg_ms.saturating_sub(t.as_millis()));
    Some(PythTick {
        price,
        confidence,
        latency_ms,
        carried_forward_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn update(json: &str) -> StreamUpdatedResponse {
        serde_json::from_str(json).expect("valid streamUpdated json")
    }

    #[test]
    fn decodes_price_from_mantissa_and_exponent() {
        let u = update(
            r#"{"subscriptionId":1,"parsed":{"timestampUs":"1700000000000000",
                "priceFeeds":[{"priceFeedId":1,"price":"11234567890000","exponent":-8,
                "confidence":100000000,"feedUpdateTimestamp":1700000000000000}]}}"#,
        );
        let t = extract_tick(&u).expect("tick");
        assert!((t.price - 112_345.678_9).abs() < 1e-6);
        assert!((t.confidence.unwrap() - 1.0).abs() < 1e-9);
        assert_eq!(t.carried_forward_ms, Some(0));
    }

    #[test]
    fn reports_carried_forward_gap() {
        let u = update(
            r#"{"subscriptionId":1,"parsed":{"timestampUs":"1700000000500000",
                "priceFeeds":[{"priceFeedId":1,"price":"10000000000000","exponent":-8,
                "feedUpdateTimestamp":1700000000000000}]}}"#,
        );
        assert_eq!(extract_tick(&u).unwrap().carried_forward_ms, Some(500));
    }

    #[test]
    fn ignores_other_feeds_and_missing_exponent() {
        let other = update(
            r#"{"subscriptionId":1,"parsed":{"timestampUs":"1700000000000000",
                "priceFeeds":[{"priceFeedId":2,"price":"1","exponent":-8}]}}"#,
        );
        assert!(extract_tick(&other).is_none());
        let no_exp = update(
            r#"{"subscriptionId":1,"parsed":{"timestampUs":"1700000000000000",
                "priceFeeds":[{"priceFeedId":1,"price":"1"}]}}"#,
        );
        assert!(extract_tick(&no_exp).is_none());
    }

    #[test]
    fn subscribe_request_serializes_real_time_channel() {
        let req = subscribe_request(SUB_REAL_TIME, Channel::RealTime).unwrap();
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["channel"], "real_time");
        assert_eq!(v["priceFeedIds"], serde_json::json!([1]));
        assert_eq!(v["deliveryFormat"], "json");
        assert_eq!(v["ignoreInvalidFeeds"], true);
    }
}
