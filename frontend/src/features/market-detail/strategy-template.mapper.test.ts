import type { RustStrategyTemplate } from '@kyros/domain'
import { cents } from '@kyros/money'
import { describe, expect, it } from 'vitest'

import { deployable, toPublicStrategies, toPublicStrategy } from './strategy-template.mapper'

const template: RustStrategyTemplate = {
  id: 'fair-value-discount',
  title: 'Buy the side the model says is cheap',
  series: ['btc-5m', 'btc-15m'],
  creator: { handle: '@kyros', username: 'Kyros', avatar: 'strategy-01.jpg' },
  envelope: { loss_budget_fraction: '0.5', max_order_size: '50' },
  primary: {
    name: 'Fair Value Discount',
    logic: 'Buy Up under fair.',
    running: 3,
    deployed_tenths: 2_500_005,
  },
  inverse: {
    name: 'Fair Value Discount (Down)',
    logic: 'Buy Down under fair.',
    running: 1,
    deployed_tenths: 500_000,
  },
  running: 4,
  deployed_tenths: 3_000_005,
  runners: ['participant-01.png'],
  ready: true,
  waiting_on: [],
}

const waiting: RustStrategyTemplate = {
  ...template,
  id: 'trend-rider',
  ready: false,
  waiting_on: [
    { signal_id: 'btc.external.horizon_momentum.v1', name: 'BTC fifteen-minute momentum' },
    { signal_id: 'btc.external.micro_momentum.v1', name: 'BTC thirty-second momentum' },
  ],
}

describe('strategy template mapper', () => {
  it('maps both prose blocks, the portrait path and tenths to cents', () => {
    const strategy = toPublicStrategy(template)

    expect(strategy.id).toBe('fair-value-discount')
    expect(strategy.templateId).toBe('fair-value-discount')
    expect(strategy.logic).toBe('Buy Up under fair.')
    expect(strategy.inverseLogic).toBe('Buy Down under fair.')
    expect(strategy.creator).toEqual({
      handle: '@kyros',
      username: 'Kyros',
      avatar: { alt: '@kyros avatar', src: '/assets/market/avatars/strategy-01.jpg' },
    })
    expect(strategy.runningCount).toBe(4)
    expect(strategy.deployedCents).toBe(cents(300_001))
    expect(strategy.runners).toEqual([
      { alt: '', src: '/assets/market/avatars/participant-01.png' },
    ])
  })

  it('leaves the portrait and runners out when the exchange has none', () => {
    const strategy = toPublicStrategy({
      ...template,
      creator: { ...template.creator, avatar: null },
      runners: [],
    })

    expect(strategy.creator).toEqual({ handle: '@kyros', username: 'Kyros' })
    expect('avatar' in strategy.creator).toBe(false)
    expect('runners' in strategy).toBe(false)
    expect('waitingOn' in strategy).toBe(false)
  })

  it('names the signals a card is waiting on, and reads no readiness as ready', () => {
    expect(toPublicStrategy(waiting).waitingOn).toEqual([
      'BTC fifteen-minute momentum',
      'BTC thirty-second momentum',
    ])
    expect(deployable(toPublicStrategy(waiting))).toBe(false)
    const { ready, waiting_on, ...legacy } = template
    expect(ready).toBe(true)
    expect(waiting_on).toEqual([])
    expect(deployable(toPublicStrategy(legacy))).toBe(true)
  })

  it('keeps catalogue order and hides the cards that cannot deploy yet', () => {
    const list = toPublicStrategies({
      series: 'btc-5m',
      templates: [template, waiting, { ...template, id: 'late-closer' }],
    })

    expect(list.map(strategy => strategy.id)).toEqual(['fair-value-discount', 'late-closer'])
  })
})
