import { act, renderHook } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

import { usePriceStream } from './use-price-stream'

class FakeWebSocket {
  static readonly OPEN = 1
  static instances: FakeWebSocket[] = []

  readonly listeners = new Map<string, Set<(event: Event) => void>>()
  readyState = FakeWebSocket.OPEN
  closed = false
  readonly url: string

  constructor(url: string | URL) {
    this.url = String(url)
    FakeWebSocket.instances.push(this)
  }

  addEventListener(type: string, listener: (event: Event) => void) {
    const listeners = this.listeners.get(type) ?? new Set()
    listeners.add(listener)
    this.listeners.set(type, listeners)
  }

  send() {}

  close() {
    this.closed = true
  }

  emit(data: unknown) {
    const event = new MessageEvent('message', { data: JSON.stringify(data) })
    for (const listener of this.listeners.get('message') ?? []) listener(event)
  }
}

const snapshot = {
  type: 'snapshot',
  series: 'btc-5m',
  duration_secs: 300,
  market_id: 7,
  strike: 79_650,
  opens_at_ms: 1_788_598_800_000,
  closes_at_ms: 1_788_598_920_000,
  ticks: [{ ts_ms: 1_788_598_801_000, price: 79_651.5 }],
}

describe('usePriceStream', () => {
  beforeEach(() => {
    vi.useFakeTimers()
    vi.stubGlobal('WebSocket', FakeWebSocket)
  })
  afterEach(() => {
    vi.runOnlyPendingTimers()
    vi.useRealTimers()
    FakeWebSocket.instances = []
    vi.unstubAllGlobals()
  })

  it('opens the series price socket and seeds history and the round from the snapshot', () => {
    const { result, unmount } = renderHook(() => usePriceStream('http://localhost:8080', 'btc-5m'))

    expect(FakeWebSocket.instances).toHaveLength(1)
    expect(FakeWebSocket.instances[0]?.url).toBe('ws://localhost:8080/ws/price?series=btc-5m')

    act(() => FakeWebSocket.instances[0]?.emit(snapshot))

    expect(result.current.feed.state).toBe('live')
    expect(result.current.points).toEqual([{ time: 1_788_598_801_000, value: 79_651.5 }])
    expect(result.current.round).toEqual({
      marketId: 7,
      strike: 79_650,
      opensAt: 1_788_598_800_000,
      closesAt: 1_788_598_920_000,
    })

    act(() =>
      FakeWebSocket.instances[0]?.emit({ type: 'tick', ts_ms: 1_788_598_802_000, price: 79_660 })
    )
    expect(result.current.points).toHaveLength(2)
    // Streams are shared per series; release this one so the next test gets a fresh socket.
    unmount()
  })

  it('keeps the price history and swaps the round on a reset for this series only', () => {
    const { result, unmount } = renderHook(() => usePriceStream('http://localhost:8080', 'btc-5m'))
    const socket = FakeWebSocket.instances[0]
    act(() => socket?.emit(snapshot))
    act(() =>
      socket?.emit({
        type: 'market_reset',
        series: 'btc-15m',
        market_id: 2,
        strike: 1,
        opens_at_ms: 1,
        closes_at_ms: 2,
        duration_secs: 900,
      })
    )
    expect(result.current.round?.marketId).toBe(7)
    act(() =>
      socket?.emit({
        type: 'market_reset',
        series: 'btc-5m',
        market_id: 8,
        strike: 79_700,
        opens_at_ms: 1_788_598_920_000,
        closes_at_ms: 1_788_599_040_000,
        duration_secs: 300,
      })
    )
    expect(result.current.points).toEqual([{ time: 1_788_598_801_000, value: 79_651.5 }])
    expect(result.current.round?.marketId).toBe(8)
    expect(result.current.round?.strike).toBe(79_700)
    unmount()
  })

  it('stays idle when the exchange origin is unset', () => {
    const { result } = renderHook(() => usePriceStream(undefined, 'btc-5m'))

    expect(FakeWebSocket.instances).toHaveLength(0)
    expect(result.current).toEqual({ feed: { state: 'paused' }, points: [] })
  })

  it('keeps the same connection and observed history across a market remount', () => {
    const firstMarket = renderHook(() => usePriceStream('http://localhost:8080', 'btc-5m'))
    const socket = FakeWebSocket.instances[0]
    act(() => socket?.emit(snapshot))
    const before = firstMarket.result.current.points
    firstMarket.unmount()
    expect(socket?.closed).toBe(false)
    const nextMarket = renderHook(() => usePriceStream('http://localhost:8080', 'btc-5m'))
    expect(FakeWebSocket.instances).toHaveLength(1)
    expect(nextMarket.result.current.points).toBe(before)
    nextMarket.unmount()
    act(() => vi.advanceTimersByTime(30_000))
    expect(socket?.closed).toBe(true)
  })
})
