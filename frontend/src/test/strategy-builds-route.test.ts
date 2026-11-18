// @vitest-environment node
import { beforeEach, describe, expect, it, vi } from 'vitest'

vi.hoisted(() => {
  // The build routes exist in local development only, like the chat routes.
  vi.stubEnv('NODE_ENV', 'development')
  vi.stubEnv('EXCHANGE_API_URL', 'https://exchange.test')
  vi.stubEnv('OPENAI_API_KEY', 'sk-test')
})

const session = vi.hoisted(() => ({ resource: 'alice' as string | null }))
vi.mock('@/mastra/chat-session', () => ({ chatResource: async () => session.resource }))
const builds = vi.hoisted(() => ({ start: vi.fn(async () => 'run-1'), read: vi.fn() }))
vi.mock('@/mastra', () => ({ mastra: {} }))
vi.mock('@/mastra/strategy-builds', () => ({ startBuild: builds.start, readBuild: builds.read }))

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
})

describe('POST /api/strategy/builds', () => {
  it('starts a build for a well-formed brief and answers its id', async () => {
    const response = await post({ brief })
    expect(response.status).toBe(202)
    expect(await response.json()).toEqual({ id: 'run-1' })
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
