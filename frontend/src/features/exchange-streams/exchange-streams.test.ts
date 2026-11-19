import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

import { getBookStream } from './book-stream'
import { FakeWebSocket } from './fake-web-socket.fixture'
import { getPositionsStream } from './positions-stream'
import { MAX_TRADES, getTradesStream } from './trades-stream'

const address = `0x${'ab'.repeat(20)}`
const identity = { address, root_address: address }
const trade = (ts: number, price = 500) => ({
  series: 'btc-5m',
  price,
  qty: 1,
  ts,
  side: 'yes',
  taker: identity,
  maker: identity,
})

let run = 0

describe('book, trades and positions streams', () => {
  beforeEach(() => {
    vi.useFakeTimers()
    vi.stubGlobal('WebSocket', FakeWebSocket)
    run += 1
  })
  afterEach(() => {
    vi.runOnlyPendingTimers()
    vi.useRealTimers()
    FakeWebSocket.reset()
    vi.unstubAllGlobals()
  })

  it('replaces the book on every frame and reports a drop', () => {
    const stream = getBookStream('http://localhost:8080', `s${run}`)
    stream.subscribe(() => undefined)
    const socket = FakeWebSocket.instances[0]
    expect(socket?.url).toBe(`ws://localhost:8080/ws/book?series=s${run}`)
    socket?.emit({ type: 'snapshot', series: 'btc-5m', bids: [{ price: 500, qty: 3 }], asks: [] })
    socket?.emit({
      type: 'snapshot',
      series: 'btc-5m',
      bids: [{ price: 510, qty: 1 }],
      asks: [{ price: 520, qty: 2 }],
    })
    expect(stream.getSnapshot()).toMatchObject({
      status: 'live',
      bids: [{ price: 510, qty: 1 }],
      asks: [{ price: 520, qty: 2 }],
    })
    socket?.drop()
    expect(stream.getSnapshot().status).toBe('disconnected')
    expect(stream.getSnapshot().asks).toHaveLength(1)
  })

  it('seeds the midpoint line from the join frame and appends only when the mid moves', () => {
    vi.setSystemTime(10_000)
    const stream = getBookStream('http://localhost:8080', `s${run}`)
    stream.subscribe(() => undefined)
    const socket = FakeWebSocket.instances[0]
    const book = { type: 'snapshot', series: 'btc-5m', bids: [], asks: [] }
    socket?.emit({
      ...book,
      market_id: 3,
      mid: 450,
      mid_history: [
        { ts_ms: 1_000, mid: 400 },
        { ts_ms: 2_000, mid: 450 },
      ],
    })
    expect(stream.getSnapshot()).toMatchObject({ marketId: 3, mid: 45 })
    expect(stream.getSnapshot().midPoints).toEqual([
      { time: 1_000, value: 40 },
      { time: 2_000, value: 45 },
    ])
    // A requote at the same mid adds nothing; a move appends at arrival time.
    socket?.emit({ ...book, market_id: 3, mid: 450 })
    expect(stream.getSnapshot().midPoints).toHaveLength(2)
    vi.setSystemTime(11_000)
    socket?.emit({ ...book, market_id: 3, mid: 500 })
    expect(stream.getSnapshot().midPoints.at(-1)).toEqual({ time: 11_000, value: 50 })
    // An empty book prints nothing and the line holds.
    socket?.emit({ ...book, market_id: 3, mid: null })
    expect(stream.getSnapshot().mid).toBeNull()
    expect(stream.getSnapshot().midPoints).toHaveLength(3)
    // A new round starts the line over.
    vi.setSystemTime(12_000)
    socket?.emit({ ...book, market_id: 4, mid: 520 })
    expect(stream.getSnapshot().midPoints).toEqual([{ time: 12_000, value: 52 }])
  })

  it('keeps the newest trades first, deduplicated across a replayed snapshot', () => {
    const stream = getTradesStream('http://localhost:8080', `s${run}`)
    stream.subscribe(() => undefined)
    const socket = FakeWebSocket.instances[0]
    socket?.emit({ type: 'snapshot', trades: [trade(2), trade(1)] })
    socket?.emit({ type: 'batch', trades: [trade(3)] })
    socket?.emit({ type: 'snapshot', trades: [trade(3), trade(2), trade(1)] })
    expect(stream.getSnapshot().trades.map(row => row.ts)).toEqual([3, 2, 1])

    const flood = Array.from({ length: MAX_TRADES + 10 }, (_, index) => trade(100 + index))
    socket?.emit({ type: 'batch', trades: flood })
    expect(stream.getSnapshot().trades).toHaveLength(MAX_TRADES)
    expect(stream.getSnapshot().trades[0]?.ts).toBe(100 + MAX_TRADES + 9)
  })

  it('carries the latest position snapshot and marks an unknown account', () => {
    const stream = getPositionsStream('http://localhost:8080', address, `s${run}`)
    stream.subscribe(() => undefined)
    const socket = FakeWebSocket.instances[0]
    expect(socket?.url).toBe(`ws://localhost:8080/ws/positions?address=${address}&series=s${run}`)
    socket?.emit({
      type: 'position_snapshot',
      as_of_unix_ms: 9,
      series: 'btc-5m',
      market_id: 3,
      address,
      side: 'yes',
      size_shares: 4,
      signed_position: 4,
      avg_entry_yes_tenths: 480,
      cost_basis_tenths: 1920,
      mark_yes_tenths: 500,
      unrealized_pnl_tenths: 80,
      realized_pnl_this_market_tenths: 0,
      balance_tenths: 998_080,
      locked_tenths: 1920,
      series_positions: [],
    })
    expect(stream.getSnapshot().status).toBe('live')
    expect(stream.getSnapshot().snapshot?.signed_position).toBe(4)

    const unknown = getPositionsStream('http://localhost:8080', `0x${'cd'.repeat(20)}`, `s${run}`)
    unknown.subscribe(() => undefined)
    const other = FakeWebSocket.instances[1]
    other?.emit({ type: 'error', error: 'unknown_account', message: 'no account at that address' })
    other?.drop()
    expect(unknown.getSnapshot().status).toBe('unknown_account')
  })
})
