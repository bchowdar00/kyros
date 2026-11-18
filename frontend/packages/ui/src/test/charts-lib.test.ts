import { describe, expect, it } from 'vitest'

import { aggregateCandles, isBullish } from '../charts/lib/candles'
import { downsampleM4 } from '../charts/lib/downsample'
import { estimateTextWidth, formatSignedValue, formatValue } from '../charts/lib/format'
import {
  complementProbabilityPoints,
  createMockPricePoints,
  createMockProbabilityPoints,
} from '../charts/lib/mock'
import { buildAreaPath, buildLinePath } from '../charts/lib/path'
import { resampleMean, smoothPoints } from '../charts/lib/downsample'
import { createPriceTickStep, impliedProbability } from '../charts/lib/tape'
import {
  centeredDomain,
  createLinearScale,
  findNearestIndex,
  linearTicks,
  nextNiceStep,
  niceDomain,
  niceStep,
} from '../charts/lib/scale'
import {
  chooseTimeStep,
  formatTimeTick,
  formatTooltipTime,
  getTimeZoneOffset,
  HOUR,
  MINUTE,
  timeTicks,
} from '../charts/lib/time'

describe('linear scale', () => {
  it('maps and inverts values across the range', () => {
    const scale = createLinearScale([0, 100], [264, 0])

    expect(scale(0)).toBe(264)
    expect(scale(50)).toBe(132)
    expect(scale(100)).toBe(0)
    expect(scale.invert(132)).toBe(50)
  })

  it('handles a flat domain without dividing by zero', () => {
    const scale = createLinearScale([5, 5], [0, 100])

    expect(scale(5)).toBe(50)
    expect(scale.invert(20)).toBe(5)
  })
})

describe('nice ticks', () => {
  it('chooses 1-2-5 steps', () => {
    expect(niceStep(100, 5)).toBe(20)
    expect(niceStep(100, 4)).toBe(20)
    expect(niceStep(1, 5)).toBe(0.2)
    expect(niceStep(7, 5)).toBe(1)
    expect(nextNiceStep(20)).toBe(50)
    expect(nextNiceStep(50)).toBe(100)
    expect(nextNiceStep(1)).toBe(2)
  })

  it('produces inclusive ticks without floating point noise', () => {
    expect(linearTicks(0, 100, 4)).toEqual([0, 20, 40, 60, 80, 100])
    expect(linearTicks(0, 1, 5)).toEqual([0, 0.2, 0.4, 0.6, 0.8, 1])
  })

  it('expands to a nice domain', () => {
    const result = niceDomain(31.2, 68.9, 5)

    expect(result.domain).toEqual([30, 70])
    expect(result.ticks).toEqual([30, 40, 50, 60, 70])
  })

  it('centres a domain on a target and grows the step until data fits', () => {
    const result = centeredDomain(75_674.18, [75_734, 75_640], { rows: 4 })

    expect(result.step).toBe(20)
    expect(result.domain).toEqual([75_594.18, 75_754.18])
    expect(result.ticks).toHaveLength(9)
    expect(result.ticks[4]).toBe(75_674.18)

    const wide = centeredDomain(100, [100, 190], { rows: 4 })

    expect(wide.step).toBe(50)
  })
})

describe('findNearestIndex', () => {
  const points = [{ time: 0 }, { time: 10 }, { time: 20 }, { time: 30 }]

  it('snaps to the closest sample', () => {
    expect(findNearestIndex(points, -5)).toBe(0)
    expect(findNearestIndex(points, 14)).toBe(1)
    expect(findNearestIndex(points, 16)).toBe(2)
    expect(findNearestIndex(points, 99)).toBe(3)
    expect(findNearestIndex([], 1)).toBe(-1)
  })
})

describe('time ticks', () => {
  const start = Date.UTC(2026, 7, 31, 23, 0)

  it('picks a step that respects the tick budget', () => {
    expect(chooseTimeStep(90 * MINUTE, 7)).toBe(15 * MINUTE)
    expect(chooseTimeStep(5 * MINUTE, 6)).toBe(MINUTE)
    expect(chooseTimeStep(24 * HOUR, 8)).toBe(3 * HOUR)
  })

  it('labels both edges of a quarter-hour window', () => {
    const quarterStart = Date.UTC(2026, 4, 23, 3, 15)
    const ticks = timeTicks(
      { end: quarterStart + 15 * MINUTE, start: quarterStart },
      { maxTicks: 8, timeZone: 'America/Los_Angeles' }
    )

    expect(ticks.step).toBe(3 * MINUTE)
    expect(ticks.values[0]).toBe(quarterStart)
    expect(ticks.values.at(-1)).toBe(quarterStart + 15 * MINUTE)
  })

  it('aligns ticks to wall-clock boundaries in the requested time zone', () => {
    const ticks = timeTicks(
      { end: start + 90 * MINUTE, start },
      { maxTicks: 7, timeZone: 'America/New_York' }
    )

    expect(ticks.step).toBe(15 * MINUTE)
    expect(ticks.values).toHaveLength(7)
    expect(
      formatTimeTick(ticks.values[0] ?? 0, ticks.detail, { timeZone: 'America/New_York' })
    ).toBe('7:00 PM')
    expect(
      formatTimeTick(ticks.values[6] ?? 0, ticks.detail, { timeZone: 'America/New_York' })
    ).toBe('8:30 PM')
  })

  it('offsets hours for half-hour time zones', () => {
    const offset = getTimeZoneOffset(start, 'Asia/Kolkata')

    expect(offset).toBe(5.5 * HOUR)
  })

  it('formats tooltip timestamps with seconds', () => {
    expect(formatTooltipTime(start + 27 * MINUTE + 14_000, { timeZone: 'America/New_York' })).toBe(
      'Aug 31, 7:27:14 PM'
    )
  })
})

