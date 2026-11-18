# btc-feeds

Turns raw provider messages into normalized `SourceEvent`s.
Point it at Coinbase, Kraken, Kalshi, Polymarket, Chainlink, Binance or Bybit text and you
get the same event structure out of all of them: exact decimal strings, a per-source
sequence, venue and receipt timestamps, and a `values` map. Nothing on the parsing path
reads a clock, allocates unboundedly or touches the network, so one input always produces
one output.

## Two ways to use it
### As a Rust dependency

```toml
[dependencies]
btc-feeds = { git = "https://github.com/tiny-xyz/signals-prototype", package = "btc-feeds", default-features = false }
```

`default-features = false` drops the CLI (and with it `clap` and `anyhow`).
```rust
use btc_feeds::{CaptureStream, FeedAdapter, RawRecord, RecordKind, ReplayAdapter};
use btc_feeds::replay::KrakenBookDepths;
let mut adapter = ReplayAdapter::new(
    CaptureStream::CoinbaseCore, "my-epoch", KrakenBookDepths::default())?;
adapter.ingest(&RawRecord::connected_at(
    CaptureStream::CoinbaseCore, 1, 1, received_at.clone(), 0, 0))?;
let outcome = adapter.ingest(&RawRecord::text_at(
    CaptureStream::CoinbaseCore, 1, 2, RecordKind::InboundText,
    message, received_at, 0))?;
for event in outcome.events { /* SourceEvent */ }
for notice in outcome.notices { /* FeedNotice */ }
```

`ReplayAdapter` dispatches every venue, so you never name a venue adapter. Ordinals start
at 1 and increase strictly across the whole stream, including across reconnects;
`connection_epoch` starts at 1 and increases by one per reconnect. Full invariants are on
`RawRecord::validate`; `cargo doc -p btc-feeds --open` carries a runnable example.

### Language-neutrally, via `btc-feeds normalize`

```
btc-feeds normalize --stream coinbase-core --publisher-epoch my-epoch < messages.txt > events.jsonl
```

* **stdin** — one raw provider message per line: exactly the WebSocket text as received, or
  for a REST-polled stream one response body per line. Blank lines are skipped.
* **stdout** — one JSON `SourceEvent` per line. **stderr** — one JSON `FeedNotice` per line:
  `{"code":…,"detail":…,"recovery":…}`.
* **exit code** — 0 at end of input; non-zero, with the error on stderr, on the first record
  the adapter refuses.
