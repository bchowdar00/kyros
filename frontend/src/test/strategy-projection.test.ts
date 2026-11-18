import { readFileSync, readdirSync } from 'node:fs'
import { join } from 'node:path'
import { describe, expect, it } from 'vitest'
import {
  AuthoringStrategySchema,
  type AuthoringDoItem,
  type AuthoringStrategy,
  type AuthoringTrigger,
} from '@/mastra/capabilities/strategy/authoring-schema'
import { projectToIr } from '@/mastra/capabilities/strategy/project-to-ir'

// The compiler's own fixtures are the contract the projection must reproduce byte for byte.
const COMPILER_TESTS = join(import.meta.dirname, '../../../server/crates/strategy-compiler/tests')
const TINY_FIXTURES = join(COMPILER_TESTS, 'fixtures/tiny')
const BTC_CORPUS = ['endgame-sweep', 'momentum-flip', 'curve-discount', 'late-momentum']

type Json = Record<string, unknown>
const obj = (value: unknown): Json => value as Json
const arr = (value: unknown): unknown[] => (value as unknown[] | undefined) ?? []
const str = (value: unknown): string | null => (typeof value === 'string' ? value : null)

function typed(spec: unknown): {
  type: AuthoringStrategy['params'][number]['type']
  variants: string[] | null
} {
  if (typeof spec === 'string') return { type: spec as 'u8', variants: null }
  const object = obj(spec)
  return {
    type: object['type'] as 'enum',
    variants: object['type'] === 'enum' ? (object['variants'] as string[]) : null,
  }
}

function leaf(node: Json): AuthoringTrigger['leaves'][number] {
  const value = node['value']
  const valueKind =
    value === undefined
      ? null
      : typeof value === 'string'
        ? 'literal'
        : 'str' in obj(value)
          ? 'str'
          : 'param'
  const valueText =
    value === undefined
      ? null
      : typeof value === 'string'
        ? value
        : (str(obj(value)['str']) ?? str(obj(value)['param']))
  return {
    signal: str(node['signal']),
    field: str(node['field']),
    expr: str(node['expr']),
    comparator: (node['comparator'] as '==' | undefined) ?? null,
    value: valueText,
    value_kind: valueKind,
  }
}

function trigger(node: Json): AuthoringTrigger {
  const rearm = obj(node['rearm'])
  const base = {
    name: node['name'] as string,
    fire: node['fire'] as 'if_true',
    rearm: rearm['mode'] as 'immediate',
    cooldown: str(rearm['cooldown']),
  }
  const timer = node['timer'] ? obj(node['timer']) : undefined
  if (timer)
    return {
      ...base,
      source: timer['kind'] as 'after',
      group: 'single',
      leaves: [],
      instrument: str(timer['instrument']),
      duration: str(timer['remaining']) ?? str(timer['elapsed']) ?? str(timer['every']),
    }
  const condition = obj(node['condition'])
  const group = 'all' in condition ? 'all' : 'any' in condition ? 'any' : 'single'
  const leaves = group === 'single' ? [condition] : (condition[group] as Json[])
  return {
    ...base,
    source: 'condition',
    group,
    leaves: leaves.map(leaf),
    instrument: null,
    duration: null,
  }
}

const EMPTY_ITEM: AuthoringDoItem = {
  kind: 'statement',
  statement: null,
  instrument: null,
  outcome: null,
  side: null,
  order_type: null,
  tif: null,
  price: null,
  price_band_lo: null,
  price_band_hi: null,
  size: null,
  max_size: null,
  tag: null,
  post_only: null,
  expire_after: null,
  reduce_only: null,
  on_fill: null,
  on_partial: null,
  on_reject: null,
  on_cancel_ack: null,
  on_expiry: null,
}

