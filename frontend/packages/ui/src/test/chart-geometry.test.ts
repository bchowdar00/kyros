import { describe, expect, it } from 'vitest'

import { layoutChart, resolveChartData } from '../charts/internal/geometry'

const priceChart = resolveChartData({
  candleInterval: undefined,
  candles: undefined,
  kind: 'price',
  live: true,
  locale: 'en-US',
  range: { start: 0, end: 60_000 },
  series: [],
  target: 79_820,
  timeZone: 'UTC',
  valueFormat: undefined,
})

describe('compact density', () => {
  const compact = resolveChartData({
    candleInterval: undefined,
    candles: undefined,
    density: 'compact',
    kind: 'price',
    live: true,
    locale: 'en-US',
    range: { start: 0, end: 60_000 },
    series: [],
    target: 79_820,
    timeZone: 'UTC',
    valueFormat: undefined,
  })

  it('keeps tight insets, four rows and small axis labels for a chart inside a reply', () => {
    const geometry = layoutChart({
      data: compact,
      glowFilterId: 'chart-glow',
      height: 128,
      width: 352,
      xDomain: [0, 60_000],
      yDomain: [79_800, 79_850],
    })

    expect(geometry.density).toBe('compact')
    expect(geometry.axisFontSize).toBe(10)
    expect(geometry.plot.top).toBe(4)
    // Only the axis label row sits under the plot; there is no hover pill row.
    expect(geometry.plot.bottom).toBe(128 - 16)
    expect(geometry.yTicks).toHaveLength(4)
    expect(geometry.yTicks[0]).toBe(79_800)
    expect(geometry.yTicks.at(-1)).toBe(79_850)
  })

  it('pins a bounded signal to its fixed domain whatever the data does', () => {
    const rsi = resolveChartData({
      candleInterval: undefined,
      candles: undefined,
      density: 'compact',
      kind: 'price',
      live: true,
      locale: 'en-US',
      range: { start: 0, end: 60_000 },
      series: [
        {
          color: 'var(--foreground)',
          id: 'rsi',
          label: 'RSI',
          points: [
            { time: 0, value: 48 },
            { time: 30_000, value: 52 },
          ],
        },
      ],
      target: undefined,
      timeZone: 'UTC',
      valueDomain: [0, 100],
      valueFormat: { style: 'number' },
    })

    expect(rsi.yTarget.domain).toEqual([0, 100])
    expect(rsi.yTarget.ticks).toEqual([0, 50, 100])
    const geometry = layoutChart({
      data: rsi,
      glowFilterId: 'chart-glow',
      height: 128,
      width: 352,
      xDomain: [0, 60_000],
      yDomain: [0, 100],
    })
    // The grid follows the fixed ticks, not evenly spaced rows.
    expect(geometry.yTicks).toEqual([0, 50, 100])
  })

  it('leaves the market screen layout unchanged by default', () => {
    const geometry = layoutChart({
      data: priceChart,
      glowFilterId: 'chart-glow',
      height: 500,
      width: 900,
      xDomain: [0, 60_000],
      yDomain: [79_800, 79_850],
    })

    expect(geometry.density).toBe('default')
    expect(geometry.plot.top).toBe(38)
    expect(geometry.plot.bottom).toBe(500 - 52)
  })
})

describe('price grid', () => {
  it.each([
    [79_818, 79_822],
    [79_800, 79_850],
    [79_811.37, 79_843.19],
    [79_600, 80_200],
  ])('keeps six evenly spaced levels across the viewport %s–%s', (low, high) => {
    const geometry = layoutChart({
      data: priceChart,
      glowFilterId: 'chart-glow',
      height: 500,
      width: 900,
      xDomain: [0, 60_000],
      yDomain: [low, high],
    })

    expect(geometry.yTicks).toHaveLength(6)
    expect(geometry.yTicks[0]).toBe(low)
    expect(geometry.yTicks.at(-1)).toBe(high)
    geometry.yTicks.forEach((tick, index) => {
      expect(geometry.yScale(tick)).toBeCloseTo(
        geometry.plot.bottom - (geometry.plot.height * index) / 5,
        6
      )
    })
  })
})
