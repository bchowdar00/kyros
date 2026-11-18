//! Bounded Coinbase Exchange REST recovery for a detected BTC-USD trade gap.
//!
//! When the WebSocket tape skips trade ids, this fetches the missing range from
//! the public `products/BTC-USD/trades` endpoint and hands it to the adapter as
//! a backfill. Paging starts at the latest page and follows only the opaque
//! `CB-AFTER` cursors Coinbase returns — a trade id is never assumed to be a
//! valid cursor — with repeated cursors and oversized pages rejected. Requests
//! revalidate rather than replay Coinbase's five-second public cache copy, and
//! a range that is still short is retried over six bounded delays.
//!
//! Recovery is fail-closed: unless the recovered trades form the exact
//! contiguous `after+1 ..= through` run, nothing is admitted, the adapter is
//! left untouched, and live publication stays closed.
//!
//! This is the live runner's transport, not part of the parsing library: a
//! consumer with its own HTTP stack fetches the range itself and calls
//! [`CoinbaseAdapter::apply_trade_backfill`](crate::coinbase::CoinbaseAdapter::apply_trade_backfill)
//! directly.

use std::collections::BTreeSet;
use std::time::Duration;

use reqwest::header::{ACCEPT, CACHE_CONTROL, HeaderValue};
use thiserror::Error;
use tokio::time::sleep;

use crate::coinbase::{CoinbaseAdapter, CoinbaseRestTrade};
use crate::secure_http::{self, SecureHttpError};
use crate::{AdapterError, IngestOutcome};

const TRADES_URL: &str = "https://api.exchange.coinbase.com/products/BTC-USD/trades";
const PAGE_SIZE: usize = 1_000;
const MAX_BACKFILL_TRADES: usize = 100_000;
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const MAX_CURSOR_BYTES: usize = 128;
const PAGINATION_HEADROOM_PAGES: usize = 8;
// Live responses exposed a five-second public cache window for this resource.
// Recovery is exceptional and remains fail-closed after this retry bound.
const CONSISTENCY_RETRY_DELAYS: [Duration; 6] = [
    Duration::from_millis(100),
    Duration::from_millis(250),
    Duration::from_millis(500),
    Duration::from_millis(1_000),
    Duration::from_millis(2_000),
    Duration::from_millis(2_500),
];

/// Fixed-host, bounded Coinbase Exchange REST recovery client.
pub struct CoinbaseRestClient {
    client: reqwest::Client,
}

impl CoinbaseRestClient {
    /// Build a recovery client on the shared secure-HTTP boundary. Fails with
    /// [`CoinbaseRestError::Http`] if that boundary cannot be constructed.
    pub fn new() -> Result<Self, CoinbaseRestError> {
        Ok(Self {
            client: secure_http::client()?,
        })
    }

    /// Recover and validate every missing trade before reopening live publication.
    pub async fn recover_trade_gap(
        &self,
        adapter: &mut CoinbaseAdapter,
        after_trade_id: u64,
        through_trade_id: u64,
    ) -> Result<IngestOutcome, CoinbaseRestError> {
        let trades = self
            .fetch_trade_range(after_trade_id, through_trade_id)
            .await?;
        let received_at = crate::raw::now_timestamp();
        adapter
            .apply_trade_backfill(after_trade_id, through_trade_id, &received_at, trades)
            .map_err(CoinbaseRestError::Adapter)
    }

    async fn fetch_trade_range(
        &self,
        after_trade_id: u64,
        through_trade_id: u64,
    ) -> Result<Vec<CoinbaseRestTrade>, CoinbaseRestError> {
        fetch_trade_range_with_retry_from(
            &HttpTradePageSource {
                client: &self.client,
            },
            after_trade_id,
            through_trade_id,
            &CONSISTENCY_RETRY_DELAYS,
        )
        .await
    }
}

pub(crate) trait CoinbaseTradeRecovery {
    async fn recover(
        &self,
        adapter: &mut CoinbaseAdapter,
        after_trade_id: u64,
        through_trade_id: u64,
    ) -> Result<IngestOutcome, CoinbaseRestError>;
}

