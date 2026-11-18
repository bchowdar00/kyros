import { describe, expect, it } from 'vitest'
import { encodeDemoOrder, exchangeMarketKey, ExchangeUint } from './exchange'
const order = {
  account: `${'0'.repeat(39)}1`,
  marketId: '7',
  side: 'buy' as const,
  price: 501,
  qty: '9007199254740993',
  salt: '42',
  validUntil: '1788610000000000000',
}
describe('exchange wire', () => {
  it('preserves canonical key order and exact u64 strings', () => {
    expect(encodeDemoOrder(order)).toBe(
      `{"version":6,"type":"new_order","body":{"salt":"42","valid_until":"1788610000000000000","account":"${order.account}","market_id":"${'0'.repeat(63)}7","side":"buy","kind":"gtc","reduce_only":false,"price":501,"qty":"9007199254740993","expiry":"0","sig":"${'0'.repeat(130)}"}}`
    )
  })
  it.each(['01', '-1', '1.5', '18446744073709551616'])(
    'rejects invalid unsigned integers: %s',
    value => {
      expect(ExchangeUint.safeParse(value).success).toBe(false)
    }
  )
  it('refuses invalid order values and oversized market IDs', () => {
    expect(() => encodeDemoOrder({ ...order, qty: '0' })).toThrow()
    expect(() => encodeDemoOrder({ ...order, price: 1000 })).toThrow()
    expect(() => exchangeMarketKey('4294967296')).toThrow()
  })
})
