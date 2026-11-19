import { chatEnabled, chatResource } from '@/mastra/chat-session'
import { prepareChat } from '@/mastra/chat-history'
import { createChatContext, STANDALONE_CHAT } from '@/mastra/request-context'
import { marketSystemMessage } from '@/mastra/market-context'
import { TEENY_AGENT_ID, TEENY_MAX_STEPS } from '@/mastra/agents/teeny'
import { chargeWriter, reportWriterUsage } from '@/mastra/writer-meter-client'
import { handleChatStream } from '@mastra/ai-sdk'
import { createUIMessageStream, createUIMessageStreamResponse } from 'ai'
import { env } from '@/env'
import { parseChatRequest, chatThreadId } from '@/features/market-detail/chat-request'

export const runtime = 'nodejs'

/** A single assistant message carrying a plain line (a meter refusal), shown in the chat. */
function plainMessageResponse(message: string) {
  const stream = createUIMessageStream({
    execute: ({ writer }) => {
      const id = 'writer-meter'
      writer.write({ type: 'text-start', id })
      writer.write({ type: 'text-delta', id, delta: message })
      writer.write({ type: 'text-end', id })
    },
  })
  return createUIMessageStreamResponse({ stream, headers: { 'Cache-Control': 'no-store' } })
}

export async function POST(request: Request) {
  // Anonymous browser sessions are a local prototype, not account authorization.
  if (!chatEnabled()) {
    return Response.json({ error: 'Chat is not enabled on this deployment.' }, { status: 503 })
  }
  if (request.headers.get('origin') !== new URL(request.url).origin) {
    return Response.json({ error: 'Invalid origin.' }, { status: 403 })
  }
  if (!env.ANTHROPIC_API_KEY) {
    return Response.json({ error: 'Chat is not configured.' }, { status: 503 })
  }
  // No exchange origin means static-fixture mode: the meter is unreachable, so refuse
  // rather than call a model unmetered.
  const exchangeUrl = env.EXCHANGE_API_URL ?? env.NEXT_PUBLIC_API_URL
  if (!exchangeUrl) {
    return Response.json({ error: 'Chat is not configured.' }, { status: 503 })
  }
  const body = await request.text()
  const parsed = parseChatRequest(body)
  if (!parsed) return Response.json({ error: 'Invalid chat request.' }, { status: 400 })

  const resource = await chatResource(true)
  if (!resource) return new Response(null, { status: 503 })

  // Meter the turn before any model call: one chat charge per turn (guest = session,
  // address = the account the caller trades as, when the client sent one).
  const charge = await chargeWriter({
    exchangeUrl,
    guest: resource,
    address: parsed.accountAddress ?? null,
    kind: 'chat',
    acceptLanguage: request.headers.get('accept-language'),
  })
  if (!charge.ok) return plainMessageResponse(charge.message)

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
      marketSystemMessage(exchangeUrl, marketSlug),
      prepareChat(memory, resource, parsed.id, marketSlug, firstPrompt),
    ])
    const stream = await handleChatStream({
      mastra,
      agentId: TEENY_AGENT_ID,
      version: 'v7',
      onError: () => 'Kyros could not respond. Please try again.',
      params: {
        messages: parsed.messages,
        // The page decides the market; Kyros is told rather than asked.
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
        // Report the turn's token usage to the meter so chat turns are counted, not just
        // charged. Best effort: reportWriterUsage never throws, and this never blocks or
        // fails the response. The model id falls back to the configured writer model.
        onFinish: event =>
          void reportWriterUsage({
            exchangeUrl,
            guest: resource,
            model: event.model?.modelId ?? env.WRITER_MODEL,
            inputTokens: event.totalUsage?.inputTokens ?? 0,
            outputTokens: event.totalUsage?.outputTokens ?? 0,
          }),
      },
    })
    return createUIMessageStreamResponse({ stream, headers: { 'Cache-Control': 'no-store' } })
  } catch {
    return Response.json({ error: 'Kyros could not respond. Please try again.' }, { status: 502 })
  }
}
