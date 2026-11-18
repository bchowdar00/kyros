import type { ChartPoint } from '@tiny/ui/charts/lib/types'

import { type PinKey, pinKey } from './signal-catalogue'

/** Recent numeric samples per field, the trace a browser row or a chat chart draws. */
export type SignalHistory = Readonly<Record<PinKey, readonly ChartPoint[]>>

/**
 * Append one frame's numeric values to the history, dropping samples older than
 * `windowMs` before the frame's timestamp. Booleans and non-numeric strings are not
 * traces and are skipped; decimal strings are read as numbers for plotting only. A frame
 * at the newest sample's timestamp is the same sample re-sent (a snapshot after a history
 * answer) and replaces it; one older than the newest is out of order and is dropped.
 */
export function appendHistory(
  history: SignalHistory,
  signalId: string,
  ts_ms: number,
  values: Readonly<Record<string, string | number | boolean>>,
  windowMs: number
): SignalHistory {
  const next: Record<PinKey, readonly ChartPoint[]> = { ...history }
  const oldest = ts_ms - windowMs
  for (const [name, raw] of Object.entries(values)) {
    if (typeof raw === 'boolean') continue
    const value = Number(raw)
    if (!Number.isFinite(value)) continue
    const key = pinKey(signalId, name)
    const kept = (next[key] ?? []).filter(point => point.time >= oldest)
    const newest = kept.at(-1)
    if (newest !== undefined && newest.time > ts_ms) continue
    const base = newest?.time === ts_ms ? kept.slice(0, -1) : kept
    next[key] = [...base, { time: ts_ms, value }]
  }
  return next
}
