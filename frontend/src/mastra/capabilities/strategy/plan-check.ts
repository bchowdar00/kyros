import { briefSides, type StrategyBrief } from './brief'
import { parseAmount } from './envelope'
import { normalizeOutcome } from './sides'
import {
  opensExposure,
  restsOnBook,
  type PlanAction,
  type PlanLeg,
  type StrategyPlan,
} from './plan'

/**
 * The plan validator: everything about a plan that can be judged against the brief
 * without a model. Each rule is a runtime failure caught before any document exists:
 * a firehose wake with nothing to pace it, a flatten on the wrong side, a trigger a
 * leg never arms, a take that cannot walk back when it gets nothing.
 */

/** The share a brief's trade share means, as a canonical decimal, or null for dollars. */
export function briefShare(brief: StrategyBrief): string | null {
  const amount = parseAmount(brief.trade_share)
  if (!amount || amount.kind !== 'fraction') return null
  const value = amount.numerator / amount.denominator
  if (!Number.isFinite(value)) return null
  return canonical(value)
}

function canonical(value: number) {
  const text = value.toFixed(6).replace(/0+$/, '').replace(/\.$/, '')
  return text === '' ? '0' : text
}

function shareEquals(share: string, expected: string) {
  return Number(share) === Number(expected)
}

function isPaced(leg: PlanLeg, action: PlanAction) {
  const wake = leg.wakes.find(w => w.trigger === action.on)
  if (!wake) return true // reported by the arming rule
  if (wake.source !== 'condition') return true // timers pace themselves
  if (wake.fire === 'on_crossing') return true
  if (wake.rearm !== 'immediate') return true
  if (wake.cooldown !== null) return true
  if (action.leads_to !== null) return true
  return action.gate !== null
}

const opposite = { up: 'down', down: 'up' } as const

