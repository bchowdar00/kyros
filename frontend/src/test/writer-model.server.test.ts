import { describe, expect, it, vi } from 'vitest'
import {
  generateWriterObject,
  sanitizeSchema,
  WRITER_MAX_TOKENS,
  WriterUnavailableError,
  type WriterRequest,
  type WriterResponse,
} from '@/mastra/writer-model'

function okResponse(object: unknown): WriterResponse {
  return {
    stop_reason: 'end_turn',
    // Thinking is left on, so a thinking block can precede the JSON text block.
    content: [{ type: 'thinking' }, { type: 'text', text: JSON.stringify(object) }],
    usage: { input_tokens: 120, output_tokens: 34 },
  }
}

function capture(response: WriterResponse) {
  const requests: WriterRequest[] = []
  const transport = vi.fn(async (request: WriterRequest) => {
    requests.push(request)
    return response
  })
  return { transport, requests }
}

const SCHEMA = { type: 'object', properties: { ok: { type: 'boolean' } }, required: ['ok'] }

describe('generateWriterObject request shape', () => {
  it('sends no sampling params and no forced tool choice', async () => {
    const { transport, requests } = capture(okResponse({ ok: true }))
    await generateWriterObject({
      transport,
      model: 'claude-opus-5-5',
      system: 'RULEBOOK',
      prompt: 'write it',
      schemaName: 'authored_strategy',
      jsonSchema: SCHEMA,
    })
    const request = requests[0]
    if (!request) throw new Error('the transport was called')
    const keys = Object.keys(request)
    expect(keys).not.toContain('temperature')
    expect(keys).not.toContain('top_p')
    expect(keys).not.toContain('top_k')
    expect(keys).not.toContain('tool_choice')
    expect(keys).not.toContain('tools')
    // Thinking stays on by omission: no thinking field, no budget.
    expect(keys).not.toContain('thinking')
    expect(request.max_tokens).toBe(WRITER_MAX_TOKENS)
    expect(WRITER_MAX_TOKENS).toBe(16000)
    expect(request.model).toBe('claude-opus-5-5')
    expect(request.system).toBe('RULEBOOK')
    expect(request.messages).toEqual([{ role: 'user', content: 'write it' }])
    // The shape is carried by structured outputs, not a tool call. The format
    // block is exactly the SDK's JSONOutputFormat: `type` and `schema`, no
    // `name` (which the API does not accept).
    expect(request.output_config.format.type).toBe('json_schema')
    expect(Object.keys(request.output_config.format).toSorted()).toEqual(['schema', 'type'])
  })

  it('returns the parsed JSON object and reports usage', async () => {
    const { transport } = capture(okResponse({ ok: true }))
    const usage: unknown[] = []
    const object = await generateWriterObject({
      transport,
      model: 'claude-sonnet-5-5',
      system: 's',
      prompt: 'p',
      schemaName: 'n',
      jsonSchema: SCHEMA,
      onUsage: u => usage.push(u),
    })
    expect(object).toEqual({ ok: true })
    expect(usage).toEqual([{ model: 'claude-sonnet-5-5', inputTokens: 120, outputTokens: 34 }])
  })
})

describe('generateWriterObject stop reasons become a failed build', () => {
  async function run(response: WriterResponse) {
    const { transport } = capture(response)
    return generateWriterObject({
      transport,
      model: 'claude-opus-5-5',
      system: 's',
      prompt: 'p',
      schemaName: 'n',
      jsonSchema: SCHEMA,
    })
  }

  it('treats a refusal as a failed build with a plain message', async () => {
    const error = await run({ stop_reason: 'refusal', content: [] }).catch((e: unknown) => e)
    expect(error).toBeInstanceOf(WriterUnavailableError)
    expect((error as Error).message).toMatch(/declined/i)
    // Plain words, no API jargon.
    expect((error as Error).message).not.toMatch(/stop_reason|refusal|4\d\d/)
  })

  it('treats max_tokens truncation as a failed build with a plain message', async () => {
    const error = await run({ stop_reason: 'max_tokens', content: [] }).catch((e: unknown) => e)
    expect(error).toBeInstanceOf(WriterUnavailableError)
    expect((error as Error).message).toMatch(/ran out of room/i)
  })

  it('reports usage even when the model refused', async () => {
    const usage: unknown[] = []
    const { transport } = capture({
      stop_reason: 'refusal',
      content: [],
      usage: { input_tokens: 7, output_tokens: 0 },
    })
    await generateWriterObject({
      transport,
      model: 'claude-haiku-5-5',
      system: 's',
      prompt: 'p',
      schemaName: 'n',
      jsonSchema: SCHEMA,
      onUsage: u => usage.push(u),
    }).catch(() => undefined)
    expect(usage).toEqual([{ model: 'claude-haiku-5-5', inputTokens: 7, outputTokens: 0 }])
  })

  it('fails when there is no JSON text block', async () => {
    const error = await run({ stop_reason: 'end_turn', content: [{ type: 'thinking' }] }).catch(
      (e: unknown) => e
    )
    expect(error).toBeInstanceOf(WriterUnavailableError)
  })
})

describe('sanitizeSchema', () => {
  it('sets additionalProperties:false on objects and strips unsupported constraints', () => {
    const cleaned = sanitizeSchema({
      type: 'object',
      properties: {
        name: { type: 'string', minLength: 1, maxLength: 8 },
        size: { type: 'integer', minimum: 0, maximum: 10 },
        nested: { type: 'object', properties: {} },
      },
      required: ['name'],
    }) as Record<string, unknown>
    expect(cleaned['additionalProperties']).toBe(false)
    const props = cleaned['properties'] as Record<string, Record<string, unknown>>
    expect(props['name']).toEqual({ type: 'string' })
    expect(props['size']).toEqual({ type: 'integer' })
    expect(props['nested']?.['additionalProperties']).toBe(false)
  })

  it('leaves an explicit additionalProperties alone', () => {
    const cleaned = sanitizeSchema({ type: 'object', additionalProperties: false }) as Record<
      string,
      unknown
    >
    expect(cleaned['additionalProperties']).toBe(false)
  })
})
