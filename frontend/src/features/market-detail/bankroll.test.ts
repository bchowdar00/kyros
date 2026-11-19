import { describe, expect, it } from 'vitest'
import type { StrategyBrief } from '@/mastra/capabilities/strategy/brief'
import { deriveEnvelope } from '@/mastra/capabilities/strategy/envelope'
import { applyBankroll, bankrollInputs, dollarsToTenths, initialBankroll } from './bankroll'

const brief: StrategyBrief = {
  series: 'btc-5m',
  summary: 'x',
  goal: 'x',
  entry: 'x',
  signal_ids: [],
  outcome: 'up',
  recurrence: 'once_per_round',
  interval: null,
  entry_style: 'market',
  exit: 'x',
  allocation: '100',
  round_share: '10%',
  trade_share: '25%',
  stop_after_loss: '50',
  stop_before_close: '10s',
  assumptions: [],
}

describe('bankroll', () => {
  it('starts from the brief and shows the shares as dollars', () => {
    const bankroll = initialBankroll(brief)
    expect(bankroll).toEqual({
      allocation: '100.00',
      roundBudget: null,
      tradeBudget: null,
      stopAfterLoss: '50.00',
    })
    expect(bankrollInputs(brief, bankroll)).toEqual({
      allocation: '100.00',
      roundBudget: '10.00',
      tradeBudget: '2.50',
      stopAfterLoss: '50.00',
    })
  })

  it('scales untouched budgets with the allocation and keeps touched ones', () => {
    const scaled = { ...initialBankroll(brief), allocation: '200' }
    expect(bankrollInputs(brief, scaled).roundBudget).toBe('20.00')
    expect(bankrollInputs(brief, scaled).tradeBudget).toBe('5.00')

    const touched = { ...scaled, roundBudget: '15' }
    expect(bankrollInputs(brief, touched)).toEqual({
      allocation: '200',
      roundBudget: '15',
      tradeBudget: '3.75',
      stopAfterLoss: '50.00',
    })
    const derived = deriveEnvelope(applyBankroll(brief, { ...touched, tradeBudget: '4' }))
    expect(derived.ok && derived.envelope).toEqual({
      allocation: '200',
      loss_budget: '50',
      round_budget: '15',
      max_order_size: '4',
      max_position_per_market: '15',
      max_open_orders: '4',
    })
  })

  it('leaves a budget blank when the money does not derive', () => {
    const inputs = bankrollInputs(brief, { ...initialBankroll(brief), allocation: '4' })
    expect(inputs.roundBudget).toBe('')
    expect(
      deriveEnvelope(applyBankroll(brief, { ...initialBankroll(brief), allocation: '4' }))
    ).toEqual({
      ok: false,
      problem: expect.stringContaining('less than one contract'),
    })
  })

  it('converts typed dollars to tenths of a cent', () => {
    expect(dollarsToTenths('100.00')).toBe(100_000)
    expect(dollarsToTenths('250.5')).toBe(250_500)
    expect(dollarsToTenths('7')).toBe(7_000)
  })
})
