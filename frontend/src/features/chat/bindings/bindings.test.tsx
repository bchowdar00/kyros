import { act, renderHook } from '@testing-library/react'
import { HttpResponse, http } from 'msw'
import { setupServer } from 'msw/node'
import type { ReactNode } from 'react'
import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest'

import { FakeWebSocket } from '@/features/exchange-streams/fake-web-socket.fixture'
import { useChartBinding } from './chart'
import { type ChatData, ChatDataProvider } from './chat-data'
import { usePositionBinding } from './market'
import { useBookBinding, useFillsBinding } from './tables'

const server = setupServer()
beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => server.resetHandlers())
afterAll(() => server.close())

const apiUrl = 'http://exchange.test'
const address = `0x${'ab'.repeat(20)}`
let run = 0
let series = ''

const catalogue = (slug: string) => ({
  series: slug,
  underlying: 'btc',
  signals: [
    {
      signal_id: 'btc.coinbase.rsi.v1',
      name: 'Coinbase RSI',
      description: '',
      location: 'internal',
      scope: { kind: 'underlying', underlying: 'btc' },
      live: true,
      quality: { state: 'ready', reasons: [] },
      fields: [{ name: 'rsi_14', label: 'RSI (14)', type: 'decimal', headline: true }],
    },
    {
      signal_id: 'btc.kraken.rsi.v1',
      name: 'Kraken RSI',
      description: '',
      location: 'internal',
      scope: { kind: 'underlying', underlying: 'btc' },
      live: false,
      quality: { state: 'unavailable', reasons: ['lane off'] },
      fields: [{ name: 'rsi_14', label: 'RSI (14)', type: 'decimal', headline: true }],
    },
  ],
})

function wrapper(data: ChatData) {
  return function Wrapper({ children }: { children: ReactNode }) {
    return <ChatDataProvider value={data}>{children}</ChatDataProvider>
  }
}

const frame = (signal_id: string, ts_ms: number, values: Record<string, string | number>) => ({
  signal_id,
  ts_ms,
  quality: { state: 'ready', reasons: [] },
  values,
})

describe('widget bindings', () => {
  beforeEach(() => {
    vi.stubGlobal('WebSocket', FakeWebSocket)
    run += 1
    series = `s${run}`
    server.use(http.get(`${apiUrl}/api/signals`, () => HttpResponse.json(catalogue(series))))
  })
  afterEach(() => {
    FakeWebSocket.reset()
    vi.unstubAllGlobals()
  })

  it('subscribes with the window, fills from history, and resolves a live chart', async () => {
    const data: ChatData = { apiUrl, series, address, timeZone: 'UTC' }
    const rsi = { type: 'signal', id: 'btc.coinbase.rsi.v1', field: 'rsi_14' } as const
    const { result } = renderHook(
      () => useChartBinding({ kind: 'chart', series, refs: [rsi], window_ms: 120_000 }, data),
      { wrapper: wrapper(data) }
    )
    expect(result.current.status).toBe('loading')
    const socket = FakeWebSocket.instances.find(instance => instance.url.includes('/ws/signals'))
    act(() => socket?.open())
    expect(socket?.sent.at(-1)).toBe(
      JSON.stringify({ kind: 'subscribe', ids: ['btc.coinbase.rsi.v1'], history_ms: 120_000 })
    )
    await vi.waitFor(() => expect(result.current.status).toBe('loading'))
    act(() => {
      socket?.emit({
        kind: 'history',
        signals: [
          {
            signal_id: 'btc.coinbase.rsi.v1',
            frames: [frame('btc.coinbase.rsi.v1', 1_000, { rsi_14: '38.2' })],
          },
        ],
      })
      socket?.emit({
        kind: 'batch',
        frames: [frame('btc.coinbase.rsi.v1', 2_000, { rsi_14: '41' })],
      })
    })
    await vi.waitFor(() => expect(result.current.status).toBe('live'))
    if (result.current.status !== 'live') return
    expect(result.current.props.title).toBe('Coinbase RSI (14)')
    expect(result.current.props.series[0]?.points).toEqual([
      { time: 1_000, value: 38.2 },
      { time: 2_000, value: 41 },
    ])
    expect(result.current.props.domain).toEqual([0, 100])
    act(() => socket?.drop())
    await vi.waitFor(() => expect(result.current.status).toBe('stale'))
  })

  it('refuses a signal the catalogue does not serve, with the reason', async () => {
    const data: ChatData = { apiUrl, series, address, timeZone: 'UTC' }
    const { result } = renderHook(
      () =>
        useChartBinding(
          {
            kind: 'chart',
            series,
            refs: [{ type: 'signal', id: 'btc.kraken.rsi.v1', field: 'rsi_14' }],
          },
          data
        ),
      { wrapper: wrapper(data) }
    )
    await vi.waitFor(() =>
      expect(result.current).toEqual({
        status: 'unavailable',
        reason: 'Kraken RSI is offline right now.',
      })
    )
    const { result: missing } = renderHook(
      () =>
        useChartBinding(
          { kind: 'chart', series, refs: [{ type: 'signal', id: 'nope.v1', field: 'x' }] },
          data
        ),
      { wrapper: wrapper(data) }
    )
    await vi.waitFor(() =>
      expect(missing.current).toEqual({
        status: 'unavailable',
        reason: `nope.v1 is not a signal of ${series}.`,
      })
    )
  })

  it('binds the book and the tape, and fails closed on account widgets without an identity', () => {
    const anonymous: ChatData = { apiUrl, series, address: null, timeZone: 'UTC' }
    const { result: book } = renderHook(
      () => useBookBinding({ kind: 'book', series, depth: 1 }, anonymous),
      { wrapper: wrapper(anonymous) }
    )
    expect(book.current.status).toBe('loading')
    const bookSocket = FakeWebSocket.instances.find(instance => instance.url.includes('/ws/book'))
    act(() =>
      bookSocket?.emit({
        type: 'snapshot',
        series,
        bids: [{ price: 610, qty: 4 }],
        asks: [{ price: 630, qty: 2 }],
      })
    )
    expect(book.current).toMatchObject({
      status: 'live',
      props: { asks: [{ price: 63, size: 2 }], bids: [{ price: 61, size: 4 }], rows: 1 },
    })

    const { result: mine } = renderHook(
      () => useFillsBinding({ kind: 'fills', series, scope: 'mine' }, anonymous),
      { wrapper: wrapper(anonymous) }
    )
    expect(mine.current).toEqual({ status: 'unavailable', reason: 'Sign in to see your fills.' })
    const { result: position } = renderHook(
      () => usePositionBinding({ kind: 'position', series }, anonymous),
      { wrapper: wrapper(anonymous) }
    )
    expect(position.current).toEqual({
      status: 'unavailable',
      reason: 'Sign in to see your position.',
    })
  })
})
