import { describe, expect, it } from 'vitest'

import { fieldPresentation } from '@/features/signals/signal-presentation'
import { resolveChart, type ResolvedLine, windowPills } from './chart'

const START = 1_700_000_000_000
const walk = (base: number, step: number, count = 6) =>
  Array.from({ length: count }, (_, index) => ({
    time: START + index * 1_000,
    value: base + index * step,
  }))

const rsi: ResolvedLine = {
  ref: { type: 'signal', id: 'btc.coinbase.rsi.v1', field: 'rsi_14' },
  label: 'Coinbase RSI (14)',
  points: walk(40, 2),
  presentation: fieldPresentation('btc.coinbase.rsi.v1', { name: 'rsi_14', type: 'decimal' }),
}
const spot: ResolvedLine = {
  ref: { type: 'spot' },
  label: 'BTC/USD',
  points: walk(80_000, 10),
  presentation: fieldPresentation('pyth.btc_usd.spot.v1', { name: 'last', type: 'decimal' }),
}
const ema: ResolvedLine = {
  ref: { type: 'signal', id: 'btc.coinbase.ema_cross.v1', field: 'ema_9' },
  label: 'Coinbase EMA 9',
  points: walk(79_990, 8),
  presentation: fieldPresentation('btc.coinbase.ema_cross.v1', { name: 'ema_9', type: 'decimal' }),
}
const up: ResolvedLine = {
  ref: { type: 'market', measure: 'up' },
  label: 'YES',
  points: walk(0.5, 0.01),
  presentation: fieldPresentation('btc-5m.current.price.v1', { name: 'up', type: 'decimal' }),
}
const context = { timeZone: 'UTC' }

describe('resolveChart', () => {
  it('draws one bounded signal on its own axis with its guides and a header value', () => {
    const props = resolveChart({ kind: 'chart', series: 'btc-5m', refs: [rsi.ref] }, [rsi], context)
    expect(props.title).toBe('Coinbase RSI (14)')
    expect(props.domain).toEqual([0, 100])
    expect(props.guides).toEqual([
      { value: 30, label: 'Oversold' },
      { value: 70, label: 'Overbought' },
    ])
    expect(props.series[0]?.color).toBe('var(--foreground)')
    expect(props.series[0]?.lastLabel).toBe('50')
    expect(props.windows?.map(window => window.label)).toEqual(['4m', '2m', '1m'])
  })

  it('shares one dollar axis for spot and an EMA, colouring spot as BTC', () => {
    const props = resolveChart(
      { kind: 'chart', series: 'btc-5m', refs: [spot.ref, ema.ref], title: 'Spot vs EMA' },
      [spot, ema],
      { ...context, strike: 80_020, roundOpenedAt: START + 2_000 }
    )
    expect(props.format).toEqual({ style: 'currency', currency: 'USD', maximumFractionDigits: 0 })
    expect(props.series.map(line => line.color)).toEqual(['var(--asset-btc)', 'var(--action)'])
    expect(props.series[1]?.points[0]?.value).toBe(79_990)
    expect(props.domain).toBeUndefined()
    expect(props.band).toBeUndefined()
    expect(props.markers).toEqual([])
  })

  it('normalises unlike units to percent change but keeps real values in the legend', () => {
    const props = resolveChart(
      { kind: 'chart', series: 'btc-5m', refs: [rsi.ref, up.ref] },
      [rsi, up],
      context
    )
    expect(props.format).toEqual({ style: 'percent', maximumFractionDigits: 1 })
    expect(props.guides).toEqual([{ value: 0 }])
    expect(props.series[0]?.points[0]?.value).toBe(0)
    expect(props.series[0]?.points.at(-1)?.value).toBeCloseTo(25)
    expect(props.series[0]?.lastLabel).toBe('50')
    expect(props.series[1]?.lastLabel).toBe('55¢')
  })

  it('shades the winning side of the strike and marks the open when asked', () => {
    const props = resolveChart(
      { kind: 'chart', series: 'btc-5m', refs: [spot.ref], show_strike: true, mark_open: true },
      [spot],
      { ...context, strike: 80_020, roundOpenedAt: START + 2_000 }
    )
    expect(props.guides).toEqual([{ value: 80_020, label: 'Strike' }])
    expect(props.band).toMatchObject({ kind: 'threshold', seriesId: 'spot', side: 'above' })
    expect(props.markers).toEqual([{ time: START + 2_000, label: 'Open' }])
  })

  it('passes the buffered points through whole so the widget sees every tick as an append', () => {
    const long = { ...spot, points: walk(80_000, 1, 3_000) }
    const props = resolveChart(
      { kind: 'chart', series: 'btc-5m', refs: [spot.ref], window_ms: 60_000 },
      [long],
      context
    )
    expect(props.series[0]?.points).toBe(long.points)
    expect(props.windows?.[0]).toEqual({ label: '1m', ms: 60_000 })
  })

  it('normalises against the first sample inside the window, not the buffer', () => {
    const long = { ...spot, points: walk(80_000, 1, 3_000) }
    const props = resolveChart(
      { kind: 'chart', series: 'btc-5m', refs: [spot.ref, rsi.ref], window_ms: 60_000 },
      [long, rsi],
      context
    )
    const base = long.points.find(point => point.time >= START + 2_999_000 - 60_000)
    const index = long.points.indexOf(base as (typeof long.points)[number])
    expect(props.series[0]?.points[index]?.value).toBe(0)
  })

  it('labels windows in minutes or seconds', () => {
    expect(windowPills(90_000).map(window => window.label)).toEqual(['1.5m', '45s', '23s'])
  })
})
