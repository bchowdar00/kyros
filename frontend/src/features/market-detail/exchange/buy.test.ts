import { expect, it } from 'vitest'
import { exchangeMarketKey } from '@tiny/domain'
import { initialAccount } from './account'
import { initialFeed } from './feed'
import { exchangeBook, exchangeOutcomes } from './book.mapper'
import { quoteExchangeBuy } from './buy'
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
