import * as z from 'zod/mini'
import { described } from './describe'

/**
 * The document the writer model emits. It is the compiler's IR v0.3 flattened into a
 * shape OpenAI strict mode accepts: every object closed, every field required
 * (absent means `null`), no unions, nesting at most four levels deep. `projectToIr`
 * turns it into the real document; the compiler validates the semantics.
 */

const IDENT = /^[a-zA-Z][a-zA-Z0-9_]*$/
const DECIMAL = /^(0|[1-9][0-9]*)(\.[0-9]*[1-9])?$/
const SIGNED_DECIMAL = /^-?(0|[1-9][0-9]*)(\.[0-9]*[1-9])?$/
const DURATION = /^[0-9]+(ms|s|m|h)$/

const ident = z.string().check(z.regex(IDENT))
const decimal = z.string().check(z.regex(DECIMAL))
const signedDecimal = z.string().check(z.regex(SIGNED_DECIMAL))
const duration = z.string().check(z.regex(DURATION))
const expression = described(
  z.string().check(z.minLength(1), z.maxLength(2000)),
  'One line of the expression grammar: printable ASCII, no double quotes, no backslashes.'
)

export const TypeNameSchema = z.enum(['u8', 'decimal', 'bool', 'string', 'enum'])

const typedValue = z.object({
  type: TypeNameSchema,
  variants: described(z.nullable(z.array(ident)), 'The variants when `type` is `enum`, else null.'),
})

const stateSlot = z.object({
  name: ident,
  kind: z.enum(['scalar', 'window', 'map']),
  value_type: TypeNameSchema,
  value_variants: described(z.nullable(z.array(ident)), 'Variants when value_type is enum.'),
  capacity: described(z.nullable(z.int()), 'Window slots only: 1 to 4096 entries.'),
  key_types: described(z.nullable(z.array(typedValue)), 'Map slots only: one entry per key.'),
  max_keys: described(z.nullable(z.int()), 'Map slots only: the maximum number of live keys.'),
})

const param = z.object({
  name: ident,
  type: TypeNameSchema,
  variants: described(z.nullable(z.array(ident)), 'Variants when type is enum.'),
  default: described(
    z.string(),
    'The default as text: a canonical decimal, `true`/`false`, a variant name, or free text.'
  ),
})

export const ComparatorSchema = z.enum(['==', '!=', '<', '<=', '>', '>='])

const conditionLeaf = z.object({
  signal: described(z.nullable(ident), 'Signal nickname for a bare or field leaf; null for expr.'),
  field: described(z.nullable(ident), 'Field of that signal, or null for a bare leaf.'),
  expr: described(
    z.nullable(expression),
    'Pure arithmetic over `signal.<nick>.<field>`, `param.*`, `envelope.*`; null unless an expr leaf.'
  ),
  comparator: described(z.nullable(ComparatorSchema), 'Null only on a bare leaf.'),
  value: described(
    z.nullable(z.string()),
    'The compared value as text: a signed decimal, a variant name, a string, or a param name.'
  ),
  value_kind: described(
    z.nullable(z.enum(['literal', 'str', 'param'])),
    '`literal` for decimals and variants, `str` for string fields, `param` for a param name.'
  ),
})

const trigger = z.object({
  name: ident,
  source: z.enum(['condition', 'window_clock', 'after', 'interval']),
  group: described(
    z.enum(['single', 'all', 'any']),
    'Condition triggers: `single` for one leaf, `all` (AND) or `any` (OR) for two or more.'
  ),
  leaves: described(z.array(conditionLeaf), 'Condition triggers only; empty for timers.'),
  instrument: described(z.nullable(ident), 'window_clock only: the instrument nickname.'),
  duration: described(
    z.nullable(duration),
    'window_clock: time before close; after: time since leg entry; interval: the period.'
  ),
  fire: z.enum(['on_crossing', 'if_true']),
  rearm: z.enum(['fresh_crossing', 'immediate', 'never']),
  cooldown: described(z.nullable(duration), 'Minimum gap between fires, or null.'),
})

const leg = z.object({
  name: ident,
  resident: described(z.boolean(), 'True when the leg may idle forever.'),
  arm: described(z.array(ident), 'Trigger names that wake this leg. Empty means terminal.'),
})

export const DoItemKindSchema = z.enum([
  'statement',
  'place',
  'cancel',
  'cancel_replace',
  'target_position',
])

const doItem = z.object({
  kind: DoItemKindSchema,
  statement: described(
    z.nullable(expression),
    '`set state.<slot> = <expr>` or `push state.<window> = <expr>`; null for intents.'
  ),
  instrument: z.nullable(ident),
  outcome: described(z.nullable(ident), 'The outcome label, e.g. `up` or `down`.'),
  side: z.nullable(z.enum(['buy', 'sell'])),
  order_type: z.nullable(z.enum(['market', 'limit'])),
  tif: z.nullable(z.enum(['ioc', 'gtc'])),
  price: described(z.nullable(expression), 'Limit orders only; clamp-wrapped.'),
  price_band_lo: described(z.nullable(decimal), 'Limit orders only.'),
  price_band_hi: described(z.nullable(decimal), 'Limit orders only.'),
  size: described(z.nullable(expression), 'Clamp-wrapped and sourced from `envelope.*`.'),
  max_size: described(z.nullable(expression), 'Sourced from `envelope.*`.'),
  tag: described(z.nullable(ident), 'Order tag; cancel intents name the tag to cancel.'),
  post_only: z.nullable(z.boolean()),
  expire_after: z.nullable(duration),
  reduce_only: z.nullable(z.boolean()),
  on_fill: described(z.nullable(ident), 'Leg to enter when the order fully fills, or null.'),
  on_partial: z.nullable(ident),
  on_reject: z.nullable(ident),
  on_cancel_ack: z.nullable(ident),
  on_expiry: z.nullable(ident),
})

const block = z.object({
  leg: described(ident, 'The leg this block belongs to. Blocks run in listed order.'),
  name: ident,
  guard: described(z.nullable(expression), 'The `if` expression; null means always runs.'),
  next_leg: described(z.nullable(ident), 'Leg to transition to after this block, or null.'),
  items: z.array(doItem),
})

export const AuthoringStrategySchema = z.object({
  name: z.string(),
  description: described(
    z.string(),
    'A faithful English restatement of what the document does, in the user’s terms.'
  ),
  instruments: z.array(z.object({ nick: ident, id: z.string() })),
  signals: z.array(z.object({ nick: ident, id: z.string() })),
  state: z.array(stateSlot),
  params: z.array(param),
  named_expressions: z.array(z.object({ name: ident, expr: expression })),
  tables: z.array(
    z.object({ name: ident, keys: z.array(signedDecimal), values: z.array(decimal) })
  ),
  triggers: z.array(trigger),
  legs: described(z.array(leg), 'In order; the first leg is the initial mode.'),
  blocks: z.array(block),
})

export type AuthoringStrategy = z.infer<typeof AuthoringStrategySchema>
export type AuthoringTrigger = z.infer<typeof trigger>
export type AuthoringDoItem = z.infer<typeof doItem>
export type AuthoringStateSlot = z.infer<typeof stateSlot>
export type AuthoringTypedValue = z.infer<typeof typedValue>
