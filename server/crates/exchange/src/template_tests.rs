//! Behaviour tests for the shipped strategy templates.
//!
//! Each test deploys a template side through the real `POST
//! /api/accounts/{addr}/strategies` handler over a real engine, platform
//! and runtime manager (the `store.rs` test rig), publishes the signal
//! frames the design says should fire it, and looks for the orders on the
//! real book and the position on the real sub-account. A template is not
//! done when it compiles; it is done when both of its sides trade the way
//! the card says they do.

use std::collections::BTreeMap;
use std::str::FromStr;
use std::time::Duration;

use axum::{Json, extract::{Path, State}};
use rust_decimal::Decimal;
use serde_json::{Value as Json_, json};

use crate::api::AppState;
use crate::api::strategy::store::tests::{MAKER, PARENT, app_state_with};
use crate::api::strategy::store::{CreateStrategyRequest, create_strategy};
use crate::api::strategy::template::deploy_template_side;
use crate::automation::lane_bridge::tests::registry_with_spot_lane;
use crate::automation::runtime::DeploymentStatus;
use crate::automation::signal_bus::{Quality, SignalFrame};
use crate::engine::TradeSource;
use crate::registry::PriceSource;
use crate::series::SERIES;
use crate::templates::{Side, TemplateCatalogue, envelope_for};
use crate::ticks::PriceTick;
use crate::types::{Address, Qty, SeriesId, Side as BookSide, Tenths};

/// A thousand dollars funded into each sub-account (a dollar is 1,000
/// tenths of a cent); the envelope below trades a $100 allocation of it.
const ALLOCATION_TENTHS: i64 = 1_000_000;

struct Rig {
    state: AppState,
    catalogue: std::sync::Arc<TemplateCatalogue>,
    series: SeriesId,
}

impl Rig {
    /// A rig on `series` with the checkout's signal definitions in the
    /// registry, every template compiled, and a maker resting `bid` /
    /// `ask` (up-side tenths) for `qty` lots so takers can fill.
    fn new(series: SeriesId, bid: u16, ask: u16, qty: u32) -> Rig {
        let registry = registry_with_spot_lane();
        let catalogue = std::sync::Arc::new(TemplateCatalogue::load(&registry, PriceSource::GeminiIndex).unwrap_or_else(|e| panic!("{e}")));
        let (mut state, _platform) = app_state_with(registry);
        state.templates = catalogue.clone();
        {
            let mut e = state.exchange.lock().unwrap();
            let maker = Address::from_str(MAKER).unwrap();
            e.place_limit(series, maker, BookSide::Bid, PriceTick::from_tenths(bid).unwrap(), Qty(qty), TradeSource::Bot).unwrap();
            e.place_limit(series, maker, BookSide::Ask, PriceTick::from_tenths(ask).unwrap(), Qty(qty), TradeSource::Bot).unwrap();
        }
        Rig { state, catalogue, series }
    }

