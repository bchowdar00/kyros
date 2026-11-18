import { fireEvent, render, screen, within } from '@testing-library/react'
import type { ValueFormat } from '@tiny/ui/charts/lib/types'
import { afterEach, describe, expect, it, vi } from 'vitest'

import { WidgetFreshness, WidgetSkeleton } from '@/features/chat/render/frames'
import { windowPills } from '@/features/chat/resolve/chart'
import { CandleChart } from '@/features/chat/widgets/candle-chart'
import { Ladder } from '@/features/chat/widgets/ladder'
import { TimeSeries } from '@/features/chat/widgets/time-series'

const WINDOW_MS = 4 * 60_000
const START = Date.now() - WINDOW_MS
const USD: ValueFormat = { style: 'currency', currency: 'USD', maximumFractionDigits: 0 }
/** One tick a second, the last one now: a live window is anchored to the clock. */
const ticks = (values: readonly number[]) =>
  values.map((value, index) => ({
    time: Date.now() - (values.length - 1 - index) * 1_000,
    value,
  }))
const spot = (values: readonly number[]) => [
  { id: 'spot', label: 'BTC/USD', color: 'var(--asset-btc)', points: ticks(values) },
]
const level = (price: number) => ({ price, size: 100 })

const guideY = (container: HTMLElement, value: number) =>
  Number(container.querySelector(`[data-chart-reference-line="${value}"] line`)?.getAttribute('y1'))
const endpointX = (container: HTMLElement) =>
  Number(
    container.querySelector('[data-chart-endpoint="spot"] circle:last-of-type')?.getAttribute('cx')
  )
const candleX = (container: HTMLElement, time: number) =>
  Number(container.querySelector(`[data-chart-candle="${time}"] rect`)?.getAttribute('x'))

afterEach(() => {
  vi.restoreAllMocks()
})

