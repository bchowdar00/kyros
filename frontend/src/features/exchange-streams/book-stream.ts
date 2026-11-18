import { type RustBookFrame, RustBookFrameSchema } from '@tiny/domain'
import type { ChartPoint } from '@tiny/ui/charts/lib/types'

import { createSharedStream, createStreamRegistry, type SharedStream } from './shared-stream'
import { openDemoSocket } from './socket'
import { exchangeSocketUrl } from './socket-url'

/** Ten tenths of a cent per percentage point: a 450-tenth midpoint is 45%. */
const TENTHS_PER_PERCENT = 10
/** Matches the exchange's per-round cap; a busier round is trimmed from the start. */
const MAX_MID_POINTS = 10_000

export interface BookState {
  status: 'connecting' | 'live' | 'disconnected'
  /** Best first each side, prices in tenths of a cent on the tick grid. */
  bids: RustBookFrame['bids']
  asks: RustBookFrame['asks']
  /** The open round the book belongs to; null before the first round opens. */
  marketId: number | null
  /** The YES midpoint the book implies, as a percentage; null on an empty book. */
  mid: number | null
  /** The round's YES midpoint over time, as percentages, oldest first. */
  midPoints: readonly ChartPoint[]
  /** When the last frame arrived, unix milliseconds; 0 before the first. */
  receivedAt: number
}

export const IDLE_BOOK: BookState = {
  status: 'connecting',
  bids: [],
  asks: [],
  marketId: null,
  mid: null,
  midPoints: [],
  receivedAt: 0,
}

const percent = (tenths: number) => tenths / TENTHS_PER_PERCENT

/**
 * The midpoint line after a frame. A join frame carries the round's history and replaces
 * the line; a new round starts it over; otherwise the frame's midpoint appends when it
 * moved. An empty book prints nothing, so the line holds its last value.
 */
export function nextMidPoints(
  previous: Pick<BookState, 'marketId' | 'midPoints'>,
  frame: Pick<RustBookFrame, 'market_id' | 'mid' | 'mid_history'>,
  receivedAt: number
): readonly ChartPoint[] {
  if (frame.mid_history !== undefined) {
    return frame.mid_history
      .map(point => ({ time: point.ts_ms, value: percent(point.mid) }))
      .slice(-MAX_MID_POINTS)
  }
  const marketId = frame.market_id ?? null
  const carried = marketId === previous.marketId ? previous.midPoints : []
  if (frame.mid === undefined || frame.mid === null) return carried
  const value = percent(frame.mid)
  const last = carried.at(-1)
  if (last !== undefined && last.value === value) return carried
  return [...carried, { time: receivedAt, value }].slice(-MAX_MID_POINTS)
}

export function applyBookFrame(
  previous: BookState,
  frame: RustBookFrame,
  receivedAt: number
): BookState {
  return {
    status: 'live',
    bids: frame.bids,
    asks: frame.asks,
    marketId: frame.market_id ?? null,
    mid: frame.mid === undefined || frame.mid === null ? null : percent(frame.mid),
    midPoints: nextMidPoints(previous, frame, receivedAt),
    receivedAt,
  }
}

/**
 * One socket per series to `/ws/book`. Every frame is the whole visible book, so the
 * newest frame replaces the last; there is no sequence to chain. The midpoint line is the
 * one thing that accumulates, seeded by the join frame's history.
 */
function createBookStream(
  apiUrl: string,
  series: string,
  onRelease: () => void
): SharedStream<BookState> {
  return createSharedStream<BookState>(
    IDLE_BOOK,
    (publish, current) => {
      const socket = openDemoSocket(exchangeSocketUrl(apiUrl, '/ws/book', { series }), {
        opened: () => undefined,
        message: payload => {
          const parsed = RustBookFrameSchema.safeParse(payload)
          if (!parsed.success) return
          publish(applyBookFrame(current(), parsed.data, Date.now()))
        },
        disconnected: () => publish({ ...current(), status: 'disconnected' }),
      })
      return { close: () => socket.close() }
    },
    onRelease
  )
}

const registry = createStreamRegistry<SharedStream<BookState>>((key, release) => {
  const separator = key.indexOf('|')
  return createBookStream(key.slice(0, separator), key.slice(separator + 1), release)
})

export function getBookStream(apiUrl: string, series: string) {
  return registry(`${apiUrl}|${series}`)
}
