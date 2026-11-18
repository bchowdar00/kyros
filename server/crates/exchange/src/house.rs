//! The house account: the exchange's own user, which runs every public
//! template so the strategy cards are never empty.
//!
//! At boot the account is registered as a user under the template
//! creator's name, credited with enough cash for the seeding, and then
//! deploys one Primary and one Inverse of every template on every series
//! it lists, through the same in-process path the template deploy route
//! runs (`deploy_template_side`). Each deployment is an ordinary funded
//! sub-account; the cards count them like anyone else's. Starting before
//! the first round opens is safe: a deployment connects to the platform
//! without a market and its window timers arm on the first open.
//!
//! Each deployment carries a significant allocation, so the cards show
//! real capital at work: an amount between [`HOUSE_ALLOCATION_MIN`] and
//! [`HOUSE_ALLOCATION_MAX`] fixed per template by a hash of its id
//! ([`house_allocation`]), the same on every boot and on every series the
//! template lists. The house does not risk the whole amount: its envelope
//! ([`house_envelope`]) caps the loss budget at a quarter of the
//! allocation, which the risk gate enforces as worst-case exposure, and
//! keeps in-flight orders under half of what remains of that budget.
//!
//! `HOUSE_SEED_ENABLED=0` skips the seeding (quick local boots).

use rust_decimal::Decimal;
use strategy_compiler::ir::Envelope;

use crate::api::AppState;
use crate::api::strategy::deploy_template_side_with;
use crate::engine::Exchange;
use crate::templates::{EnvelopeRatios, Side, TemplateCatalogue, envelope_for};
use crate::types::{Address, Tenths};

/// A fixed address, so the house is the same account across restarts.
pub const HOUSE_ADDRESS: Address = Address([0x71, 0x4e, 0x79, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01]);
/// The name the cards show for the creator; the template files name
/// their creator the same way, which is how a card knows the house runs
/// it.
pub const HOUSE_USERNAME: &str = "Tiny";
/// The least a house deployment is funded with: $8,000.
pub const HOUSE_ALLOCATION_MIN: Tenths = Tenths(8_000_000);
/// The most: $64,000.
pub const HOUSE_ALLOCATION_MAX: Tenths = Tenths(64_000_000);
/// Allocations land on whole hundreds of dollars.
const HOUSE_ALLOCATION_STEP: Tenths = Tenths(100_000);
/// The share of a house allocation the strategy may put at risk.
const HOUSE_LOSS_BUDGET_FRACTION: Decimal = Decimal::from_parts(25, 0, 0, false, 2);
/// The share of the remaining loss budget that may be in flight at once.
const HOUSE_EXPOSURE_FRACTION: Decimal = Decimal::from_parts(5, 0, 0, false, 1);
pub const HOUSE_SEED_ENV: &str = "HOUSE_SEED_ENABLED";

/// Whether boot seeding is on (the default).
pub fn seeding_enabled() -> bool {
    !matches!(std::env::var(HOUSE_SEED_ENV).as_deref(), Ok("0") | Ok("false") | Ok("off"))
}

/// FNV-1a over the bytes: a hash that is the same on every build and
/// every platform, unlike the standard library's hasher, so anything
/// derived from it (allocations, display counts) survives a restart.
pub fn stable_hash(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3))
}

/// The cash the house puts into each deployment of `template_id`: fixed
/// by the id, so a template's number is the same on every series it
/// lists and on every boot, and spread over the range in $100 steps.
pub fn house_allocation(template_id: &str) -> Tenths {
    let steps = (HOUSE_ALLOCATION_MAX.0 - HOUSE_ALLOCATION_MIN.0) / HOUSE_ALLOCATION_STEP.0 + 1;
    let pick = (stable_hash(template_id) % steps as u64) as i64;
    Tenths(HOUSE_ALLOCATION_MIN.0 + pick * HOUSE_ALLOCATION_STEP.0)
}

/// The cash the seeding needs: one allocation per template per listed
/// series per side.
pub fn seed_credit(templates: &TemplateCatalogue) -> Tenths {
    Tenths(templates.iter().map(|t| house_allocation(&t.id).0 * (t.series.len() * Side::ALL.len()) as i64).sum())
}