describe('value formatting', () => {
  it('formats percent, currency and plain numbers', () => {
    expect(formatValue(68.6, { style: 'percent' })).toBe('69%')
    expect(formatValue(68.64, { maximumFractionDigits: 1, style: 'percent' })).toBe('68.6%')
    expect(formatValue(75_694.94, { currency: 'USD', style: 'currency' })).toBe('$75,694.94')
    expect(
      formatValue(31_460_000_000, {
        maximumFractionDigits: 2,
        notation: 'compact',
        style: 'currency',
      })
    ).toBe('$31.46B')
    expect(
      formatValue(28_000_000_000, {
        maximumFractionDigits: 0,
        notation: 'compact',
        style: 'currency',
      })
    ).toBe('$28B')
    expect(formatValue(1234.5, { style: 'number' })).toBe('1,234.5')
    expect(formatValue(Number.NaN, { style: 'number' })).toBe('—')
  })

  it('adds explicit signs to deltas', () => {
    expect(formatSignedValue(20.17, { currency: 'USD', style: 'currency' })).toBe('+$20.17')
    expect(formatSignedValue(-3, { style: 'percent' })).toBe('-3%')
    expect(formatSignedValue(0, { style: 'percent' })).toBe('0%')
  })

  it('estimates wider text for longer strings', () => {
    expect(estimateTextWidth('100%', 10)).toBeGreaterThan(estimateTextWidth('0%', 10))
  })
})

describe('paths', () => {
  const points = [
    { x: 0, y: 10 },
    { x: 10, y: 20.555 },
    { x: 20, y: 5 },
  ]

  it('builds linear and step paths', () => {
    expect(buildLinePath(points)).toBe('M0 10L10 20.56L20 5')
    expect(buildLinePath(points, 'step')).toBe('M0 10H10V20.56H20V5')
    expect(buildLinePath([])).toBe('')
  })

  it('closes an area to the baseline', () => {
    expect(buildAreaPath(points, 100)).toBe('M0 10L10 20.56L20 5L20 100L0 100Z')
  })
})

describe('downsampleM4', () => {
  it('keeps extremes inside each pixel bucket', () => {
    const points = Array.from({ length: 1_000 }, (_, index) => ({
      time: index,
      value: index === 500 ? 99 : index === 501 ? 1 : 50,
    }))
    const sampled = downsampleM4(points, time => time / 10)

    expect(sampled.length).toBeLessThan(points.length)
    expect(sampled.some(point => point.value === 99)).toBe(true)
    expect(sampled.some(point => point.value === 1)).toBe(true)
    expect(sampled[0]).toBe(points[0])
    expect(sampled.at(-1)).toBe(points.at(-1))

    for (let index = 1; index < sampled.length; index += 1) {
      expect(sampled[index]?.time ?? 0).toBeGreaterThan(sampled[index - 1]?.time ?? 0)
    }
  })

  it('returns the input untouched when it is already sparse', () => {
    const points = [
      { time: 0, value: 1 },
      { time: 100, value: 2 },
    ]

    expect(downsampleM4(points, time => time)).toBe(points)
  })
})

describe('aggregateCandles', () => {
  it('buckets ticks into OHLC candles and sums volume', () => {
    const candles = aggregateCandles(
      [
        { time: 0, value: 10, volume: 1 },
        { time: 20_000, value: 15, volume: 2 },
        { time: 40_000, value: 8, volume: 3 },
        { time: 59_000, value: 12 },
        { time: 61_000, value: 13, volume: 5 },
      ],
      MINUTE
    )

    expect(candles).toHaveLength(2)
    expect(candles[0]).toEqual({ close: 12, high: 15, low: 8, open: 10, time: 0, volume: 6 })
    expect(candles[1]).toEqual({ close: 13, high: 13, low: 13, open: 13, time: 60_000, volume: 5 })
    expect(isBullish(candles[0] ?? { close: 0, high: 0, low: 0, open: 1, time: 0 })).toBe(true)
  })

  it('respects the requested range', () => {
    const candles = aggregateCandles(
      [
        { time: 0, value: 1 },
        { time: 120_000, value: 2 },
      ],
      MINUTE,
      { end: 60_000, start: 0 }
    )

    expect(candles).toHaveLength(1)
  })
})

