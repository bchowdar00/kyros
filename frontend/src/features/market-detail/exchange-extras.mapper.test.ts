import { describe, expect, it } from 'vitest'

import { toRecentMarketResults, toStatsSignals } from './exchange-extras.mapper'

describe('exchange extras mapper', () => {
  it('orders resolved rounds oldest to newest and skips the open one', () => {
    expect(
      toRecentMarketResults([
        { id: 3, strike_btc_usd: 1, opened_at: 3, closes_at: 4, status: { kind: 'open' } },
        {
          id: 2,
          strike_btc_usd: 1,
          opened_at: 2,
          closes_at: 3,
          status: { kind: 'resolved', outcome: 'no', close_price_btc_usd: 0.5 },
        },
        {
          id: 1,
          strike_btc_usd: 1,
          opened_at: 1,
          closes_at: 2,
          status: { kind: 'resolved', outcome: 'yes', close_price_btc_usd: 2 },
        },
      ])
    ).toEqual(['above', 'below'])
  })

  it('formats round stats as signal cards', () => {
    const signals = toStatsSignals({
      series: 'btc-5m',
      duration_secs: 300,
      market_id: 3,
      opened_at_ms: 0,
      closes_at_ms: 300_000,
      total_volume_tenths: 1_234_560,
      open_interest_contracts: 420,
      total_locked_tenths: 420_000,
      total_trades: 57,
      unique_traders: 12,
      largest_trade: null,
    })
    expect(signals.items.map(item => item.value)).toEqual(['$1,234.56', '$420.00'])
    expect(signals.total).toBe(2)
  })
})
