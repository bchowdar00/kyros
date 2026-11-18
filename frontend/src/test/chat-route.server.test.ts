import { beforeEach, describe, expect, it, vi } from 'vitest'
import { createUIMessageStream } from 'ai'

vi.hoisted(() => {
  vi.stubEnv('NODE_ENV', 'development')
  vi.stubEnv('EXCHANGE_API_URL', 'https://exchange.test')
  vi.stubEnv('ANTHROPIC_API_KEY', 'test-key')
})

// Keep agent construction (and the whole Mastra graph) out of this route test.
vi.mock('@/mastra/agents/teeny', () => ({ TEENY_AGENT_ID: 'teeny', TEENY_MAX_STEPS: 8 }))
const session = vi.hoisted(() => ({ enabled: true, resource: 'alice' as string | null }))
vi.mock('@/mastra/chat-session', () => ({
  chatEnabled: () => session.enabled,
  chatResource: async () => session.resource,
}))
const meter = vi.hoisted(() => ({ charge: vi.fn(), report: vi.fn() }))
vi.mock('@/mastra/writer-meter-client', () => ({
  chargeWriter: meter.charge,
  reportWriterUsage: meter.report,
}))
const chat = vi.hoisted(() => ({ handle: vi.fn() }))
vi.mock('@mastra/ai-sdk', () => ({ handleChatStream: chat.handle }))
vi.mock('@/mastra', () => ({
  mastra: { getAgent: () => ({ getMemory: async () => ({}) }) },
}))
vi.mock('@/mastra/chat-history', () => ({ prepareChat: async () => undefined }))
vi.mock('@/mastra/market-context', () => ({ marketSystemMessage: async () => 'SYSTEM' }))

import { POST } from '@/app/api/chat/route'

function post(body: unknown, origin = 'http://localhost') {
  return POST(
    new Request('http://localhost/api/chat', {
      method: 'POST',
      headers: { origin, 'content-type': 'application/json', 'accept-language': 'en-US' },
      body: JSON.stringify(body),
    })
  )
}

const request = {
  id: 'c1',
  marketSlug: 'btc-2m',
  messages: [{ id: 'm1', role: 'user', parts: [{ type: 'text', text: 'hi' }] }],
}

beforeEach(() => {
  session.enabled = true
  session.resource = 'alice'
  meter.charge.mockReset()
  meter.report.mockReset()
  chat.handle.mockReset()
  chat.handle.mockResolvedValue(
    createUIMessageStream({
      execute: ({ writer }) => {
        writer.write({ type: 'text-start', id: 'x' })
        writer.write({ type: 'text-end', id: 'x' })
      },
    })
  )
})

describe('POST /api/chat metering', () => {
  it('charges a chat turn, then calls the model on a granted charge', async () => {
    meter.charge.mockResolvedValue({ ok: true, remainingGuest: 39, remainingSite: 1999 })
    const response = await post(request)
    expect(response.status).toBe(200)
    expect(meter.charge).toHaveBeenCalledWith(
      expect.objectContaining({ guest: 'alice', address: null, kind: 'chat' })
    )
    expect(chat.handle).toHaveBeenCalledTimes(1)
  })

  it('on a 429 resting charge, calls no model and streams the chat resting message', async () => {
    meter.charge.mockResolvedValue({
      ok: false,
      reason: 'resting',
      message: 'Chat is resting until 12:00 AM UTC; templates still deploy',
    })
    const response = await post(request)
    expect(response.status).toBe(200)
    expect(chat.handle).not.toHaveBeenCalled()
    const body = await response.text()
    expect(body).toContain('Chat is resting until')
    expect(body).toContain('templates still deploy')
  })

  it('fails closed with a plain message when the meter is unavailable', async () => {
    meter.charge.mockResolvedValue({
      ok: false,
      reason: 'unavailable',
      message: 'Chat is unavailable right now; templates still deploy.',
    })
    const response = await post(request)
    expect(chat.handle).not.toHaveBeenCalled()
    const body = await response.text()
    expect(body).toContain('Chat is unavailable right now')
  })

  it('surfaces the not-switched-on line when the meter is unconfigured, calling no model', async () => {
    meter.charge.mockResolvedValue({
      ok: false,
      reason: 'unavailable',
      message: 'The strategy writer is not switched on here; templates still deploy.',
    })
    const response = await post(request)
    expect(chat.handle).not.toHaveBeenCalled()
    const body = await response.text()
    expect(body).toContain('The strategy writer is not switched on here')
  })

  it('passes the account address to the meter when the client sends one', async () => {
    meter.charge.mockResolvedValue({ ok: true, remainingGuest: 1, remainingSite: 1 })
    await post({ ...request, accountAddress: '0xabc' })
    expect(meter.charge).toHaveBeenCalledWith(expect.objectContaining({ address: '0xabc' }))
  })

  it('reports the turn token usage to the meter when the chat turn finishes', async () => {
    meter.charge.mockResolvedValue({ ok: true, remainingGuest: 1, remainingSite: 1 })
    // The model runner calls the onFinish the route hands it; drive it like a finished turn.
    chat.handle.mockImplementation(
      async (options: { params: { onFinish?: (event: unknown) => unknown } }) => {
        await options.params.onFinish?.({
          totalUsage: { inputTokens: 123, outputTokens: 45 },
          model: { modelId: 'claude-opus-5-5' },
        })
        return createUIMessageStream({
          execute: ({ writer }) => {
            writer.write({ type: 'text-start', id: 'x' })
            writer.write({ type: 'text-end', id: 'x' })
          },
        })
      }
    )
    await post(request)
    expect(meter.report).toHaveBeenCalledWith(
      expect.objectContaining({
        guest: 'alice',
        model: 'claude-opus-5-5',
        inputTokens: 123,
        outputTokens: 45,
      })
    )
  })

  it('still answers when usage reporting is impossible (no usage exposed)', async () => {
    meter.charge.mockResolvedValue({ ok: true, remainingGuest: 1, remainingSite: 1 })
    chat.handle.mockImplementation(
      async (options: { params: { onFinish?: (event: unknown) => unknown } }) => {
        await options.params.onFinish?.({ totalUsage: {}, model: undefined })
        return createUIMessageStream({
          execute: ({ writer }) => {
            writer.write({ type: 'text-start', id: 'x' })
            writer.write({ type: 'text-end', id: 'x' })
          },
        })
      }
    )
    const response = await post(request)
    expect(response.status).toBe(200)
    // Falls back to the configured model; counts default to zero.
    expect(meter.report).toHaveBeenCalledWith(
      expect.objectContaining({ inputTokens: 0, outputTokens: 0 })
    )
  })
})
