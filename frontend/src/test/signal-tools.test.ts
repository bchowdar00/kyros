import { noopObserve } from '@mastra/core/tools'
import { HttpResponse, http } from 'msw'
import { setupServer } from 'msw/node'
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest'

import { createSignalTools } from '@/mastra/capabilities/signals/tools'
import { createChatContext } from '@/mastra/request-context'

const server = setupServer()
beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => server.resetHandlers())
afterAll(() => server.close())

const baseUrl = 'http://exchange.test'
const tools = createSignalTools(baseUrl)
const context = {
  observe: noopObserve,
  requestContext: createChatContext({ marketSlug: 'btc-5m', resourceId: 'r' }),
}

const catalogue = {
  series: 'btc-5m',
  underlying: 'btc',
  signals: [
    {
      signal_id: 'btc.coinbase.rsi.v1',
      name: 'Coinbase BTC/USD RSI(14)',
      description: 'Relative strength over 14 one-minute bars.',
      location: 'internal',
      scope: { kind: 'underlying', underlying: 'btc' },
      lane: 'spot',
      live: true,
      quality: { state: 'ready', reasons: [] },
      ts_ms: 5,
      fields: [
        { name: 'rsi_14', label: 'RSI 14', type: 'decimal', headline: true, value: '38.2' },
        {
          name: 'regime',
          label: 'Regime',
          type: 'enum',
          variants: ['oversold'],
          headline: true,
          value: 'oversold',
        },
        { name: 'sample_count', label: 'Samples', type: 'u32', headline: false, value: 14 },
      ],
    },
    {
      signal_id: 'btc-5m.current.stats.v1',
      name: 'Round Stats',
      description: 'Round activity.',
      location: 'internal',
      scope: { kind: 'series', series_id: 'btc-5m' },
      live: true,
      quality: { state: 'ready', reasons: [] },
      fields: [
        {
          name: 'volume',
          label: 'Round Volume',
          type: 'decimal',
          headline: true,
          value: '2365.000',
        },
      ],
    },
  ],
}

const frame = (ts_ms: number, rsi: string) => ({
  signal_id: 'btc.coinbase.rsi.v1',
  ts_ms,
  quality: { state: 'ready', reasons: [] },
  values: { rsi_14: rsi, regime: 'oversold', sample_count: 14 },
})