/// The envelope a house deployment runs under: the template's ratios,
/// with the loss budget cut to a quarter of the allocation and in-flight
/// exposure held under half of what remains of it.
pub fn house_envelope(ratios: &EnvelopeRatios, allocation: Tenths) -> Envelope {
    let dollars = Decimal::new(allocation.0, 3);
    let mut envelope = envelope_for(ratios, dollars);
    envelope.loss_budget = crate::templates::decimal_str((dollars * HOUSE_LOSS_BUDGET_FRACTION).normalize());
    envelope.max_exposure_fraction = Some(crate::templates::decimal_str(HOUSE_EXPOSURE_FRACTION));
    envelope
}

/// Register the house as a user and credit it with enough cash for
/// every template deployment on top of its starting balance. Idempotent
/// on the identity; the credit is added each time.
pub fn seed_house_account(exchange: &mut Exchange, templates: &TemplateCatalogue) -> Address {
    exchange.register_user(HOUSE_ADDRESS);
    let acct = exchange.get_or_create_account(HOUSE_ADDRESS);
    acct.username = Some(HOUSE_USERNAME.to_string());
    exchange.ledger_mut().credit(HOUSE_ADDRESS, seed_credit(templates));
    HOUSE_ADDRESS
}

/// The outcome of the boot seeding.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SeedReport {
    pub deployed: usize,
    /// `(template, series slug, side, error)` for every side that did not start.
    pub failed: Vec<(String, String, Side, String)>,
}

