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
    expect(request.output_config?.format.type).toBe('json_schema')
    expect(Object.keys(request.output_config?.format ?? {}).toSorted()).toEqual(['schema', 'type'])
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

/** A schema with `count` nullable text fields, one nullable number and one plain field. */
function schemaWithNullableText(count: number) {
  const properties: Record<string, unknown> = {
    name: { type: 'string', minLength: 1 },
    capacity: { anyOf: [{ type: 'integer' }, { type: 'null' }] },
  }
  for (let index = 0; index < count; index += 1)
    properties[`f${index}`] = { anyOf: [{ type: 'string' }, { type: 'null' }] }
  return { type: 'object', properties, required: Object.keys(properties) }
}

function textResponse(text: string): WriterResponse {
  return {
    stop_reason: 'end_turn',
    content: [{ type: 'thinking' }, { type: 'text', text }],
    usage: { input_tokens: 10, output_tokens: 5 },
  }
}

const WRITE = {
  model: 'claude-sonnet-5-5',
  system: 'RULEBOOK',
  prompt: 'write it',
  schemaName: 'authored_strategy',
} as const

describe('generateWriterObject and a schema too large for structured outputs', () => {
  // Claude compiles a format schema into a grammar and refuses one with more than 16
  // union-typed (nullable) parameters, or whose grammar is too large.
  it('sends the schema as text in the prompt instead of as a format', async () => {
    const { transport, requests } = capture(textResponse('{"name":"n","capacity":1,"f0":"kept"}'))
    const jsonSchema = schemaWithNullableText(20)
    const object = await generateWriterObject({ ...WRITE, transport, jsonSchema })
    const request = requests[0]
    if (!request) throw new Error('the transport was called')
    expect(Object.keys(request)).not.toContain('output_config')
    expect(request.system).toBe('RULEBOOK')
    const content = request.messages[0]?.content ?? ''
    expect(content.startsWith('write it')).toBe(true)
    // The whole schema travels in the prompt, constraints included.
    expect(content).toContain(JSON.stringify(jsonSchema))
    expect(content).toContain('JSON')
    // Nullable fields the answer left out come back as null.
    expect(object).toEqual({
      name: 'n',
      capacity: 1,
      f0: 'kept',
      ...Object.fromEntries(Array.from({ length: 19 }, (_, index) => [`f${index + 1}`, null])),
    })
  })

  it('reads the object out of a code fence or surrounding words', async () => {
    const fenced = 'Here it is:\n```json\n{"name":"n","capacity":null}\n```\nDone.'
    const { transport } = capture(textResponse(fenced))
    const object = await generateWriterObject({
      ...WRITE,
      transport,
      jsonSchema: schemaWithNullableText(17),
    })
    expect(object).toEqual(expect.objectContaining({ name: 'n', capacity: null, f16: null }))
  })

  it('fails in plain words when the answer holds no JSON object', async () => {
    const { transport } = capture(textResponse('I cannot write that.'))
    await expect(
      generateWriterObject({ ...WRITE, transport, jsonSchema: schemaWithNullableText(17) })
    ).rejects.toBeInstanceOf(WriterUnavailableError)
  })

  it('keeps a schema within the limit on structured outputs, exactly as before', async () => {
    const { transport, requests } = capture(okResponse({ name: 'n', capacity: 2, f0: null }))
    const object = await generateWriterObject({
      ...WRITE,
      transport,
      jsonSchema: schemaWithNullableText(1),
    })
    const request = requests[0]
    if (!request) throw new Error('the transport was called')
    expect(request.output_config?.format.type).toBe('json_schema')
    expect(request.messages).toEqual([{ role: 'user', content: 'write it' }])
    expect(object).toEqual({ name: 'n', capacity: 2, f0: null })
  })
})

/** The SDK's error for a request the API refused with HTTP 400. */
function refusal(message: string) {
  return Object.assign(new Error(`400 {"type":"error","error":{"message":"${message}"}}`), {
    status: 400,
  })
}

describe('generateWriterObject when the API refuses a schema as a format', () => {
  it('repeats the call with the schema as text', async () => {
    const requests: WriterRequest[] = []
    const transport = vi.fn(async (request: WriterRequest) => {
      requests.push(request)
      if (request.output_config)
        throw refusal('The compiled grammar is too large, which would cause performance issues.')
      return textResponse('{"ok":true}')
    })
    const object = await generateWriterObject({ ...WRITE, transport, jsonSchema: SCHEMA })
    expect(object).toEqual({ ok: true })
    expect(requests).toHaveLength(2)
    expect(Object.keys(requests[1] ?? {})).not.toContain('output_config')
    expect(requests[1]?.messages[0]?.content).toContain(JSON.stringify(SCHEMA))
  })

  it('does not repeat any other failure', async () => {
    const overloaded = Object.assign(new Error('529 {"type":"error"}'), { status: 529 })
    const badKey = Object.assign(new Error('400 {"error":{"message":"bad request"}}'), {
      status: 400,
    })
    for (const failure of [overloaded, badKey]) {
      const transport = vi.fn(async () => {
        throw failure
      })
      await expect(generateWriterObject({ ...WRITE, transport, jsonSchema: SCHEMA })).rejects.toBe(
        failure
      )
      expect(transport).toHaveBeenCalledTimes(1)
    }
  })
})

describe('generateWriterObject and an answer that does not fit', () => {
  it('asks once more, saying what did not fit, and returns the second answer', async () => {
    const requests: WriterRequest[] = []
    const answers = [okResponse({ ok: 'yes' }), okResponse({ ok: true })]
    const transport = vi.fn(async (request: WriterRequest) => {
      requests.push(request)
      const answer = answers.shift()
      if (!answer) throw new Error('no third call is made')
      return answer
    })
    const usage: unknown[] = []
    const object = await generateWriterObject({
      ...WRITE,
      transport,
      jsonSchema: SCHEMA,
      check: answered =>
        (answered as { ok: unknown }).ok === true ? null : 'ok: expected a boolean',
      onUsage: u => usage.push(u),
    })
    expect(object).toEqual({ ok: true })
    expect(requests).toHaveLength(2)
    expect(requests[0]?.messages[0]?.content).toBe('write it')
    expect(requests[1]?.messages[0]?.content).toContain('write it')
    expect(requests[1]?.messages[0]?.content).toContain('ok: expected a boolean')
    // Both calls spent tokens.
    expect(usage).toHaveLength(2)
  })

  it('gives up after the second answer, with the reason in the error for the log', async () => {
    const { transport } = capture(okResponse({ ok: 'yes' }))
    await expect(
      generateWriterObject({
        ...WRITE,
        transport,
        jsonSchema: SCHEMA,
        check: () => 'ok: expected a boolean',
      })
    ).rejects.toThrow(/requested shape.*ok: expected a boolean/s)
    expect(transport).toHaveBeenCalledTimes(2)
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
