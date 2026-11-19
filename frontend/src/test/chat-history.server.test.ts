// @vitest-environment node
import { afterEach, describe, expect, it } from 'vitest'
import { Memory } from '@mastra/memory'
import { LibSQLStore } from '@mastra/libsql'
import { mkdtemp, rm } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { chatTitle } from '@/features/market-detail/saved-chat'
import { deleteSavedChat, listSavedChats, prepareChat, readSavedChat } from '@/mastra/chat-history'

const directories: string[] = []
afterEach(async () => {
  await Promise.all(directories.splice(0).map(path => rm(path, { recursive: true, force: true })))
})

describe('saved chat storage', () => {
  it('names chats from the first message with a bounded single-line title', () => {
    expect(chatTitle('  BTC\nprice? ')).toBe('BTC price?')
    expect(chatTitle('a'.repeat(100))).toHaveLength(62)
    expect(chatTitle('  ')).toBe('New chat')
  })
  it('persists titles and messages across connections, scopes users and markets, and preserves history', async () => {
    const directory = await mkdtemp(join(tmpdir(), 'kyros-chat-'))
    directories.push(directory)
    const storage = new LibSQLStore({ id: 'test-chat', url: `file:${join(directory, 'test.db')}` })
    const memory = new Memory({ storage })
    const thread = await prepareChat(memory, 'alice', 'chat-1', 'btc', '  What is BTC doing?  ')
    await prepareChat(memory, 'bob', 'chat-1', 'btc', 'Bob private chat')
    await prepareChat(memory, 'alice', 'chat-eth', 'eth', 'ETH chat')
    await memory.saveMessages({
      messages: [
        {
          id: 'alice-question',
          threadId: thread.id,
          resourceId: 'alice',
          role: 'user',
          createdAt: new Date(1000),
          content: { format: 2, parts: [{ type: 'text', text: 'What is BTC doing?' }] },
        },
        {
          id: 'alice-answer',
          threadId: thread.id,
          resourceId: 'alice',
          role: 'assistant',
          createdAt: new Date(2000),
          content: {
            format: 2,
            parts: [
              {
                type: 'tool-invocation',
                toolInvocation: {
                  state: 'result',
                  toolCallId: 'read',
                  toolName: 'getMarket',
                  args: {},
                  result: { status: 'ok' },
                },
              },
              {
                type: 'tool-invocation',
                toolInvocation: {
                  state: 'result',
                  toolCallId: 'draw',
                  toolName: 'showContent',
                  args: { kind: 'markdown', text: 'x' },
                  result: { ok: true, spec: { kind: 'markdown', text: 'x' } },
                },
              },
              { type: 'text', text: 'An observed answer.' },
            ],
          },
        },
      ],
    })
    await storage.close()
    const reopenedStore = new LibSQLStore({
      id: 'reopened-chat',
      url: `file:${join(directory, 'test.db')}`,
    })
    const reconnected = new Memory({ storage: reopenedStore })
    expect(await listSavedChats(reconnected, 'alice', 'btc')).toMatchObject({
      chats: [{ id: 'chat-1', title: 'What is BTC doing?' }],
      hasMore: false,
    })
    const reopened = await readSavedChat(reconnected, 'alice', 'chat-1')
    expect(reopened?.messages).toHaveLength(2)
    // The widget call is kept as a finished tool part; the read tool call is dropped.
    expect(reopened?.messages[1]?.parts).toEqual([
      {
        type: 'tool-showContent',
        toolCallId: 'draw',
        state: 'output-available',
        input: { kind: 'markdown', text: 'x' },
        output: { ok: true, spec: { kind: 'markdown', text: 'x' } },
      },
      { type: 'text', text: 'An observed answer.' },
    ])
    expect((await readSavedChat(reconnected, 'bob', 'chat-1'))?.messages).toHaveLength(0)
    expect(await readSavedChat(reconnected, 'mallory', 'chat-1')).toBeNull()
    await expect(prepareChat(reconnected, 'alice', 'chat-1', 'eth', 'overwrite')).rejects.toThrow(
      'Chat unavailable'
    )
    expect((await prepareChat(reconnected, 'alice', 'chat-1', 'btc', 'New question')).title).toBe(
      'What is BTC doing?'
    )
    // Deleting is scoped like reading: another browser cannot remove the chat.
    expect(await deleteSavedChat(reconnected, 'mallory', 'chat-1')).toBe(false)
    expect(await deleteSavedChat(reconnected, 'alice', 'chat-1')).toBe(true)
    expect(await readSavedChat(reconnected, 'alice', 'chat-1')).toBeNull()
    expect((await listSavedChats(reconnected, 'alice', 'btc')).chats).toEqual([])
    expect(await deleteSavedChat(reconnected, 'alice', 'chat-1')).toBe(false)
    expect((await readSavedChat(reconnected, 'bob', 'chat-1'))?.messages).toHaveLength(0)
    await reopenedStore.close()
  })
})