/// Deploy both sides of every template on every series it lists, for the
/// house account. A failure on one document is reported and the rest
/// still deploy.
pub async fn deploy_house_templates(state: &AppState) -> SeedReport {
    let mut report = SeedReport::default();
    for template in state.templates.iter() {
        let allocation = house_allocation(&template.id);
        let envelope = house_envelope(&template.envelope, allocation);
        for &series in &template.series {
            for side in Side::ALL {
                match deploy_template_side_with(state, HOUSE_ADDRESS, &template.id, series, side, allocation, envelope.clone()).await {
                    Ok(_) => report.deployed += 1,
                    Err(e) => report.failed.push((template.id.clone(), series.slug().to_string(), side, format!("{e:?}"))),
                }
            }
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::strategy::store::tests::app_state_with;
    use crate::api::templates::TemplateTally;
    use crate::automation::lane_bridge::tests::registry_with_spot_lane;
    use crate::registry::PriceSource;
    use crate::series::SERIES;
    use crate::templates::TemplateCatalogue;
    use axum::Json;
    use std::sync::Arc;

    /// Every listed side deploys on a fresh exchange, the house pays for
    /// them all, and every card on both series shows the house running
    /// both sides with its portrait.
    #[tokio::test]
    async fn boot_seeding_runs_every_template_side_on_every_listed_series() {
        let registry = registry_with_spot_lane();
        let templates = TemplateCatalogue::load(&registry, PriceSource::Coinbase).unwrap_or_else(|e| panic!("{e}"));
        let documents = templates.document_count();
        let (mut state, _) = app_state_with(registry);
        state.templates = Arc::new(templates);

        let credit = seed_credit(&state.templates);
        assert!(credit.0 >= HOUSE_ALLOCATION_MIN.0 * documents as i64 && credit.0 <= HOUSE_ALLOCATION_MAX.0 * documents as i64, "{credit:?}");
        let before = {
            let mut x = state.exchange.lock().unwrap();
            let house = seed_house_account(&mut x, &state.templates);
            assert_eq!(house, HOUSE_ADDRESS);
            let acct = x.account(&house).unwrap();
            assert_eq!(acct.username.as_deref(), Some(HOUSE_USERNAME));
            assert!(x.is_user_account(&house));
            acct.usdc_balance
        };
        assert_eq!(before, Tenths(1_000_000 + credit.0));

        let report = deploy_house_templates(&state).await;
        assert!(report.failed.is_empty(), "{:#?}", report.failed);
        assert_eq!(report.deployed, documents);
        assert_eq!(state.runtime.ids().await.len(), documents);
        {
            let x = state.exchange.lock().unwrap();
            assert_eq!(x.sub_accounts_of(&HOUSE_ADDRESS).len(), documents);
            assert_eq!(x.account(&HOUSE_ADDRESS).unwrap().usdc_balance, Tenths(1_000_000), "the credit paid for the seeding exactly");
            for spec in SERIES {
                let tally = TemplateTally::collect(&x, spec.id);
                for t in state.templates.for_series(spec.id) {
                    let allocation = house_allocation(&t.id);
                    for side in Side::ALL {
                        let s = tally.side(&t.id, side);
                        assert_eq!((s.running, s.deployed_tenths), (1, allocation.0), "{} {side:?} on {}", t.id, spec.slug);
                    }
                }
            }
            // Every house sub-account runs under the house envelope: a
            // quarter of its allocation at risk, half of the remainder in flight.
            for child in x.sub_accounts_of(&HOUSE_ADDRESS) {
                let acct = x.account(child).unwrap();
                let record = acct.strategy.as_ref().unwrap();
                let allocation = house_allocation(record.template_id.as_deref().unwrap());
                let dollars = Decimal::new(allocation.0, 3);
                assert_eq!(record.envelope.allocation.as_str(), dollars.normalize().to_string());
                assert_eq!(record.envelope.loss_budget.as_str(), (dollars / Decimal::from(4)).normalize().to_string());
                assert_eq!(record.envelope.max_exposure_fraction.as_ref().map(|d| d.as_str()), Some("0.5"));
                assert_eq!(acct.usdc_balance, allocation, "funded with the whole allocation");
            }
        }
        let Json(view) = crate::api::templates::get_template(
            axum::extract::State(state.clone()),
            axum::extract::Path("late-closer".into()),
            axum::extract::Query(crate::api::SeriesQuery { series: Some(SERIES[1].id) }),
        )
        .await
        .unwrap();
        let late = house_allocation("late-closer");
        assert_eq!((view.running, view.deployed_tenths), (2, 2 * late.0));
        assert_eq!(view.runners, vec!["strategy-03.jpg".to_string()]);
    }

    /// Allocations are fixed by the template id, land on whole hundreds
    /// inside the range, and differ across the catalogue.
    #[test]
    fn house_allocations_are_stable_stepped_and_in_range() {
        assert_eq!(stable_hash("late-closer"), stable_hash("late-closer"));
        assert_ne!(stable_hash("late-closer"), stable_hash("trend-rider"));
        let ids = ["fair-value-discount", "momentum-rider", "late-closer", "oracle-gap", "order-flow-lead", "vwap-reversion", "sweep-follower", "absorption-fade", "polymarket-discount", "polymarket-momentum", "trend-rider"];
        let mut distinct = std::collections::BTreeSet::new();
        for id in ids {
            let a = house_allocation(id);
            assert_eq!(a, house_allocation(id));
            assert!(a >= HOUSE_ALLOCATION_MIN && a <= HOUSE_ALLOCATION_MAX, "{id}: {a:?}");
            assert_eq!(a.0 % HOUSE_ALLOCATION_STEP.0, 0, "{id}: whole hundreds");
            distinct.insert(a.0);
        }
        assert!(distinct.len() > ids.len() / 2, "the catalogue spreads over the range: {distinct:?}");

        let ratios: EnvelopeRatios =
            serde_json::from_value(serde_json::json!({ "loss_budget_fraction": "1", "order_fraction": "0.2", "max_order_size": "200" })).unwrap();
        let env = house_envelope(&ratios, Tenths(40_000_000));
        assert_eq!(env.allocation.as_str(), "40000");
        assert_eq!(env.loss_budget.as_str(), "10000");
        assert_eq!(env.max_exposure_fraction.as_ref().map(|d| d.as_str()), Some("0.5"));
        assert_eq!(env.max_order_size.as_str(), "200");
    }

    #[test]
    fn seeding_toggle_reads_the_environment() {
        // Not set: on. The process-wide variable is left as it was found.
        let previous = std::env::var(HOUSE_SEED_ENV).ok();
        for (value, expected) in [(Some("0"), false), (Some("off"), false), (Some("1"), true), (None, true)] {
            match value {
                // SAFETY: tests in this module are the only writers of this variable.
                Some(v) => unsafe { std::env::set_var(HOUSE_SEED_ENV, v) },
                None => unsafe { std::env::remove_var(HOUSE_SEED_ENV) },
            }
            assert_eq!(seeding_enabled(), expected, "{value:?}");
        }
        match previous {
            Some(v) => unsafe { std::env::set_var(HOUSE_SEED_ENV, v) },
            None => unsafe { std::env::remove_var(HOUSE_SEED_ENV) },
        }
    }
}
