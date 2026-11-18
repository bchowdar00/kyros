import * as z from 'zod/mini'
import { ChatMarketSchema } from './saved-chat'

const chatRequest = z.object({
  marketSlug: z.optional(ChatMarketSchema),
  /** Exchange account the user trades as; reaches tools through the chat request context. */
  accountAddress: z.optional(z.string().check(z.minLength(1), z.maxLength(100))),
  trigger: z.optional(z.enum(['submit-message', 'regenerate-message'])),
  id: z.string().check(z.minLength(1), z.maxLength(100)),
  messages: z
    .array(
      z.object({
        id: z.string().check(z.minLength(1), z.maxLength(100)),
        role: z.enum(['user', 'assistant']),
        parts: z
          .array(
            z.union([
              z.object({ type: z.literal('text'), text: z.string().check(z.maxLength(8000)) }),
              z.object({ type: z.literal('step-start') }),
            ])
          )
          .check(z.maxLength(20)),
      })
    )
    .check(z.minLength(1), z.maxLength(100)),
})

export function parseChatRequest(body: string) {
  if (body.length > 64_000) return undefined
  try {
    const parsed = z.safeParse(chatRequest, JSON.parse(body))
    return parsed.success ? parsed.data : undefined
  } catch {
    return undefined
  }
}

// The caller cannot select another browser's memory by changing the chat ID.
export function chatThreadId(resource: string, conversation: string) {
  return `${resource}:${conversation}`
}
