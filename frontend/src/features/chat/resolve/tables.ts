import type { RustMarket, RustTradeRow } from '@kyros/domain'
import { cents, createMoneyFormatter } from '@kyros/money'
import { aggregateCandles } from '@kyros/ui/charts/lib/candles'
import type { ChartPoint } from '@kyros/ui/charts/lib/types'

import type { BookState } from '@/features/exchange-streams/book-stream'
import type { Fill, FillLogProps } from '../presets/fill-log'
import type { OrderBookProps } from '../presets/order-book'
import type { RoundResult, RoundResultsProps } from '../presets/round-results'
import { BOOK_DEPTH_DEFAULT, CANDLE_BUCKET_MS_DEFAULT, WINDOW_MS_DEFAULT } from '../specs/bound'
import type { CandleChartProps } from '../widgets/candle-chart'
import type { HistogramProps } from '../widgets/histogram'

const money = createMoneyFormatter('en-US')
const TENTHS_PER_CENT = 10
const clocks = new Map<string, Intl.DateTimeFormat>()

function clock(timeZone: string): Intl.DateTimeFormat {
  let formatter = clocks.get(timeZone)
  if (formatter === undefined) {
    formatter = new Intl.DateTimeFormat('en-US', {
      hour: '2-digit',
      minute: '2-digit',
      second: '2-digit',
      hour12: false,
      timeZone,
    })
    clocks.set(timeZone, formatter)
  }
  return formatter
}

/** The book's levels as the ladder draws them: prices in cents, `depth` rows a side. */
export function resolveBook(series: string, book: BookState, depth?: number): OrderBookProps {
  const rows = depth ?? BOOK_DEPTH_DEFAULT
  const level = (entry: BookState['bids'][number]) => ({
    price: entry.price / TENTHS_PER_CENT,
    size: entry.qty,
  })
  return {
    series,
    asks: book.asks.slice(0, rows).map(level),
    bids: book.bids.slice(0, rows).map(level),
    rows,
  }
}

/**
 * Trade rows as a fill log. On the tape every row is the taker's action; for one account
 * a row is included when the account took or made it, read from that account's side.
 */
export function resolveFills(
  series: string,
  trades: readonly RustTradeRow[],
  scope: { kind: 'tape' } | { kind: 'mine'; address: string },
  limit: number,
  timeZone: string
): FillLogProps {
  const format = clock(timeZone)
  const fills: Fill[] = []
  for (const trade of trades) {
    const took = scope.kind === 'tape' || trade.taker.root_address === scope.address
    const made = scope.kind === 'mine' && trade.maker.root_address === scope.address
    if (!took && !made) continue
    // The taker bought the row's side; the maker did the opposite.
    const boughtYes = trade.side === 'yes' ? took : !took
    fills.push({
      id: `${trade.ts}:${trade.price}:${trade.qty}:${trade.side}`,
      side: boughtYes ? 'YES' : 'NO',
      action: 'buy',
      priceCents: Math.round((boughtYes ? trade.price : 1000 - trade.price) / TENTHS_PER_CENT),
      size: trade.qty,
      time: format.format(new Date(trade.ts)),
    })
    if (fills.length === limit) break
  }
  return { series, fills }
}

function resolvedMoves(markets: readonly RustMarket[]): RoundResult[] {
  return markets
    .filter(market => market.status.kind === 'resolved')
    .toSorted((a, b) => a.closes_at - b.closes_at)
    .map(market => {
      const close = market.status.kind === 'resolved' ? (market.status.close_price_btc_usd ?? 0) : 0
      return {
        id: `round-${market.id}`,
        outcome: market.status.kind === 'resolved' ? market.status.outcome : 'no',
        moveDollars: close - (market.strike_btc_usd ?? 0),
      }
    })
    .map(round => ({ ...round, outcome: round.outcome === 'yes' ? 'up' : 'down' }) as RoundResult)
}

/** Recent resolved rounds, oldest first, as the outcome strip. */
export function resolveRounds(
  series: string,
  markets: readonly RustMarket[],
  count: number
): RoundResultsProps {
  return { series, rounds: resolvedMoves(markets).slice(-count) }
}

/** The moves of recent rounds as a distribution, with the open round's move pinned. */
export function resolveDistribution(
  series: string,
  markets: readonly RustMarket[],
  count: number,
  currentMoveDollars: number | undefined
): HistogramProps {
  const values = resolvedMoves(markets)
    .slice(-count)
    .map(round => round.moveDollars)
  return {
    label: `${series} distribution of round moves`,
    title: `Moves over ${values.length} rounds`,
    values,
    format: value => money.usd(cents(Math.round(value * 100)), { signDisplay: 'always' }),
    density: true,
    ...(currentMoveDollars === undefined
      ? {}
      : { current: { value: currentMoveDollars, label: 'Now' } }),
  }
}

/**
 * BTC spot bucketed into candles, with the strike as the guide. The whole buffer is
 * bucketed and the widget clips to the window, so a tick only ever changes the open candle.
 */
export function resolveCandles(
  points: readonly ChartPoint[],
  options: {
    windowMs?: number | undefined
    bucketMs?: number | undefined
    strike?: number | undefined
    timeZone: string
    title?: string | undefined
  }
): CandleChartProps {
  const bucketMs = options.bucketMs ?? CANDLE_BUCKET_MS_DEFAULT
  return {
    label: 'Bitcoin price candles',
    title: options.title ?? 'BTC candles',
    candles: aggregateCandles(points, bucketMs),
    bucketMs,
    windowMs: options.windowMs ?? WINDOW_MS_DEFAULT,
    format: { style: 'currency', currency: 'USD', maximumFractionDigits: 0 },
    timeZone: options.timeZone,
    ...(options.strike === undefined ? {} : { guide: { value: options.strike, label: 'Strike' } }),
  }
}
