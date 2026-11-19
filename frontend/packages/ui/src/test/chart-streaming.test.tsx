import { act, fireEvent, render, renderHook, screen } from '@testing-library/react'
import { expectNoA11yViolations } from '@kyros/testing/a11y'
import { describe, expect, it, vi } from 'vitest'

import { resolveChartData } from '../charts/internal/geometry'
import { useLiveSeries } from '../charts/internal/use-live-series'
import { useStableDomain } from '../charts/internal/use-stable-domain'
import { MarketChart } from '../charts/market-chart'
import {
  extendStroke,
  provisionalTip,
  TIP_LIVE_MAX_LEAD_MS,
  TIP_MAX_LEAD_MS,
} from '../charts/lib/stroke'
import { useLiveChartWindow } from '../charts/use-live-chart-window'

const points = [
  { time: 1_000, value: 79_658 },
  { time: 2_000, value: 79_658.21 },
  { time: 3_000, value: 79_658.48 },
]
const series = (samples = points) => [
  { color: 'var(--asset-btc)', id: 'btc', label: 'BTC', points: samples },
]

describe('streaming charts', () => {
  it('keeps the selected timestamp and observed price when the buffer trims and appends', async () => {
    const props = {
      height: 360,
      kind: 'price' as const,
      live: true,
      range: { start: 0, end: 10_000 },
      width: 1000,
    }
    const { container, rerender } = render(<MarketChart {...props} series={series()} />)
    const slider = screen.getByRole('slider')
    fireEvent.keyDown(slider, { key: 'End' })
    fireEvent.keyDown(slider, { key: 'ArrowLeft' })
    const selected = slider.getAttribute('aria-valuetext')
    rerender(
      <MarketChart
        {...props}
        series={series([...points.slice(1), { time: 4_000, value: 80_000 }])}
      />
    )
    expect(slider).toHaveAttribute('aria-valuenow', '2000')
    expect(slider).toHaveAttribute('aria-valuetext', selected)
    expect(screen.getByRole('tooltip')).toBeInTheDocument()
    fireEvent.keyDown(slider, { key: 'ArrowRight' })
    expect(slider).toHaveAttribute('aria-valuenow', '3000')
    fireEvent.keyDown(slider, { key: 'Escape' })
    expect(screen.queryByRole('tooltip')).not.toBeInTheDocument()
    await expectNoA11yViolations(container)
  })

  it('animates a bounded feed even when its sample count stays unchanged', async () => {
    const { result, rerender } = renderHook(({ samples }) => useLiveSeries(samples, true), {
      initialProps: { samples: series() },
    })
    const updated = series([...points.slice(1), { time: 4_000, value: 80_000 }])
    rerender({ samples: updated })
    expect(result.current[0]?.points.at(-1)?.value).toBeLessThan(80_000)
    await act(async () => {
      await new Promise(resolve => setTimeout(resolve, 80))
    })
    const moving = result.current[0]?.points.at(-1)?.value
    expect(moving).toBeGreaterThan(79_658.48)
    rerender({ samples: [...updated] })
    expect(result.current[0]?.points.at(-1)?.value).toBe(moving)
  })

  it('fits short history from the first known sample rather than leaving empty past time', () => {
    const { result } = renderHook(() =>
      useLiveChartWindow({ live: false, points, spanMs: 300_000 })
    )
    expect(result.current.range).toEqual({ start: 1_000, end: 3_000 })
  })

  it('keeps a fixed live window through new observations and reduced motion', () => {
    vi.useFakeTimers()
    vi.setSystemTime(10_000)
    vi.stubGlobal('matchMedia', () => ({ matches: true }))
    try {
      const { result, rerender, unmount } = renderHook(
        ({ samples }) => useLiveChartWindow({ live: true, points: samples, spanMs: 300_000 }),
        { initialProps: { samples: points } }
      )
      const before = result.current.range
      act(() => vi.advanceTimersByTime(2_000))
      expect(result.current.range).toEqual({ start: before.start + 2_000, end: before.end + 2_000 })
      rerender({ samples: [...points.slice(1), { time: 12_000, value: 79_660 }] })
      expect(result.current.range.end - result.current.range.start).toBe(300_000)
      expect(result.current.now).toBe(12_000)
      unmount()
    } finally {
      vi.unstubAllGlobals()
      vi.useRealTimers()
    }
  })

  it('holds its scale through small changes and expands to contain a shock', () => {
    const { result, rerender } = renderHook(({ domain }) => useStableDomain(domain, 'BTC', true), {
      initialProps: { domain: [100, 200] as const as readonly [number, number] },
    })
    rerender({ domain: [110, 190] })
    expect(result.current).toEqual([100, 200])
    rerender({ domain: [110, 400] })
    expect(result.current).toEqual([100, 400])
  })

  it('gives sub-dollar BTC ticks a minimum range and keeps the target within a large move', () => {
    const resolve = (value: number) =>
      resolveChartData({
        candleInterval: undefined,
        candles: undefined,
        kind: 'price',
        live: true,
        locale: undefined,
        range: undefined,
        series: series([...points, { time: 4_000, value }]),
        target: 79_658,
        timeZone: 'UTC',
        valueFormat: undefined,
      })
    const quiet = resolve(79_659).yTarget.domain
    expect(quiet[1] - quiet[0]).toBeGreaterThanOrEqual(4)
    expect(quiet[1] - quiet[0]).toBeLessThanOrEqual(10)
    const shock = resolve(80_000).yTarget.domain
    expect(shock[0]).toBeLessThan(79_658)
    expect(shock[1]).toBeGreaterThan(80_000)
  })

  it('commits an even grid of carried-forward, smoothed values and never rewrites it', () => {
    const ticks = Array.from({ length: 12 }, (_, index) => ({
      time: 100 + index * 200,
      value: index % 2 === 0 ? 10 : 12,
    }))
    const before = extendStroke(undefined, ticks, 500, 0.3)
    expect(before?.points.map(point => point.time)).toEqual([100, 500, 1_000, 1_500, 2_000])
    const inner = (before?.points ?? []).slice(1).map(point => point.value)
    // Calmer than the ticks, never past a value the tape did not have.
    expect(Math.max(...inner)).toBeLessThan(12)
    expect(Math.min(...inner)).toBeGreaterThan(10)

    const after = extendStroke(before, [...ticks, { time: 2_700, value: 100 }], 500, 0.3)
    expect(after?.points.slice(0, 5)).toEqual(before?.points)
    expect(after?.points.at(-1)?.time).toBe(2_500)
    // A step only joins once it can no longer change: 2 700 sits in the 3 000 step.
    expect(extendStroke(after, [...ticks, { time: 2_700, value: 100 }], 500, 0.3)).toBe(after)
    expect(ticks[1]?.value).toBe(12)
  })

  it('keeps committed points through a front-trimmed buffer and rebuilds on a reset', () => {
    const ticks = Array.from({ length: 12 }, (_, index) => ({ time: index * 200, value: index }))
    const before = extendStroke(undefined, ticks, 500, 0.3)
    const trimmed = extendStroke(before, ticks.slice(6), 500, 0.3)
    expect(trimmed?.points.every(point => point.time >= 1_200)).toBe(true)
    expect(trimmed?.points).toEqual(before?.points.filter(point => point.time >= 1_200))

    const reset = extendStroke(
      before,
      [
        { time: 0, value: 50 },
        { time: 600, value: 50 },
      ],
      500,
      0.3
    )
    expect(reset?.points[0]?.value).toBe(50)
    expect(extendStroke(before, [], 500, 0.3)).toBeUndefined()
  })

  it('lets the provisional tip ride the clock as far as the lead cap allows', () => {
    const ticks = [
      { time: 0, value: 50 },
      { time: 1_000, value: 52 },
    ]
    const stroke = extendStroke(undefined, ticks, 500, 0.3)
    expect(stroke).toBeDefined()
    if (!stroke) return

    // Within the cap the tip sits at the clock; at the default cap a quiet feed stops.
    expect(provisionalTip(stroke, 1_400)?.time).toBe(1_400)
    expect(provisionalTip(stroke, 1_000 + TIP_MAX_LEAD_MS + 5_000)?.time).toBe(
      1_000 + TIP_MAX_LEAD_MS
    )
    // A live feed keeps carrying the last price forward through a lull.
    const lull = 1_000 + TIP_MAX_LEAD_MS + 5_000
    expect(provisionalTip(stroke, lull, 0.3, TIP_LIVE_MAX_LEAD_MS)?.time).toBe(lull)
    // But never past the live cap either: a silence that long is a dead feed.
    expect(
      provisionalTip(stroke, 1_000 + TIP_LIVE_MAX_LEAD_MS + 1, 0.3, TIP_LIVE_MAX_LEAD_MS)?.time
    ).toBe(1_000 + TIP_LIVE_MAX_LEAD_MS)
    // The clock never pulls the tip behind the newest sample.
    expect(provisionalTip(stroke, 200, 0.3, TIP_LIVE_MAX_LEAD_MS)?.time).toBe(1_000)
    // Its value is the same whichever cap applies.
    expect(provisionalTip(stroke, lull, 0.3, TIP_LIVE_MAX_LEAD_MS)?.value).toBe(
      provisionalTip(stroke, 1_400)?.value
    )
  })
})
