import { describe, expect, it } from 'vitest'
import type { StrategyBrief } from '@/mastra/capabilities/strategy/brief'
import {
  centsToDecimal,
  centsToMoney,
  deriveEnvelope,
  describeMoney,
  parseAmount,
} from '@/mastra/capabilities/strategy/envelope'

const base: StrategyBrief = {
  series: 'btc-5m',
  summary: 'Buys up once the price crosses 60 cents and sells at a 5 cent gain.',
  goal: 'Buy the dip',
  entry: 'up under 0.4',
  signal_ids: [],
  outcome: 'up',
  recurrence: 'once_per_round',
  interval: null,
  entry_style: 'market',
  exit: 'hold to settlement',
  allocation: '100',
  round_share: '10%',
  trade_share: '25%',
  stop_after_loss: '100',
  stop_before_close: '10s',
  assumptions: [],
}

describe('deriveEnvelope', () => {
  it('nests the three budgets from the worked example', () => {
    const derived = deriveEnvelope(base)
    expect(derived).toEqual({
      ok: true,
      envelope: {
        allocation: '100',
        loss_budget: '100',
        round_budget: '10',
        max_order_size: '2',
        max_position_per_market: '10',
        max_open_orders: '4',
      },
      money: {
        allocation: '$100',
        roundBudget: '$10',
        tradeBudget: '$2.50',
        stopAfterLoss: '$100',
        maxOrderContracts: 2,
      },
    })
  })

  it('scales with the allocation and nothing else', () => {
    const derived = deriveEnvelope({ ...base, allocation: '200' })
    expect(derived.ok && derived.envelope).toMatchObject({
      round_budget: '20',
      max_order_size: '5',
      max_position_per_market: '20',
      loss_budget: '100',
    })
  })

  it('accepts fixed dollar shares and caps them at the level above', () => {
    const fixed = deriveEnvelope({ ...base, round_share: '$15', trade_share: '5 dollars' })
    expect(fixed.ok && fixed.envelope).toMatchObject({ round_budget: '15', max_order_size: '5' })
    const capped = deriveEnvelope({ ...base, round_share: '$500', trade_share: '$80' })
    expect(capped.ok && capped.envelope).toMatchObject({
      round_budget: '100',
      max_position_per_market: '100',
      max_order_size: '80',
    })
  })

  it('reads a bare fraction, a percent and a whole-round share', () => {
    expect(parseAmount('0.1')).toEqual({ kind: 'fraction', numerator: 1, denominator: 10 })
    expect(parseAmount('10%')).toEqual({ kind: 'fraction', numerator: 10, denominator: 100 })
    expect(parseAmount('1')).toEqual({ kind: 'fraction', numerator: 1, denominator: 1 })
    expect(parseAmount('$2.50')).toEqual({ kind: 'dollars', cents: 250 })
    expect(parseAmount('2.5')).toEqual({ kind: 'dollars', cents: 250 })
    expect(parseAmount('lots')).toBeUndefined()
    const whole = deriveEnvelope({ ...base, trade_share: '1' })
    expect(whole.ok && whole.envelope.max_order_size).toBe('10')
  })

  it('caps the stop at the allocation so the compiler accepts it', () => {
    const derived = deriveEnvelope({ ...base, stop_after_loss: '250' })
    expect(derived.ok && derived.envelope.loss_budget).toBe('100')
  })

  it('gives makers more resting orders', () => {
    const derived = deriveEnvelope({ ...base, entry_style: 'limit' })
    expect(derived.ok && derived.envelope.max_open_orders).toBe('8')
  })

  it('refuses a budget under one contract with a sentence for the user', () => {
    expect(deriveEnvelope({ ...base, allocation: '5' })).toEqual({
      ok: false,
      problem:
        'Each round would see $0.50, less than one contract; raise the allocation or the round share.',
    })
    expect(deriveEnvelope({ ...base, trade_share: '5%' })).toEqual({
      ok: false,
      problem:
        'Each order could spend $0.50, less than one contract; raise the trade share or the round budget.',
    })
    expect(deriveEnvelope({ ...base, allocation: 'some' }).ok).toBe(false)
    expect(deriveEnvelope({ ...base, stop_after_loss: '0' }).ok).toBe(false)
  })

  it('floors sub-cent amounts and never emits trailing zeros', () => {
    expect(centsToDecimal(250)).toBe('2.5')
    expect(centsToDecimal(205)).toBe('2.05')
    expect(centsToDecimal(1000)).toBe('10')
    expect(centsToMoney(250)).toBe('$2.50')
    expect(centsToMoney(1000)).toBe('$10')
    const odd = deriveEnvelope({ ...base, allocation: '33.333', round_share: '0.333' })
    expect(odd.ok && odd.envelope).toMatchObject({ allocation: '33.33', round_budget: '11.09' })
  })

  it('describes the money in one line', () => {
    expect(describeMoney(base)).toBe(
      '$100 in, sees $10 per round, up to $2.50 per order, stops after losing $100'
    )
    expect(describeMoney({ ...base, allocation: '1' })).toMatch(/less than one contract/)
  })
})