impl CoinbaseTradeRecovery for CoinbaseRestClient {
    async fn recover(
        &self,
        adapter: &mut CoinbaseAdapter,
        after_trade_id: u64,
        through_trade_id: u64,
    ) -> Result<IngestOutcome, CoinbaseRestError> {
        self.recover_trade_gap(adapter, after_trade_id, through_trade_id)
            .await
    }
}

struct TradePage {
    trades: Vec<CoinbaseRestTrade>,
    next_cursor: Option<String>,
}

trait TradePageSource {
    async fn fetch(&self, cursor: Option<&str>) -> Result<TradePage, CoinbaseRestError>;
}

struct HttpTradePageSource<'a> {
    client: &'a reqwest::Client,
}

impl TradePageSource for HttpTradePageSource<'_> {
    async fn fetch(&self, cursor: Option<&str>) -> Result<TradePage, CoinbaseRestError> {
        let mut request = self
            .client
            .get(TRADES_URL)
            .header(ACCEPT, HeaderValue::from_static("application/json"))
            // The public trade response was observed as cacheable for five seconds.
            // Gap recovery must revalidate instead of replaying a stale edge copy.
            .header(CACHE_CONTROL, HeaderValue::from_static("no-cache"))
            .query(&[("limit", PAGE_SIZE)]);
        if let Some(cursor) = cursor {
            request = request.query(&[("after", cursor)]);
        }
        let response = request.send().await.map_err(|_| CoinbaseRestError::Http)?;
        let next_cursor = match response.headers().get("cb-after") {
            Some(value) => {
                let value = value
                    .to_str()
                    .map_err(|_| CoinbaseRestError::InvalidResponse)?;
                if value.is_empty() || value.len() > MAX_CURSOR_BYTES {
                    return Err(CoinbaseRestError::InvalidResponse);
                }
                Some(value.to_owned())
            }
            None => None,
        };
        let body = secure_http::bounded_body(response, MAX_RESPONSE_BYTES).await?;
        let trades =
            serde_json::from_slice(&body).map_err(|_| CoinbaseRestError::InvalidResponse)?;
        Ok(TradePage {
            trades,
            next_cursor,
        })
    }
}

async fn fetch_trade_range_from<S: TradePageSource>(
    source: &S,
    after_trade_id: u64,
    through_trade_id: u64,
) -> Result<Vec<CoinbaseRestTrade>, CoinbaseRestError> {
    let expected = through_trade_id
        .checked_sub(after_trade_id)
        .and_then(|count| usize::try_from(count).ok())
        .ok_or(CoinbaseRestError::InvalidRange)?;
    if expected == 0 || expected > MAX_BACKFILL_TRADES {
        return Err(CoinbaseRestError::InvalidRange);
    }

    // Begin at Coinbase's latest page, then follow only the opaque `CB-AFTER`
    // cursors Coinbase returns. A trade ID is not assumed to be a valid cursor.
    let mut cursor = None;
    let maximum_pages = expected
        .div_ceil(PAGE_SIZE)
        .saturating_add(PAGINATION_HEADROOM_PAGES);
    let mut recovered = Vec::with_capacity(expected);
    let mut seen_cursors = BTreeSet::new();

    for _ in 0..maximum_pages {
        let page = source.fetch(cursor.as_deref()).await?;
        if page.trades.len() > PAGE_SIZE {
            return Err(CoinbaseRestError::InvalidResponse);
        }
        if page.trades.is_empty() {
            break;
        }

        let reached_lower_bound = page
            .trades
            .iter()
            .any(|trade| trade.trade_id <= after_trade_id);
        recovered.extend(
            page.trades.into_iter().filter(|trade| {
                trade.trade_id > after_trade_id && trade.trade_id <= through_trade_id
            }),
        );
        if recovered.len() >= expected || reached_lower_bound {
            break;
        }

        let next_cursor = page.next_cursor.ok_or(CoinbaseRestError::InvalidResponse)?;
        if !seen_cursors.insert(next_cursor.clone()) {
            return Err(CoinbaseRestError::InvalidResponse);
        }
        cursor = Some(next_cursor);
    }

    recovered.sort_unstable_by_key(|trade| trade.trade_id);
    if recovered.len() != expected
        || recovered.iter().enumerate().any(|(offset, trade)| {
            u64::try_from(offset)
                .ok()
                .and_then(|offset| after_trade_id.checked_add(offset))
                .and_then(|trade_id| trade_id.checked_add(1))
                != Some(trade.trade_id)
        })
    {
        return Err(CoinbaseRestError::RangeUnavailable {
            expected,
            recovered: recovered.len(),
        });
    }
    Ok(recovered)
}

