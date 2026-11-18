import * as fc from 'fast-check'
import { describe, expect, it } from 'vitest'

import { cents } from './cents'
import { estimateWinningPayout } from './payout'

describe('estimateWinningPayout', () => {
  it('estimates the gross return for the supplied stake and price', () => {
    expect(estimateWinningPayout(cents(10_000), cents(50))).toBe(20_000)
    expect(estimateWinningPayout(cents(10_000), cents(38))).toBe(26_316)
    expect(estimateWinningPayout(cents(10_000), cents(62))).toBe(16_129)
    expect(estimateWinningPayout(cents(0), cents(50))).toBe(0)
    expect(estimateWinningPayout(cents(1), cents(8))).toBe(12)
    expect(estimateWinningPayout(cents(3), cents(8))).toBe(38)
  })

  it('rejects invalid prices, negative stakes, and overflowing returns', () => {
    for (const price of [0, -1, 101]) {
      expect(() => estimateWinningPayout(cents(10_000), cents(price))).toThrow(RangeError)
    }
    expect(() => estimateWinningPayout(cents(-1), cents(50))).toThrow(RangeError)
    expect(() => estimateWinningPayout(cents(Number.MAX_SAFE_INTEGER), cents(1))).toThrow(
      RangeError
    )
  })

  it('keeps rounding error within half a cent', () => {
    fc.assert(
      fc.property(
        fc.integer({ min: 0, max: 1_000_000_000 }).map(cents),
        fc.integer({ min: 1, max: 100 }).map(cents),
        (stake, price) => {
          const payout = estimateWinningPayout(stake, price)
          const error = BigInt(payout) * BigInt(price) - BigInt(stake) * 100n
          const magnitude = error < 0n ? -error : error
          expect(magnitude * 2n).toBeLessThanOrEqual(BigInt(price))
          expect(payout).toBeGreaterThanOrEqual(stake)
        }
      )
    )
  })
})
