import 'server-only'
import Anthropic from '@anthropic-ai/sdk'
import { env } from '@/env'
import type { WriterModel } from './writer-models'

/**
 * The strategy writer's model calls. Teeny's chat runs through Mastra's Anthropic route,
 * but the compile workflow needs the exact request shape Claude Opus 5.5 / Sonnet 5.5 /
 * Haiku 5.5 require, so it calls the official SDK directly:
 *
 *  - never `temperature`, `top_p` or `top_k`;
 *  - never a forced `tool_choice` — structured outputs (`output_config.format`) carry the
 *    shape instead, which these models reject a forced tool for (HTTP 400);
 *  - thinking left on (no `thinking` field, no `budget_tokens`);
 *  - `max_tokens` 16000;
 *  - a `refusal` or `max_tokens` stop reason is a failed build, surfaced in plain words.
 *
 * The Mastra provider path was not used here because its structured-output request shape
 * is not under this code's control; the SDK request below is, so the rules above are
 * guaranteed and asserted in tests with an injected transport.
 */

/** Opus 5.5 / Sonnet 5.5 / Haiku 5.5 reject a forced tool, so builds are capped at this. */
export const WRITER_MAX_TOKENS = 16000

/**
 * A Claude JSON-Schema format block (a subset of JSON Schema; see `sanitizeSchema`). This is
 * the SDK's `JSONOutputFormat`: exactly `type` and `schema`. The API has no `name` field, so
 * the schema name never reaches the wire.
 */
export interface WriterOutputFormat {
  type: 'json_schema'
  schema: Record<string, unknown>
}

/** Exactly the request this module sends — no sampling params, no tool choice. */
export interface WriterRequest {
  model: string
  max_tokens: number
  system: string
  messages: { role: 'user'; content: string }[]
  output_config: { format: WriterOutputFormat }
}

export interface WriterResponse {
  stop_reason: string | null
  content: { type: string; text?: string }[]
  usage?: { input_tokens?: number; output_tokens?: number }
}

/** Tokens one call spent, for best-effort usage reporting. */
export interface WriterUsage {
  model: string
  inputTokens: number
  outputTokens: number
}

/** A `messages.create`-shaped transport, injectable so tests assert the request. */
export type WriterTransport = (request: WriterRequest) => Promise<WriterResponse>

/**
 * The writer stopped without a usable document. Carries a plain-words message the build
 * surfaces to the user (a refusal, a truncation, or an unreadable answer).
 */
export class WriterUnavailableError extends Error {
  constructor(message: string) {
    super(message)
    this.name = 'WriterUnavailableError'
  }
}

/** Keys Claude's structured outputs reject; stripped from every schema node. */
const UNSUPPORTED_SCHEMA_KEYS = [
  'minimum',
  'maximum',
  'exclusiveMinimum',
  'exclusiveMaximum',
  'multipleOf',
  'minLength',
  'maxLength',
  'minItems',
  'maxItems',
  'uniqueItems',
  'pattern',
  'minProperties',
  'maxProperties',
]

/**
 * Make a JSON schema acceptable to Claude's structured outputs: strip the numeric/string
 * constraints it rejects and set `additionalProperties: false` on every object. The writer
 * schema is validated again with zod after the call, so stripped constraints are still
 * enforced in this process.
 */
export function sanitizeSchema(schema: unknown): unknown {
  if (Array.isArray(schema)) return schema.map(sanitizeSchema)
  if (schema === null || typeof schema !== 'object') return schema
  const node: Record<string, unknown> = {}
  for (const [key, value] of Object.entries(schema as Record<string, unknown>)) {
    if (UNSUPPORTED_SCHEMA_KEYS.includes(key)) continue
    node[key] = sanitizeSchema(value)
  }
  if (node['type'] === 'object' && !('additionalProperties' in node))
    node['additionalProperties'] = false
  return node
}

/**
 * One structured-output call. Returns the parsed JSON object (the caller validates it with
 * its zod schema), or throws `WriterUnavailableError` with a plain message when the model
 * refuses, runs out of room, or does not return a JSON body.
 */
export async function generateWriterObject(args: {
  transport: WriterTransport
  model: WriterModel
  system: string
  prompt: string
  schemaName: string
  jsonSchema: Record<string, unknown>
  /** Called with the tokens this call spent, whatever the stop reason. Best effort. */
  onUsage?: (usage: WriterUsage) => void
}): Promise<unknown> {
  const request: WriterRequest = {
    model: args.model,
    max_tokens: WRITER_MAX_TOKENS,
    system: args.system,
    messages: [{ role: 'user', content: args.prompt }],
    output_config: {
      format: {
        type: 'json_schema',
        schema: sanitizeSchema(args.jsonSchema) as Record<string, unknown>,
      },
    },
  }
  const response = await args.transport(request)
  // Tokens were spent whatever the stop reason, so report before any throw.
  args.onUsage?.({
    model: args.model,
    inputTokens: response.usage?.input_tokens ?? 0,
    outputTokens: response.usage?.output_tokens ?? 0,
  })
  if (response.stop_reason === 'refusal')
    throw new WriterUnavailableError(
      'The strategy writer declined to answer this brief. Adjust the wording and try again.'
    )
  if (response.stop_reason === 'max_tokens')
    throw new WriterUnavailableError(
      'The strategy writer ran out of room before finishing. Simplify the brief and try again.'
    )
  const text = response.content.find(block => block.type === 'text')?.text
  if (text === undefined)
    throw new WriterUnavailableError('The strategy writer did not return a document.')
  try {
    return JSON.parse(text)
  } catch {
    throw new WriterUnavailableError('The strategy writer returned an unreadable document.')
  }
}

let shared: Anthropic | undefined

/** The live Anthropic transport, built from `ANTHROPIC_API_KEY`. */
export function anthropicWriterTransport(): WriterTransport {
  if (!env.ANTHROPIC_API_KEY) throw new Error('ANTHROPIC_API_KEY is not configured')
  shared ??= new Anthropic({ apiKey: env.ANTHROPIC_API_KEY })
  const client = shared
  // `output_config` (structured outputs) is part of the SDK's own request type, so the
  // request is passed with that type and the compiler checks its shape; the response is the
  // SDK's `Message`, which carries the content blocks and usage this module reads.
  return (request: Anthropic.Messages.MessageCreateParamsNonStreaming) =>
    client.messages.create(request)
}