async fn fetch_trade_range_with_retry_from<S: TradePageSource>(
    source: &S,
    after_trade_id: u64,
    through_trade_id: u64,
    retry_delays: &[Duration],
) -> Result<Vec<CoinbaseRestTrade>, CoinbaseRestError> {
    for delay in retry_delays {
        match fetch_trade_range_from(source, after_trade_id, through_trade_id).await {
            Err(CoinbaseRestError::RangeUnavailable { .. }) => sleep(*delay).await,
            result => return result,
        }
    }
    fetch_trade_range_from(source, after_trade_id, through_trade_id).await
}

/// Why gap recovery could not deliver a complete trade range. Every variant
/// leaves the adapter unchanged and live publication closed.
#[derive(Debug, Error)]
pub enum CoinbaseRestError {
    /// The requested range was empty or reversed (`through <= after`) or asked
    /// for more than 100,000 trades.
    #[error("Coinbase trade recovery range is empty, reversed, or exceeds the configured bound")]
    InvalidRange,
    /// The secure-HTTP client could not be built, a page request failed, or a
    /// body exceeded the 4 MiB response cap.
    #[error("secure Coinbase HTTP request failed")]
    Http,
    /// A page did not deserialize, carried more than 1000 trades, returned a
    /// `CB-AFTER` cursor that was non-ASCII, empty or over 128 bytes, repeated a
    /// cursor already followed, or ended without one while trades were still
    /// outstanding.
    #[error("Coinbase returned an invalid bounded trade response")]
    InvalidResponse,
    /// Paging finished without a contiguous `after+1 ..= through` run, after the
    /// six bounded retries that absorb Coinbase's five-second public cache
    /// window. Recovery fails closed rather than admitting a partial range.
    #[error(
        "Coinbase REST exposed {recovered} of {expected} required trades before the recovery deadline"
    )]
    RangeUnavailable {
        /// Trades the range requires: `through_trade_id - after_trade_id`.
        expected: usize,
        /// Distinct in-range trades REST actually exposed, always below
        /// `expected` or non-contiguous.
        recovered: usize,
    },
    /// REST returned the complete range but the adapter rejected the backfill
    /// during validation. Carries the adapter's reason.
    #[error("Coinbase trade recovery failed validation: {0}")]
    Adapter(AdapterError),
}

impl From<SecureHttpError> for CoinbaseRestError {
    fn from(_: SecureHttpError) -> Self {
        Self::Http
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, VecDeque};
    use std::sync::Mutex;
    use std::time::Duration;

    use super::{
        CoinbaseRestError, CoinbaseRestTrade, MAX_BACKFILL_TRADES, TradePage, TradePageSource,
        fetch_trade_range_from, fetch_trade_range_with_retry_from,
    };

    struct FakePages {
        pages: Mutex<BTreeMap<Option<String>, VecDeque<TradePage>>>,
        requests: Mutex<Vec<Option<String>>>,
    }

    impl FakePages {
        fn new(pages: BTreeMap<Option<String>, VecDeque<TradePage>>) -> Self {
            Self {
                pages: Mutex::new(pages),
                requests: Mutex::new(Vec::new()),
            }
        }
    }

