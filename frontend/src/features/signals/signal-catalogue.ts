import type { RustSignalCatalogue, RustSignalFrame } from '@kyros/domain'

/** A pinned card is one field of one signal. */
export type PinKey = `${string}:${string}`

export function pinKey(signalId: string, field: string): PinKey {
  return `${signalId}:${field}`
}

function statsSignalId(series: string): string {
  return `${series}.current.stats.v1`
}

/** Round volume and open interest in dollars, the Pyth BTC spot, and 10 s signed flow. */
export function defaultPins(series: string): PinKey[] {
  const stats = statsSignalId(series)
  return [
    pinKey(stats, 'volume'),
    pinKey(stats, 'collateral'),
    pinKey('pyth.btc_usd.spot.v1', 'last'),
    pinKey('btc.external.aggressor_flow.v1', 'signed_notional_usd_10s'),
  ]
}

/** Latest frame per signal id, as the stream store publishes it. */
export type SignalFrames = Readonly<Record<string, RustSignalFrame>>

/** The catalogue with every field value, quality and timestamp replaced by its latest frame. */
export function applyFrames(
  catalogue: RustSignalCatalogue,
  frames: SignalFrames
): RustSignalCatalogue {
  return {
    ...catalogue,
    signals: catalogue.signals.map(signal => {
      const frame = frames[signal.signal_id]
      if (frame === undefined) return signal
      return {
        ...signal,
        quality: frame.quality,
        ts_ms: frame.ts_ms,
        fields: signal.fields.map(field => {
          const value = frame.values[field.name]
          return value === undefined ? field : { ...field, value }
        }),
      }
    }),
  }
}
