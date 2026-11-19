import { createTool } from '@mastra/core/tools'
import type { RustMarket } from '@kyros/domain'
import * as z from 'zod/mini'

import { readChatContext } from '../../request-context'
import {
  exchangeClient,
  failed,
  resolveSeries,
  SeriesInputSchema,
  tenthsToCents,
  UNAVAILABLE,
} from '../shared/exchange'

const BOOK_DEPTH_MAX = 153
const TRADES_MAX = 50
const ROUNDS_MAX = 50
const PRICE_POINTS_MAX = 60
const RETRIEVED = () => new Date().toISOString()

function roundOf(market: {
  market_id: number | null
  strike: number | null
  opens_at_ms: number | null
  closes_at_ms: number | null
}) {
  if (market.market_id === null || market.strike === null || market.closes_at_ms === null)
    return null
  return {
    market_id: market.market_id,
    strike_usd: market.strike,
    opens_at_ms: market.opens_at_ms,
    closes_at_ms: market.closes_at_ms,
    seconds_remaining: Math.max(0, Math.round((market.closes_at_ms - Date.now()) / 1000)),
  }
}

/** A book level as the model reads it: cents and lots. */
function level(entry: { price: number; qty: number }) {
  return { price_cents: tenthsToCents(entry.price), lots: entry.qty }
}

function resolvedRounds(markets: readonly RustMarket[]) {
  return markets
    .filter(market => market.status.kind === 'resolved')
    .toSorted((a, b) => b.closes_at - a.closes_at)
    .map(market => {
      const close = market.status.kind === 'resolved' ? (market.status.close_price_btc_usd ?? 0) : 0
      const strike = market.strike_btc_usd ?? 0
      return {
        market_id: market.id,
        outcome:
          market.status.kind === 'resolved' && market.status.outcome === 'yes' ? 'up' : 'down',
        strike_usd: strike,
        close_usd: close,
        move_usd: Math.round((close - strike) * 100) / 100,
        closed_at_ms: market.closes_at * 1000,
      }
    })
}

