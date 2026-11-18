import { HttpResponse, http } from 'msw'
import { setupServer } from 'msw/node'
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest'
import { createPriceFeedsClient } from './price-feeds'

const server = setupServer()
beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => server.resetHandlers())
afterAll(() => server.close())
const tick = {
  v: 1,
  provider: 'PYTH_LAZER',
  symbol: 'Crypto.BTC/USD',
  mantissa: '9007199254740993',
  expo: -8,
  publishTimeUs: 1788610000000000,
}
const client = () =>
  createPriceFeedsClient({ baseUrl: 'http://feeds.test', timeoutMs: 100, retry: { attempts: 1 } })

describe('Price Feeds public reads', () => {
  it('reads fragmented SSE after a heartbeat, preserves integer precision, and closes the stream', async () => {
    let closed = false
    server.use(
      http.get('http://feeds.test/v1/stream/prices', ({ request }) => {
        expect(new URL(request.url).searchParams.get('symbols')).toBe(tick.symbol)
        request.signal.addEventListener('abort', () => {
          closed = true
        })
        const wire = `: keep-alive\r\n\r\ndata: ${JSON.stringify(tick)}\r\n\r\n`
        return new HttpResponse(
          new ReadableStream({
            start(controller) {
              const encoder = new TextEncoder()
              controller.enqueue(encoder.encode(wire.slice(0, 40)))
              controller.enqueue(encoder.encode(wire.slice(40)))
            },
            cancel() {
              closed = true
            },
          }),
          { headers: { 'content-type': 'text/event-stream' } }
        )
      })
    )
    expect(await client().getLivePrice(tick.symbol)).toEqual({ ok: true, data: tick })
    expect(closed).toBe(true)
  })
  it.each([
    { ...tick, symbol: 'Crypto.ETH/USD' },
    { ...tick, mantissa: 9007199254740992 },
    { ...tick, publishTimeUs: 1.5 },
    { ...tick, v: 2 },
  ])('rejects wrong symbols and invalid observations', async payload => {
    server.use(
      http.get(
        'http://feeds.test/v1/stream/prices',
        () =>
          new HttpResponse(`data: ${JSON.stringify(payload)}\n\n`, {
            headers: { 'content-type': 'text/event-stream' },
          })
      )
    )
    expect((await client().getLivePrice(tick.symbol)).ok).toBe(false)
  })
  it('bounds silent streams and honors cancellation', async () => {
    server.use(
      http.get(
        'http://feeds.test/v1/stream/prices',
        ({ request }) =>
          new HttpResponse(
            new ReadableStream({
              start(controller) {
                request.signal.addEventListener('abort', () =>
                  controller.error(request.signal.reason)
                )
              },
            }),
            { headers: { 'content-type': 'text/event-stream' } }
          )
      )
    )
    expect((await client().getLivePrice(tick.symbol)).ok).toBe(false)
    expect((await client().getLivePrice(tick.symbol, AbortSignal.abort())).ok).toBe(false)
  })
  it('validates history and transmits bounded sampling parameters', async () => {
    const history = {
      s: 'ok',
      points: [{ t: 100, mantissa: tick.mantissa, expo: -8 }],
      firstTickAtUs: 100000000,
      lastTickAtUs: 100000000,
    }
    server.use(
      http.get('http://feeds.test/v1/prices', ({ request }) => {
        expect(new URL(request.url).searchParams.get('resolution')).toBe('60')
        return HttpResponse.json(history)
      })
    )
    expect(await client().getHistory(tick.symbol, 100, 200)).toEqual({ ok: true, data: history })
  })
  it('returns failure on a service error or an invalid asset catalog', async () => {
    server.use(
      http.get('http://feeds.test/v1/assets', () =>
        HttpResponse.json({ assets: [{ symbol: 'bad' }] })
      )
    )
    expect((await client().listAssets()).ok).toBe(false)
    server.use(
      http.get('http://feeds.test/v1/assets', () => new HttpResponse(null, { status: 503 }))
    )
    expect((await client().listAssets()).ok).toBe(false)
  })
})