describe('Kyros signal tools', () => {
  it('searches the catalogue by words and returns readable headline fields', async () => {
    server.use(http.get(`${baseUrl}/api/signals`, () => HttpResponse.json(catalogue)))
    const found = await tools.searchSignals.execute?.({ query: 'coinbase rsi' }, context)
    expect(found).toMatchObject({
      status: 'ok',
      series: 'btc-5m',
      total: 1,
      signals: [
        {
          signal_id: 'btc.coinbase.rsi.v1',
          venue: 'Coinbase',
          live: true,
          fields: [
            { name: 'rsi_14', numeric: true, value: '38.2' },
            { name: 'regime', numeric: false, value: 'Oversold' },
          ],
        },
      ],
    })
    const everything = await tools.searchSignals.execute?.({}, context)
    expect((everything as { total: number }).total).toBe(2)
    const all = await tools.searchSignals.execute?.(
      { query: 'rsi', include_all_fields: true },
      context
    )
    expect((all as { signals: { fields: unknown[] }[] }).signals[0]?.fields).toHaveLength(3)
  })

  it('reads sampled history per field and refuses ids the catalogue does not serve', async () => {
    server.use(
      http.get(`${baseUrl}/api/signals`, () => HttpResponse.json(catalogue)),
      http.get(`${baseUrl}/api/signals/history`, ({ request }) => {
        const query = new URL(request.url).searchParams
        expect(query.get('ids')).toBe('btc.coinbase.rsi.v1')
        expect(query.get('history_ms')).toBe('120000')
        expect(query.get('max_points')).toBe('24')
        return HttpResponse.json({
          series: 'btc-5m',
          since_ms: 0,
          signals: [
            {
              signal_id: 'btc.coinbase.rsi.v1',
              frames: [frame(1_000, '38.2'), frame(2_000, '35.0'), frame(3_000, '41.5')],
            },
          ],
        })
      })
    )
    const read = await tools.readSignals.execute?.(
      { refs: [{ signal_id: 'btc.coinbase.rsi.v1', field: 'rsi_14' }], window_ms: 120_000 },
      context
    )
    expect(read).toMatchObject({
      status: 'ok',
      window_ms: 120_000,
      fields: [
        {
          field: 'rsi_14',
          samples: 3,
          first: 38.2,
          last: 41.5,
          low: 35,
          high: 41.5,
          last_formatted: '41.5',
          points: [
            { at_ms: 1_000, value: 38.2 },
            { at_ms: 2_000, value: 35 },
            { at_ms: 3_000, value: 41.5 },
          ],
        },
      ],
    })
    expect(
      await tools.readSignals.execute?.(
        {
          refs: [
            { signal_id: 'btc.coinbase.rsi.v1', field: 'regime' },
            { signal_id: 'nope.v1', field: 'x' },
          ],
        },
        context
      )
    ).toEqual({
      status: 'unavailable',
      series: 'btc-5m',
      reason: 'regime is a state, not a number. nope.v1 is not a signal of btc-5m.',
    })
  })

  it('finds the Polymarket quote from YES and NO, and ranks live signals ahead of offline ones', async () => {
    const underlying = { kind: 'underlying', underlying: 'btc' }
    const momentum = (venue: string, live: boolean, state: string) => ({
      signal_id: `btc.${venue}.micro_momentum.v1`,
      name: `${venue} momentum`,
      description: 'Return over the last ten and thirty seconds.',
      location: 'internal',
      scope: underlying,
      live,
      quality: { state, reasons: [] },
      fields: [
        {
          name: 'return_bps_10s',
          label: 'Return 10s',
          type: 'decimal',
          headline: true,
          value: '1.0',
        },
      ],
    })
    // Offline venues sit first in the catalogue, as they do on the exchange.
    const offline = Array.from({ length: 26 }, (_, index) =>
      momentum(`venue${index}`, false, 'unknown')
    )
    const wide = {
      series: 'btc-5m',
      underlying: 'btc',
      signals: [
        ...offline,
        {
          signal_id: 'btc.coinbase.aggressor_flow.v1',
          name: 'Coinbase aggressor flow',
          description: 'Net notional by who crossed the spread.',
          location: 'internal',
          scope: underlying,
          live: true,
          quality: { state: 'ready', reasons: [] },
          fields: [
            {
              name: 'signed_notional_usd_10s',
              label: 'Net flow 10s',
              type: 'decimal',
              headline: true,
              value: '5',
            },
          ],
        },
        momentum('kraken', true, 'warming_up'),
        momentum('coinbase', true, 'ready'),
        {
          signal_id: 'btc.polymarket.prediction_quote.v1',
          name: 'Polymarket BTC quote',
          description: 'Bid, ask and midpoint for the Up contract.',
          location: 'internal',
          scope: underlying,
          live: true,
          quality: { state: 'degraded', reasons: ['negative_quote_crossed_or_invalid'] },
          fields: [
            {
              name: 'affirmative_bid',
              label: 'Up bid',
              type: 'decimal',
              headline: true,
              value: '0.61',
            },
            {
              name: 'affirmative_ask',
              label: 'Up ask',
              type: 'decimal',
              headline: true,
              value: '0.63',
            },
          ],
        },
      ],
    }
    server.use(http.get(`${baseUrl}/api/signals`, () => HttpResponse.json(wide)))
    type Found = { total: number; signals: { signal_id: string }[] }

    const polymarket = (await tools.searchSignals.execute?.(
      { query: 'Polymarket YES NO prices' },
      context
    )) as Found
    expect(polymarket.signals[0]?.signal_id).toBe('btc.polymarket.prediction_quote.v1')

    const aggressor = (await tools.searchSignals.execute?.(
      { query: 'net aggressor flow' },
      context
    )) as Found
    expect(aggressor.signals[0]?.signal_id).toBe('btc.coinbase.aggressor_flow.v1')

    const ranked = (await tools.searchSignals.execute?.({ query: 'momentum' }, context)) as Found
    expect(ranked.total).toBe(28)
    expect(ranked.signals).toHaveLength(25)
    expect(ranked.signals.slice(0, 2).map(signal => signal.signal_id)).toEqual([
      'btc.coinbase.micro_momentum.v1',
      'btc.kraken.micro_momentum.v1',
    ])
  })

  it('fails closed without configuration', async () => {
    expect(await createSignalTools(undefined).searchSignals.execute?.({}, context)).toMatchObject({
      status: 'unavailable',
    })
  })
})
