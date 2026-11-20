import * as z from 'zod/mini'

/**
 * Kalshi's public price history (`…/candlesticks`), turned into the past of a line a chart
 * already draws live. Only the fields read here are described; a minute that does not fit is
 * skipped, never guessed at. Read-only and public; nothing here touches an account.
 */

export interface KalshiReading {
  /** The end of the minute or hour the price closed, in milliseconds. */
  time: number
  /** The middle of the closing bid and ask, as a fraction of a dollar. */
  value: number
}

const SECOND_MS = 1000
const PERCENT = 100
/** Four decimals of a dollar: a hundredth of a cent, finer than Kalshi quotes. */
const PRICE_STEPS = 10_000
/** The widest book, in cents, whose middle still says something about the price. */
const WIDEST_BOOK_CENTS = 10

const SideSchema = z.object({ close_dollars: z.optional(z.string()) })
const CandleSchema = z.object({
  end_period_ts: z.number(),
  yes_ask: SideSchema,
  yes_bid: SideSchema,
})
const CandlesSchema = z.object({ candlesticks: z.array(z.unknown()) })
const EventSchema = z.object({
  event_ticker: z.string(),
  markets: z.array(z.object({ ticker: z.string() })),
})
const EventsSchema = z.object({ events: z.array(z.unknown()) })
const BatchMarketSchema = z.object({
  candlesticks: z.array(z.unknown()),
  market_ticker: z.string(),
})
const BatchSchema = z.object({ markets: z.array(z.unknown()) })

/** One closed minute as a price, or undefined when its book was one-sided, crossed or too wide. */
function readingOf(raw: unknown): KalshiReading | undefined {
  const candle = CandleSchema.safeParse(raw)
  if (!candle.success) return undefined
  const bid = Number(candle.data.yes_bid.close_dollars)
  const ask = Number(candle.data.yes_ask.close_dollars)
  // A missing side is not a number, and every comparison with it is false.
  if (!(bid > 0 && bid < ask && ask < 1)) return undefined
  if (Math.round((ask - bid) * PERCENT) > WIDEST_BOOK_CENTS) return undefined
  return {
    time: candle.data.end_period_ts * SECOND_MS,
    value: Math.round(((bid + ask) / 2) * PRICE_STEPS) / PRICE_STEPS,
  }
}

function readingsOf(candles: readonly unknown[]): KalshiReading[] {
  return candles
    .flatMap(raw => readingOf(raw) ?? [])
    .toSorted((first, second) => first.time - second.time)
}

/** The ticker of the home side's market for the game between two teams, from an `/events` body. */
export function kalshiHomeMarket(
  body: unknown,
  teams: { away: string; home: string }
): string | undefined {
  const parsed = EventsSchema.safeParse(body)
  if (!parsed.success) return undefined
  for (const raw of parsed.data.events) {
    const event = EventSchema.safeParse(raw)
    if (!event.success || !event.data.event_ticker.endsWith(`${teams.away}${teams.home}`)) continue
    const market = event.data.markets.find(each => each.ticker.endsWith(`-${teams.home}`))
    if (market !== undefined) return market.ticker
  }
  return undefined
}

/** One market's price history, oldest first; empty for a body that does not read. */
export function toKalshiHistory(body: unknown): KalshiReading[] {
  const parsed = CandlesSchema.safeParse(body)
  return parsed.success ? readingsOf(parsed.data.candlesticks) : []
}

/** Several markets' histories from one batch answer, each oldest first, keyed by market ticker. */
export function toKalshiSparks(body: unknown): Record<string, KalshiReading[]> {
  const parsed = BatchSchema.safeParse(body)
  if (!parsed.success) return {}
  const sparks: Record<string, KalshiReading[]> = {}
  for (const raw of parsed.data.markets) {
    const market = BatchMarketSchema.safeParse(raw)
    if (!market.success) continue
    sparks[market.data.market_ticker] = readingsOf(market.data.candlesticks)
  }
  return sparks
}
