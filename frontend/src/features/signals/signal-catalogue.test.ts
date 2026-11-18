import type { RustSignal, RustSignalCatalogue } from '@tiny/domain'
import { describe, expect, it } from 'vitest'

import { applyFrames, defaultPins } from './signal-catalogue'
import { formatSignalValue } from './signal-value-format'

const stats: RustSignal = {
  signal_id: 'btc-2m.current.stats.v1',
  name: 'Round Stats',
  description: '',
  location: 'internal',
  scope: { kind: 'series', series_id: 'btc-2m' },
  live: true,
  quality: { state: 'ready', reasons: [] },
  ts_ms: 1,
  fields: [
    { name: 'volume', label: 'Round Volume', type: 'decimal', headline: true, value: '88650.000' },
    {
      name: 'open_interest',
      label: 'Open Interest (lots)',
      type: 'u32',
      headline: true,
      value: 27_934,
    },
    {
      name: 'collateral',
      label: 'Open Interest',
      type: 'decimal',
      headline: true,
      value: '27934.000',
    },
    { name: 'trades', label: 'Trades', type: 'u32', headline: true, value: 1_073 },
    { name: 'traders', label: 'Traders', type: 'u32', headline: true, value: 102 },
    { name: 'largest_trade', label: 'Largest Trade', type: 'u32', headline: true },
  ],
}

const rsi: RustSignal = {
  signal_id: 'btc.coinbase.rsi.v1',
  name: 'Coinbase RSI',
  description: '',
  location: 'internal',
  scope: { kind: 'underlying', underlying: 'btc' },
  lane: 'spot',
  live: true,
  quality: { state: 'ready', reasons: [] },
  fields: [
    { name: 'rsi_14', label: 'RSI 14', type: 'decimal', headline: true, value: '61.2345678' },
    {
      name: 'regime',
      label: 'Regime',
      type: 'enum',
      variants: ['neutral'],
      headline: true,
      value: 'neutral',
    },
    {
      name: 'window_close',
      label: 'Window Close',
      type: 'timestamp',
      headline: false,
      value: '2026-09-06T00:00:00Z',
    },
  ],
}

const catalogue: RustSignalCatalogue = {
  series: 'btc-2m',
  underlying: 'btc',
  signals: [stats, rsi],
}

describe('signal catalogue', () => {
  it('defaults to volume, open interest in dollars and the two spot prices', () => {
    expect(defaultPins('btc-2m')).toEqual([
      'btc-2m.current.stats.v1:volume',
      'btc-2m.current.stats.v1:collateral',
      'pyth.btc_usd.spot.v1:last',
      'btc.external.aggressor_flow.v1:signed_notional_usd_10s',
    ])
  })

  it('formats stats with their units and other fields by type', () => {
    const values = stats.fields.map(field => formatSignalValue(stats, field))
    expect(values).toEqual(['$88,650.00', '27,934 lots', '$27,934.00', '1,073', '102', '—'])
    const [rsiValue, regime, windowClose] = rsi.fields.map(field => formatSignalValue(rsi, field))
    expect(rsiValue).toBe('61.2346')
    expect(regime).toBe('Neutral')
    expect(windowClose).toMatch(/^\d{2}:\d{2}:\d{2}/)
    expect(
      formatSignalValue(rsi, {
        name: 'remaining',
        label: 'Time Left',
        type: 'duration_ms',
        headline: true,
        value: 113_839,
      })
    ).toBe('1:54')
    expect(
      formatSignalValue(rsi, {
        name: 'taker',
        label: 'Taker',
        type: 'string',
        headline: false,
        value: '0xb007001a2578baf44f3525fc591747a41cdea139',
      })
    ).toBe('0xb007…a139')
  })

  it('applies streamed frames over the catalogue values', () => {
    const live = applyFrames(catalogue, {
      'btc-2m.current.stats.v1': {
        signal_id: 'btc-2m.current.stats.v1',
        ts_ms: 9,
        quality: { state: 'ready', reasons: [] },
        values: { volume: '90000.000', largest_trade: 250 },
      },
    })
    const liveStats = live.signals[0]
    expect(liveStats?.ts_ms).toBe(9)
    expect(liveStats?.fields.find(f => f.name === 'volume')?.value).toBe('90000.000')
    expect(liveStats?.fields.find(f => f.name === 'largest_trade')?.value).toBe(250)
    expect(liveStats?.fields.find(f => f.name === 'trades')?.value).toBe(1_073)
    expect(live.signals[1]).toBe(rsi)
    expect(catalogue.signals[0]?.fields.find(f => f.name === 'volume')?.value).toBe('88650.000')
  })
})
