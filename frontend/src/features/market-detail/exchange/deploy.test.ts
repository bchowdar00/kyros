import { cents } from '@kyros/money'
import { describe, expect, it } from 'vitest'

import { initialAccount } from './account'
import {
  allocationCents,
  allocationTenths,
  allocationUnavailable,
  deployUnavailable,
  signalsUnavailable,
} from './deploy'

describe('strategy deploy', () => {
  it('parses a dollar input to exact cents and converts to wire tenths', () => {
    expect(allocationCents('100')).toBe(10_000)
    expect(allocationCents('25.5')).toBe(2_550)
    expect(allocationCents('0.07')).toBe(7)
    expect(allocationCents('')).toBeUndefined()
    expect(allocationCents('1.234')).toBeUndefined()
    expect(allocationCents('abc')).toBeUndefined()
    expect(allocationTenths(cents(2_550))).toBe(25_500)
  })

  it('refuses to deploy until the account is live and not already deploying', () => {
    expect(deployUnavailable(initialAccount, false)).toBe(
      'Waiting for the exchange account snapshot.'
    )
    const live = { ...initialAccount, status: 'live' as const, available: '1000000000' }
    expect(deployUnavailable(live, false)).toBeUndefined()
    expect(deployUnavailable(live, true)).toBe('Deploying. Waiting for the exchange.')
  })

  it('names the signals the exchange is still waiting on', () => {
    expect(signalsUnavailable(undefined)).toBeUndefined()
    expect(signalsUnavailable([])).toBeUndefined()
    expect(signalsUnavailable(['Strike Delta'])).toBe(
      'Waiting on Strike Delta. Deploying now would leave the strategy idle.'
    )
    expect(signalsUnavailable(['Strike Delta', 'Round Clock'])).toContain(
      'Waiting on Strike Delta and Round Clock.'
    )
  })

  it('bounds the allocation by the exchange minimum and the available balance', () => {
    expect(allocationUnavailable('9.99', cents(100_000))).toBe(
      'Enter an allocation of at least $10.'
    )
    expect(allocationUnavailable('', undefined)).toBe('Enter an allocation of at least $10.')
    expect(allocationUnavailable('10', cents(100_000))).toBeUndefined()
    expect(allocationUnavailable('1000.01', cents(100_000))).toBe(
      'Allocation exceeds your available balance.'
    )
    expect(allocationUnavailable('1000', undefined)).toBeUndefined()
  })
})
