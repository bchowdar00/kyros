import 'server-only'
import Anthropic from '@anthropic-ai/sdk'
import { env } from '@/env'
import type { WriterModel } from './writer-models'
import { countUnionParameters, MAX_UNION_PARAMETERS, withMissingNulls } from './writer-schema'

/**
 * The strategy writer's model calls. Kyros's chat runs through Mastra's Anthropic route,
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
  /** Absent when the schema is too large to compile and travels in the prompt instead. */
  output_config?: { format: WriterOutputFormat }
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

/** Answers asked for before an object that does not fit its schema fails the build. */
const MAX_ANSWERS = 2

/** How much of a misfit is kept in the error the build logs. */
const MAX_MISFIT_CHARS = 2000

/** The format instruction for a schema that travels in the prompt. */
function formatAsText(jsonSchema: Record<string, unknown>): string {
  return (
    'Answer with one JSON object and nothing else: no words before or after it and no code ' +
    'fence. Write every field the schema lists; a field that does not apply is null. The ' +
    `object must validate against this JSON Schema:\n${JSON.stringify(jsonSchema)}`
  )
}

/** The JSON object inside free text: the span from its first `{` to its last `}`. */
function jsonObjectIn(text: string): unknown {
  const start = text.indexOf('{')
  const end = text.lastIndexOf('}')
  if (start === -1 || end < start) throw new SyntaxError('No JSON object in the answer.')
  return JSON.parse(text.slice(start, end + 1))
}

/** Whether the API refused (HTTP 400) to compile the format schema into a grammar. */
function isSchemaRefusal(error: unknown): boolean {
  return (
    error instanceof Error &&
    'status' in error &&
    error.status === 400 &&
    /schema|grammar/i.test(error.message.slice(0, MAX_MISFIT_CHARS))
  )
}

interface WriterCall {
  transport: WriterTransport
  model: WriterModel
  /** Called with the tokens this call spent, whatever the stop reason. Best effort. */
  onUsage?: (usage: WriterUsage) => void
}

/** One request; the answer's text, or a `WriterUnavailableError` in plain words. */
async function answerText(call: WriterCall, request: WriterRequest): Promise<string> {
  const response = await call.transport(request)
  // Tokens were spent whatever the stop reason, so report before any throw.
  call.onUsage?.({
    model: call.model,
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
  return text
}

/**
 * One object from the writer. A schema Claude can compile is sent as a structured-output
 * format; one with too many nullable fields (see `writer-schema.ts`) travels as text in the
 * prompt, and nullable fields the answer left out are read as null. Returns the parsed
 * object, or throws `WriterUnavailableError` with a plain message when the model refuses,
 * runs out of room, or does not return a JSON body.
 *
 * `check` returns what is wrong with an answer, or null. A wrong answer is asked for once
 * more with that text; a second wrong answer throws an ordinary error carrying it, which
 * the build logs and the user sees as the generic line.
 */
export async function generateWriterObject(
  args: WriterCall & {
    system: string
    prompt: string
    schemaName: string
    jsonSchema: Record<string, unknown>
    check?: (answered: unknown) => string | null
  }
): Promise<unknown> {
  const schema = sanitizeSchema(args.jsonSchema) as Record<string, unknown>
  let asText = countUnionParameters(schema) > MAX_UNION_PARAMETERS
  let misfit: string | null = null
  const request = (): WriterRequest => {
    const asked = asText ? `${args.prompt}\n\n${formatAsText(args.jsonSchema)}` : args.prompt
    const again =
      misfit === null
        ? ''
        : `\n\nYour previous answer did not fit the format:\n${misfit}\n` +
          'Answer again, in full, in the exact format.'
    return {
      model: args.model,
      max_tokens: WRITER_MAX_TOKENS,
      system: args.system,
      messages: [{ role: 'user', content: `${asked}${again}` }],
      ...(asText ? {} : { output_config: { format: { type: 'json_schema', schema } } }),
    }
  }
  for (let answers = 1; ; answers += 1) {
    let text: string
    try {
      text = await answerText(args, request())
    } catch (error) {
      // The grammar's size limit cannot be computed here, so a schema the API will not
      // compile is found out by asking; the same call then goes with the schema as text.
      if (asText || !isSchemaRefusal(error)) throw error
      asText = true
      text = await answerText(args, request())
    }
    let answered: unknown
    try {
      answered = asText ? withMissingNulls(jsonObjectIn(text), schema) : JSON.parse(text)
    } catch {
      throw new WriterUnavailableError('The strategy writer returned an unreadable document.')
    }
    misfit = args.check?.(answered) ?? null
    if (misfit === null) return answered
    if (answers >= MAX_ANSWERS)
      throw new Error(
        `The writer did not answer in the requested shape (${args.schemaName}): ${misfit.slice(0, MAX_MISFIT_CHARS)}`
      )
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
