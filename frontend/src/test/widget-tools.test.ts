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
  { series: 'btc-2m', duration_secs: 120, current_market_id: 1, opens_at_ms: 0, closes_at_ms: 1 },
  { series: 'btc-15m', duration_secs: 900, current_market_id: 1, opens_at_ms: 0, closes_at_ms: 1 },
  {
    series: 'nba-h2h',
    duration_secs: 300,
    underlying: 'nba',
    outcomes: ['home', 'away'],
    current_market_id: 1,
    opens_at_ms: 0,
    closes_at_ms: 1,
  },
]
const nbaCatalogue = {
  series: 'nba-h2h',
  underlying: 'nba',
  signals: [
    {
      signal_id: 'nba-h2h.current.price.v1',
      name: 'Market price',
      description: '',
      location: 'internal',
      scope: { kind: 'series', series_id: 'nba-h2h' },
      live: true,
      quality: { state: 'ready', reasons: [] },
      fields: [
        { name: 'home', label: 'Home', type: 'decimal', headline: true },
        { name: 'away', label: 'Away', type: 'decimal', headline: true },
        { name: 'bid', label: 'Best bid', type: 'decimal', headline: true },
      ],
    },
    {
      signal_id: 'nba.sim.pbp.v1',
      name: 'Play-by-play',
      description: 'Every play with the score.',
      location: 'internal',
      scope: { kind: 'underlying', underlying: 'nba' },
      live: true,
      quality: { state: 'ready', reasons: [] },
      fields: [
        { name: 'home_score', label: 'Home score', type: 'u32', headline: true },
        { name: 'away_score', label: 'Away score', type: 'u32', headline: true },
      ],
    },
  ],
}
const catalogue = {
  series: 'btc-2m',
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

const accepted = (spec: object) => ({ ok: true, spec: { ...spec, series: 'btc-2m' } })

describe('Teeny widget tools', () => {
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
    const show = (refs: Ref[], slug = 'btc-2m') =>
      tools.showChart.execute?.({ kind: 'chart', series: slug, refs }, onMarket('btc-2m'))
    expect(await show([{ type: 'spot' }], 'eth-2m')).toEqual({
      ok: false,
      reason: 'eth-2m is not a series the exchange runs.',
      suggestions: ['btc-2m', 'btc-15m', 'nba-h2h'],
    })
    expect(await show([{ type: 'signal', id: 'btc.bybit.rsi.v1', field: 'rsi_14' }])).toEqual({
      ok: false,
      reason: 'btc.bybit.rsi.v1 is not a signal of btc-2m.',
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
    expect(await tools.showChart.execute?.({ kind: 'chart' }, onMarket('btc-2m'))).toMatchObject({
      ok: false,
      reason: expect.stringContaining('not valid'),
    })
  })

  it('charts the scores and the quoted side on an NBA series, and refuses BTC-only references there', async () => {
    server.use(
      http.get(`${baseUrl}/api/series`, () => HttpResponse.json(series)),
      http.get(`${baseUrl}/api/signals`, ({ request }) =>
        HttpResponse.json(
          new URL(request.url).searchParams.get('series') === 'nba-h2h' ? nbaCatalogue : catalogue
        )
      )
    )
    const tools = createWidgetTools(baseUrl)
    const show = (refs: Ref[]) =>
      tools.showChart.execute?.({ kind: 'chart', refs }, onMarket('nba-h2h'))
    const scores: Ref[] = [
      { type: 'signal', id: 'nba.sim.pbp.v1', field: 'home_score' },
      { type: 'signal', id: 'nba.sim.pbp.v1', field: 'away_score' },
    ]
    expect(await show(scores)).toEqual({
      ok: true,
      spec: { kind: 'chart', series: 'nba-h2h', refs: scores },
    })
    expect(await show([{ type: 'market', measure: 'up' }])).toMatchObject({ ok: true })
    expect(await show([{ type: 'spot' }])).toMatchObject({
      ok: false,
      reason: expect.stringContaining('no BTC price'),
    })
    expect(await show([{ type: 'strike' }])).toMatchObject({
      ok: false,
      reason: expect.stringContaining('no strike'),
    })
    expect(await tools.showChart.execute?.({ kind: 'candles' }, onMarket('nba-h2h'))).toMatchObject(
      { ok: false, reason: expect.stringContaining('no BTC price') }
    )
    expect(await tools.showMarket.execute?.({ kind: 'market' }, onMarket('nba-h2h'))).toMatchObject(
      { ok: false, suggestions: ['chart', 'stats'] }
    )
    expect(await tools.showMarket.execute?.({ kind: 'stats' }, onMarket('nba-h2h'))).toEqual({
      ok: true,
      spec: { kind: 'stats', series: 'nba-h2h' },
    })
  })

  it('passes content and pickers through without the exchange, and refuses live kinds without it', async () => {
    const offline = createWidgetTools(undefined)
    expect(
      await offline.showContent.execute?.({ kind: 'markdown', text: '**hi**' }, onMarket('btc-2m'))
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
    expect(await offline.showContent.execute?.(table, onMarket('btc-2m'))).toEqual({
      ok: true,
      spec: table,
    })
    // The tool's own input schema rejects record rows before the spec is even parsed.
    // Deliberately malformed input: the cast is the point of the test.
    const malformed = { ...table, rows: [{}, {}] } as unknown as typeof table
    expect(await offline.showContent.execute?.(malformed, onMarket('btc-2m'))).toMatchObject({
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
        onMarket('btc-2m')
      )
    ).toMatchObject({ ok: true, spec: { kind: 'picker' } })
    expect(await offline.showBook.execute?.({ kind: 'book' }, onMarket('btc-2m'))).toEqual({
      ok: false,
      reason: 'The exchange is not configured, so nothing live can be shown.',
    })
  })

  it('accepts the other live kinds once the series is known', async () => {
    server.use(http.get(`${baseUrl}/api/series`, () => HttpResponse.json(series)))
    const tools = createWidgetTools(baseUrl)
    const context = onMarket('btc-2m')
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
