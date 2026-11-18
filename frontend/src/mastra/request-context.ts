import { RequestContext } from '@mastra/core/request-context'

/**
 * What the chat route knows about the request that tools may need. Filled once per
 * request in `app/api/chat/route.ts`; read inside a tool with `readChatContext`.
 * Add a field here when a capability needs something from the request, and thread
 * it through `parseChatRequest` so the client can send it.
 */
/** The market slug a chat carries when it was not opened from a market page. */
export const STANDALONE_CHAT = 'standalone-chat'

export interface ChatContext {
  /** The market page the chat was opened from, or [`STANDALONE_CHAT`]. */
  marketSlug: string
  /** The exchange account the user is trading as, when the client supplied one. */
  accountAddress?: string
  /** The hashed browser session the memory thread belongs to. */
  resourceId: string
}

/**
 * Untyped on purpose: the tool runtime hands tools a `RequestContext<unknown>`, and the
 * class generics are not assignable to each other. Tools read it with `readChatContext`.
 */
export function createChatContext(values: ChatContext): RequestContext {
  const context = new RequestContext()
  context.set('marketSlug', values.marketSlug)
  context.set('resourceId', values.resourceId)
  if (values.accountAddress !== undefined) context.set('accountAddress', values.accountAddress)
  return context
}

/**
 * The one method a tool needs from its request context. Typing it structurally keeps
 * `RequestContext<ChatContext>` and the tool runtime's `RequestContext<unknown>` both
 * assignable, which their class generics are not under `exactOptionalPropertyTypes`.
 */
interface RawContext {
  getRaw(key: string): unknown
}

/** Read the chat context from a tool's execution context; absent outside the chat route. */
export function readChatContext(
  context: { requestContext?: RawContext | undefined } | undefined
): Partial<ChatContext> {
  const request = context?.requestContext
  if (!request) return {}
  const value = (key: keyof ChatContext) => {
    const raw = request.getRaw(key)
    return typeof raw === 'string' ? raw : undefined
  }
  const marketSlug = value('marketSlug')
  const accountAddress = value('accountAddress')
  const resourceId = value('resourceId')
  return {
    ...(marketSlug === undefined ? {} : { marketSlug }),
    ...(accountAddress === undefined ? {} : { accountAddress }),
    ...(resourceId === undefined ? {} : { resourceId }),
  }
}
