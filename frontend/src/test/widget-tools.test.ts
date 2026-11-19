import { noopObserve } from '@mastra/core/tools'
import { HttpResponse, http } from 'msw'
import { setupServer } from 'msw/node'
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest'

import type { Ref } from '@/features/chat/specs'
import { createWidgetTools } from '@/mastra/capabilities/widgets/tools'
import { createChatContext } from '@/mastra/request-context'

const server = setupServer()
beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => server.resetHandlers())
afterAll(() => server.close())

const baseUrl = 'http://exchange.test'
const onMarket = (marketSlug: string) => ({
  observe: noopObserve,
  requestContext: createChatContext({ marketSlug, resourceId: 'r' }),
})
const series = [
  { series: 'btc-5m', duration_secs: 300, current_market_id: 1, opens_at_ms: 0, closes_at_ms: 1 },
  { series: 'btc-15m', duration_secs: 900, current_market_id: 1, opens_at_ms: 0, closes_at_ms: 1 },
]
const catalogue = {
  series: 'btc-5m',
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
      fields: [
        { name: 'rsi_14', label: 'RSI', type: 'decimal', headline: true },
        { name: 'regime', label: 'Regime', type: 'enum', variants: ['a'], headline: true },
      ],
    },
    {
      signal_id: 'btc.kraken.rsi.v1',
      name: 'Kraken RSI',
      description: '',
      location: 'internal',
      scope: { kind: 'underlying', underlying: 'btc' },
      live: false,
      quality: { state: 'unavailable', reasons: [] },
      fields: [{ name: 'rsi_14', label: 'RSI', type: 'decimal', headline: true }],
    },
  ],
}

const accepted = (spec: object) => ({ ok: true, spec: { ...spec, series: 'btc-5m' } })

