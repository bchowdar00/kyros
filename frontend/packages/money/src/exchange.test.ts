import { expect, it } from 'vitest'
import { cents } from './cents'
import { estimateExchangeWinningPayout } from './exchange'
import { formatExchangeBalance, formatExchangeSpread, formatExchangeTicks } from './exchange'
it('estimates returns without rounding sub-cent quotes to cents', () => {
  expect(estimateExchangeWinningPayout(cents(10000), 501)).toBe(19960)
  expect(estimateExchangeWinningPayout(cents(10000), 625)).toBe(16000)
  expect(() => estimateExchangeWinningPayout(cents(10000), 0)).toThrow(RangeError)
})
it('retains sub-cent ticks and balances beyond Number precision', () => {
  expect(formatExchangeTicks(501)).toBe('50.1¢')
  expect(formatExchangeBalance('9007199254740993')).toBe('$9007199254.740993')
  expect(formatExchangeBalance('1')).toBe('$0.000001')
})

it('formats signed spreads without allowing negative outcome prices', () => {
  expect(formatExchangeSpread(-11)).toBe('-1.1¢')
  expect(formatExchangeSpread(-1)).toBe('-0.1¢')
  expect(formatExchangeSpread(0)).toBe('0.0¢')
  expect(formatExchangeSpread(11)).toBe('1.1¢')
  expect(() => formatExchangeTicks(-1)).toThrow(RangeError)
  expect(() => formatExchangeSpread(NaN)).toThrow(RangeError)
  expect(() => formatExchangeSpread(0.5)).toThrow(RangeError)
})

import { exchangeBudgetMicros, exchangeBuyReserve } from './exchange'
it('parses USD without float rounding and budgets the gateway fee reserve', () => {
  expect(exchangeBudgetMicros('1.01')).toBe(1010000n)
  expect(exchangeBudgetMicros('1.001')).toBeUndefined()
  expect(exchangeBudgetMicros('0')).toBeUndefined()
  expect(exchangeBuyReserve(500, 400)).toBe(510000n)
  expect(exchangeBuyReserve(999, 400)).toBe(1009000n)
  expect(exchangeBuyReserve(1, 400)).toBe(1040n)
})
