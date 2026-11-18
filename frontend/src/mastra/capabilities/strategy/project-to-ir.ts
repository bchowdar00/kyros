import type {
  AuthoringDoItem,
  AuthoringStateSlot,
  AuthoringStrategy,
  AuthoringTrigger,
  AuthoringTypedValue,
} from './authoring-schema'

/** The compiler's IR document, kept as plain JSON: the Rust types are the source of truth. */
export type IrDocument = Record<string, unknown>
type IrObject = Record<string, unknown>

export interface Projection {
  strategy: IrDocument
  /** Structural problems the schema cannot express; the compiler will refuse them too. */
  warnings: string[]
}

export const DSL_VERSION = '0.3'

function typeSpec({ type, variants }: AuthoringTypedValue): unknown {
  return type === 'enum' ? { type: 'enum', variants: variants ?? [] } : type
}

function stateSlot(slot: AuthoringStateSlot): IrObject {
  const valueType = typeSpec({ type: slot.value_type, variants: slot.value_variants })
  switch (slot.kind) {
    case 'scalar':
      return { kind: 'scalar', name: slot.name, value_type: valueType }
    case 'window':
      return {
        kind: 'window',
        name: slot.name,
        value_type: valueType,
        capacity: slot.capacity ?? 1,
      }
    case 'map':
      return {
        kind: 'map',
        name: slot.name,
        key_types: (slot.key_types ?? []).map(typeSpec),
        value_type: valueType,
        max_keys: slot.max_keys ?? 1,
        overflow: 'refuse_update',
      }
  }
}

function leafValue(value: string | null, kind: string | null): unknown {
  if (kind === 'str') return { str: value ?? '' }
  if (kind === 'param') return { param: value ?? '' }
  return value ?? ''
}

function conditionLeaf(leaf: AuthoringTrigger['leaves'][number]): IrObject {
  if (leaf.expr !== null)
    return {
      expr: leaf.expr,
      comparator: leaf.comparator ?? '==',
      value: leafValue(leaf.value, leaf.value_kind),
    }
  if (leaf.field !== null)
    return {
      signal: leaf.signal ?? '',
      field: leaf.field,
      comparator: leaf.comparator ?? '==',
      value: leafValue(leaf.value, leaf.value_kind),
    }
  return { signal: leaf.signal ?? '' }
}

function trigger(authored: AuthoringTrigger, warnings: string[]): IrObject {
  const rearm: IrObject = { mode: authored.rearm }
  if (authored.cooldown !== null) rearm['cooldown'] = authored.cooldown
  const base = { name: authored.name, fire: authored.fire, rearm }
  const duration = authored.duration ?? '1s'
  switch (authored.source) {
    case 'window_clock':
      return {
        ...base,
        timer: { kind: 'window_clock', instrument: authored.instrument ?? '', remaining: duration },
      }
    case 'after':
      return { ...base, timer: { kind: 'after', elapsed: duration } }
    case 'interval':
      return { ...base, timer: { kind: 'interval', every: duration } }
    case 'condition': {
      const leaves = authored.leaves.map(conditionLeaf)
      if (leaves.length === 0) warnings.push(`trigger \`${authored.name}\` has no condition leaves`)
      const [first] = leaves
      if (authored.group === 'single' || leaves.length === 1)
        return { ...base, condition: first ?? { signal: '' } }
      return { ...base, condition: { [authored.group]: leaves } }
    }
  }
}

function order(item: AuthoringDoItem): IrObject {
  const out: IrObject = { instrument: item.instrument ?? '' }
  if (item.outcome !== null) out['outcome'] = item.outcome
  out['side'] = item.side ?? 'buy'
  out['order_type'] = item.order_type ?? 'market'
  out['tif'] = item.tif ?? 'ioc'
  if (item.price !== null) out['price'] = item.price
  if (item.price_band_lo !== null || item.price_band_hi !== null)
    out['price_band'] = [item.price_band_lo ?? '0', item.price_band_hi ?? '1']
  out['size'] = item.size ?? ''
  out['max_size'] = item.max_size ?? ''
  if (item.post_only !== null) out['post_only'] = item.post_only
  if (item.expire_after !== null) out['expire_after'] = item.expire_after
  if (item.reduce_only !== null) out['reduce_only'] = item.reduce_only
  return out
}

