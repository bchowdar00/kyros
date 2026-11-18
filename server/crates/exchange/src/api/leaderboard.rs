//! `GET /api/leaderboard` — registered users ranked by cumulative PnL.
//!
//! Ranks real users (never bots) by their settled cash PnL, which the engine
//! snapshots once per market at resolution (see [`crate::engine::Engine`]'s
//! `settle_all`). The value is frozen between resolutions — it never reflects
//! the live/unrealized PnL of the in-progress round — so the board only moves
//! when a market closes. Bots are excluded structurally: only registered users
//! who placed an order in a round ever receive a standing.
//!
//! Pagination is server-side via `offset`/`limit`; the response carries the
//! full `total` so the client can render page controls.

use axum::{
    Json,
    extract::{Query, State},
};
use serde::{Deserialize, Serialize};

use crate::engine::LeaderboardStanding;
use crate::types::{Address, SeriesId, Tenths};

use super::AppState;

/// Default page size for `GET /api/leaderboard`.
pub const DEFAULT_LEADERBOARD_LIMIT: usize = 25;
/// Hard ceiling on a single page — clamps oversized `limit` queries.
pub const MAX_LEADERBOARD_LIMIT: usize = 100;

#[derive(Debug, Deserialize)]
pub struct LeaderboardQuery {
    #[serde(default)]
    pub offset: Option<usize>,
    #[serde(default)]
    pub limit: Option<usize>,
}

/// One ranked row. `rank` is the global 1-based position (independent of the
/// page offset). PnL fields are in tenths-of-a-cent and may be negative.
#[derive(Debug, Clone, Serialize)]
pub struct LeaderboardRow {
    pub rank: usize,
    pub address: Address,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub emoji: Option<String>,
    pub cumulative_pnl_tenths: Tenths,
    pub last_market_pnl_tenths: Tenths,
    pub markets_played: u32,
    pub markets_won: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct LeaderboardResponse {
    pub rows: Vec<LeaderboardRow>,
    /// Total ranked users — drives the client's page count.
    pub total: usize,
    pub offset: usize,
    pub limit: usize,
    /// Id of the most recently resolved market of the default series the
    /// standings reflect, or `0` before any market has resolved.
    pub as_of_market_id: u32,
    /// The same per series.
    pub as_of_markets: Vec<AsOfMarket>,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct AsOfMarket {
    pub series: SeriesId,
    /// `0` before that series has resolved a round.
    pub market_id: u32,
}

pub async fn get_leaderboard(
    State(state): State<AppState>,
    Query(q): Query<LeaderboardQuery>,
) -> Json<LeaderboardResponse> {
    let limit = q
        .limit
        .unwrap_or(DEFAULT_LEADERBOARD_LIMIT)
        .clamp(1, MAX_LEADERBOARD_LIMIT);
    let offset = q.offset.unwrap_or(0);

    let engine = state.exchange.lock().expect("exchange mutex poisoned");

    // Snapshot (address, standing) pairs, then sort by cumulative PnL desc
    // with a stable address tiebreak so paging stays deterministic across
    // requests at equal PnL.
    let mut ranked: Vec<(Address, LeaderboardStanding)> = engine
        .leaderboard_standings()
        .map(|(addr, st)| (*addr, *st))
        .collect();
    ranked.sort_unstable_by(|a, b| {
        b.1.cumulative_pnl_tenths
            .cmp(&a.1.cumulative_pnl_tenths)
            .then_with(|| a.0.cmp(&b.0))
    });

    let total = ranked.len();
    let as_of_markets: Vec<AsOfMarket> = engine
        .engines()
        .map(|e| AsOfMarket {
            series: e.series(),
            market_id: e.resolved_markets(1).next().map(|m| m.id.0).unwrap_or(0),
        })
        .collect();
    let as_of_market_id = as_of_markets
        .iter()
        .find(|m| m.series == crate::types::DEFAULT_SERIES)
        .map(|m| m.market_id)
        .unwrap_or(0);

    // `enumerate` before `skip` makes `i` the global rank index, so the first
    // row of page N is correctly numbered `offset + 1`.
    let rows = ranked
        .into_iter()
        .enumerate()
        .skip(offset)
        .take(limit)
        .map(|(i, (address, st))| {
            let acct = engine.account(&address);
            LeaderboardRow {
                rank: i + 1,
                address,
                username: acct.and_then(|a| a.username.clone()),
                color: acct.and_then(|a| a.color.clone()),
                emoji: acct.and_then(|a| a.emoji.clone()),
                cumulative_pnl_tenths: Tenths(st.cumulative_pnl_tenths),
                last_market_pnl_tenths: Tenths(st.last_market_pnl_tenths),
                markets_played: st.markets_played,
                markets_won: st.markets_won,
            }
        })
        .collect();

    Json(LeaderboardResponse {
        rows,
        total,
        offset,
        limit,
        as_of_market_id,
        as_of_markets,
    })
}
