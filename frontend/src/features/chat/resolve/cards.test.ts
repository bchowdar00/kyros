import type { RustPositionFrame, RustSignalFrame } from '@tiny/domain'
import { describe, expect, it } from 'vitest'

import { resolveMarketCard, resolvePositionCard, resolveStats } from './cards'

const frame = (signal_id: string, values: Record<string, string | number>): RustSignalFrame => ({
  signal_id,
  ts_ms: 1,
  quality: { state: 'ready', reasons: [] },
  values,
})
const address = `0x${'ab'.repeat(20)}`

describe('card resolvers', () => {
  it('builds the market card from the series internals and the spot feed', () => {
    const card = resolveMarketCard('btc-2m', {
      strike: frame('btc-2m.current.strike.v1', { price: '72754.14' }),
      price: frame('btc-2m.current.price.v1', { up: '0.62', bid: '0.61', ask: '0.63' }),
      window: frame('btc-2m.current.window.v1', { remaining: 47_000 }),
      stats: frame('btc-2m.current.stats.v1', { volume: '2365.000', open_interest: 1512 }),
      spot: frame('pyth.btc_usd.spot.v1', { last: '72783.05' }),
    })
    expect(card).toMatchObject({
      label: 'btc-2m market',
      eyebrow: 'btc-2m binary',
      eyebrowMeta: 'T−0:47',
      hero: { value: '$72,783.05', delta: '+$28.91' },
    })
    expect(card?.stats.map(stat => [stat.label, stat.value])).toEqual([
      ['Strike', '$72,754.14'],
      ['YES', '62¢'],
      ['Spread', '2¢'],
      ['Time left', '0:47'],
      ['Volume', '$2,365.00'],
      ['Open interest', '1,512 lots'],
    ])
  })

  it('needs both the strike and the spot, and shows unknowns as unknown', () => {
    expect(
      resolveMarketCard('btc-2m', { spot: frame('pyth.btc_usd.spot.v1', { last: '1' }) })
    ).toBeUndefined()
    const sparse = resolveMarketCard('btc-2m', {
      strike: frame('btc-2m.current.strike.v1', { price: '100.00' }),
      spot: frame('pyth.btc_usd.spot.v1', { last: '99.50' }),
    })
    expect(sparse?.hero.delta).toBe('-$0.50')
    expect(sparse?.stats.find(stat => stat.id === 'yes')?.value).toBe('—')
    expect(sparse?.eyebrowMeta).toBeUndefined()
  })

  it('lays out the round stats in three columns', () => {
    const grid = resolveStats(
      'btc-2m',
      frame('btc-2m.current.stats.v1', {
        volume: '2365.000',
        open_interest: 1512,
        collateral: '1512.000',
        trades: 29,
        traders: 28,
        largest_trade: 190,
      })
    )
    expect(grid.columns).toBe(3)
    expect(grid.stats.map(stat => stat.value)).toEqual([
      '$2,365.00',
      '1,512 lots',
      '$1,512.00',
      '29',
      '28',
      '190 lots',
    ])
  })

  it('reads the position for the series from the snapshot or its rows', () => {
    const snapshot: RustPositionFrame = {
      type: 'position_snapshot',
      as_of_unix_ms: 9,
      series: 'btc-2m',
      market_id: 3,
      address,
      side: 'no',
      size_shares: 4,
      signed_position: -4,
      avg_entry_yes_tenths: 580,
      cost_basis_tenths: -2080,
      mark_yes_tenths: 620,
      unrealized_pnl_tenths: 40,
      realized_pnl_this_market_tenths: 0,
      balance_tenths: 997_920,
      locked_tenths: 2080,
      series_positions: [
        {
          series: 'btc-15m',
          market_id: 8,
          side: 'yes',
          size_shares: 100,
          signed_position: 100,
          avg_entry_yes_tenths: 580,
          cost_basis_tenths: 58_000,
          mark_yes_tenths: 620,
          mark_value_tenths: 62_000,
          unrealized_pnl_tenths: 4_000,
          market_pnl_tenths: 4_000,
        },
      ],
    }
    expect(resolvePositionCard('btc-2m', snapshot, 47_000)).toEqual({
      series: 'btc-2m',
      contracts: -4,
      entryCents: 58,
      markCents: 62,
      lockedCents: 208,
      secondsRemaining: 47,
    })
    expect(resolvePositionCard('btc-15m', snapshot, undefined)).toMatchObject({
      contracts: 100,
      lockedCents: 5_800,
      secondsRemaining: 0,
    })
    expect(resolvePositionCard('eth-2m', snapshot, undefined)).toBeUndefined()
  })
})
