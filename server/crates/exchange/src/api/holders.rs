//! `GET /api/holders` — top non-flat position holders by USD size.
//!
//! Walks every account in the engine, projects each non-flat position onto
//! its current mark value, and returns the top 100 sorted descending by
//! that USD size. The mark used is the engine's `implied_mark_yes_tenths`
//! (strict mid when two-sided, implied extreme when one-sided); if the book
//! is empty we fall back to a neutral 50¢ midpoint so the sort still has
//! something to anchor against during the first ticks of a session.

use std::collections::HashMap;

use axum::{Json, extract::{Query, State}};
use serde::Serialize;

use crate::engine::{Exchange, PAYOUT_PER_CONTRACT};
use crate::types::{Address, DEFAULT_SERIES, SeriesId};

use super::positions::PositionSide;
use super::{AppState, SeriesQuery};

/// Max rows returned by `/api/holders`. Hardcoded — the endpoint exists for
/// a leaderboard view, not for paging through the full account table.
const HOLDERS_LIMIT: usize = 100;

/// Fallback mark in tenths-of-cent (50¢) used only when the book is empty
/// and no mark is available. Keeps the ranking deterministic at boot.
const NEUTRAL_MARK_TENTHS: i64 = PAYOUT_PER_CONTRACT / 2;

#[derive(Debug, Clone, Serialize)]
pub struct HolderRow {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub emoji: Option<String>,
    pub side: PositionSide,
    /// Position's USD value at the current mark, in tenths of a cent.
    /// `$1.00 = 1000`. Always non-negative.
    pub size_usd_tenths: i64,
}

pub async fn list_holders(
    State(state): State<AppState>,
    Query(q): Query<SeriesQuery>,
) -> Json<Vec<HolderRow>> {
    let exchange = state.exchange.lock().expect("exchange mutex poisoned");
    Json(build_holders(&exchange, q.series.unwrap_or(DEFAULT_SERIES)))
}

/// Top positions by marked USD size, netted onto root identities so a
/// user with several strategy sub-accounts is one holder sized by their
/// combined exposure.
pub fn build_holders(exchange: &Exchange, series: SeriesId) -> Vec<HolderRow> {
    let engine = exchange.engine(series);
    let mark = engine
        .book()
        .implied_mark_yes_tenths()
        .map(|t| t as i64)
        .unwrap_or(NEUTRAL_MARK_TENTHS);

    let mut net: HashMap<Address, i64> = HashMap::new();
    for (address, ps) in engine.positions_iter() {
        if ps.position.0 != 0 {
            *net.entry(exchange.root_of(address)).or_insert(0) += ps.position.0;
        }
    }

    let mut rows: Vec<HolderRow> = net
        .into_iter()
        .filter_map(|(root, signed)| {
            if signed == 0 {
                return None;
            }
            let (side, size) = if signed > 0 {
                (PositionSide::Yes, signed * mark)
            } else {
                (PositionSide::No, (-signed) * (PAYOUT_PER_CONTRACT - mark))
            };
            let acct = exchange.account(&root);
            Some(HolderRow {
                username: acct.and_then(|a| a.username.clone()),
                color: acct.and_then(|a| a.color.clone()),
                emoji: acct.and_then(|a| a.emoji.clone()),
                side,
                size_usd_tenths: size,
            })
        })
        .collect();

    rows.sort_unstable_by(|a, b| b.size_usd_tenths.cmp(&a.size_usd_tenths));
    rows.truncate(HOLDERS_LIMIT);
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{Engine, TradeSource};
    use crate::ticks::PriceTick;
    use crate::types::{Qty, Side, Tenths};

    fn addr(byte: u8) -> Address {
        Address([byte; 20])
    }

    #[test]
    fn sub_account_positions_net_onto_parent_identity() {
        let mut e = Engine::new(Tenths(1_000_000));
        let (parent, bot) = (addr(1), addr(2));
        e.register_user(parent);
        e.get_or_create_account(parent).username = Some("jet".into());
        e.seed_bot_account(bot, Tenths(50_000_000), "bot".into(), None, None);
        let c1 = e.create_sub_account(parent, Tenths(100_000)).unwrap();
        let c2 = e.create_sub_account(parent, Tenths(100_000)).unwrap();

        let t = PriceTick::from_tenths(500).unwrap();
        // c1 long 10, c2 short 4, parent long 1 → net long 7 under "jet".
        e.place_limit(bot, Side::Ask, t, Qty(11), TradeSource::Bot).unwrap();
        e.place_market(c1, Side::Bid, Qty(10), TradeSource::Strategy).unwrap();
        e.place_market(parent, Side::Bid, Qty(1), TradeSource::Ui).unwrap();
        e.place_limit(bot, Side::Bid, t, Qty(4), TradeSource::Bot).unwrap();
        e.place_market(c2, Side::Ask, Qty(4), TradeSource::Strategy).unwrap();

        let rows = build_holders(e.exchange(), DEFAULT_SERIES);
        let jet: Vec<_> = rows.iter().filter(|r| r.username.as_deref() == Some("jet")).collect();
        assert_eq!(jet.len(), 1, "one row for the whole family");
        assert_eq!(jet[0].side, PositionSide::Yes);
        // Book is empty after the fills → neutral 50¢ mark → 7 × 500.
        assert_eq!(jet[0].size_usd_tenths, 7 * NEUTRAL_MARK_TENTHS);
        // The bot is net short 7 on the other side.
        let bot_row = rows.iter().find(|r| r.username.as_deref() == Some("bot")).unwrap();
        assert_eq!(bot_row.side, PositionSide::No);
        assert_eq!(rows.len(), 2);
    }
}
