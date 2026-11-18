import type { RustAccountRow } from '@tiny/domain'
import { type Cents, cents, createMoneyFormatter } from '@tiny/money'

import type { MarketOutcome, MyStrategy, StrategyPosition } from '../types'

const money = createMoneyFormatter('en-US')
const TENTHS_PER_CENT = 10

/** Tenths of a cent on the wire to whole cents; an integer, never a float. */
function toCents(tenths: number): Cents {
  return cents(Math.round(tenths / TENTHS_PER_CENT))
}

type SeriesRow = RustAccountRow['series_positions'][number]

/** This market's series only; the store already keeps just the strategies homed here. */
function seriesRowOf(account: RustAccountRow, series: string): SeriesRow | undefined {
  return account.series_positions.find(position => position.series === series)
}

function positionOf(
  row: SeriesRow,
  outcomes: readonly [MarketOutcome, MarketOutcome]
): StrategyPosition | undefined {
  if (row.signed_position === 0) return undefined
  const shares = Math.abs(row.signed_position)
  const side = row.signed_position > 0 ? 'home' : 'away'
  const outcome = outcomes.find(candidate => candidate.side === side) ?? outcomes[0]
  // Worth at the mark; before anything can mark it, what it cost (magnitude: the wire
  // signs cost by direction).
  const valueTenths = row.mark_value_tenths ?? Math.abs(row.cost_basis_tenths)
  return {
    valueCents: toCents(valueTenths),
    label: `${money.count(shares)} ${outcome.label}`,
    shares,
    side,
  }
}

/** How often the strategy rows are re-ranked by size, so they do not reshuffle on every frame. */
export const STRATEGY_RANK_INTERVAL_MS = 5_000

function sharesOf(row: MyStrategy): number {
  return row.position?.shares ?? 0
}

/** Strategy ids by position size, largest first; ties keep the exchange's creation order. */
export function strategyRank(rows: readonly MyStrategy[]): string[] {
  return rows
    .slice(1)
    .toSorted((a, b) => sharesOf(b) - sharesOf(a))
    .map(row => row.id)
}

/**
 * The main account stays first. Strategies follow `rank` (the last 5-second ranking);
 * ones it has not seen yet go after, by current size, so a fresh page is already sorted.
 */
export function rankedRows(rows: readonly MyStrategy[], rank: readonly string[]): MyStrategy[] {
  const [main, ...strategies] = rows
  if (main === undefined) return []
  const known = strategies.filter(row => rank.includes(row.id))
  known.sort((a, b) => rank.indexOf(a.id) - rank.indexOf(b.id))
  const unknown = strategies
    .filter(row => !rank.includes(row.id))
    .toSorted((a, b) => sharesOf(b) - sharesOf(a))
  return [main, ...known, ...unknown]
}

/**
 * The My Accounts rows: the exchange sends the main account first and the strategies after
 * it in creation order, which is the order the panel shows. `totalPnlCents` is the row's
 * PnL in the current round of the series it shows (realized so far plus the open position
 * at the mark), so it starts from zero when the market flips.
 */
export function accountRows(
  accounts: readonly RustAccountRow[],
  series: string,
  outcomes: readonly [MarketOutcome, MarketOutcome]
): MyStrategy[] {
  return accounts.map(account => {
    const row = seriesRowOf(account, series)
    const position = row === undefined ? undefined : positionOf(row, outcomes)
    return {
      id: account.id,
      name: account.name,
      ...(position === undefined ? {} : { position }),
      status: account.status === 'paused' ? 'paused' : 'active',
      totalPnlCents: toCents(row?.market_pnl_tenths ?? 0),
      trades: account.trades_this_market,
    }
  })
}
