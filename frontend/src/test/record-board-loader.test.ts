// @vitest-environment node
import { http, HttpResponse } from 'msw'
import { setupServer } from 'msw/node'
import { afterAll, afterEach, beforeAll, describe, expect, it, vi } from 'vitest'

import { createRecordBoardLoader } from '@/features/records/record-board-loader'

const EXPLORER = 'https://explorer.test'
const EXCHANGE = 'https://exchange.test'
const CONTRACT = '0xc910cfe6b537feee16b60a9964c55fc66580586a'
const LOGS = `${EXPLORER}/api/v2/addresses/${CONTRACT}/logs`

const SEALED = '0xf849e84085a69739eeb3b0f36b4f9ad8d4548d536aceecaf711fc3f540251c6f'
const OPENED = '0x10626a129d7628f6a4cb6540632d282bc9a25250656779d609ff70b0545d66cf'
const A = `0x${'a'.repeat(64)}`
const B = `0x${'b'.repeat(64)}`
const ACCOUNT_A = `0x${'1'.repeat(40)}`

function word(value: bigint): string {
  return BigInt.asUintN(256, value).toString(16).padStart(64, '0')
}

function sealedLog(deploymentId: string, roundId: number, pnlCents: number) {
  return {
    block_number: 100 + roundId,
    block_timestamp: `2026-11-20T08:${String(roundId).padStart(2, '0')}:00.000000Z`,
    data: `0x${word(pnlCents > 0 ? 1n : 0n)}${word(BigInt(pnlCents))}${'11'.repeat(32)}`,
    topics: [SEALED, deploymentId, `0x${word(BigInt(roundId))}`, null],
    transaction_hash: `0x${word(BigInt(roundId))}`,
  }
}

function openedLog(deploymentId: string, budgetCents: number) {
  return {
    block_number: 50,
    block_timestamp: '2026-11-20T07:00:00.000000Z',
    data: `0x${'22'.repeat(32)}${word(BigInt(budgetCents))}`,
    topics: [OPENED, deploymentId, null, null],
    transaction_hash: `0x${'33'.repeat(32)}`,
  }
}

const server = setupServer()
let explorerCalls = 0

function mockTwoPagesOfLogs(): void {
  server.use(
    http.get(LOGS, ({ request }) => {
      explorerCalls += 1
      const cursor = new URL(request.url).searchParams.get('block_number')
      if (cursor === null) {
        return HttpResponse.json({
          items: [sealedLog(A, 9, -50), sealedLog(B, 8, 10)],
          next_page_params: { block_number: 60, index: 3 },
        })
      }
      return HttpResponse.json({
        items: [sealedLog(A, 7, 120), openedLog(A, 4000)],
        next_page_params: null,
      })
    })
  )
}

function mockExchange(deployment = 'Strategy_abc:7'): void {
  server.use(
    http.get(`${EXCHANGE}/api/records/${A}`, () =>
      HttpResponse.json({
        account: ACCOUNT_A,
        budget_cents: 4000,
        deployment: 'Strategy_abc:7',
        deployment_id: A,
        open_tx: null,
        program_hash: `0x${'22'.repeat(32)}`,
        seals: [],
        series: 'btc-5m',
        strategy_id: 'Strategy_abc',
      })
    ),
    http.get(`${EXCHANGE}/api/records/${B}`, () =>
      HttpResponse.json({ error: 'record_not_found' }, { status: 404 })
    ),
    http.get(`${EXCHANGE}/api/accounts/${ACCOUNT_A}`, () =>
      HttpResponse.json({
        address: ACCOUNT_A,
        strategy: { deployment_id: deployment, name: 'Quote With the Lean' },
      })
    )
  )
}

function loader(now: () => number = () => 1_000_000) {
  return createRecordBoardLoader({
    contract: CONTRACT,
    exchangeUrl: EXCHANGE,
    explorerUrl: EXPLORER,
    now,
  })
}

beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => {
  server.resetHandlers()
  explorerCalls = 0
})
afterAll(() => server.close())

