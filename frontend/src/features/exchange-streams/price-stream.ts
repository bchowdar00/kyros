import { RustPriceFrameSchema } from '@kyros/domain'
import type { ChartFeed, ChartPoint } from '@kyros/ui/charts/lib/types'

import { createSharedStream, createStreamRegistry, type SharedStream } from './shared-stream'
import { openDemoSocket } from './socket'
import { exchangeSocketUrl } from './socket-url'

// The exchange emits one tick per 200ms; a 15-minute round is 4,500 and its snapshot is
// capped at 6,000, so this never drops the round's opening ticks.
const MAX_TICKS = 6_000

export interface PriceStreamState {
  feed: ChartFeed
  points: readonly ChartPoint[]
  /** The open round, from the exchange's `/ws/price` snapshot and resets. `strike` is
   * `undefined` for a series whose rounds have none. */
  round?: { marketId: number; strike?: number; opensAt: number; closesAt: number }
}

export const IDLE_PRICE_STREAM: PriceStreamState = { feed: { state: 'paused' }, points: [] }

/**
 * One reconnecting socket per series to the exchange's `/ws/price`. The first frame carries
 * the round and its ticks plus a bounded run-up before the open; ticks then append and a
 * `market_reset` swaps the round while the price history carries on: BTC's price before the
 * open is still BTC's price, so the line never restarts from nothing.
 */
function createPriceStream(
  apiUrl: string,
  series: string,
  onRelease: () => void
): SharedStream<PriceStreamState> {
  return createSharedStream<PriceStreamState>(
    IDLE_PRICE_STREAM,
    (publish, current) => {
      const socket = openDemoSocket(exchangeSocketUrl(apiUrl, '/ws/price', { series }), {
        opened: () => undefined,
        message: payload => {
          const parsed = RustPriceFrameSchema.safeParse(payload)
          if (!parsed.success) return
          const frame = parsed.data
          const snapshot = current()
          if (frame.type === 'snapshot') {
            const points = frame.ticks.map(tick => ({ time: tick.ts_ms, value: tick.price }))
            const round =
              frame.market_id !== null && frame.opens_at_ms !== null && frame.closes_at_ms !== null
                ? {
                    marketId: frame.market_id,
                    ...(frame.strike === null ? {} : { strike: frame.strike }),
                    opensAt: frame.opens_at_ms,
                    closesAt: frame.closes_at_ms,
                  }
                : undefined
            publish({
              feed: { state: 'live' },
              points: points.slice(-MAX_TICKS),
              ...(round === undefined ? {} : { round }),
            })
            return
          }
          if (frame.type === 'market_reset') {
            if (frame.series !== series) return
            publish({
              feed: { state: 'live' },
              points: snapshot.points,
              round: {
                marketId: frame.market_id,
                // A zero strike is the exchange's "no strike".
                ...(frame.strike === 0 ? {} : { strike: frame.strike }),
                opensAt: frame.opens_at_ms,
                closesAt: frame.closes_at_ms,
              },
            })
            return
          }
          const last = snapshot.points.at(-1)
          if (last && frame.ts_ms <= last.time) return
          publish({
            ...snapshot,
            feed: { state: 'live' },
            points: [...snapshot.points, { time: frame.ts_ms, value: frame.price }].slice(
              -MAX_TICKS
            ),
          })
        },
        disconnected: () => {
          publish({ ...current(), feed: { state: 'disconnected' } })
        },
      })
      return { close: () => socket.close() }
    },
    onRelease
  )
}

const registry = createStreamRegistry<SharedStream<PriceStreamState>>((key, release) => {
  const separator = key.indexOf('|')
  return createPriceStream(key.slice(0, separator), key.slice(separator + 1), release)
})

export function getPriceStream(apiUrl: string, series: string) {
  return registry(`${apiUrl}|${series}`)
}
