import * as z from 'zod/mini'

export const ChatMarketSchema = z.string().check(z.minLength(1), z.maxLength(200))
export const SavedChatSchema = z.object({
  id: z.string(),
  title: z.string(),
  updatedAt: z.string(),
})
export const SavedChatsSchema = z.object({ chats: z.array(SavedChatSchema), hasMore: z.boolean() })

/** A finished widget tool call, kept so the widget rebinds to live data on reopen. */
const SavedToolPartSchema = z.object({
  type: z.templateLiteral(['tool-', z.string()]),
  toolCallId: z.string(),
  state: z.literal('output-available'),
  input: z.unknown(),
  output: z.unknown(),
})

export const SavedChatDetailSchema = z.object({
  chat: SavedChatSchema,
  messages: z.array(
    z.object({
      id: z.string(),
      role: z.enum(['user', 'assistant']),
      parts: z.array(
        z.union([z.object({ type: z.literal('text'), text: z.string() }), SavedToolPartSchema])
      ),
    })
  ),
})
export type SavedChat = z.infer<typeof SavedChatSchema> & {
  localStatus?: 'saving' | 'unsaved' | undefined
}

export function chatTitle(prompt: string) {
  const title = prompt.trim().replace(/\s+/g, ' ')
  return title.length > 64 ? `${title.slice(0, 61).trimEnd()}…` : title || 'New chat'
}