* **flags** — `--kraken-btc-book-depth` / `--kraken-usdt-book-depth` (Kraken only; must match
  the subscribed depth), `--configuration <file>` (a prediction market's discovery journal),
  `--subscription <file>` (one message sent *to* the venue; repeatable, replayed in order).

The envelope is synthesized for you: the Configuration record from `--configuration`, then
`Connected`, then one outbound record per `--subscription` in the order given, then one
`InboundText` record per stdin line with `received_at` from the wall clock — the only
nondeterminism, so two runs over one file differ in `received_at` and nowhere else.

**Kalshi and Polymarket** need both extra flags: they bind to one exact market and refuse
market data until they have seen the subscription that was actually sent. Write the
discovery journal to one file and each outbound subscribe message to its own file:

```
btc-feeds normalize --stream kalshi-market --publisher-epoch my-epoch \
  --configuration market.json \
  --subscription sub-book.json --subscription sub-ticker.json --subscription sub-trade.json \
  < inbound.txt > events.jsonl
```

Polymarket takes one `--subscription` (the market subscribe). Omit them and the run emits
the market-contract event, then fails closed on the first book message — by design.

## Adapters
| Stream token | Adapter | Source ids emitted | Definition |
| --- | --- | --- | --- |
| `coinbase-core` | `CoinbaseAdapter` | `coinbase.exchange.btc-usd.{trades,ticker,book-levels,product}`, `coinbase.exchange.usdt-usd.ticker` (E47 USDT/USD reference) | yes |
| `coinbase-level3` / `-mechanisms` | `CoinbaseAdapter` | `coinbase.exchange.btc-usd.product` | yes |
| `kraken-core` | `KrakenAdapter` | `kraken.spot.status`, `kraken.spot.{btc-usd,usdt-usd}.{ticker,trades,book-levels,ohlc-1m,instrument}` | yes |
| `kraken-level3` | `KrakenLevel3Adapter` | `kraken.spot.btc-usd.book-orders` | yes |
| `kalshi-market` | `KalshiAdapter` | `kalshi.btc.{market-contracts,book-levels,quotes,trades}` | yes |
| `polymarket-market` | `PolymarketAdapter` | `polymarket.btc.{market-contracts,book-levels,quotes,trades}` | yes |
| `kalshi-index` | `KalshiIndexAdapter` | `kalshi.cfbenchmarks.brti.{value,settlement-window,value-5hz}`, `kalshi.pyth.btc-usd.value` | yes (both BRTI value channels observed; the Pyth relay publishes nothing) |
| `polymarket-chainlink-twap` | `PolymarketTwapAdapter` | `polymarket.rtds.btc-usd.twap-60s` | yes |
| `binance-spot` | `BinanceAdapter` | `binance.spot.btcusdt.{book-levels,agg-trades,book-ticker}` | yes (contracts published 2026-09-03; the venue is rights-gated) |
| `binance-usdm-public` | `BinanceUsdmAdapter` (`Public`) | `binance.usdm.btcusdt.book-levels` | yes (**observed**; finalized against captured hours 2026-09-03) |
| `binance-usdm-rest` | `BinanceUsdmAdapter` (`Rest`) | `binance.usdm.btcusdt.{open-interest,open-interest-hist,funding-info}` | yes (**observed**; finalized against captured hours 2026-09-03) |
| `bybit-spot` | `BybitAdapter` (`BybitMarket::Spot`) | `bybit.spot.btcusdt.{trades,book-levels,bbo-levels}` | no |
| `bybit-linear` | `BybitAdapter` (`BybitMarket::Linear`) | `bybit.linear.btcusdt.liquidations` | no |
| `chainlink-aggregator` | `ChainlinkAdapter` | `chainlink.ethereum.btc-usd.rounds` | yes (approved) |
| `pyth-hermes-stream` | `PythHermesAdapter` (`Stream`) | `pyth.hermes.btc-usd.price` (`update_kind: stream`) | yes (**observed 2026-09-04**; refuse-closed behind `PYTH_API_KEY`) |
| `pyth-hermes-rest` | `PythHermesAdapter` (`Latest`) | `pyth.hermes.btc-usd.price` (`update_kind: latest`) | yes (**observed 2026-09-04**; REST latest anchor) |
The index channels ride **Kalshi's market socket**,
`wss://external-api-ws.kalshi.com/trade-api/ws/v2`, under the market command envelope
(observed 2026-09-03 on the Tokyo host, ruling E23 — the per-channel paths Kalshi documents
do not connect). The `kalshi-index` capture stream opens its own connection to that endpoint
so an index capture is independent of the market stream's 15-minute rollovers; the live
Kalshi lane instead subscribes the index channels on the market connection it already holds.
Three channels are subscribed, each under its own reserved command id, and `["all"]`,
`update_subscription` and every `*_list` command are never sent:

```json
{"id":101,"cmd":"subscribe","params":{"channels":["cfbenchmarks_value"],"index_ids":["BRTI"]}}
{"id":103,"cmd":"subscribe","params":{"channels":["cfbenchmarks_value_5hz"],"index_ids":["BRTI"]}}
{"id":102,"cmd":"subscribe","params":{"channels":["pyth_value"],"underlying_tickers":["Crypto.BTC/USD"]}}
```

The reserved id band (≥ 100) is what lets a `subscribed` or `error` reply — which names only
the command `id` it answers — be routed to the index adapter and attributed to a channel on
the shared socket.

`cfbenchmarks_value_5hz`'s availability **varies per connection**, which the adapter treats as
normal rather than as an error. With the same credentials throughout, one 65-minute hour was
answered `{"type":"error","msg":{"code":8,"msg":"Unknown channel name"}}` on three connections
and `subscribed` on the fourth; the next hour was accepted on the very first subscribe and held
the channel for all 65 minutes, delivering 19,497 frames on an exact 200 ms grid. Refused 3 of
4, then accepted 1 of 1 — a refusal is a property of the connection, not of the account
(ruling E25). So the subscribe goes out on **every** connection; a code-8 reply is one
`SubscriptionRejected` notice naming the channel, and it touches nothing else — not the
market channels, not the 1 Hz channel, never the lane (ruling E2). The next `Connected`
clears the refusal and re-attempts.

That notice is how availability reaches a consumer, and it carries the outcome as **typed
data**: `FeedNotice::subscription` is a `SubscriptionOutcome { source_id, accepted, code }`,
set where `ingest_ack` and `ingest_error` already know the channel and the command id. A
lane never routes on `detail`, whose wording is explicitly not part of the contract. The
Kalshi prediction lane reads that field on every outcome and turns a refusal into
`PredictionPipeline::refuse_index_channel(…, "cfbrti_subscription_rejected")`, so every
instance derived from `kalshi.cfbenchmarks.brti.value-5hz` publishes `unavailable` under that
reason until the acceptance after a reconnect clears it. The state is per **source**, never
per transport: a 5 Hz refusal leaves the 1 Hz channel and the market channels healthy.
Inside the adapter the same fact is readable as `KalshiIndexAdapter::value_5hz_unavailable`. The 5 Hz body is its own shape: `{index_id, value_usd, source_ts_ms,
received_at, data}`, with no trailing average and no quarter object.

The cross-channel equality invariant (§6.4) stays withdrawn pending a ruling. It held on all
441 index times the two channels shared in that hour — but only when compared by decimal
value, since the 1 Hz tick is free-scale and `value_usd` is fixed at 8 digits, which is
exactly the text-comparison bug audit F1 found.

`pyth_value` subscribes and is acknowledged, but has delivered **no frame** in any capture —
a little over three hours of acknowledged silence across four, including two full in-tree
hours.
Its body shape stays documented-only and the surface is refuse-closed: messages are parsed,
sequenced and ordered, and nothing is published under `kalshi.pyth.btc-usd.value` until a real
frame is captured.

Every contract here rests on **in-tree** evidence. The 65-minute `kalshi-index` capture of
2026-09-03 10:01–11:06Z replays through `btc-feeds replay` with no modification — it carries
the shipped command ids — and attributes every frame: 23,403 records → **23,636 events**
(19,497 `…value-5hz`, 3,899 `…brti.value`, 240 `…settlement-window` over four derived closes,
four of them complete), 0 adapter errors, 0 recovery notices, one connection, no drops, and
byte-identical across two runs. The earlier 08:45–09:50Z hour corroborates the 1 Hz and
settlement contracts independently (3,905 records → 4,140 events, same result). The probe
captures are history: they predate the reserved command-id band and need an id rewrite to
replay, and they survive only as the record of a refused 5 Hz subscribe. The conditional `last_60s_windowed_average_15min` quarter object was likewise never
observed — every probe frame was mid-quarter — so its absence is the normal case.

There are no `--kalshi-index-*` capture flags any more: the endpoint and the envelope each
have exactly one valid value, kept as the compiled constants
`kalshi_index::INDEX_WEBSOCKET_PATH` and the two command ids. The `btc-feeds live` runner
still refuses the `kalshi_index` stream — it drives one adapter per stream, and the index
channels now arrive on the market socket. `btc-feeds capture --stream kalshi-index` **refuses
unless `SIGNALS_ENABLED_VENUES` lists `cfbrti` or `pyth`**, checked before the credential is
read and before any byte reaches disk; each channel family is then subscribed only under its
own token. That is the one capture stream that reads the variable: `kalshi-market` and every
other adopted stream never consult it, so a malformed allow-list cannot refuse a capture path
that never depended on one.

Four further NBA streams (`nba-espn-status`, `nba-espn-daily`, `nba-espn-games`,
`sportsdataio-nba`) use the same machinery and have no definition files. Kalshi and
Polymarket additionally need a Configuration record binding them to one exact market: build
its payload with `KalshiAdapter::configuration_payload` or
`PolymarketAdapter::configuration_payload` from a REST body you fetched yourself; both
document the journal format, which embeds the body and a SHA-256 over it.

## Output contract
```json
{"schema_version":1,"source_id":"coinbase.exchange.btc-usd.ticker","source_version":"1.0.0",
 "publisher_epoch":"my-epoch","sequence":0,"event_time":"…","received_at":"…","values":{…}}
```

`sequence` is per source id, starts at 0 and increases by exactly one per event for the life
of the adapter — that is the ordering to rely on. `event_time` is the venue instant where
the venue supplies one; `received_at` is local receipt. Decimals are strings kept character
for character: scientific notation is expanded and canonical zeros trimmed, but a value
whose exact text cannot be preserved is rejected, never rounded.

The envelope schema is `schemas/source-event.schema.json`; the field-level contract for
`values` is the matching file in `definitions/btc/sources/`. A source id with no definition
file has no field contract — treat it as a candidate, not a source.
Uncertain state is never published and never silently repaired: a Kraken book whose checksum
disagrees is discarded, a delta before its snapshot is dropped with a notice, a trade gap
pauses trade publication entirely. Every parse is bounded — message size, row counts,
buffered trades, token lengths — and unmodelled venue fields are refused rather than
dropped, so a venue change fails loudly.

## The recovery loop
A `FeedNotice` carrying a `recovery` means the adapter has stopped publishing that surface:

| Action | What you do |
| --- | --- |
| `Reconnect` | Open a new connection; emit a new `Connected` record at the next epoch. |
| `BackfillCoinbaseTrades` | `GET /products/BTC-USD/trades` for the named range, then `CoinbaseAdapter::apply_trade_backfill(after, through, received_at, trades)`. Until that succeeds, no Coinbase trade is published. |
| `ResubscribeKrakenBook` / `ResubscribeKrakenLevel3` | Resubscribe at the named depth; the local book was discarded. |
| `BackfillKalshiBrti` | `GET /trade-api/v2/cfbenchmarks/history/values` over the named index interval, then `KalshiIndexAdapter::apply_history_backfill(rows)`. Until a response covers the whole interval, the quarter-hour settlement window publishes nothing; a short response is a repair failure, never a partial apply. |
| `ResubscribeKalshiBook` | Resubscribe the Kalshi book channel. |
| `RediscoverPredictionMarket` | Re-run discovery and start a fresh adapter with a new Configuration record. |

Streams that can be incomplete expose `finish()`, dispatched by `ReplayAdapter::finish` —
call it after the last message.

## Vocabulary you have to know about
`values` is venue-shaped, so the same concept is spelled differently across venues.

* **Venue sequence numbers** are five names for five different venue concepts:
  `source_sequence` (Coinbase ticker), `upstream_sequence` (Kalshi market channels, and the
  index channels' per-subscription `seq`), `cross_sequence` and
  `update_id` (Bybit), `last_update_id` (Binance). Order on the crate's own `sequence`.
* **`event_time_basis`** uses two vocabularies — `"source"`/`"receipt"` on Coinbase,
  Binance, Bybit and Chainlink, `"provider"`/`"local_receipt"` on Kalshi — and Kraken,
  Kraken level-3, Polymarket and Polymarket TWAP do not emit it at all. Branching on
  `"receipt"` alone silently treats every Kalshi book event as venue-timed.
* **Kraken's `event_time`** is local receipt on `kraken.spot.status` and both
  `*.instrument` sources and venue time on everything else, with no field distinguishing
  them, so `now - event_time` across a Kraken stream mixes the two. Branch on the source id.
* **Binance agg-trades** spell the size field `quantity`; everything else spells it `size`.

These are open source-contract questions, not oversights: changing them would break replay
of frozen evidence, so they are documented rather than fixed.

## Not included
Transport, capture recording, credentials, retry and reconnection policy are the live
runner's, not the parser's. They ship in this crate (`live`, `capture`, `prediction_live`,
the `*_auth` and `*_rest` modules) because the producer needs them, and sit outside the
library surface described here.