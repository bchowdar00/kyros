import type { VenueLine } from '@kyros/ui/charts/venue-lines'

import { type PinKey, pinKey } from '@/features/signals/signal-catalogue'
import type { SignalHistory } from '@/features/signals/signal-history'

interface VenuePin {
  id: string
  /** The venue's name, as its line is labelled. */
  label: string
  /** The signal field that carries the venue's price for the first outcome, 0 to 1. */
  pin: PinKey
}

const POLYMARKET_BTC: VenuePin = {
  id: 'polymarket',
  label: 'Polymarket',
  pin: pinKey('btc.polymarket.prediction_quote.v1', 'affirmative_midpoint'),
}

/**
 * Which other venues price the same thing a series trades, and where each price arrives.
 * Polymarket's Bitcoin market is a fifteen-minute window, so it is the same question as the
 * fifteen-minute round and not as the five-minute one, which gets no line.
 */
const VENUES: Readonly<Record<string, readonly VenuePin[]>> = {
  'btc-15m': [POLYMARKET_BTC],
  'nba-h2h': [
    { id: 'polymarket', label: 'Polymarket', pin: pinKey('nba.live.polymarket.v1', 'home_prob') },
    { id: 'kalshi', label: 'Kalshi', pin: pinKey('nba.live.kalshi.v1', 'home_prob') },
  ],
}

const PERCENT = 100
const DECIMALS = 2

/** The venues with a line on a series' chart; none for a series nothing else prices. */
export function venuePins(series: string): readonly VenuePin[] {
  return VENUES[series] ?? []
}

/** The signals those prices arrive on, for the subscription. */
export function venueSignalIds(series: string): string[] {
  return venuePins(series).map(venue => venue.pin.slice(0, venue.pin.lastIndexOf(':')))
}

/** The most readings one held line is drawn from, however long its window. */
const MOST_READINGS = 720

/**
 * Readings kept on the page for longer than the shared signals socket keeps them: each
 * venue's new readings are added to what is already held, and ones older than `windowMs`
 * before the newest are dropped. A long window is thinned to `MOST_READINGS` evenly spaced
 * readings, the newest always among them. The socket's own window is left alone, because
 * every signal tile on the page draws from it.
 */
export function extendVenueHistory(
  held: SignalHistory,
  incoming: SignalHistory,
  series: string,
  windowMs: number
): SignalHistory {
  const next: Record<PinKey, readonly { time: number; value: number }[]> = {}
  let changed = false
  // A long window is thinned as it fills: a line drawn every second need not keep every second.
  const step = windowMs / MOST_READINGS
  for (const venue of venuePins(series)) {
    const before = held[venue.pin]
    const newest = before?.at(-1)?.time ?? Number.NEGATIVE_INFINITY
    const added = (incoming[venue.pin] ?? []).filter(point => point.time > newest)
    const all = [...(before ?? [])]
    for (const point of added) {
      const last = all.at(-1)
      const anchor = all.at(-2)
      // The newest reading stands in for the last one until that one is a full step old.
      if (last !== undefined && anchor !== undefined && last.time - anchor.time < step) {
        all[all.length - 1] = point
      } else {
        all.push(point)
      }
    }
    const oldest = (all.at(-1)?.time ?? 0) - windowMs
    const kept = all.filter(point => point.time >= oldest)
    changed ||= before === undefined || added.length > 0 || kept.length !== all.length
    next[venue.pin] = kept
  }
  // Nothing new and nothing aged out: the same object, so whoever holds it need not redraw.
  return changed ? next : held
}

/**
 * Other venues' prices for a series' first outcome, in percent, for the chart.
 * A venue with fewer than two readings has no line to draw, and one whose
 * readings are not all prices between 0 and 1 is left out whole: a wrong
 * line is worse than a missing one.
 */
export function venueLinesFor(series: string, history: SignalHistory): VenueLine[] {
  const lines: VenueLine[] = []
  for (const venue of venuePins(series)) {
    const readings = history[venue.pin] ?? []
    if (readings.length < 2) continue
    if (readings.some(point => point.value < 0 || point.value > 1)) continue
    lines.push({
      id: venue.id,
      label: venue.label,
      points: readings.map(point => ({
        time: point.time,
        value: Number((point.value * PERCENT).toFixed(DECIMALS)),
      })),
    })
  }
  return lines
}