function doItem(node: unknown): AuthoringDoItem {
  if (typeof node === 'string') return { ...EMPTY_ITEM, statement: node }
  const [kind] = Object.keys(obj(node)) as [AuthoringDoItem['kind']]
  const body = obj(obj(node)[kind])
  const band = body['price_band'] as [string, string] | undefined
  const on = obj(body['on'] ?? {})
  return {
    ...EMPTY_ITEM,
    kind,
    instrument: str(body['instrument']),
    outcome: str(body['outcome']),
    side: (body['side'] as 'buy' | undefined) ?? null,
    order_type: (body['order_type'] as 'limit' | undefined) ?? null,
    tif: (body['tif'] as 'gtc' | undefined) ?? null,
    price: str(body['price']),
    price_band_lo: band?.[0] ?? null,
    price_band_hi: band?.[1] ?? null,
    size: str(body['size']),
    max_size: str(body['max_size']),
    tag: str(body['tag']),
    post_only: (body['post_only'] as boolean | undefined) ?? null,
    expire_after: str(body['expire_after']),
    reduce_only: (body['reduce_only'] as boolean | undefined) ?? null,
    on_fill: str(on['fill']),
    on_partial: str(on['partial']),
    on_reject: str(on['reject']),
    on_cancel_ack: str(on['cancel_ack']),
    on_expiry: str(on['expiry']),
  }
}

/** Test-only inverse of projectToIr, so every fixture becomes an authoring document. */
function irToAuthoring(ir: Json): AuthoringStrategy {
  const meta = obj(ir['meta'])
  const expressions = obj(ir['expressions'] ?? {})
  const legs = arr(ir['legs']).map(obj)
  return {
    name: meta['name'] as string,
    description: meta['description'] as string,
    instruments: Object.entries(obj(ir['instruments'])).map(([nick, id]) => ({
      nick,
      id: id as string,
    })),
    signals: Object.entries(obj(ir['signals'])).map(([nick, id]) => ({ nick, id: id as string })),
    state: arr(ir['state'])
      .map(obj)
      .map(slot => {
        const value = typed(slot['value_type'])
        return {
          name: slot['name'] as string,
          kind: slot['kind'] as 'scalar',
          value_type: value.type,
          value_variants: value.variants,
          capacity: (slot['capacity'] as number | undefined) ?? null,
          key_types: slot['key_types'] ? arr(slot['key_types']).map(typed) : null,
          max_keys: (slot['max_keys'] as number | undefined) ?? null,
        }
      }),
    params: arr(ir['params'])
      .map(obj)
      .map(param => ({
        name: param['name'] as string,
        ...typed(param['type']),
        default: String(param['default']),
      })),
    named_expressions: Object.entries(obj(expressions['named'] ?? {})).map(([name, expr]) => ({
      name,
      expr: expr as string,
    })),
    tables: Object.entries(obj(expressions['tables'] ?? {})).map(([name, table]) => ({
      name,
      keys: obj(table)['keys'] as string[],
      values: obj(table)['values'] as string[],
    })),
    triggers: arr(ir['triggers']).map(obj).map(trigger),
    legs: legs.map(leg => ({
      name: leg['name'] as string,
      resident: leg['resident'] as boolean,
      arm: leg['arm'] as string[],
    })),
    blocks: legs.flatMap(leg =>
      arr(leg['body'])
        .map(obj)
        .map(block => ({
          leg: leg['name'] as string,
          name: block['name'] as string,
          guard: str(block['if']),
          next_leg: str(block['then']),
          items: arr(block['do']).map(doItem),
        }))
    ),
  }
}

function fixture(path: string): Json {
  return JSON.parse(readFileSync(path, 'utf8')) as Json
}

const fixtures = [
  ...readdirSync(TINY_FIXTURES).map(name => [`tiny/${name}`, join(TINY_FIXTURES, name)] as const),
  ...BTC_CORPUS.map(
    name => [`corpus/${name}`, join(COMPILER_TESTS, `corpus/${name}.json`)] as const
  ),
]

describe('projectToIr', () => {
  it.each(fixtures)('reproduces %s from its authoring form', (_name, path) => {
    const ir = fixture(path)
    const authored = AuthoringStrategySchema.parse(irToAuthoring(ir))
    const projection = projectToIr(authored)
    expect(projection.warnings).toEqual([])
    expect(projection.strategy).toEqual(ir)
  })

  it('drops blocks that name an unknown leg and says so', () => {
    const authored = irToAuthoring(fixture(join(COMPILER_TESTS, 'corpus/late-momentum.json')))
    const [block] = authored.blocks
    if (!block) throw new Error('fixture has blocks')
    authored.blocks = [{ ...block, leg: 'nowhere' }]
    const projection = projectToIr(authored)
    expect(projection.warnings).toEqual([
      'block `buy_up` names unknown leg `nowhere` and was dropped',
    ])
    expect(projection.strategy['legs']).toEqual([
      { name: 'sniping', resident: true, arm: ['last_10s'], body: [] },
    ])
  })
})
