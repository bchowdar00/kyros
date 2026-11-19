import { HttpResponse, http } from 'msw'
import { setupServer } from 'msw/node'
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest'
import {
  createVenueRegistry,
  instrumentId,
  renderSignalDefinitions,
  renderSignalIndex,
  selectSignals,
  signalIndex,
} from '@/mastra/capabilities/strategy/registry'

const server = setupServer()
beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => server.resetHandlers())
afterAll(() => server.close())

const quality = { state: 'ok', reasons: [] }
const catalogue = {
  series: 'btc-5m',
  underlying: 'btc',
  signals: [
    {
      signal_id: 'btc-5m.current.price.v1',
      name: 'Market Price',
      description:
        'Top of book and last trade for the current btc-5m round, as up-side probabilities; down is the complement.',
      location: 'internal',
      scope: { kind: 'series', series_id: 'btc-5m' },
      live: true,
      quality,
      fields: [
        { name: 'up', label: 'Up', type: 'decimal', headline: true },
        { name: 'bid', label: 'Best Bid', type: 'decimal', headline: true },
      ],
    },
    {
      signal_id: 'btc.kraken.rsi.v1',
      name: 'Kraken RSI',
      description:
        'A causal reading over the Kraken midpoint. Rsi_14 is Wilder’s relative strength index. regime is oversold at or below thirty.',
      location: 'internal',
      scope: { kind: 'underlying', underlying: 'btc' },
      lane: 'spot',
      live: true,
      quality,
      fields: [
        { name: 'rsi_14', label: 'RSI 14', type: 'decimal', headline: true },
        {
          name: 'regime',
          label: 'Regime',
          type: 'enum',
          variants: ['oversold', 'neutral', 'overbought'],
          headline: true,
        },
      ],
    },
    {
      signal_id: 'btc.kalshi.tick_velocity.v1',
      name: 'Kalshi tick velocity',
      description: 'Never published locally.',
      location: 'internal',
      scope: { kind: 'underlying', underlying: 'btc' },
      lane: 'kalshi',
      live: false,
      quality: { state: 'unknown', reasons: [] },
      fields: [{ name: 'velocity', label: 'Velocity', type: 'decimal', headline: true }],
    },
  ],
}

describe('venue registry', () => {
  it('fetches the catalogue scoped to the series', async () => {
    server.use(
      http.get('http://exchange.test/api/signals', ({ request }) => {
        expect(new URL(request.url).searchParams.get('series')).toBe('btc-5m')
        return HttpResponse.json(catalogue)
      })
    )
    const result = await createVenueRegistry('http://exchange.test').catalogue('btc-5m')
    expect(result.ok).toBe(true)
    if (result.ok) expect(result.data.signals).toHaveLength(3)
  })

  it('indexes only live signals with a one-sentence summary and typed fields', () => {
    const index = signalIndex(catalogue as never)
    expect(index.map(entry => entry.id)).toEqual(['btc-5m.current.price.v1', 'btc.kraken.rsi.v1'])
    expect(index[1]).toEqual({
      id: 'btc.kraken.rsi.v1',
      name: 'Kraken RSI',
      summary: 'A causal reading over the Kraken midpoint.',
      fields: 'rsi_14:decimal, regime:enum(oversold|neutral|overbought)',
    })
    expect(renderSignalIndex(index)).toContain(
      '- btc.kraken.rsi.v1 — Kraken RSI. Fields: rsi_14:decimal'
    )
  })

  it('selects full definitions by id and reports unknown ids', () => {
    const { chosen, unknown } = selectSignals(catalogue as never, ['btc.kraken.rsi.v1', 'nope.v1'])
    expect(chosen.map(signal => signal.signal_id)).toEqual(['btc.kraken.rsi.v1'])
    expect(unknown).toEqual(['nope.v1'])
    const rendered = renderSignalDefinitions(chosen)
    expect(rendered).toContain('### btc.kraken.rsi.v1')
    expect(rendered).toContain('regime is oversold at or below thirty')
    expect(rendered).toContain('- regime (enum: oversold, neutral, overbought) — Regime')
  })

  it('names the series instrument the way the registry does', () => {
    expect(instrumentId('btc-5m')).toBe('btc-5m.current')
  })
})
