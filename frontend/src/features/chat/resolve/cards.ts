import type { RustPositionFrame, RustSignalFrame } from '@tiny/domain'
import { cents, createMoneyFormatter } from '@tiny/money'

import type { CardProps } from '../widgets/card'
import type { Stat, StatGridProps } from '../widgets/stat-grid'
import type { PositionCardProps } from '../presets/position-card'

const money = createMoneyFormatter('en-US')
const CENTS_PER_UNIT = 100
const TENTHS_PER_CENT = 10

/** The series-internal frames a card reads, each absent until its signal first publishes. */
export interface SeriesFrames {
  strike?: RustSignalFrame | undefined
  price?: RustSignalFrame | undefined
  window?: RustSignalFrame | undefined
  stats?: RustSignalFrame | undefined
  spot?: RustSignalFrame | undefined
}

function decimal(frame: RustSignalFrame | undefined, field: string): number | undefined {
  const raw = frame?.values[field]
  if (raw === undefined || typeof raw === 'boolean') return undefined
  const value = Number(raw)
  return Number.isFinite(value) ? value : undefined
}

/** `"88650.12"` → 8865012 cents, read lexically so no float carries money. */
function dollarsToCents(text: string | number | boolean | undefined): number | undefined {
  if (text === undefined || typeof text === 'boolean') return undefined
  const match = /^(-?)(\d+)(?:\.(\d*))?$/.exec(String(text))
  if (match === null) return undefined
  const [, sign, whole = '0', fraction = ''] = match
  const value = Number(whole) * CENTS_PER_UNIT + Number(`${fraction}00`.slice(0, 2))
  return sign === '-' ? -value : value
}

/** A YES probability on the wire (`"0.435"`) as cents on the dollar, to a tenth. */
function probabilityToCents(frame: RustSignalFrame | undefined, field: string) {
  const value = decimal(frame, field)
  return value === undefined ? undefined : Math.round(value * CENTS_PER_UNIT * 10) / 10
}

export function countdown(seconds: number): string {
  const whole = Math.max(0, Math.floor(seconds))
  return `${Math.floor(whole / 60)}:${String(whole % 60).padStart(2, '0')}`
}

/**
 * The round at a glance: spot against the strike as the hero, then YES, the spread, the
 * clock, volume and open interest. Fair value is not a live source yet, so no edge.
 */
export function resolveMarketCard(series: string, frames: SeriesFrames): CardProps | undefined {
  const strikeCents = dollarsToCents(frames.strike?.values['price'])
  const spotCents = dollarsToCents(frames.spot?.values['last'])
  if (strikeCents === undefined || spotCents === undefined) return undefined
  const yesCents = probabilityToCents(frames.price, 'up')
  const bidCents = probabilityToCents(frames.price, 'bid')
  const askCents = probabilityToCents(frames.price, 'ask')
  const remainingMs = decimal(frames.window, 'remaining')
  const volumeCents = dollarsToCents(frames.stats?.values['volume'])
  const openInterest = decimal(frames.stats, 'open_interest')
  const spread =
    bidCents !== undefined && askCents !== undefined
      ? Math.round((askCents - bidCents) * 10) / 10
      : undefined
  const stats: Stat[] = [
    { id: 'strike', label: 'Strike', value: money.usd(cents(strikeCents)) },
    { id: 'yes', label: 'YES', value: yesCents === undefined ? '—' : `${yesCents}¢` },
    { id: 'spread', label: 'Spread', value: spread === undefined ? '—' : `${spread}¢` },
    {
      id: 'clock',
      label: 'Time left',
      value: remainingMs === undefined ? '—' : countdown(remainingMs / 1000),
    },
    {
      id: 'volume',
      label: 'Volume',
      value: volumeCents === undefined ? '—' : money.usd(cents(volumeCents)),
    },
    {
      id: 'oi',
      label: 'Open interest',
      value: openInterest === undefined ? '—' : `${money.count(openInterest)} lots`,
    },
  ]
  return {
    label: `${series} market`,
    eyebrow: `${series} binary`,
    ...(remainingMs === undefined ? {} : { eyebrowMeta: `T−${countdown(remainingMs / 1000)}` }),
    hero: {
      value: money.usd(cents(spotCents)),
      delta: money.usd(cents(spotCents - strikeCents), { signDisplay: 'always' }),
    },
    stats,
  }
}

/** The round's activity from the stats signal alone. */
export function resolveStats(series: string, stats: RustSignalFrame): StatGridProps {
  const dollars = (field: string) => {
    const value = dollarsToCents(stats.values[field])
    return value === undefined ? '—' : money.usd(cents(value))
  }
  const count = (field: string) => {
    const value = decimal(stats, field)
    return value === undefined ? '—' : money.count(value)
  }
  return {
    label: `${series} round stats`,
    title: 'This round',
    columns: 3,
    stats: [
      { id: 'volume', label: 'Volume', value: dollars('volume') },
      { id: 'oi', label: 'Open interest', value: `${count('open_interest')} lots` },
      { id: 'collateral', label: 'Collateral', value: dollars('collateral') },
      { id: 'trades', label: 'Trades', value: count('trades') },
      { id: 'traders', label: 'Traders', value: count('traders') },
      { id: 'largest', label: 'Largest trade', value: `${count('largest_trade')} lots` },
    ],
  }
}

/** The user's exposure in one series from a position snapshot's row for it. */
export function resolvePositionCard(
  series: string,
  snapshot: RustPositionFrame,
  remainingMs: number | undefined
): PositionCardProps | undefined {
  const row =
    snapshot.series === series
      ? snapshot
      : snapshot.series_positions.find(position => position.series === series)
  if (row === undefined) return undefined
  return {
    series,
    contracts: row.signed_position,
    entryCents:
      row.avg_entry_yes_tenths === null
        ? 0
        : Math.round(row.avg_entry_yes_tenths / TENTHS_PER_CENT),
    markCents: row.mark_yes_tenths === null ? 0 : Math.round(row.mark_yes_tenths / TENTHS_PER_CENT),
    lockedCents: Math.round(Math.abs(row.cost_basis_tenths) / TENTHS_PER_CENT),
    secondsRemaining: remainingMs === undefined ? 0 : remainingMs / 1000,
  }
}

export { dollarsToCents, probabilityToCents }
