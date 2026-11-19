import { MINUTE, SECOND } from '@kyros/ui/charts/lib/time'
import type { ChartPoint, ValueFormat } from '@kyros/ui/charts/lib/types'

import { type FieldPresentation, shareAxis } from '@/features/signals/signal-presentation'
import type { ChartSpec, Ref } from '../specs'
import { refKey } from '../specs/refs'
import type {
  TimeSeriesBand,
  TimeSeriesGuide,
  TimeSeriesLine,
  TimeSeriesMarker,
  TimeSeriesProps,
  TimeSeriesWindow,
} from '../widgets/time-series'
import { WINDOW_MS_DEFAULT } from '../specs/bound'

/** One reference resolved by its binding: where it came from, its name, its raw samples. */
export interface ResolvedLine {
  ref: Ref
  label: string
  /** Wire values, before the presentation's scale. */
  points: readonly ChartPoint[]
  presentation: FieldPresentation
}

export interface ChartContext {
  timeZone: string
  strike?: number | undefined
  roundOpenedAt?: number | undefined
}

/** Identity follows the slot, never the rank; the BTC price always wears its own colour. */
const SLOT_COLORS = [
  'var(--foreground)',
  'var(--action)',
  'var(--warning)',
  'var(--muted-foreground)',
] as const
const SPOT_COLOR = 'var(--asset-btc)'
const PERCENT_CHANGE: ValueFormat = { style: 'percent', maximumFractionDigits: 1 }

function windowLabel(ms: number): string {
  if (ms >= MINUTE) {
    const minutes = ms / MINUTE
    return Number.isInteger(minutes) ? `${minutes}m` : `${minutes.toFixed(1)}m`
  }
  return `${Math.round(ms / SECOND)}s`
}

/** The requested window and two finer looks at it, as the pills under a chart. */
export function windowPills(windowMs: number): TimeSeriesWindow[] {
  const half = Math.round(windowMs / 2)
  const quarter = Math.round(windowMs / 4)
  return [
    { label: windowLabel(windowMs), ms: windowMs },
    { label: windowLabel(half), ms: half },
    { label: windowLabel(quarter), ms: quarter },
  ]
}

function scaled(line: ResolvedLine): readonly ChartPoint[] {
  const { scale } = line.presentation
  return scale === 1
    ? line.points
    : line.points.map(point => ({ time: point.time, value: point.value * scale }))
}

/**
 * Percent change from a line's first sample inside the window, so unlike units share one
 * plot. The base moves as the window rolls, so this presentation redraws; it is the one
 * case a chart cannot keep its history still, and only unlike-unit comparisons take it.
 */
function normalised(points: readonly ChartPoint[], windowStart: number): readonly ChartPoint[] {
  const first = (points.find(point => point.time >= windowStart) ?? points[0])?.value
  if (first === undefined || first === 0) return points.map(point => ({ ...point, value: 0 }))
  return points.map(point => ({ time: point.time, value: (point.value / first - 1) * 100 }))
}

function colorFor(ref: Ref, slot: number): string {
  if (ref.type === 'spot') return SPOT_COLOR
  return SLOT_COLORS[slot % SLOT_COLORS.length] ?? SLOT_COLORS[0]
}

/**
 * Turn a chart spec and its resolved lines into what the time-series widget draws. Every
 * presentation decision lives here: colour by slot, one axis when the units agree and a
 * percent-change axis when they do not, guides and domain from the fields, the strike
 * band on spot, the open marker and the window pills. Points pass through whole: the
 * widget clips them to its rolling window, and thinning or filtering here would make
 * every tick look like a new series instead of an append.
 */
export function resolveChart(
  spec: ChartSpec,
  lines: readonly ResolvedLine[],
  context: ChartContext
): TimeSeriesProps {
  const windowMs = spec.window_ms ?? WINDOW_MS_DEFAULT
  const end = Math.max(0, ...lines.map(line => line.points.at(-1)?.time ?? 0))
  const windowStart = end - windowMs
  const windowed = lines

  const first = windowed[0]
  const shared =
    first !== undefined &&
    (windowed.length === 1 ||
      windowed.every(line => shareAxis(first.presentation, line.presentation)))

  const series: TimeSeriesLine[] = windowed.map((line, slot) => {
    const points = scaled(line)
    const last = points.at(-1)?.value
    return {
      id: refKey(line.ref),
      label: line.label,
      color: colorFor(line.ref, slot),
      points: shared ? points : normalised(points, windowStart),
      ...(last === undefined ? {} : { lastLabel: line.presentation.formatValue(last) }),
    }
  })

  const guides: TimeSeriesGuide[] = []
  let domain: readonly [number, number] | undefined
  let format: ValueFormat = PERCENT_CHANGE
  if (shared && first !== undefined) {
    format = first.presentation.axisFormat
    domain = first.presentation.domain
    const seen = new Set<number>()
    for (const line of windowed) {
      for (const guide of line.presentation.guides ?? []) {
        if (seen.has(guide.value)) continue
        seen.add(guide.value)
        guides.push(guide.label === undefined ? { value: guide.value } : guide)
      }
      if (line.presentation.domain === undefined) domain = undefined
    }
  } else {
    guides.push({ value: 0 })
  }

  const spot = windowed.find(line => line.ref.type === 'spot')
  const spotLast = spot?.points.at(-1)?.value
  let band: TimeSeriesBand | undefined
  if (
    spec.show_strike === true &&
    shared &&
    spot !== undefined &&
    context.strike !== undefined &&
    spotLast !== undefined
  ) {
    const above = spotLast >= context.strike
    guides.push({ value: context.strike, label: 'Strike' })
    band = {
      kind: 'threshold',
      seriesId: refKey(spot.ref),
      value: context.strike,
      side: above ? 'above' : 'below',
      color: above ? 'var(--positive)' : 'var(--negative)',
    }
  }

  const markers: TimeSeriesMarker[] =
    spec.mark_open === true && context.roundOpenedAt !== undefined
      ? [{ time: context.roundOpenedAt, label: 'Open' }]
      : []

  const title = spec.title ?? (series.length === 1 ? series[0]?.label : undefined)
  return {
    label: title === undefined ? 'Chart' : `${title} chart`,
    ...(title === undefined ? {} : { title }),
    series,
    format,
    guides,
    ...(band === undefined ? {} : { band }),
    markers,
    windows: windowPills(windowMs),
    ...(domain === undefined ? {} : { domain }),
    timeZone: context.timeZone,
  }
}
