import * as z from 'zod/mini'
import { env } from '@/env'
import { chatResource } from '@/mastra/chat-session'
import { StrategyBriefSchema } from '@/mastra/capabilities/strategy/brief'

export const runtime = 'nodejs'

const MAX_BODY_BYTES = 32_000
const buildRequest = z.object({ brief: StrategyBriefSchema })

/** Starts a strategy build for the caller's session and answers with its id. */
export async function POST(request: Request) {
  if (env.NODE_ENV !== 'development') {
    return Response.json(
      { error: 'Builds are available in local development only.' },
      { status: 503 }
    )
  }
  if (request.headers.get('origin') !== new URL(request.url).origin) {
    return Response.json({ error: 'Invalid origin.' }, { status: 403 })
  }
  if (!env.OPENAI_API_KEY || !(env.EXCHANGE_API_URL ?? env.NEXT_PUBLIC_API_URL)) {
    return Response.json({ error: 'Builds are not configured.' }, { status: 503 })
  }
  const body = await request.text()
  if (body.length > MAX_BODY_BYTES)
    return Response.json({ error: 'Invalid build request.' }, { status: 400 })
  let parsed: ReturnType<typeof z.safeParse<typeof buildRequest>>
  try {
    parsed = z.safeParse(buildRequest, JSON.parse(body))
  } catch {
    return Response.json({ error: 'Invalid build request.' }, { status: 400 })
  }
  if (!parsed.success) return Response.json({ error: 'Invalid build request.' }, { status: 400 })
  const resource = await chatResource()
  if (!resource) return Response.json({ error: 'No chat session.' }, { status: 403 })
  try {
    const { mastra } = await import('@/mastra')
    const { startBuild } = await import('@/mastra/strategy-builds')
    const id = await startBuild(mastra, resource, parsed.data.brief)
    return Response.json({ id }, { status: 202, headers: { 'Cache-Control': 'no-store' } })
  } catch {
    return Response.json({ error: 'The build could not be started.' }, { status: 502 })
  }
}
