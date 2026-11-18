import { chatEnabled, chatResource } from '@/mastra/chat-session'
import { prepareChat } from '@/mastra/chat-history'
import { createChatContext, STANDALONE_CHAT } from '@/mastra/request-context'
import { marketSystemMessage } from '@/mastra/market-context'
import { TEENY_AGENT_ID, TEENY_MAX_STEPS } from '@/mastra/agents/teeny'
import { handleChatStream } from '@mastra/ai-sdk'
import { createUIMessageStreamResponse } from 'ai'
import { env } from '@/env'
import { parseChatRequest, chatThreadId } from '@/features/market-detail/chat-request'

export const runtime = 'nodejs'

export async function POST(request: Request) {
  // Anonymous browser sessions are a local prototype, not account authorization.
  if (!chatEnabled()) {
    return Response.json({ error: 'Chat is not enabled on this deployment.' }, { status: 503 })
  }
  if (request.headers.get('origin') !== new URL(request.url).origin) {
    return Response.json({ error: 'Invalid origin.' }, { status: 403 })
  }
  if (!env.OPENAI_API_KEY) {
    return Response.json({ error: 'Chat is not configured.' }, { status: 503 })
  }
  const body = await request.text()
  const parsed = parseChatRequest(body)
  if (!parsed) return Response.json({ error: 'Invalid chat request.' }, { status: 400 })

  const resource = await chatResource(true)
  if (!resource) return new Response(null, { status: 503 })

  try {
    const { mastra } = await import('@/mastra')
    const memory = await mastra.getAgent(TEENY_AGENT_ID).getMemory()
    if (!memory) throw new Error('Memory unavailable')
    const firstPrompt =
      parsed.messages
        .find(message => message.role === 'user')
        ?.parts.filter(part => part.type === 'text')
        .map(part => part.text)
        .join(' ') ?? ''
    const marketSlug = parsed.marketSlug ?? STANDALONE_CHAT
    const [system] = await Promise.all([
      marketSystemMessage(env.EXCHANGE_API_URL ?? env.NEXT_PUBLIC_API_URL, marketSlug),
      prepareChat(memory, resource, parsed.id, marketSlug, firstPrompt),
    ])
    const stream = await handleChatStream({
      mastra,
      agentId: TEENY_AGENT_ID,
      version: 'v7',
      onError: () => 'Teeny could not respond. Please try again.',
      params: {
        messages: parsed.messages,
        // The page decides the market; Teeny is told rather than asked.
        system,
        ...(parsed.trigger ? { trigger: parsed.trigger } : {}),
        memory: { thread: chatThreadId(resource, parsed.id), resource },
        // Tools read this with readChatContext(); the client never sets memory ids through it.
        requestContext: createChatContext({
          marketSlug,
          resourceId: resource,
          ...(parsed.accountAddress === undefined ? {} : { accountAddress: parsed.accountAddress }),
        }),
        maxSteps: TEENY_MAX_STEPS,
        abortSignal: request.signal,
      },
    })
    return createUIMessageStreamResponse({ stream, headers: { 'Cache-Control': 'no-store' } })
  } catch {
    return Response.json({ error: 'Teeny could not respond. Please try again.' }, { status: 502 })
  }
}