describe('Kyros widget tools', () => {
  it('accepts a chart of a live signal, filling the series from the page', async () => {
    let catalogueReads = 0
    server.use(
      http.get(`${baseUrl}/api/series`, () => HttpResponse.json(series)),
      http.get(`${baseUrl}/api/signals`, () => {
        catalogueReads += 1
        return HttpResponse.json(catalogue)
      })
    )
    const tools = createWidgetTools(baseUrl)
    const rsi = { type: 'signal', id: 'btc.coinbase.rsi.v1', field: 'rsi_14' } as const
    const shown = await tools.showChart.execute?.(
      { kind: 'chart', refs: [rsi] },
      onMarket('btc-15m')
    )
    expect(shown).toEqual({
      ok: true,
      spec: { kind: 'chart', series: 'btc-15m', refs: [rsi] },
    })
    await tools.showChart.execute?.(
      { kind: 'chart', refs: [rsi, { type: 'spot' }] },
      onMarket('btc-15m')
    )
    expect(catalogueReads).toBe(1)
  })

  it('refuses unknown series, unknown or offline signals and state fields, with suggestions', async () => {
    server.use(
      http.get(`${baseUrl}/api/series`, () => HttpResponse.json(series)),
      http.get(`${baseUrl}/api/signals`, () => HttpResponse.json(catalogue))
    )
    const tools = createWidgetTools(baseUrl)
    const show = (refs: Ref[], slug = 'btc-5m') =>
      tools.showChart.execute?.({ kind: 'chart', series: slug, refs }, onMarket('btc-5m'))
    expect(await show([{ type: 'spot' }], 'eth-2m')).toEqual({
      ok: false,
      reason: 'eth-2m is not a series the exchange runs.',
      suggestions: ['btc-5m', 'btc-15m'],
    })
    expect(await show([{ type: 'signal', id: 'btc.bybit.rsi.v1', field: 'rsi_14' }])).toEqual({
      ok: false,
      reason: 'btc.bybit.rsi.v1 is not a signal of btc-5m.',
      suggestions: ['btc.coinbase.rsi.v1', 'btc.kraken.rsi.v1'],
    })
    expect(await show([{ type: 'signal', id: 'btc.coinbase.rsi.v1', field: 'rsi' }])).toEqual({
      ok: false,
      reason: 'Coinbase RSI has no field named rsi.',
      suggestions: ['rsi_14'],
    })
    expect(
      await show([{ type: 'signal', id: 'btc.coinbase.rsi.v1', field: 'regime' }])
    ).toMatchObject({
      ok: false,
      reason: expect.stringContaining('a state, not a number'),
    })
    expect(await show([{ type: 'signal', id: 'btc.kraken.rsi.v1', field: 'rsi_14' }])).toEqual({
      ok: false,
      reason: 'Kraken RSI is offline right now.',
    })
    // Passes the flat tool schema but not the spec: a chart needs at least one reference.
    expect(await tools.showChart.execute?.({ kind: 'chart' }, onMarket('btc-5m'))).toMatchObject({
      ok: false,
      reason: expect.stringContaining('not valid'),
    })
  })

  it('passes content and pickers through without the exchange, and refuses live kinds without it', async () => {
    const offline = createWidgetTools(undefined)
    expect(
      await offline.showContent.execute?.({ kind: 'markdown', text: '**hi**' }, onMarket('btc-5m'))
    ).toEqual({ ok: true, spec: { kind: 'markdown', text: '**hi**' } })
    // Rows are positional cells; a row keyed by column id is what the model cannot send.
    const table = {
      kind: 'table' as const,
      title: 'Momentum',
      columns: [
        { id: 'id', label: 'Signal' },
        { id: 'venue', label: 'Venue' },
      ],
      rows: [
        ['btc.kraken.roc.v1', 'Kraken'],
        ['btc.coinbase.roc.v1', 'Coinbase'],
      ],
    }
    expect(await offline.showContent.execute?.(table, onMarket('btc-5m'))).toEqual({
      ok: true,
      spec: table,
    })
    // The tool's own input schema rejects record rows before the spec is even parsed.
    // Deliberately malformed input: the cast is the point of the test.
    const malformed = { ...table, rows: [{}, {}] } as unknown as typeof table
    expect(await offline.showContent.execute?.(malformed, onMarket('btc-5m'))).toMatchObject({
      error: true,
    })
    expect(
      await offline.showAsk.execute?.(
        {
          kind: 'picker',
          prompt: 'Side?',
          options: [
            { id: 'y', title: 'YES' },
            { id: 'n', title: 'NO' },
          ],
        },
        onMarket('btc-5m')
      )
    ).toMatchObject({ ok: true, spec: { kind: 'picker' } })
    expect(await offline.showBook.execute?.({ kind: 'book' }, onMarket('btc-5m'))).toEqual({
      ok: false,
      reason: 'The exchange is not configured, so nothing live can be shown.',
    })
  })

  it('accepts the other live kinds once the series is known', async () => {
    server.use(http.get(`${baseUrl}/api/series`, () => HttpResponse.json(series)))
    const tools = createWidgetTools(baseUrl)
    const context = onMarket('btc-5m')
    const book = { kind: 'book', depth: 15 } as const
    expect(await tools.showBook.execute?.(book, context)).toEqual(accepted(book))
    const stats = { kind: 'stats' } as const
    expect(await tools.showMarket.execute?.(stats, context)).toEqual(accepted(stats))
    const position = { kind: 'position' } as const
    expect(await tools.showPosition.execute?.(position, context)).toEqual(accepted(position))
    const fills = { kind: 'fills', scope: 'mine' } as const
    expect(await tools.showTrades.execute?.(fills, context)).toEqual(accepted(fills))
    const distribution = { kind: 'distribution', count: 20 } as const
    expect(await tools.showRounds.execute?.(distribution, context)).toEqual(accepted(distribution))
    const ticket = { kind: 'ticket', side: 'YES', amount_cents: 2_500 } as const
    expect(await tools.showAsk.execute?.(ticket, context)).toEqual(accepted(ticket))
    const scenario = { kind: 'scenario', variable: 'btc_move' } as const
    expect(await tools.showAsk.execute?.(scenario, context)).toEqual(accepted(scenario))
    const candles = { kind: 'candles', bucket_ms: 30_000 } as const
    expect(await tools.showChart.execute?.(candles, context)).toEqual(accepted(candles))
  })
})
