import { exchangeMarketKey } from '@tiny/domain'
import { cents, exchangeBudgetMicros, exchangeBuyReserve, type Cents } from '@tiny/money'
import type { MarketSide } from '../types'
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
export function tradingUnavailable(feed: FeedState, account: AccountState): string | undefined {
  if (feed.status !== 'live') return `Exchange feed ${feed.status}. Waiting for a fresh book.`
  if (feed.state !== 'open' || feed.marketId === null)
    return 'The simulated exchange market is closed.'
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