    impl TradePageSource for FakePages {
        async fn fetch(&self, cursor: Option<&str>) -> Result<TradePage, CoinbaseRestError> {
            let cursor = cursor.map(str::to_owned);
            self.requests
                .lock()
                .expect("request lock is available")
                .push(cursor.clone());
            self.pages
                .lock()
                .expect("page lock is available")
                .get_mut(&cursor)
                .and_then(VecDeque::pop_front)
                .ok_or(CoinbaseRestError::InvalidResponse)
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn pages_toward_older_trade_ids_until_the_gap_is_complete() {
        let second_cursor = "601".to_owned();
        let source = FakePages::new(BTreeMap::from([
            (
                None,
                VecDeque::from([TradePage {
                    trades: (601..=1_600).rev().map(trade).collect(),
                    next_cursor: Some(second_cursor.clone()),
                }]),
            ),
            (
                Some(second_cursor.clone()),
                VecDeque::from([TradePage {
                    trades: (101..=600).rev().map(trade).collect(),
                    next_cursor: Some("100".to_owned()),
                }]),
            ),
        ]));

        let recovered = fetch_trade_range_from(&source, 100, 1_600)
            .await
            .expect("two descending Coinbase pages cover the gap");
        assert_eq!(recovered.len(), 1_500);
        assert_eq!(recovered.first().map(|trade| trade.trade_id), Some(101));
        assert_eq!(recovered.last().map(|trade| trade.trade_id), Some(1_600));
        assert_eq!(
            *source.requests.lock().expect("request lock is available"),
            vec![None, Some(second_cursor)]
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn rejects_repeated_pagination_cursor() {
        let repeated_cursor = "501".to_owned();
        let source = FakePages::new(BTreeMap::from([
            (
                None,
                VecDeque::from([TradePage {
                    trades: (1_002..=2_001).rev().map(trade).collect(),
                    next_cursor: Some(repeated_cursor.clone()),
                }]),
            ),
            (
                Some(repeated_cursor.clone()),
                VecDeque::from([TradePage {
                    trades: (502..=1_001).rev().map(trade).collect(),
                    next_cursor: Some(repeated_cursor),
                }]),
            ),
        ]));
        let error = fetch_trade_range_from(&source, 1, 2_001)
            .await
            .expect_err("Coinbase cursor must not repeat");
        assert!(matches!(error, CoinbaseRestError::InvalidResponse));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn retries_only_when_the_rest_range_is_not_yet_available() {
        let source = FakePages::new(BTreeMap::from([(
            None,
            VecDeque::from([
                TradePage {
                    trades: vec![trade(100)],
                    next_cursor: Some("99".to_owned()),
                },
                TradePage {
                    trades: vec![trade(102), trade(101)],
                    next_cursor: Some("100".to_owned()),
                },
            ]),
        )]));
        let recovered = fetch_trade_range_with_retry_from(&source, 100, 102, &[Duration::ZERO])
            .await
            .expect("a transient REST visibility lag is retried once");
        assert_eq!(
            recovered
                .iter()
                .map(|trade| trade.trade_id)
                .collect::<Vec<_>>(),
            vec![101, 102]
        );
        assert_eq!(
            *source.requests.lock().expect("request lock is available"),
            vec![None, None]
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn reports_an_incomplete_visible_range_without_mutating_it() {
        let source = FakePages::new(BTreeMap::from([(
            None,
            VecDeque::from([TradePage {
                trades: vec![trade(101), trade(100)],
                next_cursor: Some("100".to_owned()),
            }]),
        )]));
        let error = fetch_trade_range_from(&source, 100, 102)
            .await
            .expect_err("one of two required trades is unavailable");
        assert!(matches!(
            error,
            CoinbaseRestError::RangeUnavailable {
                expected: 2,
                recovered: 1
            }
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn rejects_oversized_or_empty_ranges_without_http() {
        let source = FakePages::new(BTreeMap::new());
        assert!(matches!(
            fetch_trade_range_from(&source, 10, 10).await,
            Err(CoinbaseRestError::InvalidRange)
        ));
        assert!(matches!(
            fetch_trade_range_from(&source, 0, (MAX_BACKFILL_TRADES as u64) + 1).await,
            Err(CoinbaseRestError::InvalidRange)
        ));
        assert!(
            source
                .requests
                .lock()
                .expect("request lock is available")
                .is_empty()
        );
    }

    fn trade(trade_id: u64) -> CoinbaseRestTrade {
        CoinbaseRestTrade {
            trade_id,
            price: "60000.0".to_owned(),
            size: "0.01".to_owned(),
            side: "buy".to_owned(),
            time: "2026-08-13T12:00:00Z".to_owned(),
        }
    }
}
