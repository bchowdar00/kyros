'use client'

import { useMemo, useState } from 'react'

import {
  extendStroke,
  provisionalTip,
  STROKE_EMA_ALPHA,
  TIP_MAX_LEAD_MS,
  type Stroke,
} from '../lib/stroke'
import type { LineSeries } from '../lib/types'

interface StrokeCache {
  source: readonly LineSeries[]
  strokes: ReadonlyMap<string, Stroke>
}

/** Extends each series' stroke from the previous cache; a series with no samples has none. */
function extendAll(
  previous: ReadonlyMap<string, Stroke>,
  series: readonly LineSeries[]
): ReadonlyMap<string, Stroke> {
  const next = new Map<string, Stroke>()

  for (const item of series) {
    const stroke = extendStroke(previous.get(item.id), item.points)

    if (stroke) {
      next.set(item.id, stroke)
    }
  }

  return next
}

/**
 * Maps each series to its drawn form: an append-only, lightly smoothed grid that
 * is extended tick by tick and never rewritten (see `extendStroke`), plus one
 * provisional tip at `now` (at most `maxLeadMs` past the newest sample) so the line
 * grows every frame rather than once per bucket. Hover, labels and headlines keep
 * reading the observed series; only the stroke, the endpoint mark and the value flag
 * follow this one.
 */
export function useStrokeSeries(
  series: readonly LineSeries[],
  now: number,
  enabled: boolean,
  maxLeadMs = TIP_MAX_LEAD_MS
): readonly LineSeries[] {
  const [cache, setCache] = useState<StrokeCache>(() => ({
    source: series,
    strokes: extendAll(new Map(), series),
  }))
  // The clock ticks every frame; only a changed series extends the grid. Deriving the
  // cache from the previous one during render keeps committed points across renders.
  const strokes = cache.source === series ? cache.strokes : extendAll(cache.strokes, series)

  if (cache.source !== series) {
    setCache({ source: series, strokes })
  }

  return useMemo(() => {
    if (!enabled) {
      return series
    }

    return series.map(item => {
      const stroke = strokes.get(item.id)

      if (!stroke) {
        return item
      }

      const tip = provisionalTip(stroke, now, STROKE_EMA_ALPHA, maxLeadMs)

      return { ...item, points: tip ? [...stroke.points, tip] : stroke.points }
    })
  }, [enabled, maxLeadMs, now, series, strokes])
}
