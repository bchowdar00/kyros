import { describe, expect, it, vi } from 'vitest'
import { chargeWriter, reportWriterUsage, restingMessage } from '@/mastra/writer-meter-client'

const EXCHANGE = 'https://exchange.test'

function jsonResponse(status: number, body: unknown) {
  return new Response(JSON.stringify(body), {
    status,
    headers: { 'content-type': 'application/json' },
  })
}

// A UTC midnight, for a deterministic resting time.
const RESET_MS = Date.UTC(2026, 10, 19, 0, 0, 0)

function expectedTime(locale: string) {
  return new Intl.DateTimeFormat(locale, {
    hour: 'numeric',
    minute: '2-digit',
    timeZoneName: 'short',
    timeZone: 'UTC',
  }).format(new Date(RESET_MS))
}

describe('restingMessage', () => {
  it('is the exact build line, in the given locale', () => {
    expect(restingMessage('build', RESET_MS, 'en-US')).toBe(
      `The strategy writer is resting until ${expectedTime('en-US')}; templates still deploy`
    )
  })
  it('is the exact chat line', () => {
    expect(restingMessage('chat', RESET_MS, 'en-US')).toBe(
      `Chat is resting until ${expectedTime('en-US')}; templates still deploy`
    )
  })
  it('falls back to en-US for a malformed Accept-Language', () => {
    expect(restingMessage('chat', RESET_MS, '???')).toBe(restingMessage('chat', RESET_MS, 'en-US'))
  })
})

describe('chargeWriter', () => {
  it('returns the remaining allowances on a granted charge', async () => {
    const fetchImpl = vi.fn(async () =>
      jsonResponse(200, { remaining_guest: 5, remaining_site: 99 })
    )
    const result = await chargeWriter({
      exchangeUrl: EXCHANGE,
      guest: 'g',
      address: null,
      kind: 'build',
      fetchImpl: fetchImpl as unknown as typeof fetch,
    })
    expect(result).toEqual({ ok: true, remainingGuest: 5, remainingSite: 99 })
    const [url, init] = fetchImpl.mock.calls[0] as unknown as [URL, RequestInit]
    expect(url.toString()).toBe('https://exchange.test/api/writer/charge')
    expect(JSON.parse(init.body as string)).toEqual({ guest: 'g', address: null, kind: 'build' })
  })

  it('produces the exact build resting line on a 429', async () => {
    const fetchImpl = vi.fn(async () =>
      jsonResponse(429, { scope: 'guest', resets_at_ms: RESET_MS })
    )
    const result = await chargeWriter({
      exchangeUrl: EXCHANGE,
      guest: 'g',
      address: 'addr',
      kind: 'build',
      acceptLanguage: 'en-US',
      fetchImpl: fetchImpl as unknown as typeof fetch,
    })
    expect(result).toEqual({
      ok: false,
      reason: 'resting',
      message: `The strategy writer is resting until ${expectedTime('en-US')}; templates still deploy`,
    })
  })

  it('produces the exact chat resting line on a 429', async () => {
    const fetchImpl = vi.fn(async () =>
      jsonResponse(429, { scope: 'site', resets_at_ms: RESET_MS })
    )
    const result = await chargeWriter({
      exchangeUrl: EXCHANGE,
      guest: 'g',
      address: null,
      kind: 'chat',
      acceptLanguage: 'en-US',
      fetchImpl: fetchImpl as unknown as typeof fetch,
    })
    expect(result).toMatchObject({
      ok: false,
      reason: 'resting',
      message: `Chat is resting until ${expectedTime('en-US')}; templates still deploy`,
    })
  })

  it('fails closed when the meter cannot be reached', async () => {
    const fetchImpl = vi.fn(async () => {
      throw new Error('network down')
    })
    const result = await chargeWriter({
      exchangeUrl: EXCHANGE,
      guest: 'g',
      address: null,
      kind: 'chat',
      fetchImpl: fetchImpl as unknown as typeof fetch,
    })
    expect(result).toMatchObject({ ok: false, reason: 'unavailable' })
  })

  it('fails closed on an unexpected status', async () => {
    const fetchImpl = vi.fn(async () => jsonResponse(500, { error: 'boom' }))
    const result = await chargeWriter({
      exchangeUrl: EXCHANGE,
      guest: 'g',
      address: null,
      kind: 'build',
      fetchImpl: fetchImpl as unknown as typeof fetch,
    })
    expect(result).toMatchObject({ ok: false, reason: 'unavailable' })
  })

  const NOT_SWITCHED_ON = 'The strategy writer is not switched on here; templates still deploy.'

  it('treats a 503 (meter unconfigured) as the writer not switched on, no model call', async () => {
    const fetchImpl = vi.fn(async () => jsonResponse(503, { error: 'writer_meter_unconfigured' }))
    const result = await chargeWriter({
      exchangeUrl: EXCHANGE,
      guest: 'g',
      address: null,
      kind: 'build',
      fetchImpl: fetchImpl as unknown as typeof fetch,
    })
    expect(result).toEqual({ ok: false, reason: 'unavailable', message: NOT_SWITCHED_ON })
  })

  it('treats a 401 (wrong/absent token) as the writer not switched on, for chat too', async () => {
    const fetchImpl = vi.fn(async () => jsonResponse(401, { error: 'unauthorized' }))
    const result = await chargeWriter({
      exchangeUrl: EXCHANGE,
      guest: 'g',
      address: null,
      kind: 'chat',
      fetchImpl: fetchImpl as unknown as typeof fetch,
    })
    expect(result).toEqual({ ok: false, reason: 'unavailable', message: NOT_SWITCHED_ON })
  })
})

describe('reportWriterUsage', () => {
  it('posts the token counts and never throws on failure', async () => {
    const fetchImpl = vi.fn(async () => jsonResponse(204, {}))
    await reportWriterUsage({
      exchangeUrl: EXCHANGE,
      guest: 'g',
      model: 'claude-opus-5-5',
      inputTokens: 10,
      outputTokens: 2,
      fetchImpl: fetchImpl as unknown as typeof fetch,
    })
    const [url, init] = fetchImpl.mock.calls[0] as unknown as [URL, RequestInit]
    expect(url.toString()).toBe('https://exchange.test/api/writer/usage')
    expect(JSON.parse(init.body as string)).toEqual({
      guest: 'g',
      model: 'claude-opus-5-5',
      input_tokens: 10,
      output_tokens: 2,
    })
    const throwing = vi.fn(async () => {
      throw new Error('down')
    })
    await expect(
      reportWriterUsage({
        exchangeUrl: EXCHANGE,
        guest: 'g',
        model: 'm',
        inputTokens: 1,
        outputTokens: 1,
        fetchImpl: throwing as unknown as typeof fetch,
      })
    ).resolves.toBeUndefined()
  })
})
