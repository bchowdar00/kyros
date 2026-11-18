import type { AuthoringDoItem, AuthoringStrategy } from './authoring-schema'
import type { CompileSummary } from './compile-client'
import type { PlanAction, PlanLeg, StrategyPlan } from './plan'
import { BTC_SIDES, type Sides } from './sides'

/**
 * The mechanical half of review: does the document have the shape the plan committed
 * to? Leg names and order, each leg's arm set, each trigger's edge policy, each
 * action's order (side, outcome, type, flag, tag, share, transition), each fate, each
 * memory slot. A mismatch here is a repair, not a note on a card; the prose readback
 * runs only on a clean result.
 */

type Item = AuthoringDoItem
type Block = AuthoringStrategy['blocks'][number]

function guardMentions(block: Block, trigger: string) {
  return block.guard !== null && block.guard.includes(`triggered(${trigger})`)
}

function sameSet(a: readonly string[], b: readonly string[]) {
  const left = new Set(a)
  const right = new Set(b)
  return left.size === right.size && [...left].every(x => right.has(x))
}

interface OrderShape {
  kind: Item['kind']
  /** The market's own label for the side. */
  outcome?: string
  side?: 'buy' | 'sell'
  order_type?: 'market' | 'limit'
  tif?: 'ioc' | 'gtc'
  reduce_only?: boolean
}

function matches(item: Item, shape: OrderShape) {
  if (item.kind !== shape.kind) return false
  if (shape.outcome !== undefined && item.outcome !== shape.outcome) return false
  if (shape.side !== undefined && item.side !== shape.side) return false
  if (shape.order_type !== undefined && item.order_type !== shape.order_type) return false
  if (shape.tif !== undefined && item.tif !== shape.tif) return false
  if (shape.reduce_only !== undefined && (item.reduce_only ?? false) !== shape.reduce_only)
    return false
  return true
}

/** The order shapes each action kind must produce, in the market's labels. */
function shapes(sides: Sides) {
  const takeUp: OrderShape = {
    kind: 'place',
    outcome: sides.quoted,
    side: 'buy',
    order_type: 'limit',
    tif: 'ioc',
    reduce_only: false,
  }
  const takeDown: OrderShape = { ...takeUp, outcome: sides.other }
  const restBid: OrderShape = {
    kind: 'place',
    outcome: sides.quoted,
    side: 'buy',
    order_type: 'limit',
    tif: 'gtc',
  }
  const restAsk: OrderShape = { ...restBid, side: 'sell' }
  const flattenLong: OrderShape = {
    kind: 'place',
    outcome: sides.quoted,
    side: 'sell',
    order_type: 'market',
    reduce_only: true,
  }
  const flattenShort: OrderShape = { ...flattenLong, side: 'buy' }
  return { takeUp, takeDown, restBid, restAsk, flattenLong, flattenShort }
}

function flattenShape(leg: PlanLeg, s: ReturnType<typeof shapes>): OrderShape[] {
  switch (leg.holds) {
    case 'up':
      return [s.flattenLong]
    case 'down':
      return [s.flattenShort]
    default:
      return [{ kind: 'target_position', reduce_only: true }, s.flattenLong, s.flattenShort]
  }
}

/** What items an action must produce, in order, or null when no order is expected. */
function expectedItems(leg: PlanLeg, action: PlanAction, sides: Sides): OrderShape[][] | null {
  const s = shapes(sides)
  switch (action.kind) {
    case 'take_up':
      return [[s.takeUp]]
    case 'take_down':
      return [[s.takeDown]]
    case 'rest_bid':
      return [[s.restBid]]
    case 'rest_ask':
      return [[s.restAsk]]
    case 'requote':
      return [[{ kind: 'cancel_replace' }]]
    case 'cancel':
      return [[{ kind: 'cancel' }]]
    case 'flatten':
      return flattenShape(leg, s).map(shape => [shape])
    case 'flip':
      return leg.holds === 'up' ? [[s.flattenLong, s.takeDown]] : [[s.flattenShort, s.takeUp]]
    case 'record':
    case 'none':
      return null
  }
}

function itemTag(item: Item) {
  return item.tag
}

/** The block in `leg` that answers `action`, matched by trigger, tag and order shape. */
function findBlock(
  blocks: Block[],
  leg: PlanLeg,
  action: PlanAction,
  sides: Sides
): { block: Block; ordered: Item[] } | undefined {
  const candidates = blocks.filter(b => b.leg === leg.name && guardMentions(b, action.on))
  const expected = expectedItems(leg, action, sides)
  for (const block of candidates) {
    if (expected === null) {
      const placesNothing = !block.items.some(i => i.kind !== 'statement')
      if (action.kind === 'none' && placesNothing) return { block, ordered: [] }
      if (action.kind === 'record' && block.items.some(i => i.kind === 'statement'))
        return { block, ordered: [] }
      continue
    }
    for (const sequence of expected) {
      const ordered: Item[] = []
      let cursor = 0
      for (const shape of sequence) {
        const index = block.items.findIndex(
          (item, i) =>
            i >= cursor &&
            matches(item, shape) &&
            (sequence.length > 1 || action.tag === null || itemTag(item) === action.tag)
        )
        if (index === -1) break
        const found = block.items[index]
        if (found) ordered.push(found)
        cursor = index + 1
      }
      if (ordered.length === sequence.length) return { block, ordered }
    }
  }
  return undefined
}

