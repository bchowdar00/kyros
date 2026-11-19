import { exchangeMarketKey, type RustGeminiSeries } from '@kyros/domain'
import { cents, exchangeBudgetMicros, exchangeBuyReserve, type Cents } from '@kyros/money'
import type { GeminiBookState, MarketSide } from '../types'
import type { AccountState } from './account'
import type { FeedState } from './feed'

export interface ExchangeBuyQuote {
  marketId: string
  side: 'buy' | 'sell'
  price: number
  outcomePrice: number
  qty: string
  reserve: string
  payoutCents: Cents
}
export interface MarketTrading {
  availableBalanceCents: Cents | undefined
  reason: string | undefined
  notice: string
  quote: (amount: string, side: MarketSide) => ExchangeBuyQuote | undefined
  submit: (quote: ExchangeBuyQuote) => void
}
/** Why a closed round cannot trade, from where Gemini's series stands. */
function closedRoundReason(gemini: RustGeminiSeries | undefined): string {
  if (gemini?.phase === 'waiting_for_result')
    return "This round is closed. Waiting for Gemini's result."
  if (gemini?.phase === 'waiting_for_window') return "Waiting for Gemini's next round to open."
  return 'This round is closed. The next one opens shortly.'
}
/**
 * Why the market cannot take an order now, or undefined when it can. `geminiBook` is the
 * series' Gemini block as last polled: the exchange mirrors Gemini's book, so a quiet or
 * missing mirror, a round Gemini has closed, or a poll that failed stops trading even while
 * the round is open here.
 */
export function tradingUnavailable(
  feed: FeedState,
  account: AccountState,
  geminiBook?: GeminiBookState
): string | undefined {
  if (feed.status !== 'live') return `Exchange feed ${feed.status}. Waiting for a fresh book.`
  if (geminiBook?.status === 'poll-failed')
    return "Could not confirm Gemini's book. Trading resumes when the exchange answers."
  const gemini = geminiBook?.gemini
  if (feed.state !== 'open' || feed.marketId === null) return closedRoundReason(gemini)
  if (gemini !== undefined && gemini.phase !== 'open') return closedRoundReason(gemini)
  if (gemini?.book === 'stale')
    return "Gemini's book has gone quiet. Trading resumes when it is live again."
  if (gemini?.book === 'waiting') return "Waiting for Gemini's book."
  if (account.status !== 'live' || account.available === null)
    return 'Waiting for the exchange account snapshot.'
  if (feed.tickStep === null || account.fees[exchangeMarketKey(feed.marketId)] === undefined)
    return 'Waiting for the exchange market configuration.'
  return undefined
}
export function quoteExchangeBuy(
  feed: FeedState,
  account: AccountState,
  amount: string,
  side: MarketSide
): ExchangeBuyQuote | undefined {
  if (
    tradingUnavailable(feed, account) !== undefined ||
    feed.marketId === null ||
    account.available === null
  )
    return undefined
  const budget = exchangeBudgetMicros(amount)
  const level = side === 'home' ? feed.asks[0] : feed.bids[0]
  const rate = account.fees[exchangeMarketKey(feed.marketId)]
  if (
    budget === undefined ||
    level === undefined ||
    rate === undefined ||
    budget > BigInt(account.available)
  )
    return undefined
  const price = side === 'home' ? level.price : 1000 - level.price
  const perLot = exchangeBuyReserve(price, rate)
  const affordable = budget / perLot
  const qty = affordable < BigInt(level.qty) ? affordable : BigInt(level.qty)
  if (qty === 0n || qty * 100n > BigInt(Number.MAX_SAFE_INTEGER)) return undefined
  return {
    marketId: feed.marketId,
    side: side === 'home' ? 'buy' : 'sell',
    price: level.price,
    outcomePrice: price,
    qty: String(qty),
    reserve: String(qty * perLot),
    payoutCents: cents(Number(qty * 100n)),
  }
}
export function availableExchangeCents(available: string | null): Cents | undefined {
  if (available === null) return undefined
  const value = BigInt(available) / 10000n
  return value <= BigInt(Number.MAX_SAFE_INTEGER) ? cents(Number(value)) : undefined
}
