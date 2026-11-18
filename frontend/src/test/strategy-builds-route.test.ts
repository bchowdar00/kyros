// @vitest-environment node
import { beforeEach, describe, expect, it, vi } from 'vitest'

vi.hoisted(() => {
  // The build routes exist in local development only, like the chat routes.
  vi.stubEnv('NODE_ENV', 'development')
  vi.stubEnv('EXCHANGE_API_URL', 'https://exchange.test')
  vi.stubEnv('ANTHROPIC_API_KEY', 'test-key')
})

const session = vi.hoisted(() => ({ resource: 'alice' as string | null }))
vi.mock('@/mastra/chat-session', () => ({ chatResource: async () => session.resource }))
const builds = vi.hoisted(() => ({ start: vi.fn(async () => 'run-1'), read: vi.fn() }))
vi.mock('@/mastra', () => ({ mastra: {} }))
vi.mock('@/mastra/strategy-builds', () => ({ startBuild: builds.start, readBuild: builds.read }))
// The writer meter gates every build; each test sets the charge outcome in beforeEach.
const meter = vi.hoisted(() => ({ charge: vi.fn() }))
vi.mock('@/mastra/writer-meter-client', () => ({ chargeWriter: meter.charge }))

import { POST } from '@/app/api/strategy/builds/route'
import { GET } from '@/app/api/strategy/builds/[id]/route'

const brief = {
  series: 'btc-2m',
  summary: 'Buys up once the price crosses 60 cents and sells at a 5 cent gain.',
  goal: 'Buy up when it gets rich',
  entry: 'up price crosses above 60 cents',
  signal_ids: [],
  outcome: 'up',
  recurrence: 'once_per_round',
  interval: null,
  entry_style: 'market',
  exit: 'hold to settlement',
  allocation: '100',
  round_share: '10%',
  trade_share: '25%',
  stop_after_loss: '100',
  stop_before_close: '10s',
  assumptions: ['Full size per order.'],
}

function post(body: unknown, origin = 'http://localhost') {
  return POST(
    new Request('http://localhost/api/strategy/builds', {
      method: 'POST',
      headers: { origin, 'content-type': 'application/json' },
      body: JSON.stringify(body),
    })
  )
}

beforeEach(() => {
  session.resource = 'alice'
  builds.start.mockClear()
  builds.read.mockReset()
  meter.charge.mockClear()
  meter.charge.mockResolvedValue({ ok: true, remainingGuest: 7, remainingSite: 299 })
})

describe('POST /api/strategy/builds', () => {
  it('charges then starts a build for a well-formed brief and answers its id', async () => {
    const response = await post({ brief })
    expect(response.status).toBe(202)
    expect(await response.json()).toEqual({ id: 'run-1' })
    expect(meter.charge).toHaveBeenCalledWith(
      expect.objectContaining({ guest: 'alice', address: null, kind: 'build' })
    )
    expect(builds.start).toHaveBeenCalledWith({}, 'alice', brief)
  })
  it('refuses a cross-origin call', async () => {
    expect((await post({ brief }, 'http://evil.test')).status).toBe(403)
    expect(builds.start).not.toHaveBeenCalled()
  })
  it('refuses a malformed brief', async () => {
    expect((await post({ brief: { ...brief, recurrence: 'sometimes' } })).status).toBe(400)
    expect(builds.start).not.toHaveBeenCalled()
  })
  it('refuses a caller without a chat session', async () => {
    session.resource = null
    expect((await post({ brief })).status).toBe(403)
  })
  it('on a 429 resting charge, starts no build and returns the resting message', async () => {
    meter.charge.mockResolvedValue({
      ok: false,
      reason: 'resting',
      message: 'The strategy writer is resting until 12:00 AM UTC; templates still deploy',
    })
    const response = await post({ brief })
    expect(response.status).toBe(429)
    expect(await response.json()).toMatchObject({
      message: 'The strategy writer is resting until 12:00 AM UTC; templates still deploy',
    })
    // A refused charge calls no model: the build never starts.
    expect(builds.start).not.toHaveBeenCalled()
  })
  it('fails closed when the meter is unavailable, starting no build', async () => {
    meter.charge.mockResolvedValue({
      ok: false,
      reason: 'unavailable',
      message: 'The strategy writer is unavailable right now; templates still deploy.',
    })
    const response = await post({ brief })
    expect(response.status).toBe(503)
    expect(builds.start).not.toHaveBeenCalled()
  })
  it('surfaces the not-switched-on line (503) when the meter is unconfigured, starting no build', async () => {
    meter.charge.mockResolvedValue({
      ok: false,
      reason: 'unavailable',
      message: 'The strategy writer is not switched on here; templates still deploy.',
    })
    const response = await post({ brief })
    expect(response.status).toBe(503)
    expect(await response.json()).toMatchObject({
      message: 'The strategy writer is not switched on here; templates still deploy.',
    })
    expect(builds.start).not.toHaveBeenCalled()
  })
})

describe('GET /api/strategy/builds/{id}', () => {
  const context = { params: Promise.resolve({ id: 'run-1' }) }
  it('answers the build the session owns', async () => {
    const status = { id: 'run-1', status: 'running', steps: [], result: null, error: null }
    builds.read.mockResolvedValue(status)
    const response = await GET(new Request('http://localhost/api/strategy/builds/run-1'), context)
    expect(response.status).toBe(200)
    expect(await response.json()).toEqual(status)
    expect(builds.read).toHaveBeenCalledWith({}, 'alice', 'run-1')
  })
  it('hides builds the session does not own', async () => {
    builds.read.mockResolvedValue(null)
    const response = await GET(new Request('http://localhost/api/strategy/builds/run-1'), context)
    expect(response.status).toBe(404)
  })
})
