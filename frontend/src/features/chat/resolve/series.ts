import type { RustSignalCatalogue } from '@tiny/domain'

import type { Ref } from '../specs'

// What a series is made of, shared by the widget tool on the server and the bindings in the
// browser. Nothing here touches React or a socket, so it stays importable from both.

/** The per-series internals the exchange publishes, by the kind segment of their ids. */
export function seriesSignalId(series: string, kind: 'strike' | 'price' | 'window' | 'stats') {
  return `${series}.current.${kind}.v1`
}

/** The underlying whose rounds have a BTC spot and a strike. */
export const BTC_UNDERLYING = 'btc'

/**
 * The field of the series' price signal that carries the quoted side's probability:
 * `up` on a BTC series, `home` on an NBA one. The registry lists it first.
 */
export function quotedField(catalogue: Pick<RustSignalCatalogue, 'signals'>, series: string) {
  const price = catalogue.signals.find(entry => entry.signal_id === seriesSignalId(series, 'price'))
  return price?.fields[0]?.name ?? 'up'
}

/** Why a BTC-only reference cannot be drawn on a series of another underlying. */
export function refuseNonBtc(ref: Ref, underlying: string | undefined): string | undefined {
  if (underlying === undefined || underlying === BTC_UNDERLYING) return undefined
  if (ref.type === 'spot') return 'This series has no BTC price; its rounds are not about BTC.'
  if (ref.type === 'strike') return 'This series has no strike; its rounds settle on a game.'
  return undefined
}
