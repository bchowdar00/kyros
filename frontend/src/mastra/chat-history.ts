import type { MastraMemory } from '@mastra/core/memory'
import { chatThreadId } from '../features/market-detail/chat-request'
import { chatTitle } from '../features/market-detail/saved-chat'

/** The agent's widget tools, whose calls a reopened chat keeps so the widgets rebind. */
const SHOW_TOOL_PREFIX = 'show'

export async function prepareChat(
  memory: MastraMemory,
  resourceId: string,
  id: string,
  marketSlug: string,
  prompt: string
) {
  const threadId = chatThreadId(resourceId, id)
  const thread = await memory.getThreadById({ threadId })
  if (thread) {
    if (thread.resourceId !== resourceId || thread.metadata?.['marketSlug'] !== marketSlug)
      throw new Error('Chat unavailable')
    return thread
  }
  return memory.createThread({
    threadId,
    resourceId,
    title: chatTitle(prompt),
    metadata: { marketSlug, conversationId: id },
  })
}

export async function listSavedChats(
  memory: MastraMemory,
  resourceId: string,
  marketSlug: string,
  page = 0
) {
  const threads = await memory.listThreads({
    filter: { resourceId, metadata: { marketSlug } },
    perPage: 20,
    page,
    orderBy: { field: 'updatedAt', direction: 'DESC' },
  })
  return {
    chats: threads.threads.flatMap(thread => {
      const id = thread.metadata?.['conversationId']
      return typeof id === 'string' && thread.resourceId === resourceId
        ? [{ id, title: thread.title || 'New chat', updatedAt: thread.updatedAt.toISOString() }]
        : []
    }),
    hasMore: threads.hasMore,
  }
}

type SavedPart =
  | { type: 'text'; text: string }
  | {
      type: `tool-${string}`
      toolCallId: string
      state: 'output-available'
      input: unknown
      output: unknown
    }

/**
 * What a stored message part becomes on the client: text stays text, and a finished call
 * to a widget tool becomes the tool part the transcript renders, so the widget rebinds to
 * live data when the chat reopens. Other tool calls are the agent's working and are dropped.
 */
function savedPart(part: { type: string } & Record<string, unknown>): SavedPart | undefined {
  if (part.type === 'text' && typeof part['text'] === 'string')
    return { type: 'text', text: part['text'] }
  if (part.type !== 'tool-invocation') return undefined
  const invocation = part['toolInvocation']
  if (typeof invocation !== 'object' || invocation === null) return undefined
  const call = invocation as Record<string, unknown>
  const name = call['toolName']
  if (typeof name !== 'string' || !name.startsWith(SHOW_TOOL_PREFIX)) return undefined
  if (call['state'] !== 'result' || typeof call['toolCallId'] !== 'string') return undefined
  return {
    type: `tool-${name}`,
    toolCallId: call['toolCallId'],
    state: 'output-available',
    input: call['args'],
    output: call['result'],
  }
}

export async function readSavedChat(memory: MastraMemory, resourceId: string, id: string) {
  const threadId = chatThreadId(resourceId, id)
  const thread = await memory.getThreadById({ threadId })
  if (!thread || thread.resourceId !== resourceId) return null
  const history = await memory.recall({
    threadId,
    resourceId,
    perPage: false,
    orderBy: { field: 'createdAt', direction: 'ASC' },
  })
  return {
    chat: { id, title: thread.title || 'New chat', updatedAt: thread.updatedAt.toISOString() },
    messages: history.messages.flatMap(message => {
      if (message.role !== 'user' && message.role !== 'assistant') return []
      const parts = message.content.parts.flatMap(part => {
        const saved = savedPart(part)
        return saved === undefined ? [] : [saved]
      })
      return parts.length ? [{ id: message.id, role: message.role, parts }] : []
    }),
  }
}

/** Removes a chat with its messages; `false` when it is not this browser's to remove. */
export async function deleteSavedChat(memory: MastraMemory, resourceId: string, id: string) {
  const threadId = chatThreadId(resourceId, id)
  const thread = await memory.getThreadById({ threadId })
  if (!thread || thread.resourceId !== resourceId) return false
  await memory.deleteThread(threadId)
  return true
}