function onTable(item: AuthoringDoItem): IrObject | undefined {
  const entries: [string, string | null][] = [
    ['fill', item.on_fill],
    ['partial', item.on_partial],
    ['reject', item.on_reject],
    ['cancel_ack', item.on_cancel_ack],
    ['expiry', item.on_expiry],
  ]
  const present = entries.filter((entry): entry is [string, string] => entry[1] !== null)
  return present.length ? Object.fromEntries(present) : undefined
}

function doItem(item: AuthoringDoItem): unknown {
  switch (item.kind) {
    case 'statement':
      return item.statement ?? ''
    case 'cancel':
      return { cancel: { tag: item.tag ?? '' } }
    case 'cancel_replace': {
      const { instrument, ...rest } = order(item)
      return { cancel_replace: { tag: item.tag ?? '', instrument, ...rest } }
    }
    case 'place': {
      const place: IrObject = { ...order(item), tag: item.tag ?? '' }
      const on = onTable(item)
      if (on) place['on'] = on
      return { place }
    }
    case 'target_position': {
      const target: IrObject = { instrument: item.instrument ?? '' }
      if (item.outcome !== null) target['outcome'] = item.outcome
      target['size'] = item.size ?? ''
      target['max_size'] = item.max_size ?? ''
      target['tag'] = item.tag ?? ''
      if (item.reduce_only !== null) target['reduce_only'] = item.reduce_only
      const on = onTable(item)
      if (on) target['on'] = on
      return { target_position: target }
    }
  }
}

function expressions(authored: AuthoringStrategy): IrObject | undefined {
  const out: IrObject = {}
  if (authored.named_expressions.length)
    out['named'] = Object.fromEntries(authored.named_expressions.map(e => [e.name, e.expr]))
  if (authored.tables.length)
    out['tables'] = Object.fromEntries(
      authored.tables.map(t => [
        t.name,
        { keys: t.keys, values: t.values, interpolation: 'linear' },
      ])
    )
  return Object.keys(out).length ? out : undefined
}

/** Total: every authoring document projects to some IR document. */
export function projectToIr(authored: AuthoringStrategy): Projection {
  const warnings: string[] = []
  const legNames = new Set(authored.legs.map(leg => leg.name))
  for (const block of authored.blocks)
    if (!legNames.has(block.leg))
      warnings.push(`block \`${block.name}\` names unknown leg \`${block.leg}\` and was dropped`)

  const strategy: IrDocument = {
    meta: { name: authored.name, description: authored.description, dsl_version: DSL_VERSION },
    instruments: Object.fromEntries(authored.instruments.map(i => [i.nick, i.id])),
    signals: Object.fromEntries(authored.signals.map(s => [s.nick, s.id])),
    state: authored.state.map(stateSlot),
  }
  if (authored.params.length)
    strategy['params'] = authored.params.map(p => ({
      name: p.name,
      type: typeSpec({ type: p.type, variants: p.variants }),
      default: p.type === 'bool' ? p.default === 'true' : p.default,
    }))
  const exprs = expressions(authored)
  if (exprs) strategy['expressions'] = exprs
  strategy['triggers'] = authored.triggers.map(t => trigger(t, warnings))
  strategy['legs'] = authored.legs.map(leg => ({
    name: leg.name,
    resident: leg.resident,
    arm: leg.arm,
    body: authored.blocks
      .filter(block => block.leg === leg.name)
      .map(block => {
        const out: IrObject = { name: block.name }
        if (block.guard !== null) out['if'] = block.guard
        out['do'] = block.items.map(doItem)
        // The compiler's block field is spelled `then`; the authoring side calls it next_leg.
        // oxlint-disable-next-line unicorn/no-thenable
        if (block.next_leg !== null) out['then'] = block.next_leg
        return out
      }),
  }))
  return { strategy, warnings }
}
