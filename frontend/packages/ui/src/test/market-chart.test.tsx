import { act, fireEvent, render, screen } from '@testing-library/react'
import { expectNoA11yViolations } from '@kyros/testing/a11y'
import { describe, expect, it, vi } from 'vitest'

import { ReferenceLine, TimeMarker } from '../charts/chart-overlays'
import { MINUTE } from '../charts/lib/time'
import {
  complementProbabilityPoints,
  createMockPricePoints,
  createMockProbabilityPoints,
} from '../charts/lib/mock'
import { MarketChart } from '../charts/market-chart'

const start = Date.UTC(2026, 7, 31, 23, 0)
const range = { end: start + 90 * MINUTE, start }
const timeZone = 'America/New_York'
const away = createMockProbabilityPoints({
  count: 2_000,
  range: { end: start + 78 * MINUTE, start },
  seed: 11,
})
const series = [
  { color: 'var(--market-away)', id: 'away', label: 'BOS', points: away },
  {
    color: 'var(--market-home)',
    id: 'home',
    label: 'LAL',
    points: complementProbabilityPoints(away),
  },
]
const priceStart = Date.UTC(2026, 4, 23, 3, 25)
const priceRange = { end: priceStart + 5 * MINUTE, start: priceStart }
const btc = createMockPricePoints({
  band: 60,
  basePrice: 75_674.18,
  count: 240,
  range: { end: priceStart + 4 * MINUTE, start: priceStart },
  seed: 5,
})

const liveSeries = (points: readonly { time: number; value: number }[]) => [
  { color: 'var(--market-away)', id: 'live', label: 'Live', points },
]

/** Probability y scale for a 292px chart: the plot spans 7px to 264px. */
// Plot bounds for a 292px chart: the axis row and the hover pill below it.
const PLOT_BOTTOM = 242
const PLOT_SPAN = 235
const probabilityY = (value: number) => PLOT_BOTTOM - (PLOT_SPAN * value) / 100

const endpointX = (container: HTMLElement) =>
  Number(
    container.querySelector('[data-chart-endpoint="zoom"] circle:last-of-type')?.getAttribute('cx')
  )

const endpointY = (container: HTMLElement) =>
  Number(
    container.querySelector('[data-chart-endpoint="live"] circle:last-of-type')?.getAttribute('cy')
  )

const liveEndpointX = (container: HTMLElement) =>
  Number(
    container.querySelector('[data-chart-endpoint="live"] circle:last-of-type')?.getAttribute('cx')
  )

