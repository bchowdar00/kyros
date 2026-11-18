import * as z from 'zod/mini'
import type { Envelope } from './envelope'
import type { IrDocument } from './project-to-ir'

/** What `POST /api/strategy/compile` answers on success. */
export const CompileSummarySchema = z.object({
  strategy_id: z.string(),
  name: z.string(),
  description: z.string(),
  series: z.string(),
  subscriptions: z.array(z.string()),
  instruments: z.record(z.string(), z.string()),
  params: z.array(z.object({ name: z.string(), type: z.unknown() })),
  legs: z.array(z.string()),
  warnings: z.array(z.unknown()),
})
export type CompileSummary = z.infer<typeof CompileSummarySchema>

const CompileFailureSchema = z.object({
  error: z.literal('compile_failed'),
  message: z.string(),
  detail: z.object({ diagnostics: z.array(z.unknown()), rendered: z.array(z.string()) }),
})

const RuleDiagnosticSchema = z.object({
  severity: z.string(),
  path: z.string(),
  detail: z.object({
    rule: z.object({ code: z.string(), message: z.string(), help: z.optional(z.string()) }),
  }),
})

export type CompileOutcome =
  | { ok: true; summary: CompileSummary; warnings: string[] }
  /** The compiler ran and refused the document; `rendered` is what the repair step reads. */
  | { ok: false; kind: 'diagnostics'; rendered: string[]; message: string }
  /** The document did not even deserialize, or the exchange could not be reached. */
  | { ok: false; kind: 'rejected'; message: string }

const REQUEST_TIMEOUT_MS = 10_000

/** A one-line rendering of a compiler warning, for the readback. */
export function renderWarning(diagnostic: unknown): string {
  const parsed = z.safeParse(RuleDiagnosticSchema, diagnostic)
  if (!parsed.success) return JSON.stringify(diagnostic)
  const { severity, path, detail } = parsed.data
  return `${severity}[${detail.rule.code}] at ${path}: ${detail.rule.message}`
}

export async function compileStrategy(
  apiUrl: string,
  strategy: IrDocument,
  envelope: Envelope,
  fetchImpl: typeof fetch = fetch
): Promise<CompileOutcome> {
  let response: Response
  try {
    response = await fetchImpl(new URL('api/strategy/compile', `${apiUrl.replace(/\/$/, '')}/`), {
      method: 'POST',
      headers: { 'content-type': 'application/json', accept: 'application/json' },
      body: JSON.stringify({ strategy, envelope }),
      signal: AbortSignal.timeout(REQUEST_TIMEOUT_MS),
      cache: 'no-store',
    })
  } catch {
    return { ok: false, kind: 'rejected', message: 'The exchange could not be reached.' }
  }
  const text = await response.text()
  let body: unknown
  try {
    body = JSON.parse(text)
  } catch {
    // Axum answers a document that fails to deserialize with plain text.
    return { ok: false, kind: 'rejected', message: text || `HTTP ${response.status}` }
  }
  if (response.ok) {
    const summary = z.safeParse(CompileSummarySchema, body)
    if (!summary.success)
      return { ok: false, kind: 'rejected', message: 'The exchange answered an unexpected shape.' }
    return { ok: true, summary: summary.data, warnings: summary.data.warnings.map(renderWarning) }
  }
  const failure = z.safeParse(CompileFailureSchema, body)
  if (failure.success)
    return {
      ok: false,
      kind: 'diagnostics',
      rendered: failure.data.detail.rendered,
      message: failure.data.message,
    }
  const generic = z.safeParse(z.object({ message: z.string() }), body)
  return {
    ok: false,
    kind: 'rejected',
    message: generic.success ? generic.data.message : `HTTP ${response.status}`,
  }
}
