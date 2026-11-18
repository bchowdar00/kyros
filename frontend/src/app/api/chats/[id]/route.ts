import { chatEnabled, chatResource } from '@/mastra/chat-session'
import { deleteSavedChat, readSavedChat } from '@/mastra/chat-history'
import { TEENY_AGENT_ID } from '@/mastra/agents/teeny'

export const runtime = 'nodejs'
export async function GET(_request: Request, context: { params: Promise<{ id: string }> }) {
  if (!chatEnabled()) return new Response(null, { status: 503 })
  const resource = await chatResource()
  if (!resource) return new Response(null, { status: 404 })
  const { id } = await context.params
  if (!id || id.length > 100) return new Response(null, { status: 400 })
  try {
    const { mastra } = await import('@/mastra')
    const memory = await mastra.getAgent(TEENY_AGENT_ID).getMemory()
    if (!memory) throw new Error('Memory unavailable')
    const saved = await readSavedChat(memory, resource, id)
    return saved
      ? Response.json(saved, { headers: { 'Cache-Control': 'no-store' } })
      : new Response(null, { status: 404 })
  } catch {
    return Response.json({ error: 'This chat could not be loaded.' }, { status: 503 })
  }
}

export async function DELETE(request: Request, context: { params: Promise<{ id: string }> }) {
  if (!chatEnabled()) return new Response(null, { status: 503 })
  if (request.headers.get('origin') !== new URL(request.url).origin)
    return new Response(null, { status: 403 })
  const resource = await chatResource()
  if (!resource) return new Response(null, { status: 404 })
  const { id } = await context.params
  if (!id || id.length > 100) return new Response(null, { status: 400 })
  try {
    const { mastra } = await import('@/mastra')
    const memory = await mastra.getAgent(TEENY_AGENT_ID).getMemory()
    if (!memory) throw new Error('Memory unavailable')
    const removed = await deleteSavedChat(memory, resource, id)
    return new Response(null, { status: removed ? 204 : 404 })
  } catch {
    return Response.json({ error: 'This chat could not be deleted.' }, { status: 503 })
  }
}