describe('the record board loader', () => {
  it('reads every page of the contract logs and ranks the strategies', async () => {
    mockTwoPagesOfLogs()
    mockExchange()

    const board = await loader()()

    expect(board).toMatchObject({
      contract: CONTRACT,
      status: 'ok',
      totals: { rounds: 3, strategies: 2, wonRounds: 2 },
      truncated: false,
    })
    if (board.status !== 'ok') throw new Error('expected a board')
    expect(board.rows.map(row => [row.deploymentId, row.pnlCents, row.rounds])).toEqual([
      [A, 70, 2],
      [B, 10, 1],
    ])
    expect(board.rows[0]?.budgetCents).toBe(4000)
  })

  it('names a strategy and its market from the exchange when the exchange still knows it', async () => {
    mockTwoPagesOfLogs()
    mockExchange()

    const board = await loader()()

    if (board.status !== 'ok') throw new Error('expected a board')
    expect(board.rows[0]).toMatchObject({ name: 'Quote With the Lean', series: 'btc-5m' })
    expect(board.rows[1]?.name).toBeUndefined()
    expect(board.rows[1]?.series).toBeUndefined()
  })

  it('does not borrow the name of a newer deployment in the same account', async () => {
    mockTwoPagesOfLogs()
    mockExchange('Strategy_other:9')

    const board = await loader()()

    if (board.status !== 'ok') throw new Error('expected a board')
    expect(board.rows[0]).toMatchObject({ series: 'btc-5m' })
    expect(board.rows[0]?.name).toBeUndefined()
  })

  it('still shows the board when the exchange does not answer', async () => {
    mockTwoPagesOfLogs()
    server.use(http.get(`${EXCHANGE}/*`, () => HttpResponse.error()))

    const board = await loader()()

    if (board.status !== 'ok') throw new Error('expected a board')
    expect(board.rows).toHaveLength(2)
    expect(board.rows[0]?.name).toBeUndefined()
  })

  it('works with no exchange configured', async () => {
    mockTwoPagesOfLogs()

    const load = createRecordBoardLoader({
      contract: CONTRACT,
      exchangeUrl: undefined,
      explorerUrl: EXPLORER,
    })

    expect(await load()).toMatchObject({ status: 'ok', totals: { strategies: 2 } })
  })

  it('says the board is unavailable when the explorer fails, and asks again after five seconds', async () => {
    server.use(
      http.get(LOGS, () => {
        explorerCalls += 1
        return HttpResponse.json({ message: 'down' }, { status: 500 })
      })
    )
    let clock = 1_000_000
    const load = loader(() => clock)

    expect(await load()).toEqual({ status: 'unavailable' })
    clock += 4000
    expect(await load()).toEqual({ status: 'unavailable' })
    expect(explorerCalls).toBe(1)

    clock += 2000
    mockTwoPagesOfLogs()
    mockExchange()
    expect(await load()).toMatchObject({ status: 'ok' })
  })

  it('shows no board at all when one page of several cannot be read', async () => {
    server.use(
      http.get(LOGS, ({ request }) =>
        new URL(request.url).searchParams.get('block_number') === null
          ? HttpResponse.json({
              items: [sealedLog(A, 9, -50)],
              next_page_params: { block_number: 60, index: 3 },
            })
          : HttpResponse.json({ message: 'down' }, { status: 502 })
      )
    )

    expect(await loader()()).toEqual({ status: 'unavailable' })
  })

  it('says the board is unavailable when the explorer answers with another shape', async () => {
    server.use(http.get(LOGS, () => HttpResponse.json({ items: 'nope' })))

    expect(await loader()()).toEqual({ status: 'unavailable' })
  })

  it('keeps a good answer for thirty seconds, then reads the chain again behind the kept one', async () => {
    mockTwoPagesOfLogs()
    mockExchange()
    let clock = 1_000_000
    const load = loader(() => clock)

    const first = await load()
    clock += 29_000
    expect(await load()).toBe(first)
    expect(explorerCalls).toBe(2)

    // Past thirty seconds the visitor still gets the kept board at once; the new read runs
    // behind it and is what the next visitor sees.
    clock += 2000
    expect(await load()).toBe(first)
    await vi.waitFor(() => expect(explorerCalls).toBe(4))
    await vi.waitFor(async () => expect(await load()).not.toBe(first))
    expect(await load()).toMatchObject({
      asOf: new Date(1_031_000).toISOString(),
      status: 'ok',
    })
  })

  it('keeps showing the last good board when a later read fails', async () => {
    mockTwoPagesOfLogs()
    mockExchange()
    let clock = 1_000_000
    const load = loader(() => clock)
    const good = await load()

    server.use(
      http.get(LOGS, () => {
        explorerCalls += 1
        return HttpResponse.json({ message: 'down' }, { status: 500 })
      })
    )
    clock += 31_000
    expect(await load()).toBe(good)
    await vi.waitFor(() => expect(explorerCalls).toBe(3))

    // The failure does not replace the board, and the explorer is left alone for five seconds.
    clock += 4000
    expect(await load()).toBe(good)
    expect(explorerCalls).toBe(3)

    clock += 2000
    expect(await load()).toBe(good)
    await vi.waitFor(() => expect(explorerCalls).toBe(4))
  })

  it('follows a page cursor that carries an empty field', async () => {
    server.use(
      http.get(LOGS, ({ request }) => {
        explorerCalls += 1
        const params = new URL(request.url).searchParams
        if (params.get('block_number') === null) {
          return HttpResponse.json({
            items: [sealedLog(A, 9, -50)],
            next_page_params: { block_number: 60, index: 3, transaction_index: null },
          })
        }
        expect(params.has('transaction_index')).toBe(false)
        return HttpResponse.json({ items: [sealedLog(A, 7, 120)], next_page_params: null })
      })
    )

    const load = createRecordBoardLoader({
      contract: CONTRACT,
      exchangeUrl: undefined,
      explorerUrl: EXPLORER,
    })

    expect(await load()).toMatchObject({ status: 'ok', totals: { rounds: 2 } })
  })

  it('reads the chain once for visitors who arrive together', async () => {
    mockTwoPagesOfLogs()
    mockExchange()
    const load = loader()

    const [first, second] = await Promise.all([load(), load()])

    expect(explorerCalls).toBe(2)
    expect(second).toBe(first)
  })

  it('stops at the page limit and says the list is cut', async () => {
    server.use(
      http.get(LOGS, () => {
        explorerCalls += 1
        return HttpResponse.json({
          items: [sealedLog(A, 9, 5)],
          next_page_params: { block_number: 60, index: 3 },
        })
      })
    )

    const load = createRecordBoardLoader({
      contract: CONTRACT,
      exchangeUrl: undefined,
      explorerUrl: EXPLORER,
      maxPages: 3,
    })
    const board = await load()

    expect(board).toMatchObject({ status: 'ok', truncated: true })
    expect(explorerCalls).toBe(3)
  })
})
