import { chatEnabled, chatResource } from '@/mastra/chat-session'

export const runtime = 'nodejs'

/** The current state of one build the caller's session started. */
export async function GET(_request: Request, context: { params: Promise<{ id: string }> }) {
  if (!chatEnabled()) return new Response(null, { status: 503 })
  const resource = await chatResource()
  if (!resource) return new Response(null, { status: 404 })
  const { id } = await context.params
  if (!id || id.length > 100) return new Response(null, { status: 400 })
  try {
    const { mastra } = await import('@/mastra')
    const { readBuild } = await import('@/mastra/strategy-builds')
    const build = await readBuild(mastra, resource, id)
    return build
      ? Response.json(build, { headers: { 'Cache-Control': 'no-store' } })
      : new Response(null, { status: 404 })
  } catch {
    return Response.json({ error: 'This build could not be read.' }, { status: 503 })
  }
}