describe('MarketChart', () => {
  it('lets a caller override the kind-derived gridline tone', () => {
    const { container } = render(
      <MarketChart
        gridTone="subtle"
        height={292}
        kind="probability"
        range={range}
        series={series}
        timeZone={timeZone}
        width={1110}
      />
    )

    const gridLines = container.querySelectorAll('[data-chart-grid] line')
    expect(gridLines.length).toBeGreaterThan(0)
    for (const line of gridLines) {
      expect(line.classList.contains('stroke-border-subtle')).toBe(true)
      expect(line.classList.contains('stroke-highlight')).toBe(false)
    }
  })

  it('emphasizes the time label nearest the latest sample when asked', () => {
    const { container } = render(
      <MarketChart
        emphasizeNowTick
        height={292}
        kind="probability"
        range={range}
        series={series}
        timeZone={timeZone}
        width={1110}
      />
    )

    const emphasized = container.querySelectorAll('[data-chart-now-tick]')
    expect(emphasized).toHaveLength(1)
    // The series ends 78 minutes in (7:00 PM start → latest ≈ 8:18 PM), so 8:15 PM is nearest.
    expect(emphasized[0]?.textContent).toBe('8:15 PM')
    expect(emphasized[0]?.classList.contains('fill-foreground')).toBe(true)
  })

  it('keeps every time label muted by default', () => {
    const { container } = render(
      <MarketChart
        height={292}
        kind="probability"
        range={range}
        series={series}
        timeZone={timeZone}
        width={1110}
      />
    )

    expect(container.querySelector('[data-chart-now-tick]')).toBeNull()
  })

  it('draws a probability chart with fixed axes, glowing series and endpoint labels', async () => {
    const { container } = render(
      <MarketChart
        height={292}
        kind="probability"
        range={range}
        series={series}
        timeZone={timeZone}
        width={1110}
      />
    )

    expect(
      screen.getByRole('figure', { name: /Probability over time: BOS 31%, LAL 69%/ })
    ).toBeInTheDocument()
    expect(container.querySelectorAll('[data-chart-series]')).toHaveLength(2)

    for (const path of container.querySelectorAll('[data-chart-series-line]')) {
      expect(path.getAttribute('d')).toMatch(/^M[\d.]+ [\d.]+C/)
    }

    expect(screen.getByText('100%')).toBeInTheDocument()
    expect(screen.getByText('0%')).toBeInTheDocument()
    expect(screen.getByText('7:00 PM')).toBeInTheDocument()
    expect(screen.getByText('8:30 PM')).toBeInTheDocument()
    expect(screen.getByText('7:45 PM')).toBeInTheDocument()
    expect(container.querySelectorAll('[data-chart-endpoint]')).toHaveLength(2)
    expect(container.querySelectorAll('[data-chart-endpoint-label]')).toHaveLength(2)
    await expectNoA11yViolations(container)
    expect(screen.getAllByText('31%').length).toBeGreaterThan(0)
    expect(screen.getAllByText('69%').length).toBeGreaterThan(0)
  })

  it('scrubs with the keyboard and reports hovered values', async () => {
    const onHoverChange = vi.fn()
    const { container } = render(
      <MarketChart
        height={292}
        kind="probability"
        onHoverChange={onHoverChange}
        range={range}
        series={series}
        timeZone={timeZone}
        width={1110}
      />
    )
    const slider = screen.getByRole('slider')

    act(() => {
      slider.focus()
    })
    fireEvent.keyDown(slider, { key: 'Home' })

    expect(container.querySelector('[data-chart-crosshair]')).toBeInTheDocument()
    expect(container.querySelectorAll('[data-chart-hover-dot]')).toHaveLength(2)
    expect(slider).toHaveAttribute('aria-valuetext', expect.stringContaining('7:00:00 PM'))
    expect(onHoverChange).toHaveBeenLastCalledWith(
      expect.objectContaining({
        time: start,
        values: [
          expect.objectContaining({ label: 'BOS', seriesId: 'away' }),
          expect.objectContaining({ label: 'LAL', seriesId: 'home' }),
        ],
      })
    )

    fireEvent.keyDown(slider, { key: 'ArrowRight' })
    expect(onHoverChange).toHaveBeenLastCalledWith(expect.objectContaining({ time: away[1]?.time }))

    fireEvent.keyDown(slider, { key: 'Escape' })
    expect(onHoverChange).toHaveBeenLastCalledWith(null)
    expect(container.querySelector('[data-chart-crosshair]')).not.toBeInTheDocument()
  })

  it('snaps pointer moves to the nearest sample and clears on leave', () => {
    const onHoverChange = vi.fn()
    render(
      <MarketChart
        height={292}
        kind="probability"
        onHoverChange={onHoverChange}
        range={range}
        series={series}
        timeZone={timeZone}
        width={1110}
      />
    )
    const slider = screen.getByRole('slider')

    fireEvent.pointerMove(slider, { clientX: 0, clientY: 100 })
    expect(onHoverChange).toHaveBeenLastCalledWith(expect.objectContaining({ time: start }))

    fireEvent.pointerLeave(slider)
    expect(onHoverChange).toHaveBeenLastCalledWith(null)
  })

  it('centres a price chart on its target with a labelled reference line', () => {
    const { container } = render(
      <MarketChart
        height={360}
        kind="price"
        range={priceRange}
        series={[{ color: 'var(--asset-btc)', id: 'btc', label: 'BTC', points: btc }]}
        target={75_674.18}
        targetLabel="Target"
        timeZone="America/Los_Angeles"
        width={1255}
      />
    )

    expect(container.querySelector('[data-chart-reference-line]')).toBeInTheDocument()
    expect(screen.getByText('Target')).toBeInTheDocument()
    for (const label of [
      '75,634.18',
      '75,650.18',
      '75,666.18',
      '75,682.18',
      '75,698.18',
      '75,714.18',
    ]) {
      expect(screen.getByText(label)).toBeInTheDocument()
    }
    expect(screen.getByText('8:25 PM')).toBeInTheDocument()
    expect(screen.getByText('8:30 PM')).toBeInTheDocument()
    expect(container.querySelector('[data-chart-endpoint-labels]')).not.toBeInTheDocument()
  })

  it('renders the same series as candles when the kind changes', () => {
    const { container, rerender } = render(
      <MarketChart
        height={360}
        kind="price"
        range={priceRange}
        series={[{ color: 'var(--asset-btc)', id: 'btc', label: 'BTC', points: btc }]}
        width={1255}
      />
    )

    expect(container.querySelector('[data-chart-candles]')).not.toBeInTheDocument()

    rerender(
      <MarketChart
        candleInterval={15_000}
        height={360}
        kind="candlestick"
        range={priceRange}
        series={[{ color: 'var(--asset-btc)', id: 'btc', label: 'BTC', points: btc }]}
        width={1255}
      />
    )

    const candles = container.querySelectorAll('[data-chart-candle]')

    expect(candles).toHaveLength(17)
    expect(container.querySelector('[data-chart-candle-readout]')?.textContent).toMatch(/^O /)
    expect(screen.getByRole('figure', { name: /Price candles over time/ })).toBeInTheDocument()

    const slider = screen.getByRole('slider')
    fireEvent.keyDown(slider, { key: 'End' })
    expect(container.querySelector('[data-chart-axis-tag]')).toBeInTheDocument()
    expect(slider).toHaveAttribute('aria-valuetext', expect.stringContaining('open'))
  })

  it('keeps a live tip moving through a quiet feed inside a fixed round window', async () => {
    // A round window that ends in the future, and a feed whose last sample is a few
    // seconds old: the line must still grow with the clock, not park behind the sample.
    const now = Date.now()
    const roundRange = { end: now + 30_000, start: now - 30_000 }
    const quiet = Array.from({ length: 27 }, (_, index) => ({
      time: now - 30_000 + index * 1_000,
      value: 55,
    }))
    const { container } = render(
      <MarketChart
        height={292}
        kind="probability"
        live
        range={roundRange}
        series={liveSeries(quiet)}
        width={1110}
      />
    )
    const before = liveEndpointX(container)
    expect(before).toBeGreaterThan(0)

    // Several short waits, so the frames render as they would on screen; one long wait
    // inside `act` would collapse them into a single render and the sprung tip would
    // only have its first step.
    for (let step = 0; step < 6; step += 1) {
      await act(async () => {
        await new Promise(resolve => setTimeout(resolve, 150))
      })
    }

    // The plot spans about 16px/s of this window; the sprung tip trails the clock by a
    // fraction of a second, so 900ms of clock is still a clear move to the right.
    expect(liveEndpointX(container)).toBeGreaterThan(before + 6)
    expect(endpointY(container)).toBeCloseTo(probabilityY(55), 0)

    // Without a live feed the same chart holds still.
    const still = render(
      <MarketChart
        height={292}
        kind="probability"
        range={roundRange}
        series={liveSeries(quiet)}
        width={1110}
      />
    )
    const parked = liveEndpointX(still.container)
    await act(async () => {
      await new Promise(resolve => setTimeout(resolve, 300))
    })
    expect(liveEndpointX(still.container)).toBe(parked)
  })

  it('glides a newly appended live sample into place instead of snapping', async () => {
    const liveRange = { end: start + 20 * MINUTE, start }
    const basePoints = Array.from({ length: 10 }, (_, index) => ({
      time: start + index * MINUTE,
      value: 50,
    }))

    const { container, rerender } = render(
      <MarketChart
        height={292}
        kind="probability"
        live
        range={liveRange}
        series={liveSeries(basePoints)}
        width={1110}
      />
    )

    expect(endpointY(container)).toBeCloseTo(probabilityY(50), 1)

    rerender(
      <MarketChart
        height={292}
        kind="probability"
        live
        range={liveRange}
        series={liveSeries([...basePoints, { time: start + 10 * MINUTE, value: 80 }])}
        width={1110}
      />
    )

    expect(endpointY(container)).toBeGreaterThan(probabilityY(80) + 20)

    await act(async () => {
      await new Promise(resolve => setTimeout(resolve, 250))
    })

    const midway = endpointY(container)
    expect(midway).toBeGreaterThan(probabilityY(80))
    expect(midway).toBeLessThan(probabilityY(50))

    await act(async () => {
      await new Promise(resolve => setTimeout(resolve, 1_600))
    })

    // The drawn tip is the smoothed stroke's provisional end: it keeps gliding towards
    // the new sample and settles between the two values until later buckets commit.
    const settled = endpointY(container)
    expect(settled).toBeLessThan(midway)
    expect(settled).toBeGreaterThan(probabilityY(80))
  })

  it('composes custom overlays through the chart geometry context', () => {
    const { container } = render(
      <MarketChart height={292} kind="probability" range={range} series={series} width={1110}>
        <ReferenceLine dashed label="Even" value={50} />
        <TimeMarker label="Tip-off" time={start + 5 * MINUTE} />
      </MarketChart>
    )

    expect(container.querySelector('[data-chart-reference-line="50"]')).toBeInTheDocument()
    expect(screen.getByText('Even')).toBeInTheDocument()
    expect(container.querySelector('[data-chart-time-marker]')).toBeInTheDocument()
    expect(screen.getByText('Tip-off')).toBeInTheDocument()
  })

  it('glides the time axis when the window changes', async () => {
    const points = Array.from({ length: 200 }, (_, index) => ({
      time: start + index * 15_000,
      value: 50 + (index % 10),
    }))
    const wide = { end: start + 60 * MINUTE, start }
    const narrow = { end: start + 60 * MINUTE, start: start + 45 * MINUTE }
    const chart = (timeRange: typeof wide) => (
      <MarketChart
        height={292}
        kind="price"
        range={timeRange}
        series={[{ color: 'var(--asset-btc)', id: 'zoom', label: 'Zoom', points }]}
        timeZone={timeZone}
        width={1110}
      />
    )

    const settled = render(chart(narrow))
    const target = endpointX(settled.container)
    settled.unmount()

    const { container, rerender } = render(chart(wide))
    const before = endpointX(container)
    expect(target).toBeLessThan(before)

    rerender(chart(narrow))
    expect(endpointX(container)).toBeCloseTo(before, 1)

    await act(async () => {
      await new Promise(resolve => setTimeout(resolve, 150))
    })

    const midway = endpointX(container)
    expect(midway).toBeLessThan(before)
    expect(midway).toBeGreaterThan(target)

    await act(async () => {
      await new Promise(resolve => setTimeout(resolve, 600))
    })

    expect(endpointX(container)).toBeCloseTo(target, 1)
    expect(screen.getByText('7:45 PM')).toBeInTheDocument()
  })

  it('shows an empty state when there is nothing to draw', () => {
    const { container, rerender } = render(
      <MarketChart height={292} kind="probability" range={range} series={[]} width={1110} />
    )

    expect(screen.getByText('No data yet')).toBeInTheDocument()
    expect(screen.getByText('100%')).toBeInTheDocument()
    expect(container.querySelector('[data-chart-series]')).not.toBeInTheDocument()

    rerender(<MarketChart height={292} kind="price" series={[]} width={1110} />)
    expect(screen.getByText('No data yet')).toBeInTheDocument()
    expect(container.querySelectorAll('[data-chart-y-axis] text')).toHaveLength(0)

    rerender(<MarketChart height={292} kind="candlestick" series={[]} width={1110} />)
    expect(screen.getByText('No data yet')).toBeInTheDocument()
    expect(container.querySelector('[data-chart-candle-readout]')).not.toBeInTheDocument()
  })

  it('handles a single sample and skips non-finite values', () => {
    const { container } = render(
      <MarketChart
        height={292}
        kind="probability"
        range={range}
        series={[
          {
            color: 'var(--positive)',
            id: 'one',
            label: 'One',
            points: [{ time: start, value: 40 }],
          },
          {
            color: 'var(--negative)',
            id: 'gaps',
            label: 'Gaps',
            points: [
              { time: start, value: 60 },
              { time: start + MINUTE, value: Number.NaN },
              { time: start + 2 * MINUTE, value: 62 },
            ],
          },
        ]}
        width={1110}
      />
    )

    expect(container.querySelector('[data-chart-endpoint="one"]')).toBeInTheDocument()
    expect(screen.getByRole('figure', { name: /One 40%, Gaps 62%/ })).toBeInTheDocument()
  })

  it('accepts pre-aggregated candles without a tick series', () => {
    const candles = Array.from({ length: 12 }, (_, index) => ({
      close: 101 + index,
      high: 103 + index,
      low: 99 + index,
      open: 100 + index,
      time: start + index * MINUTE,
    }))
    const { container } = render(
      <MarketChart
        candleInterval={MINUTE}
        candles={candles}
        height={292}
        kind="candlestick"
        series={[]}
        valueFormat={{ style: 'number' }}
        width={1110}
      />
    )

    expect(container.querySelectorAll('[data-chart-candle]')).toHaveLength(12)
    expect(screen.getByRole('figure', { name: /close 112/ })).toBeInTheDocument()
    expect(container.querySelector('[data-chart-candle-readout]')?.textContent).toContain('C 112')
  })

  it('keeps the endpoint inside the plot when the data ends before the window', () => {
    const { container } = render(
      <MarketChart
        height={292}
        kind="price"
        range={{ end: start + 3 * MINUTE, start: start + 2 * MINUTE }}
        series={[
          {
            color: 'var(--asset-btc)',
            id: 'early',
            label: 'Early',
            points: [
              { time: start, value: 10 },
              { time: start + MINUTE, value: 12 },
            ],
          },
        ]}
        width={1110}
      />
    )

    const dot = container.querySelector('[data-chart-endpoint="early"] circle:last-of-type')
    expect(Number(dot?.getAttribute('cx'))).toBeLessThan(0)
    expect(container.querySelector('[data-chart-hit-area]')).toBeInTheDocument()
  })

  it('labels long windows by day and formats negative values', () => {
    const day = 24 * 60 * MINUTE
    const { container } = render(
      <MarketChart
        height={292}
        kind="price"
        range={{ end: start + 30 * day, start }}
        series={[
          {
            color: 'var(--positive)',
            id: 'pnl',
            label: 'PnL',
            points: Array.from({ length: 31 }, (_, index) => ({
              time: start + index * day,
              value: -500 + index * 40,
            })),
          },
        ]}
        showEndpointLabels
        timeZone={timeZone}
        valueFormat={{ style: 'number' }}
        width={1110}
      />
    )

    const dayLabels = [...container.querySelectorAll('[data-chart-x-axis] text')].map(
      node => node.textContent
    )
    expect(dayLabels.length).toBeGreaterThanOrEqual(3)

    for (const label of dayLabels) {
      expect(label).toMatch(/^(Aug|Sep|Oct) \d{1,2}$/)
    }

    expect(container.querySelector('[data-chart-y-axis]')?.textContent).toMatch(/^-\d/)
    expect(screen.getByRole('figure', { name: /PnL 700/ })).toBeInTheDocument()
  })

  it('clears a stale crosshair when the chart kind changes', () => {
    const { container, rerender } = render(
      <MarketChart height={292} kind="probability" range={range} series={series} width={1110} />
    )
    const slider = screen.getByRole('slider')

    fireEvent.pointerMove(slider, { clientX: 300, clientY: 100 })
    expect(container.querySelector('[data-chart-crosshair]')).toBeInTheDocument()

    rerender(
      <MarketChart
        candleInterval={MINUTE}
        height={292}
        kind="candlestick"
        range={range}
        series={series}
        width={1110}
      />
    )

    expect(container.querySelector('[data-chart-crosshair]')).not.toBeInTheDocument()
    expect(container.querySelectorAll('[data-chart-candle]').length).toBeGreaterThan(0)
  })

  it('renders only the figure shell until it has a size', () => {
    const { container } = render(<MarketChart kind="probability" series={series} />)

    expect(screen.getByRole('figure')).toBeInTheDocument()
    expect(container.querySelector('svg')).not.toBeInTheDocument()
  })
})
