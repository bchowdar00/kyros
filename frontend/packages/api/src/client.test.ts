import { HttpResponse, delay, http } from 'msw'
import { setupServer } from 'msw/node'
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest'
import * as z from 'zod/mini'

import { createApiClient } from './client'

const MarketSchema = z.object({ id: z.int(), price_cents: z.int() })
const server = setupServer()
const noSleep = () => Promise.resolve()

beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => server.resetHandlers())
afterAll(() => server.close())

function client(overrides: Partial<Parameters<typeof createApiClient>[0]> = {}) {
  return createApiClient({ baseUrl: 'https://api.test/v1', sleep: noSleep, ...overrides })
}

describe('createApiClient', () => {
  it('parses a successful response through the schema', async () => {
    server.use(
      http.get('https://api.test/v1/markets/300001', ({ request }) => {
        expect(new URL(request.url).searchParams.get('include')).toBe('quotes')
        expect(request.headers.get('accept')).toBe('application/json')

        return HttpResponse.json({ id: 300_001, price_cents: 31 })
      })
    )

    const result = await client().get('/markets/300001', {
      schema: MarketSchema,
      searchParams: { include: 'quotes', omitted: undefined },
    })

    expect(result).toEqual({ ok: true, data: { id: 300_001, price_cents: 31 } })
  })

  it('reports a schema mismatch as a parse failure with the path', async () => {
    server.use(
      http.get('https://api.test/v1/markets/1', () =>
        HttpResponse.json({ id: 1, price_cents: 31.5 })
      )
    )

    const result = await client().get('/markets/1', { schema: MarketSchema })

    expect(result.ok).toBe(false)
    if (!result.ok) {
      expect(result.error.step).toBe('parse')
      expect(result.error.message).toContain('price_cents')
      expect(result.error.retryable).toBe(false)
    }
  })

  it('retries GET on a 503 and succeeds on the next attempt', async () => {
    let calls = 0
    server.use(
      http.get('https://api.test/v1/markets/1', () => {
        calls += 1

        return calls === 1
          ? new HttpResponse(null, { status: 503 })
          : HttpResponse.json({ id: 1, price_cents: 40 })
      })
    )

    const result = await client().get('/markets/1', { schema: MarketSchema })

    expect(calls).toBe(2)
    expect(result.ok).toBe(true)
  })

  it('does not retry a 404', async () => {
    let calls = 0
    server.use(
      http.get('https://api.test/v1/markets/1', () => {
        calls += 1

        return new HttpResponse(null, { status: 404 })
      })
    )

    const result = await client().get('/markets/1', { schema: MarketSchema })

    expect(calls).toBe(1)
    expect(result.ok).toBe(false)
    if (!result.ok) {
      expect(result.error.step).toBe('http')
      expect(result.error.status).toBe(404)
    }
  })

  it('gives up after the configured number of attempts', async () => {
    let calls = 0
    server.use(
      http.get('https://api.test/v1/markets/1', () => {
        calls += 1

        return HttpResponse.error()
      })
    )

    const result = await client({ retry: { attempts: 3 } }).get('/markets/1', {
      schema: MarketSchema,
    })

    expect(calls).toBe(3)
    expect(result.ok).toBe(false)
    if (!result.ok) {
      expect(result.error.step).toBe('network')
    }
  })

  it('times out and reports the timeout step', async () => {
    server.use(
      http.get('https://api.test/v1/markets/1', async () => {
        await delay('infinite')

        return HttpResponse.json({})
      })
    )

    const result = await client({ retry: { attempts: 1 }, timeoutMs: 30 }).get('/markets/1', {
      schema: MarketSchema,
    })

    expect(result.ok).toBe(false)
    if (!result.ok) {
      expect(result.error.step).toBe('timeout')
    }
  })

  it('sends JSON and only retries a POST that carries an idempotency key', async () => {
    let plainCalls = 0
    let keyedCalls = 0
    server.use(
      http.post('https://api.test/v1/orders', async ({ request }) => {
        const key = request.headers.get('idempotency-key')
        expect(await request.json()).toEqual({ outcome_id: 400_001, size: 10 })

        if (key === null) {
          plainCalls += 1

          return new HttpResponse(null, { status: 503 })
        }

        keyedCalls += 1

        return keyedCalls === 1
          ? new HttpResponse(null, { status: 503 })
          : HttpResponse.json({ id: 1, price_cents: 31 })
      })
    )

    const plain = await client().post('/orders', {
      body: { outcome_id: 400_001, size: 10 },
      schema: MarketSchema,
    })
    const keyed = await client().post('/orders', {
      body: { outcome_id: 400_001, size: 10 },
      idempotencyKey: 'order-abc',
      schema: MarketSchema,
    })

    expect(plainCalls).toBe(1)
    expect(plain.ok).toBe(false)
    expect(keyedCalls).toBe(2)
    expect(keyed.ok).toBe(true)
  })
})