export function checkPlan(brief: StrategyBrief, plan: StrategyPlan): string[] {
  const problems: string[] = []
  const legs = new Map(plan.legs.map(leg => [leg.name, leg]))
  const memory = new Set(plan.memory.map(m => m.slot))
  const fates = new Map(plan.fates.map(f => [f.tag, f]))

  if (plan.legs.length === 0) return ['the plan has no legs']
  const [first] = plan.legs
  if (first && first.holds !== 'flat')
    problems.push(
      `the first leg \`${first.name}\` must hold flat: every round starts with no position`
    )

  // Triggers: one policy per name across legs.
  const policies = new Map<string, string>()
  for (const leg of plan.legs) {
    for (const wake of leg.wakes) {
      const policy = JSON.stringify([wake.source, wake.fire, wake.rearm, wake.cooldown])
      const known = policies.get(wake.trigger)
      if (known !== undefined && known !== policy)
        problems.push(`trigger \`${wake.trigger}\` has two different policies in different legs`)
      policies.set(wake.trigger, policy)
    }
  }

  const expected = briefShare(brief)
  let takesUp = 0
  let takesDown = 0
  let rests = 0
  let flips = 0

  for (const leg of plan.legs) {
    const armed = new Set(leg.wakes.map(w => w.trigger))
    for (const action of leg.actions) {
      if (!armed.has(action.on))
        problems.push(`leg \`${leg.name}\` acts on \`${action.on}\` but does not arm it`)
      if (action.leads_to !== null && !legs.has(action.leads_to))
        problems.push(`leg \`${leg.name}\` leads to unknown leg \`${action.leads_to}\``)
      if (action.gate !== null && !memory.has(action.gate))
        problems.push(`leg \`${leg.name}\` is gated by undeclared memory \`${action.gate}\``)

      if (opensExposure(action.kind) && !isPaced(leg, action))
        problems.push(
          `leg \`${leg.name}\` places on \`${action.on}\` on every publish with nothing to pace it: add a cooldown, a counter gate, or a transition out of the leg`
        )

      if (action.kind === 'take_up') takesUp += 1
      if (action.kind === 'take_down') takesDown += 1
      if (restsOnBook(action.kind)) rests += 1

      if (action.kind === 'flip') {
        flips += 1
        if (leg.holds === 'flat' || leg.holds === 'either')
          problems.push(
            `leg \`${leg.name}\` flips but holds \`${leg.holds}\`: a flip needs a known side`
          )
        else {
          const target = action.leads_to === null ? null : legs.get(action.leads_to)
          if (!target || target.holds !== opposite[leg.holds])
            problems.push(
              `leg \`${leg.name}\` flips from ${leg.holds} but does not lead to a leg holding ${opposite[leg.holds]}`
            )
        }
      }
      if (action.kind === 'flatten' && leg.holds === 'flat')
        problems.push(`leg \`${leg.name}\` flattens but holds nothing`)

      if (
        (action.kind === 'take_up' || action.kind === 'take_down' || restsOnBook(action.kind)) &&
        expected !== null
      ) {
        if (action.share === null)
          problems.push(
            `leg \`${leg.name}\` places on \`${action.on}\` without a share; the brief's trade share is ${expected}`
          )
        else if (!shareEquals(action.share, expected))
          problems.push(
            `leg \`${leg.name}\` spends ${action.share} of the round budget on \`${action.on}\` but the brief's trade share is ${expected}`
          )
      }

      // A take that moves on must know what to do when it gets nothing.
      if ((action.kind === 'take_up' || action.kind === 'take_down') && action.leads_to !== null) {
        const fate = action.tag === null ? undefined : fates.get(action.tag)
        if (!fate || fate.expiry === null || fate.reject === null)
          problems.push(
            `leg \`${leg.name}\` takes with tag \`${action.tag ?? '?'}\` and moves to \`${action.leads_to}\`, but no fate routes expiry and reject back: an unfilled take would strand the round`
          )
      }
    }
  }

  // Side coverage against the brief, in the market's own labels.
  const sides = briefSides(brief)
  const outcome = normalizeOutcome(brief.outcome, sides)
  if (outcome === undefined)
    problems.push(
      `the brief's side \`${brief.outcome}\` is not a side of this market: it is ${sides.quoted}, ${sides.other} or either`
    )
  if (outcome === 'either' && flips === 0 && (takesUp === 0 || takesDown === 0))
    problems.push(
      `the brief says either side, but the plan does not take both ${sides.quoted} (take_up) and ${sides.other} (take_down)`
    )
  if (outcome === 'up' && takesDown > 0 && flips === 0)
    problems.push(`the brief says ${sides.quoted}, but the plan takes ${sides.other} (take_down)`)
  if (outcome === 'down' && takesUp > 0 && flips === 0)
    problems.push(`the brief says ${sides.other}, but the plan takes ${sides.quoted} (take_up)`)

  // Entry style.
  if (brief.entry_style === 'limit' && rests === 0)
    problems.push('the brief rests a limit order, but the plan never rests one')
  if (brief.entry_style === 'market' && rests > 0)
    problems.push('the brief takes the market, but the plan rests orders')

  // Recurrence.
  if (brief.recurrence === 'on_interval') {
    const interval = plan.legs.some(leg => leg.wakes.some(w => w.source === 'interval'))
    if (!interval)
      problems.push('the brief fires on an interval, but the plan has no interval wake')
  }
  if (brief.recurrence === 'once_per_round') {
    for (const leg of plan.legs)
      for (const action of leg.actions) {
        if (!opensExposure(action.kind) || action.kind === 'flip') continue
        const wake = leg.wakes.find(w => w.trigger === action.on)
        const timer = wake?.source === 'window_clock' || wake?.source === 'after'
        const once =
          timer || action.leads_to !== null || action.gate !== null || wake?.rearm === 'never'
        if (!once)
          problems.push(
            `the brief fires once per round, but leg \`${leg.name}\` can place on \`${action.on}\` again: leave the leg, spend the trigger, or gate on a counter`
          )
      }
  }

  // The stop timer.
  if (plan.stop_before_close !== brief.stop_before_close)
    problems.push(
      `the plan stops ${plan.stop_before_close ?? 'never'} before the close but the brief says ${brief.stop_before_close}`
    )
  if (plan.stop_before_close !== null)
    for (const leg of plan.legs) {
      const places = leg.actions.some(a => opensExposure(a.kind))
      const stops = leg.wakes.some(w => w.source === 'window_clock')
      if (places && !stops)
        problems.push(`leg \`${leg.name}\` places orders but has no stop-before-close wake`)
    }

  return problems
}
