import { describe, expect, it } from 'vitest'
import { exchangeMarketKey, type RustGeminiSeries } from '@kyros/domain'
import { initialAccount } from './account'
import { initialFeed } from './feed'
import { exchangeBook, exchangeOutcomes } from './book.mapper'
import { quoteExchangeBuy, tradingUnavailable } from './buy'
const feed = {
  ...initialFeed,
  status: 'live' as const,
  marketId: '7',
  state: 'open',
  tickStep: 1,
  bids: [{ price: 499, qty: '100', orders: 3 }],
  asks: [{ price: 501, qty: '100', orders: 3 }],
}
const account = {
  ...initialAccount,
  status: 'live' as const,
  available: '100000000',
  fees: { [exchangeMarketKey('7')]: 400 },
}
it('sizes UP and DOWN orders to a budget inclusive of fee reserves', () => {
  const up = quoteExchangeBuy(feed, account, '5', 'home')
  const down = quoteExchangeBuy(feed, account, '5', 'away')
  expect(up).toMatchObject({
    side: 'buy',
    price: 501,
    outcomePrice: 501,
    qty: '9',
    reserve: '4599000',
  })
  expect(down).toMatchObject({
    side: 'sell',
    price: 499,
    outcomePrice: 501,
    qty: '9',
    reserve: '4599000',
  })
})
it('fails closed for missing liquidity, fees, stale feed, and unaffordable amounts', () => {
  expect(quoteExchangeBuy({ ...feed, status: 'stale' }, account, '5', 'home')).toBeUndefined()
  expect(quoteExchangeBuy(feed, { ...account, fees: {} }, '5', 'home')).toBeUndefined()
  expect(quoteExchangeBuy({ ...feed, bids: [] }, account, '5', 'away')).toBeUndefined()
  expect(quoteExchangeBuy(feed, account, '101', 'home')).toBeUndefined()
  expect(quoteExchangeBuy(feed, account, '0.01', 'home')).toBeUndefined()
})
it('uses live sub-cent quotes and exact book totals instead of fixture prices', () => {
  const outcomes = exchangeOutcomes(
    [
      { label: 'DOWN', side: 'away', priceCents: 69 },
      { label: 'UP', side: 'home', priceCents: 31 },
    ],
    feed
  )
  expect(outcomes).toEqual([
    { label: 'DOWN', side: 'away', priceTicks: 501 },
    { label: 'UP', side: 'home', priceTicks: 501 },
  ])
  expect(exchangeBook(feed).bids[0]).toMatchObject({
    priceTicks: 499,
    qty: '100',
    totalMicros: '49900000',
  })
})

function polled(gemini: RustGeminiSeries | undefined) {
  return { status: 'polled' as const, gemini }
}

describe('tradingUnavailable', () => {
  const block = {
    env: 'production' as const,
    series: 'BTC05M',
    phase: 'open' as const,
    book: 'live' as const,
  }
  const gemini = polled(block)
  it('waits for a fresh book while the feed is not live', () => {
    expect(tradingUnavailable({ ...feed, status: 'stale' }, account, gemini)).toBe(
      'Exchange feed stale. Waiting for a fresh book.'
    )
  })
  it("says a closed round is waiting for Gemini's result", () => {
    expect(
      tradingUnavailable(
        { ...feed, state: 'closed' },
        account,
        polled({ ...block, phase: 'waiting_for_result' })
      )
    ).toBe("This round is closed. Waiting for Gemini's result.")
  })
  it("says the series is waiting for Gemini's next round", () => {
    expect(
      tradingUnavailable(
        { ...feed, marketId: null },
        account,
        polled({ ...block, phase: 'waiting_for_window' })
      )
    ).toBe("Waiting for Gemini's next round to open.")
  })
  it('says a closed round without a Gemini phase reopens shortly', () => {
    expect(tradingUnavailable({ ...feed, state: 'closed' }, account, polled(undefined))).toBe(
      'This round is closed. The next one opens shortly.'
    )
  })
  it("disables an open round while Gemini's book is quiet", () => {
    expect(tradingUnavailable(feed, account, polled({ ...block, book: 'stale' }))).toBe(
      "Gemini's book has gone quiet. Trading resumes when it is live again."
    )
  })
  it("disables an open round until Gemini's book arrives", () => {
    expect(tradingUnavailable(feed, account, polled({ ...block, book: 'waiting' }))).toBe(
      "Waiting for Gemini's book."
    )
  })
  it("stops trading when Gemini's book could not be confirmed, whatever the last block said", () => {
    expect(tradingUnavailable(feed, account, { status: 'poll-failed', gemini: block })).toBe(
      "Could not confirm Gemini's book. Trading resumes when the exchange answers."
    )
  })
  it("treats an open round as closed while Gemini's phase waits for the result", () => {
    expect(
      tradingUnavailable(feed, account, polled({ ...block, phase: 'waiting_for_result' }))
    ).toBe("This round is closed. Waiting for Gemini's result.")
  })
  it('treats an open round as closed while Gemini waits for its next window', () => {
    expect(
      tradingUnavailable(feed, account, polled({ ...block, phase: 'waiting_for_window' }))
    ).toBe("Waiting for Gemini's next round to open.")
  })
  it('waits for the account snapshot and the market configuration', () => {
    expect(tradingUnavailable(feed, { ...account, available: null }, gemini)).toBe(
      'Waiting for the exchange account snapshot.'
    )
    expect(tradingUnavailable(feed, { ...account, fees: {} }, gemini)).toBe(
      'Waiting for the exchange market configuration.'
    )
    expect(tradingUnavailable(feed, account, gemini)).toBeUndefined()
  })
})