/** `sides` names what the market calls the quoted side and its complement; BTC's by default. */
export function checkDocumentAgainstPlan(
  plan: StrategyPlan,
  authored: AuthoringStrategy,
  summary?: Pick<CompileSummary, 'legs'>,
  sides: Sides = BTC_SIDES
): string[] {
  const mismatches: string[] = []
  const planLegs = plan.legs.map(l => l.name)
  const docLegs = authored.legs.map(l => l.name)
  if (planLegs.join(',') !== docLegs.join(','))
    mismatches.push(
      `the plan has legs ${planLegs.join(', ')} but the document has ${docLegs.join(', ')}`
    )
  if (summary && summary.legs.join(',') !== planLegs.join(','))
    mismatches.push(
      `the compiled program has legs ${summary.legs.join(', ')} but the plan has ${planLegs.join(', ')}`
    )

  const triggers = new Map(authored.triggers.map(t => [t.name, t]))
  for (const leg of plan.legs) {
    const docLeg = authored.legs.find(l => l.name === leg.name)
    if (!docLeg) continue
    const wakes = leg.wakes.map(w => w.trigger)
    if (!sameSet(docLeg.arm, wakes))
      mismatches.push(
        `leg \`${leg.name}\` arms ${docLeg.arm.join(', ') || 'nothing'} but the plan wakes it on ${wakes.join(', ') || 'nothing'}`
      )
    for (const wake of leg.wakes) {
      const trigger = triggers.get(wake.trigger)
      if (!trigger) {
        mismatches.push(`trigger \`${wake.trigger}\` is in the plan but not in the document`)
        continue
      }
      if (
        trigger.source !== wake.source ||
        trigger.fire !== wake.fire ||
        trigger.rearm !== wake.rearm ||
        trigger.cooldown !== wake.cooldown
      )
        mismatches.push(
          `trigger \`${wake.trigger}\` is ${trigger.source} ${trigger.fire}/${trigger.rearm}${trigger.cooldown ? ` cooldown ${trigger.cooldown}` : ''} in the document but ${wake.source} ${wake.fire}/${wake.rearm}${wake.cooldown ? ` cooldown ${wake.cooldown}` : ''} in the plan`
        )
    }
    for (const action of leg.actions) {
      const found = findBlock(authored.blocks, leg, action, sides)
      if (!found) {
        mismatches.push(
          `leg \`${leg.name}\` has no block that does \`${action.kind}\` on \`${action.on}\`${action.tag ? ` with tag \`${action.tag}\`` : ''}`
        )
        continue
      }
      const { block, ordered } = found
      if ((block.next_leg ?? null) !== action.leads_to)
        mismatches.push(
          `block \`${block.name}\` in \`${leg.name}\` moves to ${block.next_leg ?? 'nowhere'} but the plan says ${action.leads_to ?? 'stay'}`
        )
      if (action.share !== null) {
        const sized = ordered.find(i => i.size !== null)
        if (sized && !sized.size?.includes(`envelope.round_budget * ${action.share} `))
          mismatches.push(
            `block \`${block.name}\` in \`${leg.name}\` sizes with \`${sized.size ?? ''}\` but the plan spends ${action.share} of the round budget`
          )
      }
      if (action.gate !== null && !(block.guard ?? '').includes(`state.${action.gate}`))
        mismatches.push(
          `block \`${block.name}\` in \`${leg.name}\` is not gated on state.${action.gate}`
        )
    }
  }

  for (const fate of plan.fates) {
    const orders = authored.blocks
      .flatMap(b => b.items)
      .filter(i => i.kind === 'place' && i.tag === fate.tag)
    if (orders.length === 0) {
      mismatches.push(`no order carries tag \`${fate.tag}\` but the plan routes its fates`)
      continue
    }
    for (const order of orders) {
      const pairs: [string, string | null, string | null][] = [
        ['fill', order.on_fill, fate.fill],
        ['expiry', order.on_expiry, fate.expiry],
        ['reject', order.on_reject, fate.reject],
      ]
      for (const [key, actual, wanted] of pairs)
        if (actual !== wanted)
          mismatches.push(
            `tag \`${fate.tag}\` routes ${key} to ${actual ?? 'stay'} but the plan says ${wanted ?? 'stay'}`
          )
    }
  }

  for (const slot of plan.memory) {
    const declared = authored.state.find(s => s.name === slot.slot)
    if (!declared)
      mismatches.push(`memory \`${slot.slot}\` is in the plan but not declared in state`)
    else if (declared.value_type !== slot.type)
      mismatches.push(
        `memory \`${slot.slot}\` is ${declared.value_type} in the document but ${slot.type} in the plan`
      )
  }

  if (plan.stop_before_close !== null) {
    const stop = authored.triggers.find(
      t => t.source === 'window_clock' && t.duration === plan.stop_before_close
    )
    if (!stop)
      mismatches.push(`no window_clock trigger stops ${plan.stop_before_close} before the close`)
  }

  return mismatches
}
