import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

import { FakeWebSocket } from './fake-web-socket.fixture'
import { RELEASE_DELAY_MS } from './shared-stream'
import { CALM_INTERVAL_MS, getSignalsStream, HISTORY_WINDOW_MS } from './signals-stream'

const frame = (signal_id: string, ts_ms: number, values: Record<string, string | number>) => ({
  signal_id,
  ts_ms,
  quality: { state: 'ready', reasons: [] },
  values,
})

let series = 0

describe('getSignalsStream', () => {
  beforeEach(() => {
    vi.useFakeTimers()
    vi.stubGlobal('WebSocket', FakeWebSocket)
    series += 1
  })
  afterEach(() => {
    vi.runOnlyPendingTimers()
    vi.useRealTimers()
    FakeWebSocket.reset()
    vi.unstubAllGlobals()
  })

  it('shares one socket, subscribes to the union of requests, and merges frames', () => {
    const stream = getSignalsStream('http://localhost:8080', `s${series}`)
    expect(FakeWebSocket.instances).toHaveLength(0)
    const seen: string[] = []
    const stop = stream.subscribe(() => seen.push(stream.getSnapshot().status))
    const socket = FakeWebSocket.instances[0]
    expect(socket?.url).toBe(`ws://localhost:8080/ws/signals?series=s${series}`)

    const withdrawStrip = stream.register({ ids: ['a.v1'] })
    socket?.open()
    expect(socket?.sent).toEqual([JSON.stringify({ kind: 'subscribe', ids: ['a.v1'] })])
    expect(seen.at(-1)).toBe('live')

    // A second consumer widens the subscription; an identical request sends nothing.
    const withdrawChart = stream.register({ ids: ['b.v1', 'a.v1'], historyMs: 240_000 })
    expect(socket?.sent.at(-1)).toBe(
      JSON.stringify({ kind: 'subscribe', ids: ['a.v1', 'b.v1'], history_ms: 240_000 })
    )
    const withdrawTwin = stream.register({ ids: ['a.v1'] })
    expect(socket?.sent).toHaveLength(2)

    socket?.emit({ kind: 'snapshot', frames: [frame('a.v1', 1, { x: '1' })] })
    socket?.emit({
      kind: 'batch',
      frames: [frame('b.v1', 2, { y: 2 }), frame('a.v1', 3, { x: '3' })],
    })
    socket?.emit({ kind: 'error', message: 'ignored' })
    socket?.emit({ kind: 'batch', frames: [{ not: 'a frame' }] })
    expect(stream.getSnapshot().frames['a.v1']?.values).toEqual({ x: '3' })
    expect(stream.getSnapshot().frames['b.v1']?.ts_ms).toBe(2)
    expect(stream.getSnapshot().history['a.v1:x']).toEqual([
      { time: 1, value: 1 },
      { time: 3, value: 3 },
    ])

    // Browsing asks for everything; leaving narrows back to the remaining ids.
    const withdrawBrowse = stream.register({ ids: 'series' })
    expect(socket?.sent.at(-1)).toBe(
      JSON.stringify({ kind: 'subscribe', set: 'series', history_ms: 240_000 })
    )
    withdrawBrowse()
    withdrawChart()
    expect(socket?.sent.at(-1)).toBe(JSON.stringify({ kind: 'subscribe', ids: ['a.v1'] }))

    // A dropped socket reconnects and resubscribes with the current union.
    socket?.drop()
    expect(stream.getSnapshot().status).toBe('disconnected')
    vi.advanceTimersByTime(1_000)
    const reopened = FakeWebSocket.instances[1]
    reopened?.open()
    expect(reopened?.sent).toEqual([JSON.stringify({ kind: 'subscribe', ids: ['a.v1'] })])

    // The last listener leaving releases the socket after the grace period.
    withdrawStrip()
    withdrawTwin()
    stop()
    vi.advanceTimersByTime(RELEASE_DELAY_MS)
    expect(reopened?.closed).toBe(true)
  })

  it('fills the trace from a history answer, deduplicating against live frames', () => {
    const stream = getSignalsStream('http://localhost:8080', `s${series}`)
    stream.subscribe(() => undefined)
    stream.register({ ids: ['a.v1'], historyMs: 10_000 })
    const socket = FakeWebSocket.instances[0]
    socket?.open()
    expect(stream.getSnapshot().historyLoaded).toBe(false)

    socket?.emit({
      kind: 'history',
      signals: [
        {
          signal_id: 'a.v1',
          frames: [frame('a.v1', 1_000, { x: '1' }), frame('a.v1', 2_000, { x: '2' })],
        },
      ],
    })
    socket?.emit({ kind: 'snapshot', frames: [frame('a.v1', 2_000, { x: '2' })] })
    socket?.emit({ kind: 'batch', frames: [frame('a.v1', 3_000, { x: 3, s: 'sweep' })] })
    expect(stream.getSnapshot().historyLoaded).toBe(true)
    expect(stream.getSnapshot().history['a.v1:x']).toEqual([
      { time: 1_000, value: 1 },
      { time: 2_000, value: 2 },
      { time: 3_000, value: 3 },
    ])
    expect(stream.getSnapshot().history['a.v1:s']).toBeUndefined()

    // The trace keeps the widest window any consumer asked for, never less than a minute.
    socket?.emit({ kind: 'batch', frames: [frame('a.v1', 3_000 + HISTORY_WINDOW_MS, { x: '4' })] })
    expect(stream.getSnapshot().history['a.v1:x']?.map(point => point.value)).toEqual([3, 4])
  })

  it('republishes the calm snapshot at most once a second', () => {
    const stream = getSignalsStream('http://localhost:8080', `s${series}`)
    stream.subscribe(() => undefined)
    stream.register({ ids: ['a.v1'] })
    const socket = FakeWebSocket.instances[0]
    socket?.open()

    let calmPublishes = 0
    stream.subscribeCalm(() => {
      calmPublishes += 1
    })
    socket?.emit({ kind: 'snapshot', frames: [frame('a.v1', 1_000, { x: '1' })] })
    expect(calmPublishes).toBe(1)
    socket?.emit({ kind: 'batch', frames: [frame('a.v1', 1_200, { x: '2' })] })
    socket?.emit({ kind: 'batch', frames: [frame('a.v1', 1_400, { x: 3 })] })
    expect(calmPublishes).toBe(1)
    expect(stream.getCalmSnapshot().frames['a.v1']?.values).toEqual({ x: '1' })

    vi.advanceTimersByTime(CALM_INTERVAL_MS)
    expect(calmPublishes).toBe(2)
    expect(stream.getCalmSnapshot().frames['a.v1']?.values).toEqual({ x: 3 })
    expect(stream.getCalmSnapshot().history['a.v1:x']).toHaveLength(3)
  })
})
