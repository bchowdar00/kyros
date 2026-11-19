import { type RustTradeRow, RustTradesFrameSchema } from '@kyros/domain'

import { createSharedStream, createStreamRegistry, type SharedStream } from './shared-stream'
import { openDemoSocket } from './socket'
import { exchangeSocketUrl } from './socket-url'

/** Enough for a fill log and a tape; a busy round prints a few hundred taker orders. */
export const MAX_TRADES = 200

export interface TradesState {
  status: 'connecting' | 'live' | 'disconnected'
  /** Newest first. */
  trades: readonly RustTradeRow[]
}

export const IDLE_TRADES: TradesState = { status: 'connecting', trades: [] }

function tradeKey(trade: RustTradeRow) {
  return `${trade.ts}:${trade.price}:${trade.qty}:${trade.side}:${trade.taker.address}`
}

/**
 * One socket per series to `/ws/trades`: a snapshot on connect, then one batch per taker
 * order. Rows are deduplicated on their content, since a reconnect replays the snapshot.
 */
function createTradesStream(
  apiUrl: string,
  series: string,
  onRelease: () => void
): SharedStream<TradesState> {
  return createSharedStream<TradesState>(
    IDLE_TRADES,
    (publish, current) => {
      const socket = openDemoSocket(exchangeSocketUrl(apiUrl, '/ws/trades', { series }), {
        opened: () => undefined,
        message: payload => {
          const parsed = RustTradesFrameSchema.safeParse(payload)
          if (!parsed.success) return
          const seen = new Set<string>()
          const merged: RustTradeRow[] = []
          for (const trade of [...parsed.data.trades, ...current().trades]) {
            const key = tradeKey(trade)
            if (seen.has(key)) continue
            seen.add(key)
            merged.push(trade)
          }
          merged.sort((a, b) => b.ts - a.ts)
          publish({ status: 'live', trades: merged.slice(0, MAX_TRADES) })
        },
        disconnected: () => publish({ ...current(), status: 'disconnected' }),
      })
      return { close: () => socket.close() }
    },
    onRelease
  )
}

const registry = createStreamRegistry<SharedStream<TradesState>>((key, release) => {
  const separator = key.indexOf('|')
  return createTradesStream(key.slice(0, separator), key.slice(separator + 1), release)
})

export function getTradesStream(apiUrl: string, series: string) {
  return registry(`${apiUrl}|${series}`)
}
