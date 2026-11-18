import 'server-only'
import { env } from '@/env'

/**
 * Talks to the exchange's writer meter so a model is never called unmetered. Every chat
 * turn and strategy build charges once, server-side, before any model call. The meter —
 * not this process — enforces the per-guest, per-address and site-wide daily caps, so a
 * stranger cannot drain the owner's Claude credits.
 */

export type ChargeKind = 'build' | 'chat'

export type ChargeResult =
  | { ok: true; remainingGuest: number; remainingSite: number }
  /** A cap refused the charge (HTTP 429); `message` is the user-facing resting line. */
  | { ok: false; reason: 'resting'; message: string }
  /** The meter could not be reached or answered unexpectedly; fail closed. */
  | { ok: false; reason: 'unavailable'; message: string }

const CHARGE_TIMEOUT_MS = 4_000
const USAGE_TIMEOUT_MS = 4_000

function meterHeaders(): Record<string, string> {
  const headers: Record<string, string> = { 'content-type': 'application/json' }
  if (env.WRITER_METER_TOKEN) headers['x-writer-token'] = env.WRITER_METER_TOKEN
  return headers
}

function pickLocale(acceptLanguage: string | null | undefined): string {
  const first = acceptLanguage?.split(',')[0]?.split(';')[0]?.trim()
  if (!first) return 'en-US'
  try {
    // Reject a structurally malformed header value before handing it to Intl.
    Intl.getCanonicalLocales(first)
    return first
  } catch {
    return 'en-US'
  }
}

/** The resting line shown when a cap refuses a charge, in the caller's locale (UTC clock). */
export function restingMessage(
  kind: ChargeKind,
  resetsAtMs: number,
  acceptLanguage?: string | null
): string {
  const time = new Intl.DateTimeFormat(pickLocale(acceptLanguage), {
    hour: 'numeric',
    minute: '2-digit',
    timeZoneName: 'short',
    timeZone: 'UTC',
  }).format(new Date(resetsAtMs))
  const subject = kind === 'chat' ? 'Chat' : 'The strategy writer'
  return `${subject} is resting until ${time}; templates still deploy`
}

/** Charge one unit for `kind`. A refusal (resting) or an unreachable meter both fail closed. */
export async function chargeWriter(args: {
  exchangeUrl: string
  guest: string
  address: string | null
  kind: ChargeKind
  acceptLanguage?: string | null
  fetchImpl?: typeof fetch
}): Promise<ChargeResult> {
  const fetchImpl = args.fetchImpl ?? fetch
  const url = new URL('api/writer/charge', `${args.exchangeUrl.replace(/\/$/, '')}/`)
  let response: Response
  try {
    response = await fetchImpl(url, {
      method: 'POST',
      headers: meterHeaders(),
      body: JSON.stringify({ guest: args.guest, address: args.address, kind: args.kind }),
      signal: AbortSignal.timeout(CHARGE_TIMEOUT_MS),
      cache: 'no-store',
    })
  } catch {
    return { ok: false, reason: 'unavailable', message: unavailableMessage(args.kind) }
  }
  if (response.status === 429) {
    let resetsAtMs = Date.now()
    try {
      const body = (await response.json()) as { resets_at_ms?: number }
      if (typeof body.resets_at_ms === 'number') resetsAtMs = body.resets_at_ms
    } catch {
      // Keep the fallback reset time; the message still reads sensibly.
    }
    return {
      ok: false,
      reason: 'resting',
      message: restingMessage(args.kind, resetsAtMs, args.acceptLanguage),
    }
  }
  // A closed meter (no token configured) answers 503, and a wrong/absent token answers 401.
  // Both mean the writer is not switched on here; the model is not called either way.
  if (response.status === 401 || response.status === 503)
    return { ok: false, reason: 'unavailable', message: writerNotSwitchedOnMessage() }
  if (!response.ok)
    return { ok: false, reason: 'unavailable', message: unavailableMessage(args.kind) }
  try {
    const body = (await response.json()) as { remaining_guest?: number; remaining_site?: number }
    return {
      ok: true,
      remainingGuest: body.remaining_guest ?? 0,
      remainingSite: body.remaining_site ?? 0,
    }
  } catch {
    return { ok: false, reason: 'unavailable', message: unavailableMessage(args.kind) }
  }
}

function unavailableMessage(kind: ChargeKind): string {
  return kind === 'chat'
    ? 'Chat is unavailable right now; templates still deploy.'
    : 'The strategy writer is unavailable right now; templates still deploy.'
}

/** Shown when the meter is closed (503) or rejects the token (401): the writer is switched off. */
function writerNotSwitchedOnMessage(): string {
  return 'The strategy writer is not switched on here; templates still deploy.'
}

/** Report the tokens a completed model call spent. Best effort: never throws. */
export async function reportWriterUsage(args: {
  exchangeUrl: string
  guest: string
  model: string
  inputTokens: number
  outputTokens: number
  fetchImpl?: typeof fetch
}): Promise<void> {
  const fetchImpl = args.fetchImpl ?? fetch
  try {
    const url = new URL('api/writer/usage', `${args.exchangeUrl.replace(/\/$/, '')}/`)
    await fetchImpl(url, {
      method: 'POST',
      headers: meterHeaders(),
      body: JSON.stringify({
        guest: args.guest,
        model: args.model,
        input_tokens: args.inputTokens,
        output_tokens: args.outputTokens,
      }),
      signal: AbortSignal.timeout(USAGE_TIMEOUT_MS),
      cache: 'no-store',
    })
  } catch {
    // Usage reporting is advisory; a failure never fails the user's request.
  }
}
