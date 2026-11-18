import * as fc from 'fast-check'
import { describe, expect, it } from 'vitest'

import {
  BPS_PER_UNIT,
  applyBps,
  bps,
  priceCentsToProbabilityBps,
  probabilityBps,
  probabilityBpsToPriceCents,
} from './bps'
import { addCents, cents, compareCents, subtractCents, sumCents } from './cents'
import { createMoneyFormatter } from './format'
import { divideRoundHalfEven } from './rounding'

const safeCents = fc.integer({ min: -1_000_000_000_000, max: 1_000_000_000_000 }).map(cents)
const ratio = fc.integer({ min: 0, max: BPS_PER_UNIT }).map(bps)

describe('cents', () => {
  it('rejects anything that is not a safe integer', () => {
    expect(() => cents(1.5)).toThrow(RangeError)
    expect(() => cents(Number.NaN)).toThrow(RangeError)
    expect(() => cents(2 ** 53)).toThrow(RangeError)
    expect(cents(-250)).toBe(-250)
  })

  it('adds and subtracts exactly', () => {
    fc.assert(
      fc.property(safeCents, safeCents, (left, right) => {
        expect(subtractCents(addCents(left, right), right)).toBe(left)
        expect(addCents(left, right)).toBe(addCents(right, left))
      })
    )
  })

  it('sums the same way as sequential addition', () => {
    fc.assert(
      fc.property(fc.array(safeCents, { maxLength: 50 }), values => {
        const expected = values.reduce((total, value) => total + value, 0)
        expect(sumCents(values)).toBe(expected)
      })
    )
  })

  it('orders values', () => {
    expect(compareCents(cents(1), cents(2))).toBe(-1)
    expect(compareCents(cents(2), cents(2))).toBe(0)
    expect(compareCents(cents(3), cents(2))).toBe(1)
  })
})

describe('divideRoundHalfEven', () => {
  it('rounds ties to the even neighbour in both directions', () => {
    expect(divideRoundHalfEven(25n, 10n)).toBe(2n)
    expect(divideRoundHalfEven(35n, 10n)).toBe(4n)
    expect(divideRoundHalfEven(-25n, 10n)).toBe(-2n)
    expect(divideRoundHalfEven(-35n, 10n)).toBe(-4n)
    expect(divideRoundHalfEven(26n, 10n)).toBe(3n)
    expect(divideRoundHalfEven(-26n, 10n)).toBe(-3n)
  })
})

describe('bps', () => {
  it('bounds probabilities to [0, 10000]', () => {
    expect(() => probabilityBps(-1)).toThrow(RangeError)
    expect(() => probabilityBps(10_001)).toThrow(RangeError)
    expect(probabilityBps(10_000)).toBe(10_000)
  })

  it('applying 100% is the identity and 0% is zero', () => {
    fc.assert(
      fc.property(safeCents, amount => {
        expect(applyBps(amount, bps(BPS_PER_UNIT))).toBe(amount)
        expect(applyBps(amount, bps(0))).toBe(0)
      })
    )
  })

  it('never exceeds the amount for ratios at or below 100%', () => {
    fc.assert(
      fc.property(
        safeCents.filter(amount => amount >= 0),
        ratio,
        (amount, fee) => {
          const applied = applyBps(amount, fee)
          expect(applied).toBeGreaterThanOrEqual(0)
          expect(applied).toBeLessThanOrEqual(amount)
        }
      )
    )
  })

  it('is monotonic in the ratio', () => {
    fc.assert(
      fc.property(
        safeCents.filter(amount => amount >= 0),
        ratio,
        ratio,
        (amount, low, high) => {
          const [smaller, larger] = low <= high ? [low, high] : [high, low]
          expect(applyBps(amount, smaller)).toBeLessThanOrEqual(applyBps(amount, larger))
        }
      )
    )
  })

  it('converts between price and probability exactly on whole cents', () => {
    fc.assert(
      fc.property(fc.integer({ min: 0, max: 100 }).map(cents), price => {
        const probability = priceCentsToProbabilityBps(price)
        expect(probability).toBe(price * 100)
        expect(probabilityBpsToPriceCents(probability)).toBe(price)
      })
    )
  })

  it('rounds sub-cent probabilities half to even', () => {
    expect(probabilityBpsToPriceCents(probabilityBps(3_150))).toBe(32)
    expect(probabilityBpsToPriceCents(probabilityBps(3_250))).toBe(32)
    expect(probabilityBpsToPriceCents(probabilityBps(3_251))).toBe(33)
  })
})

describe('createMoneyFormatter', () => {
  const enUS = createMoneyFormatter('en-US')
  const deDE = createMoneyFormatter('de-DE')

  it('formats USD from cents with the caller locale', () => {
    expect(enUS.usd(cents(294_044))).toBe('$2,940.44')
    expect(enUS.usd(cents(-1_250), { signDisplay: 'always' })).toBe('-$12.50')
    expect(enUS.usd(cents(1_250), { signDisplay: 'always' })).toBe('+$12.50')
    expect(enUS.usd(cents(120_000_000), { compact: true })).toBe('$1.2M')
    // Intl separates the symbol with a non-breaking space in de-DE.
    expect(deDE.usd(cents(294_044))).toBe('2.940,44 $')
  })

  it('keeps compact precision separate between callers', () => {
    const deployed = cents(208_000_000)
    expect(enUS.usd(deployed, { compact: true, maximumFractionDigits: 2 })).toBe('$2.08M')
    expect(enUS.usd(deployed, { compact: true })).toBe('$2.1M')
  })

  it('formats whole-dollar previews without changing the default cents precision', () => {
    expect(enUS.usd(cents(10_000), { maximumFractionDigits: 0 })).toBe('$100')
    expect(enUS.usd(cents(26_316), { maximumFractionDigits: 0 })).toBe('$263')
    expect(enUS.usd(cents(16_129), { maximumFractionDigits: 0 })).toBe('$161')
    expect(enUS.usd(cents(26_316))).toBe('$263.16')
  })

  it('formats prices, probabilities and counts', () => {
    expect(enUS.price(cents(31))).toBe('31¢')
    expect(enUS.percent(probabilityBps(3_100))).toBe('31%')
    expect(enUS.percent(probabilityBps(425), { maximumFractionDigits: 2 })).toBe('4.25%')
    expect(enUS.count(12_345)).toBe('12,345')
    expect(enUS.decimal(61.2345678)).toBe('61.2346')
    expect(enUS.decimal(79_845.735, 2)).toBe('79,845.74')
    expect(deDE.count(12_345)).toBe('12.345')
  })

  it('reuses Intl instances across calls', () => {
    const first = enUS.usd(cents(100))
    const second = enUS.usd(cents(100))
    expect(first).toBe(second)
  })
})
