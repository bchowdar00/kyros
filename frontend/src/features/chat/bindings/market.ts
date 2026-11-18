'use client'

import { getPositionsStream, IDLE_POSITIONS } from '@/features/exchange-streams/positions-stream'
import { useSignalsStream } from '@/features/exchange-streams/use-signals-stream'
import { useSignalCatalogue } from '@/features/market-detail/use-signal-catalogue'
import type { PositionCardProps } from '../presets/position-card'
import { resolveMarketCard, resolvePositionCard, resolveStats } from '../resolve/cards'
import type { MarketSpec, PositionSpec, StatsSpec } from '../specs'
import type { CardProps } from '../widgets/card'
import type { StatGridProps } from '../widgets/stat-grid'
import {
  type Binding,
  LOADING,
  remainingMs,
  seriesSignalId,
  unavailable,
  useStream,
} from './binding'
import type { ChatData } from './chat-data'

const SPOT_SIGNAL_SUFFIX = '.btc_usd.spot.v1'

/** The live spot feed's id, whichever source the exchange runs with. */
function spotSignalId(signals: readonly { signal_id: string; live: boolean }[]) {
  return signals.find(signal => signal.signal_id.endsWith(SPOT_SIGNAL_SUFFIX) && signal.live)
    ?.signal_id
}

/** The round at a glance from the series internals and the spot feed. */
export function useMarketBinding(spec: MarketSpec, data: ChatData): Binding<CardProps> {
  const { data: catalogue } = useSignalCatalogue(data.apiUrl, spec.series)
  const spot = catalogue ? spotSignalId(catalogue.signals) : undefined
  const ids = [
    seriesSignalId(spec.series, 'strike'),
    seriesSignalId(spec.series, 'price'),
    seriesSignalId(spec.series, 'window'),
    seriesSignalId(spec.series, 'stats'),
    ...(spot === undefined ? [] : [spot]),
  ]
  const signals = useSignalsStream(data.apiUrl, spec.series, ids)
  if (catalogue === undefined) return LOADING
  if (catalogue === null) return unavailable('The signal catalogue could not be read.')
  if (spot === undefined) return unavailable('The BTC price feed is offline right now.')
  const card = resolveMarketCard(spec.series, {
    strike: signals.frames[ids[0] ?? ''],
    price: signals.frames[ids[1] ?? ''],
    window: signals.frames[ids[2] ?? ''],
    stats: signals.frames[ids[3] ?? ''],
    spot: signals.frames[spot],
  })
  if (card === undefined) {
    return signals.status === 'disconnected'
      ? unavailable('The signals socket is disconnected.')
      : LOADING
  }
  const props = spec.title === undefined ? card : { ...card, eyebrow: spec.title }
  return { status: signals.status === 'disconnected' ? 'stale' : 'live', props }
}

/** The round's activity from the stats signal. */
export function useStatsBinding(spec: StatsSpec, data: ChatData): Binding<StatGridProps> {
  const id = seriesSignalId(spec.series, 'stats')
  const signals = useSignalsStream(data.apiUrl, spec.series, [id])
  const frame = signals.frames[id]
  if (frame === undefined) {
    return signals.status === 'disconnected'
      ? unavailable('The signals socket is disconnected.')
      : LOADING
  }
  const grid = resolveStats(spec.series, frame)
  const props = spec.title === undefined ? grid : { ...grid, title: spec.title }
  return { status: signals.status === 'disconnected' ? 'stale' : 'live', props }
}

/** The user's position in the series from the positions socket, with the round clock. */
export function usePositionBinding(spec: PositionSpec, data: ChatData): Binding<PositionCardProps> {
  const windowId = seriesSignalId(spec.series, 'window')
  const signals = useSignalsStream(data.apiUrl, spec.series, [windowId])
  const positions = useStream(
    getPositionsStream(data.apiUrl, data.address ?? '', spec.series),
    IDLE_POSITIONS
  )
  if (data.address === null) return unavailable('Sign in to see your position.')
  if (positions.status === 'unknown_account')
    return unavailable('The exchange does not know this account.')
  if (positions.snapshot === undefined) {
    return positions.status === 'disconnected'
      ? unavailable('The positions socket is disconnected.')
      : LOADING
  }
  const props = resolvePositionCard(
    spec.series,
    positions.snapshot,
    remainingMs(signals.frames[windowId]?.values)
  )
  if (props === undefined) return unavailable(`No position row for ${spec.series}.`)
  return { status: positions.status === 'disconnected' ? 'stale' : 'live', props }
}
