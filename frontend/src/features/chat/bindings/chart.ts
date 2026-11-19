'use client'

import type { RustSignal, RustSignalCatalogue } from '@kyros/domain'

import { getPriceStream, IDLE_PRICE_STREAM } from '@/features/exchange-streams/price-stream'
import { useSignalsStream } from '@/features/exchange-streams/use-signals-stream'
import { useSignalCatalogue } from '@/features/market-detail/use-signal-catalogue'
import { pinnedCardLabel } from '@/features/signals/signal-cards'
import { pinKey } from '@/features/signals/signal-catalogue'
import { fieldPresentation } from '@/features/signals/signal-presentation'
import { type ChartContext, resolveChart, type ResolvedLine } from '../resolve/chart'
import { quotedField, refuseNonBtc } from '../resolve/series'
import { resolveCandles } from '../resolve/tables'
import type { CandlesSpec, ChartSpec, Ref } from '../specs'
import { WINDOW_MS_DEFAULT } from '../specs/bound'
import type { CandleChartProps } from '../widgets/candle-chart'
import type { TimeSeriesProps } from '../widgets/time-series'
import { type Binding, LOADING, seriesSignalId, unavailable, useStream } from './binding'
import type { ChatData } from './chat-data'

const MARKET_LABELS = { bid: 'Best bid', ask: 'Best ask', last: 'Last trade' } as const
const SPOT_SIGNAL_SUFFIX = '.btc_usd.spot.v1'

/**
 * Which catalogued signal and field a reference reads; `spot` reads the price stream. The
 * market's `up` measure is the quoted side, whatever the series calls it.
 */
function signalOf(
  ref: Ref,
  series: string,
  catalogue: Pick<RustSignalCatalogue, 'signals'>
): { id: string; field: string } | undefined {
  switch (ref.type) {
    case 'signal':
      return { id: ref.id, field: ref.field }
    case 'market':
      return {
        id: seriesSignalId(series, 'price'),
        field: ref.measure === 'up' ? quotedField(catalogue, series) : ref.measure,
      }
    case 'strike':
      return { id: seriesSignalId(series, 'strike'), field: 'price' }
    default:
      return undefined
  }
}

function labelOf(ref: Ref, signal: RustSignal | undefined, field: string): string {
  switch (ref.type) {
    case 'spot':
      return 'BTC/USD'
    case 'strike':
      return 'Strike'
    case 'market':
      return ref.measure === 'up'
        ? (signal?.fields.find(f => f.name === field)?.label ?? 'YES')
        : MARKET_LABELS[ref.measure]
    default:
      return signal === undefined
        ? `${ref.id}:${ref.field}`
        : pinnedCardLabel(signal, signal.fields.find(f => f.name === field)?.label ?? field)
  }
}

/** A reference the catalogue cannot serve, with the reason a frame can show. */
function refuse(ref: Ref, catalogue: RustSignalCatalogue, series: string): string | undefined {
  const nonBtc = refuseNonBtc(ref, catalogue.underlying)
  if (nonBtc !== undefined) return nonBtc
  const target = signalOf(ref, series, catalogue)
  if (target === undefined) return undefined
  const signal = catalogue.signals.find(entry => entry.signal_id === target.id)
  if (signal === undefined) return `${target.id} is not a signal of ${series}.`
  const field = signal.fields.find(entry => entry.name === target.field)
  if (field === undefined) return `${signal.name} has no field named ${target.field}.`
  if (!signal.live) return `${signal.name} is offline right now.`
  return undefined
}

/** One or more references over a window, from the signals socket and the price stream. */
export function useChartBinding(spec: ChartSpec, data: ChatData): Binding<TimeSeriesProps> {
  const windowMs = spec.window_ms ?? WINDOW_MS_DEFAULT
  const { data: catalogue } = useSignalCatalogue(data.apiUrl, spec.series)
  const known = catalogue ?? { signals: [] }
  const ids = [
    ...new Set(
      spec.refs.flatMap(ref => {
        const target = signalOf(ref, spec.series, known)
        return target === undefined ? [] : [target.id]
      })
    ),
  ]
  const signals = useSignalsStream(data.apiUrl, spec.series, ids, windowMs)
  const price = useStream(getPriceStream(data.apiUrl, spec.series), IDLE_PRICE_STREAM)
  const wantsSpot = spec.refs.some(ref => ref.type === 'spot')

  if (catalogue === undefined) return LOADING
  if (catalogue === null) return unavailable('The signal catalogue could not be read.')
  for (const ref of spec.refs) {
    const reason = refuse(ref, catalogue, spec.series)
    if (reason !== undefined) return unavailable(reason)
  }

  const lines: ResolvedLine[] = spec.refs.map(ref => {
    if (ref.type === 'spot') {
      return {
        ref,
        label: 'BTC/USD',
        points: price.points,
        presentation: fieldPresentation(`pyth${SPOT_SIGNAL_SUFFIX}`, {
          name: 'last',
          type: 'decimal',
        }),
      }
    }
    const target = signalOf(ref, spec.series, catalogue)
    const signal =
      target === undefined
        ? undefined
        : catalogue.signals.find(entry => entry.signal_id === target.id)
    const field = target?.field ?? ''
    const type = signal?.fields.find(entry => entry.name === field)?.type ?? 'decimal'
    return {
      ref,
      label: labelOf(ref, signal, field),
      points: target === undefined ? [] : (signals.history[pinKey(target.id, field)] ?? []),
      presentation: fieldPresentation(target?.id ?? '', { name: field, type }),
    }
  })

  const waitingOnSignals = ids.length > 0 && !signals.historyLoaded
  const waitingOnSpot = wantsSpot && price.feed.state !== 'live' && price.points.length === 0
  if (waitingOnSignals || waitingOnSpot) return LOADING
  if (lines.every(line => line.points.length === 0)) return LOADING

  const context: ChartContext = {
    timeZone: data.timeZone,
    strike: price.round?.strike,
    roundOpenedAt: price.round?.opensAt,
  }
  const dropped =
    signals.status === 'disconnected' || (wantsSpot && price.feed.state === 'disconnected')
  return { status: dropped ? 'stale' : 'live', props: resolveChart(spec, lines, context) }
}

/** BTC spot as candles from the price stream, with the round's strike. */
export function useCandlesBinding(spec: CandlesSpec, data: ChatData): Binding<CandleChartProps> {
  const price = useStream(getPriceStream(data.apiUrl, spec.series), IDLE_PRICE_STREAM)
  if (price.points.length === 0) {
    return price.feed.state === 'disconnected'
      ? unavailable('The price feed is disconnected.')
      : LOADING
  }
  const props = resolveCandles(price.points, {
    windowMs: spec.window_ms,
    bucketMs: spec.bucket_ms,
    strike: price.round?.strike,
    timeZone: data.timeZone,
    title: spec.title,
  })
  return { status: price.feed.state === 'disconnected' ? 'stale' : 'live', props }
}
