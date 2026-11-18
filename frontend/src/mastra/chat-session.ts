import { randomUUID, createHash } from 'node:crypto'
import { cookies } from 'next/headers'
import { env } from '@/env'

/**
 * Whether the chat routes serve at all. Anonymous browser sessions are a local prototype,
 * not account authorization, so production needs the explicit `CHAT_ENABLED=1` opt-in.
 */
export function chatEnabled() {
  return env.NODE_ENV === 'development' || env.CHAT_ENABLED === '1'
}

// Local browser identity only. Replace with verified account identity before production.
export async function chatResource(create = false) {
  const cookieStore = await cookies()
  const existing = cookieStore.get('teeny-session')?.value
  const valid = existing && /^[a-f0-9-]{36}$/.test(existing)
  if (!valid && !create) return null
  const session = valid ? existing : randomUUID()
  if (create)
    cookieStore.set('teeny-session', session, {
      httpOnly: true,
      sameSite: 'strict',
      path: '/',
      maxAge: 60 * 60 * 24 * 365,
    })
  return createHash('sha256').update(session).digest('hex')
}
