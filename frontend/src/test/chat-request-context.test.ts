import { describe, expect, it } from 'vitest'
import { createChatContext, readChatContext } from '@/mastra/request-context'

describe('chat request context', () => {
  it('round-trips what the chat route knows into a tool', () => {
    const requestContext = createChatContext({
      marketSlug: 'btc-5m',
      resourceId: 'resource',
      accountAddress: '0xabc',
    })
    expect(readChatContext({ requestContext })).toEqual({
      marketSlug: 'btc-5m',
      resourceId: 'resource',
      accountAddress: '0xabc',
    })
  })
  it('omits the account when the client did not send one', () => {
    const requestContext = createChatContext({ marketSlug: 'standalone-chat', resourceId: 'r' })
    expect(readChatContext({ requestContext })).toEqual({
      marketSlug: 'standalone-chat',
      resourceId: 'r',
    })
  })
  it('is empty outside the chat route', () => {
    expect(readChatContext(undefined)).toEqual({})
    expect(readChatContext({})).toEqual({})
  })
})
