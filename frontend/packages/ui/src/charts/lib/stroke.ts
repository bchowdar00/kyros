import { findNearestIndex } from './scale'
import type { ChartPoint } from './types'

/** One stroke point every half second, so a 5 Hz tape no longer renders as a comb. */
export const STROKE_STEP_MS = 500
/** EMA weight per grid step: 1 follows every tick, lower is calmer but lags more. */
export const STROKE_EMA_ALPHA = 0.3
/**
 * How far past the newest observed sample the provisional tip may reach. Within it the
 * tip rides the chart clock so the line grows every frame; a feed quieter than this is
 * stale, and the tip stops rather than carry a price into the future indefinitely.
 */
export const TIP_MAX_LEAD_MS = 2_000
/**
 * The lead a live feed is allowed. A quiet book is still the price until it changes, and
 * the exchange republishes a series' price at least once a second, so the line keeps
 * riding the clock through a lull; a silence this long means the feed itself is gone.
 */
export const TIP_LIVE_MAX_LEAD_MS = 15_000

/**
 * The drawn form of a series: a committed, append-only grid. Every entry is
 * final the moment it is added, so history never moves under the reader.
 */
export interface Stroke {
  /** Smoothed samples on an even, epoch-anchored time grid. */
  points: readonly ChartPoint[]
  /** The newest source sample folded in; a later series that still holds it is an append. */
  sourceLast: ChartPoint | undefined
  /** Index of `sourceLast` in the source at the time it was folded in. */
  sourceLength: number
  /** The latest observed value carried into the next grid bucket. */
  latest: number
}

const EMPTY: Stroke = { latest: Number.NaN, points: [], sourceLast: undefined, sourceLength: 0 }

/**
 * A stroke can be extended, rather than rebuilt, when the series is the one it
 * was built from plus newer samples: its last folded sample is still present at
 * the same time and value.
 */
function isAppend(stroke: Stroke, source: readonly ChartPoint[]): number {
  const { sourceLast } = stroke

  if (!sourceLast) {
    return -1
  }

  const guess = stroke.sourceLength - 1
  const candidate = source[guess]

  if (candidate && candidate.time === sourceLast.time && candidate.value === sourceLast.value) {
    return guess
  }

  const index = findNearestIndex(source, sourceLast.time)
  const nearest = source[index]

  return nearest && nearest.time === sourceLast.time && nearest.value === sourceLast.value
    ? index
    : -1
}

/**
 * Advances `stroke` with the samples in `source`. The line starts at the first
 * observed sample; from there, grid buckets that have fully elapsed (a newer
 * sample exists past them) are committed once: the value carried forward at the
 * bucket time, blended into a causal EMA. Committed points are never revisited;
 * anything but an append (a reset, a rollover, a new series) rebuilds from
 * scratch. Points older than the source's first sample are dropped so a rolling
 * buffer does not grow the stroke without bound. Returns `previous` itself when
 * nothing changed, and `undefined` for an empty source.
 */
export function extendStroke(
  previous: Stroke | undefined,
  source: readonly ChartPoint[],
  stepMs = STROKE_STEP_MS,
  alpha = STROKE_EMA_ALPHA
): Stroke | undefined {
  const first = source[0]
  const last = source.at(-1)

  if (!first || !last || stepMs <= 0) {
    return undefined
  }

  const appendAt = previous ? isAppend(previous, source) : -1
  const base = appendAt >= 0 && previous ? previous : EMPTY

  if (
    base === previous &&
    previous.sourceLength === source.length &&
    previous.sourceLast?.time === last.time &&
    previous.sourceLast.value === last.value &&
    (previous.points[0]?.time ?? first.time) >= first.time
  ) {
    return previous
  }

  const points = [...base.points]
  let latest = base.latest
  let smoothed = points.at(-1)?.value
  let index = appendAt >= 0 ? appendAt + 1 : 0

  if (points.length === 0 && Number.isFinite(first.value)) {
    points.push({ time: first.time, value: first.value })
    latest = first.value
    smoothed = first.value
    index = 1
  }

  let time =
    points.length > 0
      ? Math.floor((points.at(-1) as ChartPoint).time / stepMs) * stepMs + stepMs
      : Math.ceil(first.time / stepMs) * stepMs

  // A bucket commits only once a sample exists past it, so its carried value is final.
  // It carries the newest sample strictly before its instant; a tick landing exactly on
  // the grid belongs to the bucket that follows.
  while (time < last.time) {
    while (index < source.length && (source[index] as ChartPoint).time < time) {
      const value = (source[index] as ChartPoint).value

      if (Number.isFinite(value)) {
        latest = value
      }

      index += 1
    }

    if (Number.isFinite(latest)) {
      smoothed =
        smoothed === undefined || alpha >= 1 ? latest : alpha * latest + (1 - alpha) * smoothed
      points.push({ time, value: smoothed })
    }

    time += stepMs
  }

  while (index < source.length) {
    const value = (source[index] as ChartPoint).value

    if (Number.isFinite(value)) {
      latest = value
    }

    index += 1
  }

  const start = points.findIndex(point => point.time >= first.time)

  return {
    latest,
    points: start > 0 ? points.slice(start) : points,
    sourceLast: last,
    sourceLength: source.length,
  }
}

/**
 * The uncommitted end of the line: what the next bucket will commit, placed at `now`
 * (clamped to at most `maxLeadMs` past the newest sample). Its x follows the clock and
 * its y follows the latest tick, so the tip moves continuously instead of stepping once
 * per bucket. When the bucket commits, the committed point lands where this one already was.
 */
export function provisionalTip(
  stroke: Stroke,
  now: number,
  alpha = STROKE_EMA_ALPHA,
  maxLeadMs = TIP_MAX_LEAD_MS
): ChartPoint | undefined {
  const { latest, sourceLast } = stroke
  const committed = stroke.points.at(-1)

  if (!sourceLast || !Number.isFinite(latest)) {
    return undefined
  }

  const time = Math.min(Math.max(now, sourceLast.time), sourceLast.time + maxLeadMs)

  if (committed && time <= committed.time) {
    return undefined
  }

  const value =
    committed === undefined || alpha >= 1 ? latest : alpha * latest + (1 - alpha) * committed.value

  return { time, value }
}
