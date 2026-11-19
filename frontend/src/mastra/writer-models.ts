/**
 * The Claude models the writer may run, and how Mastra's model router names
 * them. Kept free of the Anthropic SDK so `env.ts` (which the client bundle
 * reaches) can validate `WRITER_MODEL` without pulling a server-only package.
 */

/** The only accepted `WRITER_MODEL` values; the first is the default. */
export const WRITER_MODELS = ['claude-opus-5-5', 'claude-sonnet-5-5', 'claude-haiku-5-5'] as const

export type WriterModel = (typeof WRITER_MODELS)[number]

/** The Mastra model-router string for a writer model (Kyros's chat agent). */
export function writerModelRouter(model: WriterModel): `anthropic/${WriterModel}` {
  return `anthropic/${model}`
}