    fn slug(&self) -> &'static str {
        self.series.slug()
    }

    /// Deploy one side of a template on the rig's series for $100 and
    /// return the sub-account and the deployment id.
    async fn deploy(&self, template: &str, side: Side) -> (Address, String) {
        let t = self.catalogue.get(template).expect("template exists");
        let compiled = t.compiled(self.series, side).expect("listed on the series");
        let req = CreateStrategyRequest {
            allocation_tenths: ALLOCATION_TENTHS,
            name: None,
            strategy: compiled.source.clone(),
            envelope: envelope_for(&t.envelope, Decimal::from(100)),
            params: BTreeMap::new(),
        };
        let (_, Json(d)) = create_strategy(State(self.state.clone()), Path(PARENT.into()), Json(req))
            .await
            .unwrap_or_else(|e| panic!("{template} {side:?} deploys: {e:?}"));
        (d.summary.id, d.strategy.deployment_id)
    }

    fn publish(&self, signal_id: &str, values: &[(&str, Json_)]) {
        self.state.bus.publish(SignalFrame {
            signal_id: signal_id.to_string(),
            ts_ms: crate::pricing::feed::unix_now_ms(),
            quality: Quality::ready(),
            values: values.iter().map(|(k, v)| (k.to_string(), v.clone())).collect(),
        });
    }

    /// The exchange's own book frame for the rig's series.
    fn publish_book(&self, bid: &str, ask: &str) {
        let up = (Decimal::from_str(bid).unwrap() + Decimal::from_str(ask).unwrap()) / Decimal::from(2);
        let id = format!("{}.current.price.v1", self.slug());
        self.publish(
            &id,
            &[
                ("up", json!(up.normalize().to_string())),
                ("down", json!((Decimal::ONE - up).normalize().to_string())),
                ("bid", json!(bid)),
                ("ask", json!(ask)),
                ("last", json!("0")),
            ],
        );
    }

    fn publish_delta(&self, delta_bps: &str) {
        let id = format!("{}.current.delta.v1", self.slug());
        self.publish(&id, &[("delta_bps", json!(delta_bps)), ("spot", json!("50000.00")), ("strike", json!("50000.00"))]);
    }

    fn publish_window(&self, remaining_ms: u64) {
        let id = format!("{}.current.window.v1", self.slug());
        self.publish(&id, &[("phase", json!("open")), ("seq", json!(1)), ("remaining", json!(remaining_ms))]);
    }

    /// Rest one more maker order (up-side tenths) so a later taker can fill.
    fn rest(&self, side: BookSide, tenths: u16, qty: u32) {
        let mut e = self.state.exchange.lock().unwrap();
        let maker = Address::from_str(MAKER).unwrap();
        e.place_limit(self.series, maker, side, PriceTick::from_tenths(tenths).unwrap(), Qty(qty), TradeSource::Bot).unwrap();
    }

    // Lane signals carry their whole declared field set or the registry
    // refuses the frame; these fill the fields a test does not care about.

    fn publish_momentum(&self, return_bps_10s: &str, direction_10s: &str) {
        self.publish(
            "btc.external.micro_momentum.v1",
            &[
                ("return_bps_1s", json!("0")),
                ("return_bps_10s", json!(return_bps_10s)),
                ("return_bps_30s", json!(return_bps_10s)),
                ("direction_10s", json!(direction_10s)),
                ("sample_count", json!(40)),
                ("window_close", json!(1_757_000_000_000u64)),
            ],
        );
    }

    fn publish_flow(&self, flow_direction_10s: &str) {
        self.publish(
            "btc.external.aggressor_flow.v1",
            &[
                ("signed_notional_usd_1s", json!("0")),
                ("signed_notional_usd_10s", json!("0")),
                ("signed_notional_usd_60s", json!("0")),
                ("cumulative_volume_delta_base_10s", json!("0")),
                ("cumulative_volume_delta_base_60s", json!("0")),
                ("gross_notional_usd_60s", json!("0")),
                ("trade_count_60s", json!(10)),
                ("flow_direction_10s", json!(flow_direction_10s)),
                ("window_close", json!(1_757_000_000_000u64)),
            ],
        );
    }

    fn publish_venues(&self, reference_price: &str) {
        self.publish(
            "btc.external.spot_reference.v1",
            &[
                ("reference_price", json!(reference_price)),
                ("coinbase_mid", json!(reference_price)),
                ("kraken_mid", json!(reference_price)),
                ("dispersion_bps", json!("0")),
                ("healthy_venue_count", json!(2)),
                ("max_input_age_ms", json!(100)),
                ("reference_time", json!(1_757_000_000_000u64)),
            ],
        );
    }

    fn publish_absorption(&self, state: &str, flow_direction: &str) {
        self.publish(
            "btc.external.absorption_state.v1",
            &[
                ("signed_flow_notional_usd_10s", json!("0")),
                ("price_response_bps_10s", json!("0")),
                ("flow_to_depth_ratio", json!("0")),
                ("response_bps_per_depth_turn", json!("0")),
                ("flow_direction", json!(flow_direction)),
                ("state", json!(state)),
                ("window_close", json!(1_757_000_000_000u64)),
            ],
        );
    }

    fn publish_ofi(&self, ofi_base_10s: &str) {
        self.publish(
            "btc.external.order_flow_imbalance.v1",
            &[
                ("ofi_base_1s", json!("0")),
                ("ofi_base_10s", json!(ofi_base_10s)),
                ("ofi_base_60s", json!("0")),
                ("event_count_60s", json!(10)),
                ("window_close", json!(1_757_000_000_000u64)),
            ],
        );
    }

    fn publish_depth(&self, combined_imbalance: &str) {
        self.publish(
            "btc.external.actionable_depth_imbalance.v1",
            &[
                ("band_bps", json!(10)),
                ("coinbase_imbalance", json!(combined_imbalance)),
                ("kraken_imbalance", json!(combined_imbalance)),
                ("combined_imbalance", json!(combined_imbalance)),
                ("bid_depth_base", json!("10")),
                ("ask_depth_base", json!("10")),
                ("max_book_age_ms", json!(100)),
                ("observation_time", json!(1_757_000_000_000u64)),
            ],
        );
    }

    fn publish_vwap(&self, deviation_bps_60s: &str) {
        self.publish(
            "btc.external.vwap_deviation.v1",
            &[
                ("vwap_10s", json!("50000")),
                ("vwap_60s", json!("50000")),
                ("reference_price", json!("50000")),
                ("deviation_bps_10s", json!(deviation_bps_60s)),
                ("deviation_bps_60s", json!(deviation_bps_60s)),
                ("base_volume_60s", json!("100")),
                ("trade_count_60s", json!(50)),
                ("window_close", json!(1_757_000_000_000u64)),
            ],
        );
    }

    fn publish_horizon(&self, return_bps_1m: &str) {
        self.publish(
            "btc.external.horizon_momentum.v1",
            &[
                ("return_bps_1m", json!(return_bps_1m)),
                ("return_bps_5m", json!("0")),
                ("return_bps_15m", json!("0")),
                ("direction_5m", json!("flat")),
                ("window_close", json!(1_757_000_000_000u64)),
            ],
        );
    }

    fn publish_sweep(&self, venue: &str, direction: &str) {
        let state = if direction == "none" { "quiet" } else { "sweep" };
        self.publish(
            &format!("btc.{venue}.sweep_detection.v1"),
            &[
                ("detection_state", json!(state)),
                ("direction", json!(direction)),
                ("swept_levels", json!(4)),
                ("swept_size", json!("2.5")),
                ("min_levels", json!(3)),
                ("observation_time", json!(1_757_000_000_000u64)),
            ],
        );
    }

    fn publish_strike(&self) {
        let id = format!("{}.current.strike.v1", self.slug());
        self.publish(&id, &[("price", json!("50000.00"))]);
    }

    /// Polymarket's contract for a round whose strike matches ours.
    fn publish_contract(&self) {
        self.publish(
            "btc.polymarket.market_contract.v1",
            &[
                ("market_id", json!("0xabc")),
                ("market_generation", json!(1)),
                ("window_open", json!(1_757_000_000_000u64)),
                ("window_close", json!(1_757_000_900_000u64)),
                ("strike", json!("50001")),
                ("opening_strike_source", json!("exact")),
                ("settlement_benchmark", json!("chainlink_btc_usd_twap_60s")),
                ("averaging_window_seconds", json!(60)),
                ("comparator", json!("greater_than_or_equal")),
                ("tie_outcome", json!("up")),
                ("affirmative_outcome", json!("up")),
                ("negative_outcome", json!("down")),
                ("minimum_tick_size", json!("0.01")),
                ("minimum_order_size", json!("5")),
                ("rules_sha256", json!("deadbeef")),
                ("contract_time", json!(1_757_000_000_000u64)),
            ],
        );
    }

    fn publish_poly_quote(&self, affirmative_midpoint: &str, negative_midpoint: &str) {
        self.publish(
            "btc.polymarket.prediction_quote.v1",
            &[
                ("market_id", json!("0xabc")),
                ("market_generation", json!(1)),
                ("affirmative_bid", json!(affirmative_midpoint)),
                ("affirmative_ask", json!(affirmative_midpoint)),
                ("affirmative_midpoint", json!(affirmative_midpoint)),
                ("negative_bid", json!(negative_midpoint)),
                ("negative_ask", json!(negative_midpoint)),
                ("negative_midpoint", json!(negative_midpoint)),
                ("last_trade_outcome", json!("up")),
                ("last_trade_price", json!(affirmative_midpoint)),
                ("last_trade_size", json!("10")),
                ("max_quote_age_ms", json!(100)),
                ("quote_time", json!(1_757_000_000_000u64)),
            ],
        );
    }

    fn publish_poly_momentum(&self, return_pp_30s: &str, direction_30s: &str) {
        self.publish(
            "btc.polymarket.probability_momentum.v1",
            &[
                ("return_pp_10s", json!("0")),
                ("return_pp_30s", json!(return_pp_30s)),
                ("return_pp_2m", json!("0")),
                ("direction_30s", json!(direction_30s)),
                ("window_close", json!(1_757_000_000_000u64)),
            ],
        );
    }

    /// The deployment's live orders as (outcome, price, remaining).
    async fn live_orders(&self, id: &str) -> Vec<(String, String, String)> {
        self.status(id)
            .await
            .live_orders
            .iter()
            .map(|o| {
                (
                    o.outcome.clone().unwrap_or_default(),
                    o.price.map(|p| p.normalize().to_string()).unwrap_or_default(),
                    o.remaining.normalize().to_string(),
                )
            })
            .collect()
    }

    async fn wait_for_live_order(&self, id: &str, outcome: &str, price: &str) -> Vec<(String, String, String)> {
        for _ in 0..200 {
            let live = self.live_orders(id).await;
            if live.iter().any(|(o, p, _)| o == outcome && p == price) {
                return live;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        self.live_orders(id).await
    }

    fn position(&self, child: Address) -> i64 {
        self.state.exchange.lock().unwrap().engine(self.series).position(&child).position.0
    }

    async fn status(&self, id: &str) -> DeploymentStatus {
        self.state.runtime.status(id).await.expect("deployment resident")
    }

    /// Poll until `pred` holds, at most two seconds.
    async fn wait_for(&self, mut pred: impl FnMut(&Rig) -> bool) -> bool {
        for _ in 0..200 {
            if pred(self) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        pred(self)
    }

    async fn wait_for_position(&self, child: Address, test: impl Fn(i64) -> bool) -> i64 {
        self.wait_for(|r| test(r.position(child))).await;
        self.position(child)
    }

    async fn wait_for_leg(&self, id: &str, leg: &str) -> String {
        for _ in 0..200 {
            let s = self.status(id).await;
            if s.current_leg == leg {
                return s.current_leg;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        self.status(id).await.current_leg
    }
}

// ───────────────────────────── fair-value-discount ─────────────────────────────

/// Fair for Up with delta +20 bps and ten seconds left is ~1.0; with -20
/// bps it is ~0.0. The maker rests 0.30 / 0.40, so Up is cheap under a
/// high fair and Down is cheap (its ask is 0.70) under a low fair.
#[tokio::test]
async fn fair_value_discount_buys_the_cheap_side_and_sells_out_at_fair() {
    let rig = Rig::new(SERIES[0].id, 300, 400, 500);
    let (child, id) = rig.deploy("fair-value-discount", Side::Primary).await;
    assert_eq!(rig.status(&id).await.current_leg, "hunting");

    rig.publish_window(10_000);
    rig.publish_book("0.3", "0.4");
    rig.publish_delta("20");
    let pos = rig.wait_for_position(child, |p| p > 0).await;
    assert_eq!(pos, 50, "$20 at the 0.40 ask is fifty lots of Up");
    assert_eq!(rig.wait_for_leg(&id, "holding_up").await, "holding_up");

    // Fair collapses: the 0.30 bid is now above fair, so sell out.
    rig.publish_delta("-20");
    let pos = rig.wait_for_position(child, |p| p == 0).await;
    assert_eq!(pos, 0, "sold the fifty lots into the bid");
    assert_eq!(rig.wait_for_leg(&id, "hunting").await, "hunting");

    // With fair near zero the Down side is cheap: the next evaluation
    // buys Down, which is a sale of Up into the same bid. The row fired
    // (masked) a moment ago, so the 2-minute entry cooldown has to lapse.
    tokio::time::sleep(Duration::from_millis(3_100)).await;
    rig.publish_delta("-20");
    let pos = rig.wait_for_position(child, |p| p < 0).await;
    assert_eq!(pos, -28, "$20 at the 0.70 Down ask is twenty-eight lots of Down");
    assert_eq!(rig.wait_for_leg(&id, "holding_down").await, "holding_down");
    let s = rig.status(&id).await;
    assert!(s.log.iter().any(|l| l.contains("[order]")), "{:#?}", s.log);
}

/// A template deployed through the public route sizes its orders from
/// the deployer's own allocation, not from the envelope the catalogue was
/// compiled against at boot. Forty dollars at a fifth per order is $8,
/// which is twenty lots at the 0.40 ask.
#[tokio::test]
async fn a_template_deploy_sizes_orders_from_its_own_allocation() {
    let rig = Rig::new(SERIES[0].id, 300, 400, 500);
    let parent = Address::from_str(PARENT).unwrap();
    let d = deploy_template_side(&rig.state, parent, "fair-value-discount", rig.series, Side::Primary, Tenths(40_000)).await.unwrap();
    assert_eq!(d.strategy.envelope.allocation.as_str(), "40");

    rig.publish_window(10_000);
    rig.publish_book("0.3", "0.4");
    rig.publish_delta("20");
    let pos = rig.wait_for_position(d.summary.id, |p| p > 0).await;
    let log = rig.status(&d.strategy.deployment_id).await.log;
    assert_eq!(pos, 20, "$8 at the 0.40 ask is twenty lots of Up: {log:#?}");
}

/// An immediate-or-cancel that finds no size is an expiry, not a cancel
/// ack; the on table must route it back to hunting or the deployment sits
/// in a holding leg with nothing held.
#[tokio::test]
async fn an_unfilled_entry_returns_to_hunting() {
    // The only ask is at 0.99: the entry at 0.41 cannot fill.
    let rig = Rig::new(SERIES[0].id, 300, 990, 500);
    let (child, id) = rig.deploy("fair-value-discount", Side::Primary).await;
    rig.publish_window(10_000);
    rig.publish_book("0.3", "0.4");
    rig.publish_delta("20");
    let s = rig.status(&id).await;
    assert!(rig.wait_for(|r| r.position(child) == 0).await);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let s2 = rig.status(&id).await;
    assert!(s2.log.iter().any(|l| l.contains("[order]")), "the entry was submitted: {:#?}", s.log);
    assert_eq!(s2.current_leg, "hunting", "an expired entry rearms hunting: {:#?}", s2.log);
    assert_eq!(rig.position(child), 0);
}

/// The inverse trades the direction of fair between ticks (one second
/// apart on the 2-minute series): a rise buys Up, a fall flips to Down.
#[tokio::test]
async fn fair_value_drift_rides_fair_and_flips_on_a_turn() {
    let rig = Rig::new(SERIES[0].id, 300, 400, 500);
    let (child, id) = rig.deploy("fair-value-discount", Side::Inverse).await;
    rig.publish_window(10_000);
    rig.publish_book("0.3", "0.4");

    // First tick only remembers fair (0.5 at zero delta).
    rig.publish_delta("0");
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    assert_eq!(rig.status(&id).await.current_leg, "hunting");
    assert_eq!(rig.position(child), 0);

    // Fair jumps to ~1.0 while the ask is 0.40: ride Up.
    rig.publish_delta("20");
    let pos = rig.wait_for_position(child, |p| p > 0).await;
    assert_eq!(pos, 50);
    assert_eq!(rig.wait_for_leg(&id, "holding_up").await, "holding_up");

    // Fair turns down hard: flip. Fifty lots close plus $20 of Down at 0.70.
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    rig.publish_delta("-20");
    let pos = rig.wait_for_position(child, |p| p < 0).await;
    assert_eq!(pos, -28);
    assert_eq!(rig.wait_for_leg(&id, "holding_down").await, "holding_down");
}

// ─────────────────────────────── momentum-rider ───────────────────────────────

#[tokio::test]
async fn momentum_rider_takes_the_side_of_the_burst() {
    let up = Rig::new(SERIES[0].id, 300, 400, 500);
    let (child, id) = up.deploy("momentum-rider", Side::Primary).await;
    up.publish_book("0.3", "0.4");
    up.publish_flow("buy");
    up.publish_momentum("5", "up");
    assert_eq!(up.wait_for_position(child, |p| p > 0).await, 50);
    assert_eq!(up.wait_for_leg(&id, "holding").await, "holding");

    let down = Rig::new(SERIES[0].id, 300, 400, 500);
    let (child, id) = down.deploy("momentum-rider", Side::Primary).await;
    down.publish_book("0.3", "0.4");
    down.publish_flow("sell");
    down.publish_momentum("-5", "down");
    assert_eq!(down.wait_for_position(child, |p| p < 0).await, -28);
    assert_eq!(down.wait_for_leg(&id, "holding").await, "holding");
}

#[tokio::test]
async fn burst_fader_waits_for_the_stall_then_fades_and_takes_profit() {
    let rig = Rig::new(SERIES[0].id, 300, 400, 500);
    let (child, id) = rig.deploy("momentum-rider", Side::Inverse).await;
    rig.publish_book("0.3", "0.4");
    rig.publish_flow("buy");
    rig.publish_momentum("5", "up");
    assert_eq!(rig.wait_for_leg(&id, "after_up").await, "after_up");
    assert_eq!(rig.position(child), 0, "no order on the burst itself");

    // The burst stalls: fade it with Down (a sale of Up into the 0.30 bid).
    rig.publish_momentum("0.5", "flat");
    assert_eq!(rig.wait_for_position(child, |p| p < 0).await, -28);
    assert_eq!(rig.wait_for_leg(&id, "holding_down").await, "holding_down");

    // Down was bought at 0.70; a Down bid of 0.74 or better takes profit.
    // That is an Up ask at 0.26 or under, so rest one and tick the book.
    // The book trigger's one-second cooldown started on the first book
    // frame, so let it lapse before the frame that should fire it.
    rig.rest(BookSide::Ask, 250, 500);
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    rig.publish_book("0.2", "0.25");
    assert_eq!(rig.wait_for_position(child, |p| p == 0).await, 0);
    assert_eq!(rig.wait_for_leg(&id, "done").await, "done");
}

// ───────────────────────────────── late-closer ─────────────────────────────────

#[tokio::test]
async fn late_closer_takes_the_side_spot_favors_inside_the_late_window() {
    let up = Rig::new(SERIES[0].id, 300, 400, 500);
    let (child, id) = up.deploy("late-closer", Side::Primary).await;
    up.publish_book("0.3", "0.4");
    up.publish_delta("5");
    assert_eq!(up.status(&id).await.current_leg, "waiting");
    up.publish_window(15_000);
    assert_eq!(up.wait_for_position(child, |p| p > 0).await, 50);
    assert_eq!(up.wait_for_leg(&id, "holding").await, "holding");

    let down = Rig::new(SERIES[0].id, 300, 400, 500);
    let (child, id) = down.deploy("late-closer", Side::Primary).await;
    down.publish_book("0.3", "0.4");
    down.publish_window(15_000);
    down.publish_delta("-5");
    assert_eq!(down.wait_for_position(child, |p| p < 0).await, -28);
    assert_eq!(down.wait_for_leg(&id, "holding").await, "holding");
}

#[tokio::test]
async fn late_lottery_buys_the_losing_side_for_a_dime() {
    // Up is priced 0.92 / 0.95: decided. Down's ask is 0.08.
    let rig = Rig::new(SERIES[0].id, 920, 950, 500);
    let (child, id) = rig.deploy("late-closer", Side::Inverse).await;
    rig.publish_book("0.92", "0.95");
    rig.publish_window(15_000);
    let pos = rig.wait_for_position(child, |p| p < 0).await;
    assert_eq!(pos, -125, "$10 of Down at 0.08");
    assert_eq!(rig.wait_for_leg(&id, "holding").await, "holding");
}

// ────────────────────────────────── oracle-gap ──────────────────────────────────

#[tokio::test]
async fn oracle_gap_follows_the_oracle_either_way() {
    let up = Rig::new(SERIES[0].id, 300, 400, 500);
    let (child, id) = up.deploy("oracle-gap", Side::Primary).await;
    up.publish_book("0.3", "0.4");
    up.publish_venues("49990");
    up.publish("btc-5m.current.delta.v1", &[("delta_bps", json!("2")), ("spot", json!("50010")), ("strike", json!("50000"))]);
    assert_eq!(up.wait_for_position(child, |p| p > 0).await, 50);
    assert_eq!(up.wait_for_leg(&id, "holding").await, "holding");

    let down = Rig::new(SERIES[0].id, 300, 400, 500);
    let (child, id) = down.deploy("oracle-gap", Side::Primary).await;
    down.publish_book("0.3", "0.4");
    down.publish_venues("50010");
    down.publish("btc-5m.current.delta.v1", &[("delta_bps", json!("-2")), ("spot", json!("49990")), ("strike", json!("50000"))]);
    assert_eq!(down.wait_for_position(child, |p| p < 0).await, -28);
    assert_eq!(down.wait_for_leg(&id, "holding").await, "holding");
}

#[tokio::test]
async fn oracle_snapback_waits_for_the_gap_to_close() {
    let rig = Rig::new(SERIES[0].id, 300, 400, 500);
    let (child, id) = rig.deploy("oracle-gap", Side::Inverse).await;
    rig.publish_book("0.3", "0.4");
    rig.publish_venues("49990");
    rig.publish("btc-5m.current.delta.v1", &[("delta_bps", json!("2")), ("spot", json!("50010")), ("strike", json!("50000"))]);
    assert_eq!(rig.wait_for_leg(&id, "led_up").await, "led_up");
    assert_eq!(rig.position(child), 0);

    // The venues catch up: take Down.
    rig.publish_venues("50010");
    assert_eq!(rig.wait_for_position(child, |p| p < 0).await, -28);
    assert_eq!(rig.wait_for_leg(&id, "holding_down").await, "holding_down");
}

// ─────────────────────────────── order-flow-lead ───────────────────────────────

#[tokio::test]
async fn order_flow_lead_front_runs_the_building_side() {
    let up = Rig::new(SERIES[0].id, 300, 400, 500);
    let (child, id) = up.deploy("order-flow-lead", Side::Primary).await;
    up.publish_book("0.3", "0.4");
    up.publish_depth("0.3");
    up.publish_ofi("3");
    assert_eq!(up.wait_for_position(child, |p| p > 0).await, 50);
    assert_eq!(up.wait_for_leg(&id, "holding").await, "holding");

    let down = Rig::new(SERIES[0].id, 300, 400, 500);
    let (child, id) = down.deploy("order-flow-lead", Side::Primary).await;
    down.publish_book("0.3", "0.4");
    down.publish_depth("-0.3");
    down.publish_ofi("-3");
    assert_eq!(down.wait_for_position(child, |p| p < 0).await, -28);
    assert_eq!(down.wait_for_leg(&id, "holding").await, "holding");
}

#[tokio::test]
async fn order_flow_fade_only_fires_once_the_price_has_moved() {
    // Bids building while Up already trades at 0.65: the move is in.
    let rig = Rig::new(SERIES[0].id, 550, 650, 500);
    let (child, id) = rig.deploy("order-flow-lead", Side::Inverse).await;
    rig.publish_book("0.55", "0.65");
    rig.publish_depth("0.3");
    rig.publish_ofi("3");
    let pos = rig.wait_for_position(child, |p| p < 0).await;
    assert_eq!(pos, -44, "$20 of Down at its 0.45 ask");
    assert_eq!(rig.wait_for_leg(&id, "holding_down").await, "holding_down");

    // A cheap book (Up at 0.30) would not have fired it.
    let quiet = Rig::new(SERIES[0].id, 300, 400, 500);
    let (child, id) = quiet.deploy("order-flow-lead", Side::Inverse).await;
    quiet.publish_book("0.3", "0.4");
    quiet.publish_depth("0.3");
    quiet.publish_ofi("3");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(quiet.position(child), 0);
    assert_eq!(quiet.status(&id).await.current_leg, "hunting");
}

// ─────────────────────────────── absorption-fade ───────────────────────────────

#[tokio::test]
async fn absorption_fade_buys_against_absorbed_pressure_either_way() {
    let up = Rig::new(SERIES[0].id, 300, 400, 500);
    let (child, id) = up.deploy("absorption-fade", Side::Primary).await;
    up.publish_book("0.3", "0.4");
    up.publish_absorption("absorbing", "sell");
    assert_eq!(up.wait_for_position(child, |p| p > 0).await, 50);
    assert_eq!(up.wait_for_leg(&id, "holding").await, "holding");

    let down = Rig::new(SERIES[0].id, 300, 400, 500);
    let (child, id) = down.deploy("absorption-fade", Side::Primary).await;
    down.publish_book("0.3", "0.4");
    down.publish_absorption("absorbing", "buy");
    assert_eq!(down.wait_for_position(child, |p| p < 0).await, -28);
    assert_eq!(down.wait_for_leg(&id, "holding").await, "holding");
}

#[tokio::test]
async fn pressure_rider_goes_with_working_pressure_and_flips_when_it_is_absorbed() {
    let rig = Rig::new(SERIES[0].id, 300, 400, 500);
    let (child, id) = rig.deploy("absorption-fade", Side::Inverse).await;
    rig.publish_book("0.3", "0.4");
    rig.publish_absorption("price_responsive", "buy");
    assert_eq!(rig.wait_for_position(child, |p| p > 0).await, 50);
    assert_eq!(rig.wait_for_leg(&id, "holding_up").await, "holding_up");

    // The same buying starts to get absorbed: close the fifty and go $20 Down.
    rig.publish_absorption("absorbing", "buy");
    assert_eq!(rig.wait_for_position(child, |p| p < 0).await, -28);
    assert_eq!(rig.wait_for_leg(&id, "holding_down").await, "holding_down");
}

// ─────────────────────────────── vwap-reversion ───────────────────────────────

#[tokio::test]
async fn vwap_reversion_joins_the_bid_on_the_stretched_side_and_requotes() {
    // BTC pushed below VWAP with Up at 0.35: join the 0.30 bid on Up.
    let up = Rig::new(SERIES[0].id, 300, 400, 500);
    let (child, id) = up.deploy("vwap-reversion", Side::Primary).await;
    up.publish_book("0.3", "0.4");
    up.publish_vwap("-3");
    let live = up.wait_for_live_order(&id, "up", "0.3").await;
    assert_eq!(live, vec![("up".into(), "0.3".into(), "66".into())], "$20 rests at the 0.30 bid");
    assert_eq!(up.position(child), 0, "a resting bid is not a fill");
    assert_eq!(up.wait_for_leg(&id, "quoting_up").await, "quoting_up");

    // The touch moves to 0.32: the requote follows it after the cooldown.
    up.rest(BookSide::Bid, 320, 500);
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    up.publish_book("0.32", "0.4");
    let live = up.wait_for_live_order(&id, "up", "0.32").await;
    assert_eq!(live.len(), 1, "one live bid after the replace: {live:?}");
    assert_eq!(live[0].1, "0.32");

    // BTC pushed above VWAP with Down at 0.35 (book 0.60 / 0.70): bid Down at 0.30.
    let down = Rig::new(SERIES[0].id, 600, 700, 500);
    let (child, id) = down.deploy("vwap-reversion", Side::Primary).await;
    down.publish_book("0.6", "0.7");
    down.publish_vwap("3");
    let live = down.wait_for_live_order(&id, "down", "0.3").await;
    assert_eq!(live, vec![("down".into(), "0.3".into(), "66".into())]);
    assert_eq!(down.position(child), 0);
    assert_eq!(down.wait_for_leg(&id, "quoting_down").await, "quoting_down");
}

#[tokio::test]
async fn vwap_rider_takes_the_stretch_and_flips_through_vwap() {
    let rig = Rig::new(SERIES[0].id, 300, 400, 500);
    let (child, id) = rig.deploy("vwap-reversion", Side::Inverse).await;
    rig.publish_book("0.3", "0.4");
    rig.publish_vwap("3");
    assert_eq!(rig.wait_for_position(child, |p| p > 0).await, 50);
    assert_eq!(rig.wait_for_leg(&id, "holding_up").await, "holding_up");

    rig.publish_vwap("-3");
    assert_eq!(rig.wait_for_position(child, |p| p < 0).await, -28);
    assert_eq!(rig.wait_for_leg(&id, "holding_down").await, "holding_down");
}

// ───────────────────────────────── trend-rider ─────────────────────────────────

#[tokio::test]
async fn trend_rider_takes_the_trend_and_stops_out_on_a_reversal() {
    let rig = Rig::new(SERIES[1].id, 300, 400, 500);
    let (child, id) = rig.deploy("trend-rider", Side::Primary).await;
    rig.publish_book("0.3", "0.4");
    rig.publish_momentum("5", "up");
    rig.publish_horizon("10");
    assert_eq!(rig.wait_for_position(child, |p| p > 0).await, 50);
    assert_eq!(rig.wait_for_leg(&id, "holding_up").await, "holding_up");

    // Paid 0.40; a bid at 0.30 or under is the stop. Flatten at market.
    tokio::time::sleep(Duration::from_millis(2_100)).await;
    rig.publish_book("0.28", "0.4");
    assert_eq!(rig.wait_for_position(child, |p| p == 0).await, 0);
    assert_eq!(rig.wait_for_leg(&id, "done").await, "done");

    let down = Rig::new(SERIES[1].id, 300, 400, 500);
    let (child, id) = down.deploy("trend-rider", Side::Primary).await;
    down.publish_book("0.3", "0.4");
    down.publish_momentum("-5", "down");
    down.publish_horizon("-10");
    assert_eq!(down.wait_for_position(child, |p| p < 0).await, -28);
    assert_eq!(down.wait_for_leg(&id, "holding_down").await, "holding_down");
}

#[tokio::test]
async fn trend_reversal_enters_on_the_turn_and_flips_when_the_trend_reasserts() {
    let rig = Rig::new(SERIES[1].id, 300, 400, 500);
    let (child, id) = rig.deploy("trend-rider", Side::Inverse).await;
    rig.publish_book("0.3", "0.4");
    rig.publish_momentum("5", "up");
    rig.publish_horizon("10");
    assert_eq!(rig.wait_for_leg(&id, "trended_up").await, "trended_up");
    assert_eq!(rig.position(child), 0, "the trend itself places nothing");

    // The thirty-second move turns against the trend: take Down.
    rig.publish_momentum("-4", "down");
    assert_eq!(rig.wait_for_position(child, |p| p < 0).await, -28);
    assert_eq!(rig.wait_for_leg(&id, "holding_down").await, "holding_down");

    // The trend reasserts: flip, closing the twenty-eight and going $20 Up.
    // The reassert row was already true (masked) at the start, so let its
    // cooldown lapse first.
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    rig.publish_momentum("5", "up");
    assert_eq!(rig.wait_for_position(child, |p| p > 0).await, 50);
    assert_eq!(rig.wait_for_leg(&id, "done").await, "done");
}

// ─────────────────────────────── sweep-follower ───────────────────────────────

#[tokio::test]
async fn sweep_follower_goes_with_the_sweep_and_the_fader_goes_against_it() {
    let rig = Rig::new(SERIES[0].id, 300, 400, 500);
    let (child, id) = rig.deploy("sweep-follower", Side::Primary).await;
    rig.publish_book("0.3", "0.4");
    rig.publish_sweep("kraken", "none");
    rig.publish_sweep("coinbase", "buy");
    assert_eq!(rig.wait_for_position(child, |p| p > 0).await, 50);
    assert_eq!(rig.wait_for_leg(&id, "holding").await, "holding");

    let down = Rig::new(SERIES[0].id, 300, 400, 500);
    let (child, id) = down.deploy("sweep-follower", Side::Primary).await;
    down.publish_book("0.3", "0.4");
    down.publish_sweep("coinbase", "none");
    down.publish_sweep("kraken", "sell");
    assert_eq!(down.wait_for_position(child, |p| p < 0).await, -28);
    assert_eq!(down.wait_for_leg(&id, "holding").await, "holding");

    let fade = Rig::new(SERIES[0].id, 300, 400, 500);
    let (child, id) = fade.deploy("sweep-follower", Side::Inverse).await;
    fade.publish_book("0.3", "0.4");
    fade.publish_sweep("kraken", "none");
    fade.publish_sweep("coinbase", "buy");
    assert_eq!(fade.wait_for_position(child, |p| p < 0).await, -28);
    assert_eq!(fade.wait_for_leg(&id, "holding").await, "holding");
}

// ───────────────────────────── polymarket-discount ─────────────────────────────

#[tokio::test]
async fn polymarket_discount_bids_under_the_richer_polymarket_side() {
    // Polymarket says Up is 0.50 while our ask is 0.40: bid Up one cent
    // under its midpoint. That bid crosses our ask, so it fills at 0.40.
    let up = Rig::new(SERIES[1].id, 300, 400, 500);
    let (child, id) = up.deploy("polymarket-discount", Side::Primary).await;
    up.publish_book("0.3", "0.4");
    up.publish_strike();
    up.publish_contract();
    up.publish_poly_quote("0.5", "0.5");
    assert_eq!(up.wait_for_position(child, |p| p > 0).await, 40, "$20 over the 0.50 midpoint");
    assert_eq!(up.wait_for_leg(&id, "done").await, "done");

    // Polymarket says Down is 0.80 while our Down ask is 0.70: bid Down.
    let down = Rig::new(SERIES[1].id, 300, 400, 500);
    let (child, id) = down.deploy("polymarket-discount", Side::Primary).await;
    down.publish_book("0.3", "0.4");
    down.publish_strike();
    down.publish_contract();
    down.publish_poly_quote("0.2", "0.8");
    assert_eq!(down.wait_for_position(child, |p| p < 0).await, -25, "$20 over the 0.80 midpoint");
    assert_eq!(down.wait_for_leg(&id, "done").await, "done");
}

#[tokio::test]
async fn polymarket_convergence_fades_our_rich_side_and_exits_when_the_two_agree() {
    // Our bid on Up is 0.30 against a Polymarket midpoint of 0.20: take Down.
    let rig = Rig::new(SERIES[1].id, 300, 400, 500);
    let (child, id) = rig.deploy("polymarket-discount", Side::Inverse).await;
    rig.publish_book("0.3", "0.4");
    rig.publish_strike();
    rig.publish_contract();
    rig.publish_poly_quote("0.2", "0.8");
    assert_eq!(rig.wait_for_position(child, |p| p < 0).await, -28);
    assert_eq!(rig.wait_for_leg(&id, "holding_down").await, "holding_down");

    // Polymarket comes to us: its Down midpoint meets our Down bid of 0.60.
    rig.publish_poly_quote("0.4", "0.6");
    assert_eq!(rig.wait_for_position(child, |p| p == 0).await, 0);
    assert_eq!(rig.wait_for_leg(&id, "done").await, "done");
}

// ───────────────────────────── polymarket-momentum ─────────────────────────────

#[tokio::test]
async fn polymarket_momentum_follows_the_odds_and_the_inverse_fades_them() {
    let up = Rig::new(SERIES[1].id, 300, 400, 500);
    let (child, id) = up.deploy("polymarket-momentum", Side::Primary).await;
    up.publish_book("0.3", "0.4");
    up.publish_strike();
    up.publish_contract();
    up.publish_poly_momentum("4", "up");
    let pos = up.wait_for_position(child, |p| p > 0).await;
    let s = up.status(&id).await;
    assert_eq!(pos, 50, "leg {} armed {:?} log {:#?}", s.current_leg, s.armed_triggers, s.log);
    assert_eq!(up.wait_for_leg(&id, "holding").await, "holding");

    let fade = Rig::new(SERIES[1].id, 300, 400, 500);
    let (child, id) = fade.deploy("polymarket-momentum", Side::Inverse).await;
    fade.publish_book("0.3", "0.4");
    fade.publish_strike();
    fade.publish_contract();
    fade.publish_poly_momentum("4", "up");
    assert_eq!(fade.wait_for_position(child, |p| p < 0).await, -28);
    assert_eq!(fade.wait_for_leg(&id, "holding").await, "holding");
}


// ───────────────────────────────── inside-quote ─────────────────────────────────

fn sorted(mut live: Vec<(String, String, String)>) -> Vec<(String, String, String)> {
    live.sort();
    live
}

/// The book is 0.40 / 0.50. With BTC five basis points above the strike
/// the primary rests an Up bid one cent inside the best bid; five below,
/// a Down bid one cent inside the best Down bid (1 - 0.50, so 0.51).
#[tokio::test]
async fn inside_quote_rests_one_cent_inside_on_the_side_the_strike_favours() {
    let up = Rig::new(SERIES[0].id, 400, 500, 500);
    let (child, id) = up.deploy("inside-quote", Side::Primary).await;
    up.publish_book("0.4", "0.5");
    up.publish_delta("5");
    let live = up.wait_for_live_order(&id, "up", "0.41").await;
    assert_eq!(live, vec![("up".into(), "0.41".into(), "48".into())], "$20 rests at 0.41");
    assert_eq!(up.position(child), 0, "a resting bid is not a fill");
    assert_eq!(up.wait_for_leg(&id, "quoting").await, "quoting");

    let down = Rig::new(SERIES[0].id, 400, 500, 500);
    let (child, id) = down.deploy("inside-quote", Side::Primary).await;
    down.publish_book("0.4", "0.5");
    down.publish_delta("-5");
    let live = down.wait_for_live_order(&id, "down", "0.51").await;
    assert_eq!(live, vec![("down".into(), "0.51".into(), "39".into())], "$20 rests at 0.51 Down");
    assert_eq!(down.position(child), 0);
}

#[tokio::test]
async fn inside_quote_stays_out_without_a_lean_or_without_room() {
    // BTC is on the strike: no side is favoured.
    let flat = Rig::new(SERIES[0].id, 400, 500, 500);
    let (_, id) = flat.deploy("inside-quote", Side::Primary).await;
    flat.publish_book("0.4", "0.5");
    flat.publish_delta("1");
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(flat.live_orders(&id).await.is_empty());
    assert_eq!(flat.status(&id).await.current_leg, "hunting");

    // A two-cent spread has no room for a quote a cent inside each side.
    let tight = Rig::new(SERIES[0].id, 400, 420, 500);
    let (_, id) = tight.deploy("inside-quote", Side::Primary).await;
    tight.publish_book("0.4", "0.42");
    tight.publish_delta("5");
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(tight.live_orders(&id).await.is_empty());

    // One side of the book is empty (the frame says bid 0): nothing to be inside of.
    let empty = Rig::new(SERIES[0].id, 400, 500, 500);
    let (_, id) = empty.deploy("inside-quote", Side::Primary).await;
    empty.publish_book("0", "0.5");
    empty.publish_delta("5");
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(empty.live_orders(&id).await.is_empty());
}

/// The quote is pulled after the refresh interval and put back one cent
/// inside the best bid as it stands once the book has been published
/// without it, so it never climbs over its own order.
#[tokio::test]
async fn inside_quote_pulls_and_requotes_inside_the_new_best() {
    let rig = Rig::new(SERIES[0].id, 400, 500, 500);
    let (_, id) = rig.deploy("inside-quote", Side::Primary).await;
    rig.publish_book("0.4", "0.5");
    rig.publish_delta("5");
    rig.wait_for_live_order(&id, "up", "0.41").await;

    // Our own bid is now the best in the frame; the refresh pulls it.
    tokio::time::sleep(Duration::from_millis(5_100)).await;
    rig.publish_book("0.41", "0.5");
    assert_eq!(rig.wait_for_leg(&id, "settling").await, "settling");
    assert!(rig.live_orders(&id).await.is_empty(), "pulled");
    // A lean arriving before the book has been republished places nothing.
    rig.publish_delta("5");
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(rig.live_orders(&id).await.is_empty(), "no quote off the stale book");

    // The best bid moved to 0.43 meanwhile; the book is published without ours.
    rig.rest(BookSide::Bid, 430, 500);
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    rig.publish_book("0.43", "0.5");
    assert_eq!(rig.wait_for_leg(&id, "hunting").await, "hunting");
    // The lean row was last seen true on that book frame, while masked; its
    // one-second cooldown runs from there.
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    rig.publish_delta("5");
    let live = rig.wait_for_live_order(&id, "up", "0.44").await;
    assert_eq!(live.len(), 1, "one live bid: {live:?}");
    assert_eq!(live[0].1, "0.44");
}

#[tokio::test]
async fn inside_quote_holds_a_fill_and_stops_quoting() {
    let rig = Rig::new(SERIES[0].id, 400, 500, 500);
    let (child, id) = rig.deploy("inside-quote", Side::Primary).await;
    rig.publish_book("0.4", "0.5");
    rig.publish_delta("5");
    rig.wait_for_live_order(&id, "up", "0.41").await;
    // Someone sells down to our bid.
    rig.rest(BookSide::Ask, 410, 48);
    assert_eq!(rig.wait_for_position(child, |p| p > 0).await, 48);
    assert_eq!(rig.wait_for_leg(&id, "holding").await, "holding");
    rig.publish_book("0.4", "0.5");
    rig.publish_delta("5");
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(rig.live_orders(&id).await.is_empty(), "one fill a round");
}

/// The inverse quotes both sides a cent inside while BTC sits on the
/// strike, sized so the pair costs the stake; when both fill the pair is
/// closed for the spread and it starts again.
#[tokio::test]
async fn two_sided_quote_rests_both_sides_when_the_strike_says_nothing() {
    let rig = Rig::new(SERIES[0].id, 400, 500, 500);
    let (child, id) = rig.deploy("inside-quote", Side::Inverse).await;
    rig.publish_book("0.4", "0.5");
    rig.publish_delta("1");
    rig.wait_for_live_order(&id, "down", "0.51").await;
    let live = sorted(rig.wait_for_live_order(&id, "up", "0.41").await);
    assert_eq!(
        live,
        vec![("down".into(), "0.51".into(), "21".into()), ("up".into(), "0.41".into(), "21".into())],
        "$20 buys 21 pairs at 0.41 + 0.51"
    );

    // The Up bid fills; the Down bid stays out to finish the pair.
    rig.rest(BookSide::Ask, 410, 21);
    assert_eq!(rig.wait_for_position(child, |p| p > 0).await, 21);
    assert_eq!(rig.wait_for_leg(&id, "one_side").await, "one_side");
    assert_eq!(rig.live_orders(&id).await, vec![("down".into(), "0.51".into(), "21".into())]);

    // Someone bids Up at 0.49, which is our Down bid at 0.51: the pair is whole.
    rig.rest(BookSide::Bid, 490, 21);
    assert_eq!(rig.wait_for_position(child, |p| p == 0).await, 0);
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    rig.publish_book("0.4", "0.5");
    assert_eq!(rig.wait_for_leg(&id, "hunting").await, "hunting");
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    rig.publish_delta("1");
    rig.wait_for_live_order(&id, "down", "0.51").await;
    let live = rig.wait_for_live_order(&id, "up", "0.41").await;
    assert_eq!(live.len(), 2, "a new pair: {live:?}");
}

#[tokio::test]
async fn two_sided_quote_stays_out_when_the_strike_leans() {
    let rig = Rig::new(SERIES[0].id, 400, 500, 500);
    let (_, id) = rig.deploy("inside-quote", Side::Inverse).await;
    rig.publish_book("0.4", "0.5");
    rig.publish_delta("5");
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(rig.live_orders(&id).await.is_empty());
}

// ─────────────────────────────────── late-fade ───────────────────────────────────

/// Fifty seconds left, Up bid 0.92 and nobody offering Up at all: the
/// primary buys Down, small. The same with offers still there does nothing.
#[tokio::test]
async fn late_fade_takes_the_other_side_small_when_the_offers_are_gone() {
    let rig = Rig::new(SERIES[0].id, 920, 990, 500);
    let (child, id) = rig.deploy("late-fade", Side::Primary).await;
    rig.publish_book("0.92", "1");
    rig.publish_window(50_000);
    assert_eq!(rig.wait_for_position(child, |p| p < 0).await, -62, "$5 of Down at 0.08");
    assert_eq!(rig.wait_for_leg(&id, "holding").await, "holding");

    // The mirror image: Up offered at 0.08 and nobody bidding for it.
    let down = Rig::new(SERIES[0].id, 10, 80, 500);
    let (child, _) = down.deploy("late-fade", Side::Primary).await;
    down.publish_book("0", "0.08");
    down.publish_window(50_000);
    assert_eq!(down.wait_for_position(child, |p| p > 0).await, 62, "$5 of Up at 0.08");

    let two_sided = Rig::new(SERIES[0].id, 920, 950, 500);
    let (child, id) = two_sided.deploy("late-fade", Side::Primary).await;
    two_sided.publish_book("0.92", "0.95");
    two_sided.publish_window(50_000);
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(two_sided.position(child), 0, "offers still there");
    assert_eq!(two_sided.status(&id).await.current_leg, "waiting");

    let early = Rig::new(SERIES[0].id, 920, 990, 500);
    let (child, _) = early.deploy("late-fade", Side::Primary).await;
    early.publish_book("0.92", "1");
    early.publish_window(120_000);
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(early.position(child), 0, "not inside the last minute");
}

/// The inverse goes with the runaway: it bids a cent above the best bid
/// for the favourite, small, and holds what fills.
#[tokio::test]
async fn late_join_bids_a_cent_above_the_best_for_the_favourite() {
    let rig = Rig::new(SERIES[0].id, 920, 990, 500);
    let (child, id) = rig.deploy("late-fade", Side::Inverse).await;
    rig.publish_book("0.92", "1");
    rig.publish_window(50_000);
    let live = rig.wait_for_live_order(&id, "up", "0.93").await;
    assert_eq!(live, vec![("up".into(), "0.93".into(), "5".into())], "$5 rests at 0.93");
    assert_eq!(rig.position(child), 0);
    rig.rest(BookSide::Ask, 930, 5);
    assert_eq!(rig.wait_for_position(child, |p| p > 0).await, 5);
    assert_eq!(rig.wait_for_leg(&id, "holding").await, "holding");
}

/// The NBA template: the model's home win probability against the home
/// book. A cheap home ask is bought; once the bid reaches the model the
/// position is sold out and the deployment hunts again.
#[tokio::test]
async fn model_vs_crowd_buys_home_under_the_model_and_sells_out_at_it() {
    let nba = SERIES.iter().find(|s| s.underlying == "nba").expect("an NBA series in the table").id;
    let rig = Rig::new(nba, 300, 400, 500);
    let (child, id) = rig.deploy("model-vs-crowd", Side::Primary).await;
    assert_eq!(rig.status(&id).await.current_leg, "hunting");

    rig.publish_window(60_000);
    let price_id = format!("{}.current.price.v1", rig.slug());
    rig.publish(
        &price_id,
        &[("home", json!("0.35")), ("away", json!("0.65")), ("bid", json!("0.3")), ("ask", json!("0.4")), ("last", json!("0"))],
    );
    // The model says 0.70 while the ask is 0.40: buy home.
    rig.publish(crate::registry::NBA_WINPROB_SIGNAL, &[("home", json!("0.700000"))]);
    let pos = rig.wait_for_position(child, |p| p > 0).await;
    let s = rig.status(&id).await;
    assert_eq!(pos, 50, "$20 at the 0.40 ask is fifty lots of home: {:#?}", s.log);
    assert_eq!(rig.wait_for_leg(&id, "holding_home").await, "holding_home");

    // The model collapses under the 0.30 bid: sell out.
    rig.publish(crate::registry::NBA_WINPROB_SIGNAL, &[("home", json!("0.200000"))]);
    assert_eq!(rig.wait_for_position(child, |p| p == 0).await, 0);
    assert_eq!(rig.wait_for_leg(&id, "hunting").await, "hunting");
}

// ───────────────────────────── lead-change (NBA) ─────────────────────────────

impl Rig {
    /// The NBA series' book frame: outcome fields are `home` / `away`.
    fn publish_home_book(&self, bid: &str, ask: &str) {
        let home = (Decimal::from_str(bid).unwrap() + Decimal::from_str(ask).unwrap()) / Decimal::from(2);
        let id = format!("{}.current.price.v1", self.slug());
        self.publish(
            &id,
            &[
                ("home", json!(home.normalize().to_string())),
                ("away", json!((Decimal::ONE - home).normalize().to_string())),
                ("bid", json!(bid)),
                ("ask", json!(ask)),
                ("last", json!("0")),
            ],
        );
    }

    /// One in-play play with the given scores, then the margin it implies.
    fn publish_play(&self, seq: u32, quarter: u8, home_score: u32, away_score: u32) {
        self.publish(
            crate::registry::NBA_PBP_SIGNAL,
            &[
                ("seq", json!(seq)),
                ("action", json!("two_pointer")),
                ("result", json!("made")),
                ("team", json!("BOS")),
                ("player", json!(1)),
                ("phase", json!("in_play")),
                ("home_team", json!("BOS")),
                ("away_team", json!("LAL")),
                ("quarter", json!(quarter)),
                ("home_score_before", json!(home_score)),
                ("away_score_before", json!(away_score)),
                ("home_score", json!(home_score)),
                ("away_score", json!(away_score)),
                ("clock_remaining", json!("600.0")),
            ],
        );
        self.publish(crate::registry::NBA_MARGIN_SIGNAL, &[("value", json!((i64::from(home_score) - i64::from(away_score)).to_string()))]);
    }
}

/// On the NBA book (quoted side `home`), Back the Leader buys Home when
/// Home leads and flips the whole position to Away when Away takes the
/// lead: the flip closes the Home lots and adds the stake on Away. In
/// the fourth quarter the next lead change is the last flip.
#[tokio::test]
async fn lead_change_backs_the_leader_and_flips_on_the_nba_book() {
    let rig = Rig::new(SERIES[2].id, 450, 550, 500);
    let (child, id) = rig.deploy("lead-change", Side::Primary).await;
    assert_eq!(rig.status(&id).await.current_leg, "hunting");

    rig.publish_home_book("0.45", "0.55");
    rig.publish_play(1, 1, 2, 0);
    let pos = rig.wait_for_position(child, |p| p > 0).await;
    assert_eq!(pos, 9, "$5 at the 0.55 Home ask is nine lots of Home");
    assert_eq!(rig.wait_for_leg(&id, "holding_home").await, "holding_home");

    // Away takes the lead before the fourth: close the nine Home lots and
    // buy the stake on Away as one Away buy (a sale of Home into the 0.45
    // bid): 9 + $5 / 0.55 = 18 lots, leaving nine lots of Away.
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    rig.publish_play(2, 2, 2, 5);
    let pos = rig.wait_for_position(child, |p| p < 0).await;
    assert_eq!(pos, -9);
    assert_eq!(rig.wait_for_leg(&id, "holding_away").await, "holding_away");

    // In the fourth quarter Home retakes the lead: one last flip, then locked.
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    rig.publish_play(3, 4, 9, 5);
    let pos = rig.wait_for_position(child, |p| p > 0).await;
    assert_eq!(pos, 9);
    assert_eq!(rig.wait_for_leg(&id, "locked").await, "locked");
}

// ───────────────────────────── venue-gap (NBA) ─────────────────────────────

impl Rig {
    /// One Polymarket reading of the home team's price.
    fn publish_polymarket_line(&self, home_prob: &str) {
        let home = Decimal::from_str(home_prob).unwrap();
        self.publish(
            crate::registry::NBA_POLYMARKET_SIGNAL,
            &[
                ("home_team", json!("BOS")),
                ("away_team", json!("LAL")),
                ("home_prob", json!(home.normalize().to_string())),
                ("away_prob", json!((Decimal::ONE - home).normalize().to_string())),
            ],
        );
    }

    /// One Kalshi reading of the home team's price.
    fn publish_kalshi_line(&self, home_prob: &str) {
        let home = Decimal::from_str(home_prob).unwrap();
        self.publish(
            crate::registry::NBA_KALSHI_SIGNAL,
            &[
                ("home_team", json!("BOS")),
                ("away_team", json!("LAL")),
                ("home_prob", json!(home.normalize().to_string())),
                ("away_prob", json!((Decimal::ONE - home).normalize().to_string())),
            ],
        );
    }

    fn publish_margin(&self, margin: i64) {
        self.publish(crate::registry::NBA_MARGIN_SIGNAL, &[("value", json!(margin.to_string()))]);
    }
}

/// Fade the Line Move treats a Polymarket jump as noise only when the
/// score did not change between the two readings. A jump that came with a
/// basket is news and is left alone; the next jump, with the score where
/// it was, is faded with Away.
#[tokio::test]
async fn the_line_move_fader_leaves_a_jump_that_came_with_a_basket_alone() {
    let nba = SERIES.iter().find(|s| s.underlying == "nba").expect("an NBA series in the table").id;
    let rig = Rig::new(nba, 450, 550, 500);
    let (child, id) = rig.deploy("venue-gap", Side::Inverse).await;
    assert_eq!(rig.status(&id).await.current_leg, "hunting");

    rig.publish_home_book("0.45", "0.55");
    rig.publish_margin(0);
    rig.publish_polymarket_line("0.50");
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Home scores and the line rises three cents with it: not a fade.
    rig.publish_margin(2);
    rig.publish_polymarket_line("0.53");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(rig.position(child), 0, "a move that came with a basket is not noise");
    assert_eq!(rig.status(&id).await.current_leg, "hunting");

    // The line rises three more cents with the score where it was: fade it.
    rig.publish_polymarket_line("0.56");
    let pos = rig.wait_for_position(child, |p| p < 0).await;
    assert_eq!(pos, -9, "$5 at the 0.55 Away ask is nine lots of Away");
    assert_eq!(rig.wait_for_leg(&id, "holding_away").await, "holding_away");
}

/// While a position is held no reading is compared. After the exit the
/// first reading only starts the comparison again: a line that drifted
/// during the hold is not a jump.
#[tokio::test]
async fn after_a_hold_the_line_move_fader_starts_from_the_next_reading() {
    let nba = SERIES.iter().find(|s| s.underlying == "nba").expect("an NBA series in the table").id;
    // Nine lots at the ask: the entry takes all of them.
    let rig = Rig::new(nba, 450, 550, 9);
    let (child, id) = rig.deploy("venue-gap", Side::Inverse).await;

    rig.publish_home_book("0.45", "0.55");
    rig.publish_margin(0);
    rig.publish_polymarket_line("0.50");
    tokio::time::sleep(Duration::from_millis(150)).await;
    // The line falls three cents with the score where it was: fade it with Home.
    rig.publish_polymarket_line("0.47");
    assert_eq!(rig.wait_for_position(child, |p| p > 0).await, 9);
    assert_eq!(rig.wait_for_leg(&id, "holding_home").await, "holding_home");

    // Home's bid is three cents over the entry: take the profit.
    rig.rest(BookSide::Bid, 580, 20);
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    rig.publish_home_book("0.58", "0.68");
    assert_eq!(rig.wait_for_position(child, |p| p == 0).await, 0);
    assert_eq!(rig.wait_for_leg(&id, "hunting").await, "hunting");

    // The line is back at 0.50, three cents above the reading at the
    // entry. Nothing was compared during the hold, so this is no jump.
    rig.publish_polymarket_line("0.50");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(rig.position(child), 0, "a drift over the hold is not a jump");

    // The next reading is compared with that one: three cents up is faded.
    rig.publish_polymarket_line("0.53");
    assert!(rig.wait_for_position(child, |p| p < 0).await < 0);
}

// ───────────────────────────── kalshi-gap (NBA) ─────────────────────────────

/// Fade the Kalshi Move treats a Kalshi jump as noise only when the score
/// did not change between the two readings. A jump that came with a basket
/// is news and is left alone; the next jump, with the score where it was,
/// is faded with Away.
#[tokio::test]
async fn the_kalshi_move_fader_leaves_a_jump_that_came_with_a_basket_alone() {
    let nba = SERIES.iter().find(|s| s.underlying == "nba").expect("an NBA series in the table").id;
    let rig = Rig::new(nba, 450, 550, 500);
    let (child, id) = rig.deploy("kalshi-gap", Side::Inverse).await;
    assert_eq!(rig.status(&id).await.current_leg, "hunting");

    rig.publish_home_book("0.45", "0.55");
    rig.publish_margin(0);
    rig.publish_kalshi_line("0.50");
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Home scores and the line rises three cents with it: not a fade.
    rig.publish_margin(2);
    rig.publish_kalshi_line("0.53");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(rig.position(child), 0, "a move that came with a basket is not noise");
    assert_eq!(rig.status(&id).await.current_leg, "hunting");

    // The line rises three more cents with the score where it was: fade it.
    rig.publish_kalshi_line("0.56");
    let pos = rig.wait_for_position(child, |p| p < 0).await;
    assert_eq!(pos, -9, "$5 at the 0.55 Away ask is nine lots of Away");
    assert_eq!(rig.wait_for_leg(&id, "holding_away").await, "holding_away");
}

/// While a position is held no reading is compared. After the exit the
/// first reading only starts the comparison again: a line that drifted
/// during the hold is not a jump.
#[tokio::test]
async fn after_a_hold_the_kalshi_move_fader_starts_from_the_next_reading() {
    let nba = SERIES.iter().find(|s| s.underlying == "nba").expect("an NBA series in the table").id;
    // Nine lots at the ask: the entry takes all of them.
    let rig = Rig::new(nba, 450, 550, 9);
    let (child, id) = rig.deploy("kalshi-gap", Side::Inverse).await;

    rig.publish_home_book("0.45", "0.55");
    rig.publish_margin(0);
    rig.publish_kalshi_line("0.50");
    tokio::time::sleep(Duration::from_millis(150)).await;
    // The line falls three cents with the score where it was: fade it with Home.
    rig.publish_kalshi_line("0.47");
    assert_eq!(rig.wait_for_position(child, |p| p > 0).await, 9);
    assert_eq!(rig.wait_for_leg(&id, "holding_home").await, "holding_home");

    // Home's bid is three cents over the entry: take the profit.
    rig.rest(BookSide::Bid, 580, 20);
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    rig.publish_home_book("0.58", "0.68");
    assert_eq!(rig.wait_for_position(child, |p| p == 0).await, 0);
    assert_eq!(rig.wait_for_leg(&id, "hunting").await, "hunting");

    // The line is back at 0.50, three cents above the reading at the
    // entry. Nothing was compared during the hold, so this is no jump.
    rig.publish_kalshi_line("0.50");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(rig.position(child), 0, "a drift over the hold is not a jump");

    // The next reading is compared with that one: three cents up is faded.
    rig.publish_kalshi_line("0.53");
    assert!(rig.wait_for_position(child, |p| p < 0).await < 0);
}
