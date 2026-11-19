import { type RustSignalFrame, RustSignalsMessageSchema } from '@kyros/domain'
import type { ChartPoint } from '@kyros/ui/charts/lib/types'

import type { SignalFrames } from '@/features/signals/signal-catalogue'
import { appendHistory, type SignalHistory } from '@/features/signals/signal-history'
import { createSharedStream, createStreamRegistry } from './shared-stream'
import { openDemoSocket } from './socket'
import { exchangeSocketUrl } from './socket-url'

/** What one consumer wants from the socket: some ids or the whole series, with a window. */
export interface SignalsRequest {
  ids: readonly string[] | 'series'
  /** How much history to keep and to ask the server for on subscribe. */
  historyMs?: number
}

export interface SignalsState {
  status: 'connecting' | 'live' | 'disconnected'
  frames: SignalFrames
  history: SignalHistory
  /** True once the server answered the current subscription with its history. */
  historyLoaded: boolean
}

export interface CalmSignals {
  frames: SignalFrames
  history: SignalHistory
}

/** The trace a browser row draws when no consumer asked for more. */
export const HISTORY_WINDOW_MS = 60_000
/** The browser panel refreshes at most this often, so nothing on it flickers. */
export const CALM_INTERVAL_MS = 1000

export const IDLE_SIGNALS: SignalsState = {
  status: 'connecting',
  frames: {},
  history: {},
  historyLoaded: false,
}

interface Subscription {
  ids: readonly string[] | 'series'
  historyMs: number | undefined
}

/** The union of every consumer's request: everything if anyone browses, else the ids. */
function unionRequests(requests: Iterable<SignalsRequest>): Subscription {
  const ids = new Set<string>()
  let series = false
  let historyMs: number | undefined
  for (const request of requests) {
    if (request.ids === 'series') series = true
    else for (const id of request.ids) ids.add(id)
    if (request.historyMs !== undefined) historyMs = Math.max(historyMs ?? 0, request.historyMs)
  }
  return { ids: series ? 'series' : [...ids].toSorted(), historyMs }
}

function sameSubscription(a: Subscription, b: Subscription): boolean {
  if (a.historyMs !== b.historyMs) return false
  if (a.ids === 'series' || b.ids === 'series') return a.ids === b.ids
  return a.ids.length === b.ids.length && a.ids.every((id, index) => id === b.ids[index])
}

function subscribeFrame(subscription: Subscription): string {
  return JSON.stringify({
    kind: 'subscribe',
    ...(subscription.ids === 'series' ? { set: 'series' } : { ids: subscription.ids }),
    ...(subscription.historyMs === undefined ? {} : { history_ms: subscription.historyMs }),
  })
}

/** Insert retained frames into the history, keeping points sorted and unique by time. */
function mergeHistory(
  history: SignalHistory,
  signalId: string,
  frames: readonly RustSignalFrame[],
  windowMs: number
): SignalHistory {
  let appended: SignalHistory = {}
  for (const frame of frames)
    appended = appendHistory(appended, frame.signal_id, frame.ts_ms, frame.values, windowMs)
  const next: Record<string, readonly ChartPoint[]> = { ...history }
  for (const [key, points] of Object.entries(appended)) {
    if (!key.startsWith(`${signalId}:`)) continue
    const byTime = new Map<number, ChartPoint>()
    for (const point of points) byTime.set(point.time, point)
    for (const point of next[key] ?? []) byTime.set(point.time, point)
    const merged = [...byTime.values()].toSorted((a, b) => a.time - b.time)
    const newest = merged.at(-1)?.time ?? 0
    next[key] = merged.filter(point => point.time >= newest - windowMs)
  }
  return next
}

/**
 * One reconnecting socket per series to the exchange's `/ws/signals`, shared by the strip,
 * the browser panel and every chat widget on the page. Each consumer registers what it
 * wants; the store subscribes to the union and re-sends it whenever the union changes or
 * the socket reconnects. Frames merge into one latest-per-signal map, and every numeric
 * value is kept as a trace for the widest window any consumer asked for. A `history`
 * answer fills the trace before the first live frame. The calm snapshot republishes at
 * most once a second for the browser panel.
 */