export function createExchangeTools(baseUrl: string | undefined) {
  const client = baseUrl === undefined ? undefined : exchangeClient(baseUrl)

  const getMarket = createTool({
    id: 'get-market',
    description:
      'The open round of a series at a glance: strike, BTC spot, seconds left, the YES best bid and ask in cents, and round volume, open interest, trades and traders. Series defaults to the market the chat was opened from.',
    inputSchema: z.object({ series: SeriesInputSchema }),
    execute: async ({ series: requested }, context) => {
      if (!client) return UNAVAILABLE
      const series = resolveSeries(requested, context)
      const [price, book, stats] = await Promise.all([
        client.getPriceHistory(series, 1, context?.abortSignal),
        client.getBook(series, 1, context?.abortSignal),
        client.getMarketStats(series, context?.abortSignal),
      ])
      if (!price.ok || !book.ok || !stats.ok)
        return failed('The exchange did not answer for this series.', series)
      const bid = book.data.bids[0]?.price
      const ask = book.data.asks[0]?.price
      return {
        status: 'ok',
        source: 'Kyros',
        retrievedAt: RETRIEVED(),
        series,
        round: roundOf(price.data),
        spot_usd: price.data.ticks.at(-1)?.price ?? null,
        yes: {
          bid_cents: bid === undefined ? null : tenthsToCents(bid),
          ask_cents: ask === undefined ? null : tenthsToCents(ask),
          spread_cents: bid === undefined || ask === undefined ? null : tenthsToCents(ask - bid),
        },
        stats: {
          volume_cents: tenthsToCents(stats.data.total_volume_tenths),
          open_interest_lots: stats.data.open_interest_contracts,
          trades: stats.data.total_trades,
          traders: stats.data.unique_traders,
        },
      }
    },
  })

  const getBook = createTool({
    id: 'get-book',
    description:
      'The canonical YES order book of a series: best-first levels each side with prices in cents and sizes in lots, plus spread and depth imbalance over the shown levels. To buy NO at price p, sell YES at 100 − p.',
    inputSchema: z.object({
      series: SeriesInputSchema,
      depth: z.optional(z.int().check(z.minimum(1), z.maximum(BOOK_DEPTH_MAX))),
    }),
    execute: async ({ series: requested, depth = 7 }, context) => {
      if (!client) return UNAVAILABLE
      const series = resolveSeries(requested, context)
      const book = await client.getBook(series, depth, context?.abortSignal)
      if (!book.ok) return failed('The book could not be read.', series)
      const bidDepth = book.data.bids.reduce((total, entry) => total + entry.qty, 0)
      const askDepth = book.data.asks.reduce((total, entry) => total + entry.qty, 0)
      const bid = book.data.bids[0]?.price
      const ask = book.data.asks[0]?.price
      return {
        status: 'ok',
        source: 'Kyros',
        retrievedAt: RETRIEVED(),
        series,
        bids: book.data.bids.map(level),
        asks: book.data.asks.map(level),
        spread_cents: bid === undefined || ask === undefined ? null : tenthsToCents(ask - bid),
        imbalance:
          Math.round(((bidDepth - askDepth) / Math.max(1, bidDepth + askDepth)) * 100) / 100,
      }
    },
  })

  const getPositions = createTool({
    id: 'get-positions',
    description:
      "The user's balance and their position in every series: signed lots (positive YES, negative NO), average entry, mark and unrealized P&L in cents. Needs the user to be signed in.",
    inputSchema: z.object({ series: SeriesInputSchema }),
    execute: async ({ series: requested }, context) => {
      if (!client) return UNAVAILABLE
      const { accountAddress } = readChatContext(context)
      if (accountAddress === undefined)
        return failed('The user is not signed in, so there is no account to read.')
      const series = resolveSeries(requested, context)
      const positions = await client.getPositions(accountAddress, series, context?.abortSignal)
      if (!positions.ok)
        return failed(
          positions.error.status === 404
            ? 'The exchange does not know this account.'
            : 'Positions could not be read.',
          series
        )
      const snapshot = positions.data
      return {
        status: 'ok',
        source: 'Kyros',
        retrievedAt: RETRIEVED(),
        balance_cents: tenthsToCents(snapshot.balance_tenths),
        locked_cents: tenthsToCents(snapshot.locked_tenths),
        positions: snapshot.series_positions.map(row => ({
          series: row.series,
          market_id: row.market_id,
          side: row.side,
          signed_lots: row.signed_position,
          avg_entry_cents:
            row.avg_entry_yes_tenths === null ? null : tenthsToCents(row.avg_entry_yes_tenths),
          mark_cents: row.mark_yes_tenths === null ? null : tenthsToCents(row.mark_yes_tenths),
          cost_basis_cents: tenthsToCents(row.cost_basis_tenths),
          unrealized_pnl_cents:
            row.unrealized_pnl_tenths === null ? null : tenthsToCents(row.unrealized_pnl_tenths),
        })),
      }
    },
  })

  const getTrades = createTool({
    id: 'get-trades',
    description:
      "Recent fills. `tape` is the whole series' recent trades; `mine` is the user's own orders across series with fill status and realized P&L (needs sign-in).",
    inputSchema: z.object({
      series: SeriesInputSchema,
      scope: z.enum(['tape', 'mine']),
      limit: z.optional(z.int().check(z.minimum(1), z.maximum(TRADES_MAX))),
    }),
    execute: async ({ series: requested, scope, limit = 10 }, context) => {
      if (!client) return UNAVAILABLE
      const series = resolveSeries(requested, context)
      if (scope === 'tape') {
        const fills = await client.getTrades(series, limit, context?.abortSignal)
        if (!fills.ok) return failed('Recent trades could not be read.', series)
        return {
          status: 'ok',
          source: 'Kyros',
          retrievedAt: RETRIEVED(),
          series,
          trades: fills.data.slice(0, limit).map(fill => ({
            at_ms: fill.ts,
            taker_bought: fill.taker_side === 'bid' ? 'YES' : 'NO',
            price_cents: tenthsToCents(fill.price),
            lots: fill.qty,
            kind: fill.kind,
          })),
        }
      }
      const { accountAddress } = readChatContext(context)
      if (accountAddress === undefined)
        return failed('The user is not signed in, so there are no fills to read.')
      const mine = await client.getUserTrades(accountAddress, context?.abortSignal)
      if (!mine.ok) return failed('Your orders could not be read.')
      return {
        status: 'ok',
        source: 'Kyros',
        retrievedAt: RETRIEVED(),
        orders: mine.data.trades.slice(0, limit).map(row => ({
          series: row.series,
          at_ms: row.ts,
          bought: row.side === 'yes' ? 'YES' : 'NO',
          order_type: row.order_type,
          status: row.status,
          requested_lots: row.qty_requested,
          filled_lots: row.filled_qty,
          avg_price_cents: row.avg_price === undefined ? null : tenthsToCents(row.avg_price),
          realized_pnl_cents: tenthsToCents(row.realized_pnl_tenths),
          settled: row.settled,
          ...(row.strategy_name === undefined ? {} : { strategy: row.strategy_name }),
        })),
      }
    },
  })

  const getRounds = createTool({
    id: 'get-rounds',
    description:
      'Recent resolved rounds of a series, newest first: outcome (up or down), strike, close and the move in dollars, with up and down counts and the current streak.',
    inputSchema: z.object({
      series: SeriesInputSchema,
      count: z.optional(z.int().check(z.minimum(1), z.maximum(ROUNDS_MAX))),
    }),
    execute: async ({ series: requested, count = 12 }, context) => {
      if (!client) return UNAVAILABLE
      const series = resolveSeries(requested, context)
      const history = await client.getMarketsHistory(series, count, context?.abortSignal)
      if (!history.ok) return failed('Round history could not be read.', series)
      const rounds = resolvedRounds(history.data).slice(0, count)
      const ups = rounds.filter(round => round.outcome === 'up').length
      let streak = 0
      for (const round of rounds) {
        if (round.outcome !== rounds[0]?.outcome) break
        streak += 1
      }
      return {
        status: 'ok',
        source: 'Kyros',
        retrievedAt: RETRIEVED(),
        series,
        up: ups,
        down: rounds.length - ups,
        streak: rounds[0] === undefined ? null : { outcome: rounds[0].outcome, length: streak },
        rounds,
      }
    },
  })

  const getExchangePriceHistory = createTool({
    id: 'get-exchange-price-history',
    description:
      'The BTC price the rounds settle on, over the open round and its run-up: first, last, low and high, plus up to 60 evenly sampled points. This is the settlement feed, unlike the Price Feeds tools.',
    inputSchema: z.object({
      series: SeriesInputSchema,
      max_points: z.optional(z.int().check(z.minimum(2), z.maximum(PRICE_POINTS_MAX))),
    }),
    execute: async ({ series: requested, max_points = 24 }, context) => {
      if (!client) return UNAVAILABLE
      const series = resolveSeries(requested, context)
      const price = await client.getPriceHistory(series, max_points, context?.abortSignal)
      if (!price.ok) return failed('The price history could not be read.', series)
      const prices = price.data.ticks.map(tick => tick.price)
      if (prices.length === 0) return failed('No price has been observed yet.', series)
      return {
        status: 'ok',
        source: 'Kyros',
        retrievedAt: RETRIEVED(),
        series,
        round: roundOf(price.data),
        first_usd: prices[0],
        last_usd: prices.at(-1),
        low_usd: Math.min(...prices),
        high_usd: Math.max(...prices),
        points: price.data.ticks.map(tick => ({ at_ms: tick.ts_ms, usd: tick.price })),
      }
    },
  })

  return { getMarket, getBook, getPositions, getTrades, getRounds, getExchangePriceHistory }
}
