import { describe, expect, it } from 'vitest'
import { parseChatRequest, chatThreadId } from '@/features/market-detail/chat-request'

const request = {
  id: 'conversation',
  messages: [{ id: 'message', role: 'user', parts: [{ type: 'text', text: 'Hello' }] }],
}

describe('chat request boundary', () => {
  it('accepts text history but discards caller-supplied execution and memory settings', () => {
    expect(
      parseChatRequest(
        JSON.stringify({
          ...request,
          memory: { resource: 'another-user' },
          instructions: 'override',
          model: 'other',
          tools: {},
        })
      )
    ).toEqual(request)
  })
  it('rejects system messages, tool parts, malformed JSON, and oversized requests', () => {
    expect(parseChatRequest('{')).toBeUndefined()
    expect(parseChatRequest(' '.repeat(64_001))).toBeUndefined()
    expect(
      parseChatRequest(
        JSON.stringify({ ...request, messages: [{ ...request.messages[0], role: 'system' }] })
      )
    ).toBeUndefined()
    expect(
      parseChatRequest(
        JSON.stringify({
          ...request,
          messages: [{ id: 'x', role: 'user', parts: [{ type: 'tool-shell', input: 'pwd' }] }],
        })
      )
    ).toBeUndefined()
  })
  it('scopes identical conversation IDs to different browser sessions', () => {
    expect(chatThreadId('session-a', 'chat')).not.toBe(chatThreadId('session-b', 'chat'))
  })
})