function createSignalsStream(apiUrl: string, series: string, onRelease: () => void) {
  const requests = new Set<SignalsRequest>()
  let subscription = unionRequests(requests)
  let send: ((frame: string) => boolean) | undefined
  let calm: CalmSignals = { frames: {}, history: {} }
  let calmPublishedAt = Number.NEGATIVE_INFINITY
  let calmTimer: ReturnType<typeof setTimeout> | undefined
  const calmListeners = new Set<() => void>()

  const windowMs = () => Math.max(HISTORY_WINDOW_MS, subscription.historyMs ?? 0)

  const stream = createSharedStream<SignalsState>(
    IDLE_SIGNALS,
    (publish, current) => {
      const socket = openDemoSocket(exchangeSocketUrl(apiUrl, '/ws/signals', { series }), {
        opened: sendFrame => {
          send = sendFrame
          sendFrame(subscribeFrame(subscription))
          publish({ ...current(), status: 'live' })
        },
        message: payload => {
          const parsed = RustSignalsMessageSchema.safeParse(payload)
          if (!parsed.success || parsed.data.kind === 'error') return
          const state = current()
          if (parsed.data.kind === 'history') {
            let history = state.history
            for (const signal of parsed.data.signals)
              history = mergeHistory(history, signal.signal_id, signal.frames, windowMs())
            publish({ ...state, history, historyLoaded: true })
            return
          }
          const frames = { ...state.frames }
          let history = state.history
          for (const frame of parsed.data.frames) {
            frames[frame.signal_id] = frame
            history = appendHistory(history, frame.signal_id, frame.ts_ms, frame.values, windowMs())
          }
          publish({ ...state, status: 'live', frames, history })
        },
        disconnected: () => {
          send = undefined
          publish({ ...current(), status: 'disconnected' })
        },
      })
      return {
        close() {
          send = undefined
          socket.close()
        },
      }
    },
    onRelease
  )

  const publishCalm = () => {
    calmTimer = undefined
    calmPublishedAt = Date.now()
    const { frames, history } = stream.getSnapshot()
    calm = { frames, history }
    for (const listener of calmListeners) listener()
  }
  const scheduleCalm = () => {
    if (calmListeners.size === 0 || calmTimer !== undefined) return
    const due = calmPublishedAt + CALM_INTERVAL_MS - Date.now()
    if (due <= 0) publishCalm()
    else calmTimer = setTimeout(publishCalm, due)
  }
  const resubscribe = () => {
    const next = unionRequests(requests)
    if (sameSubscription(subscription, next)) return
    subscription = next
    stream.publish({ ...stream.getSnapshot(), historyLoaded: false })
    send?.(subscribeFrame(subscription))
  }

  return {
    getSnapshot: stream.getSnapshot,
    subscribe(listener: () => void) {
      const unsubscribe = stream.subscribe(() => {
        listener()
        scheduleCalm()
      })
      return unsubscribe
    },
    /** Add what a consumer wants; returns the function that withdraws it. */
    register(request: SignalsRequest): () => void {
      requests.add(request)
      resubscribe()
      return () => {
        requests.delete(request)
        resubscribe()
      }
    },
    /** The same data, republished at most once per second, for the browser panel. */
    subscribeCalm(listener: () => void) {
      calmListeners.add(listener)
      return () => {
        calmListeners.delete(listener)
        if (calmListeners.size === 0 && calmTimer !== undefined) {
          clearTimeout(calmTimer)
          calmTimer = undefined
        }
      }
    },
    getCalmSnapshot: () => calm,
  }
}

export type SignalsStream = ReturnType<typeof createSignalsStream>

const registry = createStreamRegistry<SignalsStream>((key, release) => {
  const separator = key.indexOf('|')
  return createSignalsStream(key.slice(0, separator), key.slice(separator + 1), release)
})

export function getSignalsStream(apiUrl: string, series: string): SignalsStream {
  return registry(`${apiUrl}|${series}`)
}