describe('mock data', () => {
  const range = { end: 4_800_000, start: 0 }

  it('is deterministic, ordered and bounded', () => {
    const first = createMockProbabilityPoints({ count: 500, range, seed: 7 })
    const second = createMockProbabilityPoints({ count: 500, range, seed: 7 })

    expect(first).toEqual(second)
    expect(first[0]?.time).toBe(range.start)
    expect(first.at(-1)?.time).toBe(range.end)

    for (const point of first) {
      expect(point.value).toBeGreaterThanOrEqual(1)
      expect(point.value).toBeLessThanOrEqual(99)
    }
  })

  it('complements probabilities to 100', () => {
    const points = createMockProbabilityPoints({ count: 50, range, seed: 3 })
    const complement = complementProbabilityPoints(points)

    expect((points[10]?.value ?? 0) + (complement[10]?.value ?? 0)).toBeCloseTo(100)
  })

  it('maps the walk onto a price band', () => {
    const points = createMockPricePoints({ band: 100, basePrice: 1_000, count: 50, range, seed: 3 })

    for (const point of points) {
      expect(point.value).toBeGreaterThanOrEqual(900)
      expect(point.value).toBeLessThanOrEqual(1_100)
      expect(point.volume).toBeGreaterThanOrEqual(0)
    }
  })
})

describe('resampleMean', () => {
  const dense = Array.from({ length: 100 }, (_, index) => ({
    time: index * 1_000,
    // A sawtooth: every other sample is an extreme, which is what reads as jitter.
    value: index % 2 === 0 ? 10 : 20,
  }))

  it('averages a dense series onto an even grid and keeps the newest sample exact', () => {
    const resampled = resampleMean(dense, 10)

    expect(resampled).toHaveLength(10)
    expect(resampled.slice(1, -1).every(point => point.value === 15)).toBe(true)
    expect(resampled.at(-1)).toEqual(dense.at(-1))
  })

  it('leaves a series that is not denser than the grid alone', () => {
    expect(resampleMean(dense, 100)).toBe(dense)
    expect(resampleMean(dense, 2)).toBe(dense)
    expect(resampleMean([], 10)).toEqual([])
  })
})

describe('smoothPoints', () => {
  it('settles a spike without moving the endpoints', () => {
    const spiky = [
      { time: 0, value: 0 },
      { time: 1, value: 0 },
      { time: 2, value: 8 },
      { time: 3, value: 0 },
      { time: 4, value: 0 },
    ]
    const smoothed = smoothPoints(spiky)

    expect(smoothed[0]).toEqual(spiky[0])
    expect(smoothed.at(-1)).toEqual(spiky.at(-1))
    expect(smoothed[2]?.value).toBe(4)
    expect(smoothed[1]?.value).toBe(2)
    expect(smoothed.map(point => point.time)).toEqual([0, 1, 2, 3, 4])
  })

  it('passes a series too short to smooth straight through', () => {
    const short = [
      { time: 0, value: 1 },
      { time: 1, value: 5 },
    ]

    expect(smoothPoints(short)).toBe(short)
  })
})

describe('monotone curve', () => {
  const points = [
    { x: 0, y: 10 },
    { x: 10, y: 0 },
    { x: 20, y: 10 },
  ]

  it('draws cubic segments that cannot overshoot a turn', () => {
    const path = buildLinePath(points, 'monotone')

    expect(path.startsWith('M0 10C')).toBe(true)
    expect(path).not.toContain('L')
    // The tangent at the low point is flat, so the curve cannot dip below it.
    expect(path).toContain('C3.33 6.67,6.67 0,10 0')
  })
})

describe('impliedProbability', () => {
  it('maps price distance from the target onto a bounded probability', () => {
    expect(impliedProbability(100, 100, 10)).toBe(50)
    expect(impliedProbability(110, 100, 10)).toBeGreaterThan(95)
    expect(impliedProbability(90, 100, 10)).toBeLessThan(5)
    expect(impliedProbability(1_000, 100, 10)).toBe(99)
  })
})

describe('createPriceTickStep', () => {
  it('pulls the tape back towards the target and keeps the noise inside the band', () => {
    const step = createPriceTickStep({ band: 100, target: 1_000 })

    expect(step({ time: 0, value: 1_000 }, () => 0.5)).toBe(1_000)
    // Far below the target, the pull is upwards even with the noise at its lowest.
    expect(step({ time: 0, value: 900 }, () => 0)).toBeGreaterThan(896)
    expect(step({ time: 0, value: 900 }, () => 1)).toBeLessThan(906)
  })
})
