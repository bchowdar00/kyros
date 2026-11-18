import * as z from 'zod/mini'
import { env } from '@/env'
import { chatResource } from '@/mastra/chat-session'
import { StrategyBriefSchema } from '@/mastra/capabilities/strategy/brief'
import { chargeWriter } from '@/mastra/writer-meter-client'

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
  // No exchange origin means static-fixture mode: refuse rather than call a model
  // unmetered. No Claude key means the writer cannot run either.
  const exchangeUrl = env.EXCHANGE_API_URL ?? env.NEXT_PUBLIC_API_URL
  if (!env.ANTHROPIC_API_KEY || !exchangeUrl) {
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

  // Meter the build before any model call: one charge per build (repairs included). The
  // route does not know the caller's account address, so the per-address cap is not used.
  const charge = await chargeWriter({
    exchangeUrl,
    guest: resource,
    address: null,
    kind: 'build',
    acceptLanguage: request.headers.get('accept-language'),
  })
  if (!charge.ok)
    return Response.json(
      {
        error: charge.reason === 'resting' ? 'writer_resting' : 'writer_unavailable',
        message: charge.message,
      },
      { status: charge.reason === 'resting' ? 429 : 503, headers: { 'Cache-Control': 'no-store' } }
    )

  try {
    const { mastra } = await import('@/mastra')
    const { startBuild } = await import('@/mastra/strategy-builds')
    const id = await startBuild(mastra, resource, parsed.data.brief)
    return Response.json({ id }, { status: 202, headers: { 'Cache-Control': 'no-store' } })
  } catch {
    return Response.json({ error: 'The build could not be started.' }, { status: 502 })
  }
}
