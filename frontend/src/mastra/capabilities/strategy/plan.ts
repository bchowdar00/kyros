import * as z from 'zod/mini'
import { described } from './describe'

/**
 * The structural plan the writer commits to before any document is written: the
 * output of the derivation procedure's first five steps. Every field is a fact the
 * validator can check against the brief without a model, and the mechanical review
 * later checks the compiled document against it. Shaped for OpenAI strict mode: every
 * field required, absent means null, no unions.
 */

const IDENT = /^[a-zA-Z][a-zA-Z0-9_]*$/
const DURATION = /^[0-9]+(ms|s|m|h)$/
const SHARE = /^(0|1|0\.[0-9]*[1-9])$/

const ident = z.string().check(z.regex(IDENT))
const duration = z.string().check(z.regex(DURATION))

/** What a leg's position is while the strategy is in that mode. */
export const HoldsSchema = z.enum(['flat', 'up', 'down', 'either'])

/** What one wake does. Every kind maps onto one fragment the document is built from. */
export const ActionKindSchema = z.enum([
  /** Buy up at once: an immediate limit a cent through the ask. */
  'take_up',
  /** Buy down at once: an immediate limit a cent through the complement of the bid. */
  'take_down',
  /** Rest a bid on up. */
  'rest_bid',
  /** Rest an ask on up. */
  'rest_ask',
  /** Cancel-replace every resting tag to follow the book. */
  'requote',
  /** Cancel resting tags. */
  'cancel',
  /** Sell the whole position at market, reduce-only, on the side the leg holds. */
  'flatten',
  /** Flatten, then take the other side with the dollars released. */
  'flip',
  /** Only remember something (state writes), place nothing. */
  'record',
  /** Nothing but the transition. */
  'none',
])
export type ActionKind = z.infer<typeof ActionKindSchema>

export const PlanWakeSchema = z.object({
  trigger: described(ident, 'The trigger name the document will declare.'),
  phrase: described(z.string(), 'The words in the brief this wake comes from.'),
  source: z.enum(['condition', 'window_clock', 'after', 'interval']),
  fire: z.enum(['on_crossing', 'if_true']),
  rearm: z.enum(['fresh_crossing', 'immediate', 'never']),
  cooldown: described(z.nullable(duration), 'Minimum gap between fires, or null.'),
})
export type PlanWake = z.infer<typeof PlanWakeSchema>

export const PlanActionSchema = z.object({
  on: described(ident, 'The wake (trigger name) this action answers.'),
  kind: ActionKindSchema,
  tag: described(z.nullable(ident), 'The order tag placed or cancelled, or null.'),
  share: described(
    z.nullable(z.string().check(z.regex(SHARE))),
    'For takes and rests: the fraction of the round budget one order spends, e.g. `0.25`. Null otherwise.'
  ),
  gate: described(
    z.nullable(ident),
    'A state slot the guard reads and the block writes (a counter or flag that stops re-fire), or null.'
  ),
  leads_to: described(z.nullable(ident), 'The leg entered after this action, or null to stay.'),
})
export type PlanAction = z.infer<typeof PlanActionSchema>

export const PlanLegSchema = z.object({
  name: ident,
  holds: HoldsSchema,
  wakes: described(z.array(PlanWakeSchema), 'Everything armed while in this leg.'),
  actions: described(z.array(PlanActionSchema), 'What each wake does, in block order.'),
})
export type PlanLeg = z.infer<typeof PlanLegSchema>

export const PlanMemorySchema = z.object({
  slot: ident,
  type: z.enum(['u8', 'decimal', 'bool', 'string']),
  holds: described(z.string(), 'What the slot remembers, in words.'),
})

export const PlanFateSchema = z.object({
  tag: ident,
  fill: described(z.nullable(ident), 'Leg on a full fill, or null to stay.'),
  expiry: described(z.nullable(ident), 'Leg when an immediate order gets nothing, or null.'),
  reject: described(z.nullable(ident), 'Leg when the venue or the risk gate refuses, or null.'),
})

export const StrategyPlanSchema = z.object({
  situations: described(
    z.array(z.string()),
    'The situations the strategy can be in, in the user’s words, one per leg.'
  ),
  legs: described(z.array(PlanLegSchema), 'In order; the first is where the strategy starts.'),
  memory: z.array(PlanMemorySchema),
  fates: z.array(PlanFateSchema),
  stop_before_close: described(
    z.nullable(duration),
    'How long before the close the stop timer fires, or null when there is none.'
  ),
})
export type StrategyPlan = z.infer<typeof StrategyPlanSchema>

const takes = new Set<ActionKind>(['take_up', 'take_down', 'flip'])
const rests = new Set<ActionKind>(['rest_bid', 'rest_ask'])

/** Whether an action places a new order that opens exposure. */
export function opensExposure(kind: ActionKind) {
  return takes.has(kind) || rests.has(kind)
}

/** Whether an action rests on the book. */
export function restsOnBook(kind: ActionKind) {
  return rests.has(kind)
}

export function renderPlan(plan: StrategyPlan) {
  const lines: string[] = []
  lines.push(`Situations: ${plan.situations.join('; ')}`)
  for (const leg of plan.legs) {
    lines.push(`Leg ${leg.name} (holds ${leg.holds})`)
    for (const wake of leg.wakes) {
      const cooldown = wake.cooldown ? `, cooldown ${wake.cooldown}` : ''
      lines.push(
        `  wake ${wake.trigger}: ${wake.source}, ${wake.fire} / ${wake.rearm}${cooldown} — "${wake.phrase}"`
      )
    }
    for (const action of leg.actions) {
      const parts = [`  on ${action.on}: ${action.kind}`]
      if (action.tag) parts.push(`tag ${action.tag}`)
      if (action.share) parts.push(`share ${action.share}`)
      if (action.gate) parts.push(`gated by state.${action.gate}`)
      parts.push(action.leads_to ? `then ${action.leads_to}` : 'stay')
      lines.push(parts.join(', '))
    }
  }
  if (plan.memory.length)
    lines.push(`Memory: ${plan.memory.map(m => `${m.slot} (${m.type}): ${m.holds}`).join('; ')}`)
  if (plan.fates.length)
    lines.push(
      `Fates: ${plan.fates
        .map(
          f =>
            `${f.tag}: fill→${f.fill ?? 'stay'}, expiry→${f.expiry ?? 'stay'}, reject→${f.reject ?? 'stay'}`
        )
        .join('; ')}`
    )
  lines.push(`Stop before close: ${plan.stop_before_close ?? 'none'}`)
  return lines.join('\n')
}
