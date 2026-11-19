import * as z from 'zod/mini'
import { describe, expect, it } from 'vitest'
import { AuthoringStrategySchema } from '@/mastra/capabilities/strategy/authoring-schema'
import { StrategyPlanSchema } from '@/mastra/capabilities/strategy/plan'
import { sanitizeSchema } from '@/mastra/writer-model'
import {
  countUnionParameters,
  MAX_UNION_PARAMETERS,
  withMissingNulls,
} from '@/mastra/writer-schema'

const ORDER = {
  type: 'object',
  properties: {
    name: { type: 'string' },
    tag: { anyOf: [{ type: 'string' }, { type: 'null' }], description: 'Or null.' },
    post_only: { type: ['boolean', 'null'] },
    capacity: { anyOf: [{ type: 'integer' }, { type: 'null' }] },
  },
  required: ['name', 'tag', 'post_only', 'capacity'],
  additionalProperties: false,
}
const DOCUMENT = {
  type: 'object',
  properties: {
    orders: { type: 'array', items: ORDER },
    keys: { anyOf: [{ type: 'array', items: ORDER }, { type: 'null' }] },
  },
  required: ['orders', 'keys'],
  additionalProperties: false,
}

describe('countUnionParameters', () => {
  it('counts anyOf nodes and type arrays at any depth', () => {
    expect(countUnionParameters(DOCUMENT)).toBe(7)
    expect(countUnionParameters({ type: 'object', properties: { a: { type: 'string' } } })).toBe(0)
  })
})

describe('withMissingNulls', () => {
  it('writes null for a nullable field the answer left out, at any depth', () => {
    const answered = { orders: [{ name: 'a' }, { name: 'b', tag: 't', capacity: 3 }] }
    expect(withMissingNulls(answered, DOCUMENT)).toEqual({
      orders: [
        { name: 'a', tag: null, post_only: null, capacity: null },
        { name: 'b', tag: 't', post_only: null, capacity: 3 },
      ],
      keys: null,
    })
  })

  it('fills inside a nullable list of objects', () => {
    expect(withMissingNulls({ orders: [], keys: [{ name: 'k', tag: 't' }] }, DOCUMENT)).toEqual({
      orders: [],
      keys: [{ name: 'k', tag: 't', post_only: null, capacity: null }],
    })
  })

  it('never invents a field that cannot be null, and leaves what was answered alone', () => {
    const answered = { orders: [{ tag: '', post_only: false, capacity: 0, extra: 1 }], keys: null }
    expect(withMissingNulls(answered, DOCUMENT)).toEqual(answered)
    expect(withMissingNulls('not an object', DOCUMENT)).toBe('not an object')
  })
})

describe('the schemas the writer sends', () => {
  it('has a strategy document with more nullable fields than Claude will compile', () => {
    const document = sanitizeSchema(z.toJSONSchema(AuthoringStrategySchema))
    expect(countUnionParameters(document)).toBeGreaterThan(MAX_UNION_PARAMETERS)
  })

  it('has a plan within the limit', () => {
    const plan = sanitizeSchema(z.toJSONSchema(StrategyPlanSchema))
    expect(countUnionParameters(plan)).toBeLessThanOrEqual(MAX_UNION_PARAMETERS)
  })

  it('validates a strategy document whose inapplicable fields were left out', () => {
    const answered = {
      name: 'Late gap',
      description: 'Buys Up once when BTC is 10 bps above the strike at two minutes.',
      instruments: [{ nick: 'mkt', id: 'btc-5m.current' }],
      signals: [{ nick: 'gap', id: 'btc-5m.current.delta.v1' }],
      state: [{ name: 'done', kind: 'scalar', value_type: 'bool' }],
      params: [{ name: 'edge', type: 'decimal', default: '10' }],
      named_expressions: [],
      tables: [],
      triggers: [
        {
          name: 'late',
          source: 'window_clock',
          group: 'single',
          leaves: [
            { signal: 'gap', field: 'bps', comparator: '>=', value: 'edge', value_kind: 'param' },
          ],
          instrument: 'mkt',
          duration: '2m',
          fire: 'if_true',
          rearm: 'never',
        },
      ],
      legs: [{ name: 'flat', resident: true, arm: ['late'] }],
      blocks: [
        {
          leg: 'flat',
          name: 'enter',
          items: [
            {
              kind: 'place',
              instrument: 'mkt',
              outcome: 'up',
              side: 'buy',
              order_type: 'market',
              tif: 'ioc',
              size: 'clamp(envelope.allocation / 10, 1, 100)',
              max_size: 'envelope.allocation',
            },
          ],
        },
      ],
    }
    expect(z.safeParse(AuthoringStrategySchema, answered).success).toBe(false)
    const schema = sanitizeSchema(z.toJSONSchema(AuthoringStrategySchema))
    const filled = z.safeParse(AuthoringStrategySchema, withMissingNulls(answered, schema))
    expect(filled.success).toBe(true)
    expect(filled.data?.blocks[0]?.items[0]?.price).toBeNull()
    expect(filled.data?.blocks[0]?.guard).toBeNull()
    expect(filled.data?.triggers[0]?.leaves[0]?.expr).toBeNull()
  })
})