describe('chart widgets keep the reply still', () => {
  it('draws the skeleton with the same rows the live chart will have', () => {
    const windows = windowPills(WINDOW_MS)
    const skeleton = render(
      <WidgetSkeleton
        label="Fair vs market chart"
        chart={{ title: 'Fair', legend: true, windows }}
      />
    )
    const pending = screen.getByRole('region', { name: 'Fair vs market chart' })
    expect(pending).toHaveAttribute('aria-busy', 'true')
    expect(within(pending).getByRole('list', { name: 'Series' })).toBeInTheDocument()
    const pendingPills = within(pending).getAllByRole('button')
    expect(pendingPills.map(pill => pill.textContent)).toEqual(['4m', '2m', '1m'])
    for (const pill of pendingPills) expect(pill).toBeDisabled()
    skeleton.unmount()

    render(
      <TimeSeries
        label="Fair vs market chart"
        title="Fair"
        live
        timeZone="UTC"
        format={{ style: 'percent' }}
        series={[
          { id: 'fair', label: 'Fair', color: 'var(--foreground)', points: ticks([58, 59, 60]) },
          { id: 'market', label: 'Market', color: 'var(--action)', points: ticks([54, 55, 56]) },
        ]}
        windows={windows}
      />
    )
    const live = screen.getByRole('region', { name: 'Fair vs market chart' })
    expect(live).not.toHaveAttribute('aria-busy')
    expect(within(live).getByRole('list', { name: 'Series' })).toBeInTheDocument()
    expect(
      within(within(live).getByRole('group', { name: 'Window' }))
        .getAllByRole('button')
        .map(pill => pill.textContent)
    ).toEqual(['4m', '2m', '1m'])
  })

  it('scrubs a compact chart with a crosshair but no pills, and reads the value in the header', () => {
    const { container } = render(
      <TimeSeries
        label="BTC chart"
        title="BTC"
        live
        timeZone="UTC"
        format={USD}
        series={spot([80_000, 80_010, 80_020])}
      />
    )
    fireEvent.keyDown(screen.getByRole('slider'), { key: 'End' })
    expect(container.querySelector('[data-chart-crosshair]')).not.toBeNull()
    expect(container.querySelector('[data-chart-axis-tag]')).toBeNull()
    expect(container.querySelector('[data-chart-crosshair] rect')).toBeNull()
    expect(screen.getByRole('region', { name: 'BTC chart' })).toHaveTextContent('$80,020')
  })

  it('holds the value axis through an ordinary tick and only moves the tip', () => {
    const values = [79_980, 79_990, 80_000, 80_010, 80_020]
    const props = {
      label: 'BTC chart',
      title: 'BTC',
      live: true,
      timeZone: 'UTC',
      format: USD,
      guides: [{ value: 80_000, label: 'Strike' }],
      windows: windowPills(WINDOW_MS),
    }
    const [line] = spot(values)
    if (!line) throw new Error('one line')
    const { container, rerender } = render(<TimeSeries {...props} series={[line]} />)
    const guideBefore = guideY(container, 80_000)
    const tipBefore = endpointX(container)
    expect(guideBefore).toBeGreaterThan(0)

    const last = line.points.at(-1)
    if (!last) throw new Error('a last tick')
    const appended = [
      { ...line, points: [...line.points, { time: last.time + 200, value: 80_025 }] },
    ]
    rerender(<TimeSeries {...props} series={appended} />)
    expect(guideY(container, 80_000)).toBe(guideBefore)
    // A 200ms tick on a four-minute window moves the tip by a fraction of a pixel.
    expect(Math.abs(endpointX(container) - tipBefore)).toBeLessThan(1)
  })

  it('places candles by bucket time so a new candle does not squeeze the others', () => {
    const bucketMs = 10_000
    const candle = (index: number, open: number) => ({
      time: START + index * bucketMs,
      open,
      high: open + 5,
      low: open - 5,
      close: open + 2,
    })
    const candles = Array.from({ length: 20 }, (_, index) => candle(index, 80_000 + index))
    const props = {
      label: 'BTC candles',
      bucketMs,
      windowMs: WINDOW_MS,
      format: USD,
      timeZone: 'UTC',
      live: true,
    }
    const { container, rerender } = render(<CandleChart {...props} candles={candles} />)
    const probe = candles[10]?.time ?? 0
    const before = candleX(container, probe)
    expect(before).toBeGreaterThan(0)

    rerender(<CandleChart {...props} candles={[...candles, candle(20, 80_020)]} />)
    expect(container.querySelector(`[data-chart-candle="${START + 20 * bucketMs}"]`)).not.toBeNull()
    // Only the clock moved the window between the two renders: well under a pixel.
    expect(Math.abs(candleX(container, probe) - before)).toBeLessThan(1)
  })

  it('schedules no animation frames when the user prefers reduced motion', () => {
    vi.stubGlobal(
      'matchMedia',
      vi.fn().mockImplementation((query: string) => ({
        matches: query.includes('prefers-reduced-motion'),
        media: query,
        addEventListener: () => {},
        removeEventListener: () => {},
      }))
    )
    const frames = vi.spyOn(window, 'requestAnimationFrame')
    const { rerender } = render(
      <TimeSeries
        label="BTC chart"
        live
        timeZone="UTC"
        format={USD}
        series={spot([80_000, 80_010])}
      />
    )
    rerender(
      <TimeSeries
        label="BTC chart"
        live
        timeZone="UTC"
        format={USD}
        series={spot([80_000, 80_010, 80_020])}
      />
    )
    expect(frames).not.toHaveBeenCalled()
    vi.unstubAllGlobals()
  })

  it('keeps every ladder side at its full row count when the book is thin', () => {
    render(
      <Ladder
        label="Thin book"
        asks={[level(63), level(64), level(65)]}
        bids={[level(61)]}
        rows={7}
        formatPrice={price => `${price}¢`}
        formatSize={size => String(size)}
      />
    )
    const asks = screen.getByRole('list', { name: 'Asks, best price last' })
    const bids = screen.getByRole('list', { name: 'Bids, best price first' })
    expect(within(asks).getAllByRole('listitem')).toHaveLength(3)
    expect(asks.querySelectorAll('li')).toHaveLength(7)
    expect(within(bids).getAllByRole('listitem')).toHaveLength(1)
    expect(bids.querySelectorAll('li')).toHaveLength(7)
  })

  it('reserves the freshness line while live and shows it when the feed drops', () => {
    const { rerender } = render(
      <WidgetFreshness status="live">
        <p>widget</p>
      </WidgetFreshness>
    )
    expect(screen.queryByText('Feed dropped. Showing the last values received.')).toHaveAttribute(
      'aria-hidden',
      'true'
    )
    rerender(
      <WidgetFreshness status="stale">
        <p>widget</p>
      </WidgetFreshness>
    )
    expect(screen.getByText('Feed dropped. Showing the last values received.')).toBeVisible()
  })
})
