import { chatEnabled, chatResource } from '@/mastra/chat-session'
import { listSavedChats } from '@/mastra/chat-history'
import { TEENY_AGENT_ID } from '@/mastra/agents/teeny'
import { ChatMarketSchema } from '@/features/market-detail/saved-chat'

export const runtime = 'nodejs'
export async function GET(request: Request) {
  if (!chatEnabled()) return new Response(null, { status: 503 })
  const url = new URL(request.url)
  const market = ChatMarketSchema.safeParse(url.searchParams.get('marketSlug'))
  const page = Number(url.searchParams.get('page') ?? '0')
  if (!market.success || !Number.isSafeInteger(page) || page < 0)
    return new Response(null, { status: 400 })
  const resource = await chatResource()
  if (!resource)
    return Response.json(
      { chats: [], hasMore: false },
      { headers: { 'Cache-Control': 'no-store' } }
    )
  try {
    const { mastra } = await import('@/mastra')
    const memory = await mastra.getAgent(TEENY_AGENT_ID).getMemory()
    if (!memory) throw new Error('Memory unavailable')
    return Response.json(await listSavedChats(memory, resource, market.data, page), {
      headers: { 'Cache-Control': 'no-store' },
    })
  } catch {
    return Response.json({ error: 'Saved chats could not be loaded.' }, { status: 503 })
  }
}
