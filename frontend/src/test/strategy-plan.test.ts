import { describe, expect, it } from 'vitest'
import type { StrategyBrief } from '@/mastra/capabilities/strategy/brief'
import {
  StrategyPlanSchema,
  renderPlan,
  type PlanAction,
  type PlanLeg,
  type PlanWake,
  type StrategyPlan,
} from '@/mastra/capabilities/strategy/plan'
import { briefShare, checkPlan } from '@/mastra/capabilities/strategy/plan-check'

const brief: StrategyBrief = {
  series: 'btc-5m',
  summary: 'Buys whichever side crosses 70 cents and flips when it falls under 50.',
  goal: 'ride momentum and flip',
  entry: 'either side above 70 cents',
  signal_ids: [],
  outcome: 'either',
  recurrence: 'once_per_round',
  interval: null,
  entry_style: 'market',
  exit: 'flip when the held side falls below 50 cents, else hold to settlement',
  allocation: '100',
  round_share: '10%',
  trade_share: '50%',
  stop_after_loss: '100',
  stop_before_close: '10s',
  assumptions: [],
}

const wake = (fields: Partial<PlanWake> & { trigger: string }): PlanWake => ({
  phrase: fields.trigger,
  source: 'condition',
  fire: 'on_crossing',
  rearm: 'fresh_crossing',
  cooldown: null,
  ...fields,
})
const stop = wake({ trigger: 'last_10s', source: 'window_clock' })
const action = (
  fields: Partial<PlanAction> & { on: string; kind: PlanAction['kind'] }
): PlanAction => ({
  tag: null,
  share: null,
  gate: null,
  leads_to: null,
  ...fields,
})
const leg = (fields: Partial<PlanLeg> & { name: string }): PlanLeg => ({
  holds: 'flat',
  wakes: [],
  actions: [],
  ...fields,
})

/** The flip plan: three situations, a stop timer everywhere, walk-backs on the takes. */
const flip: StrategyPlan = {
  situations: ['hunting', 'holding up', 'holding down'],
  legs: [
    leg({
      name: 'hunting',
      wakes: [wake({ trigger: 'up_rich' }), wake({ trigger: 'down_rich' }), stop],
      actions: [
        action({
          on: 'up_rich',
          kind: 'take_up',
          tag: 'entry',
          share: '0.5',
          leads_to: 'holding_up',
        }),
        action({
          on: 'down_rich',
          kind: 'take_down',
          tag: 'entry',
          share: '0.5',
          leads_to: 'holding_down',
        }),
        action({ on: 'last_10s', kind: 'none', leads_to: 'done' }),
      ],
    }),
    leg({
      name: 'holding_up',
      holds: 'up',
      wakes: [wake({ trigger: 'up_cheap' }), stop],
      actions: [
        action({ on: 'up_cheap', kind: 'flip', tag: 'flip', leads_to: 'holding_down' }),
        action({ on: 'last_10s', kind: 'none', leads_to: 'done' }),
      ],
    }),
    leg({
      name: 'holding_down',
      holds: 'down',
      wakes: [wake({ trigger: 'down_cheap' }), stop],
      actions: [
        action({ on: 'down_cheap', kind: 'flip', tag: 'flip', leads_to: 'holding_up' }),
        action({ on: 'last_10s', kind: 'none', leads_to: 'done' }),
      ],
    }),
    leg({ name: 'done' }),
  ],
  memory: [],
  fates: [
    { tag: 'entry', fill: null, expiry: 'hunting', reject: 'hunting' },
    { tag: 'flip', fill: null, expiry: 'hunting', reject: 'hunting' },
  ],
  stop_before_close: '10s',
}

/** The first action of a leg, for patching. */
function firstAction(l: PlanLeg): PlanAction {
  const [first] = l.actions
  if (!first) throw new Error(`leg ${l.name} has no actions`)
  return first
}

function withLeg(plan: StrategyPlan, name: string, patch: (leg: PlanLeg) => PlanLeg): StrategyPlan {
  return { ...plan, legs: plan.legs.map(l => (l.name === name ? patch(l) : l)) }
}

describe('checkPlan', () => {
  it('accepts the flip plan and renders it', () => {
    expect(StrategyPlanSchema.safeParse(flip).success).toBe(true)
    expect(checkPlan(brief, flip)).toEqual([])
    expect(renderPlan(flip)).toContain('Leg holding_up (holds up)')
    expect(renderPlan(flip)).toContain('on up_cheap: flip, tag flip, then holding_down')
  })

  it('reads the brief share as a canonical fraction', () => {
    expect(briefShare(brief)).toBe('0.5')
    expect(briefShare({ ...brief, trade_share: '0.25' })).toBe('0.25')
    expect(briefShare({ ...brief, trade_share: '$2.50' })).toBeNull()
  })

  it('requires a three-state flip to lead to the opposite side', () => {
    const twoState = {
      ...flip,
      legs: flip.legs.filter(l => l.name !== 'holding_down'),
    }
    const problems = checkPlan(brief, twoState)
    expect(problems).toContainEqual(expect.stringContaining('unknown leg `holding_down`'))
    expect(problems).toContainEqual(expect.stringContaining('does not lead to a leg holding down'))
  })

  it('refuses a flatten in a leg that holds nothing and a flip from an unknown side', () => {
    const wrong = withLeg(flip, 'holding_up', l => ({
      ...l,
      holds: 'flat',
      actions: [
        action({ on: 'up_cheap', kind: 'flatten', leads_to: 'done' }),
        ...l.actions.slice(1),
      ],
    }))
    expect(checkPlan(brief, wrong)).toContainEqual(
      expect.stringContaining('flattens but holds nothing')
    )
    const either = withLeg(flip, 'holding_up', l => ({ ...l, holds: 'either' }))
    expect(checkPlan(brief, either)).toContainEqual(
      expect.stringContaining('a flip needs a known side')
    )
  })

  it('refuses an action on a trigger the leg does not arm', () => {
    const unarmed = withLeg(flip, 'holding_up', l => ({ ...l, wakes: [stop] }))
    expect(checkPlan(brief, unarmed)).toContainEqual(
      expect.stringContaining('acts on `up_cheap` but does not arm it')
    )
  })

  it('refuses a firehose wake with nothing to pace it, and accepts each pacing device', () => {
    const firehose = (extra: Partial<PlanWake>, act: Partial<PlanAction>) =>
      withLeg(flip, 'hunting', l => ({
        ...l,
        wakes: [wake({ trigger: 'up_rich', fire: 'if_true', rearm: 'immediate', ...extra }), stop],
        actions: [
          action({ on: 'up_rich', kind: 'take_up', tag: 'entry', share: '0.5', ...act }),
          action({ on: 'last_10s', kind: 'none', leads_to: 'done' }),
        ],
      }))
    const every = { ...brief, outcome: 'up' as const, recurrence: 'every_time' as const }
    expect(checkPlan(every, firehose({}, { leads_to: null }))).toContainEqual(
      expect.stringContaining('nothing to pace it')
    )
    expect(checkPlan(every, firehose({ cooldown: '5s' }, { leads_to: null }))).not.toContainEqual(
      expect.stringContaining('nothing to pace it')
    )
    const gated = {
      ...firehose({}, { leads_to: null, gate: 'adds' }),
      memory: [{ slot: 'adds', type: 'u8' as const, holds: 'adds so far' }],
    }
    expect(checkPlan(every, gated)).not.toContainEqual(
      expect.stringContaining('nothing to pace it')
    )
    expect(checkPlan(every, firehose({}, { leads_to: 'holding_up' }))).not.toContainEqual(
      expect.stringContaining('nothing to pace it')
    )
  })

  it('reports an undeclared gate slot', () => {
    const gated = withLeg(flip, 'hunting', l => ({
      ...l,
      actions: [{ ...firstAction(l), gate: 'count' }, ...l.actions.slice(1)],
    }))
    expect(checkPlan(brief, gated)).toContainEqual(
      expect.stringContaining('undeclared memory `count`')
    )
  })

  it('holds the trade share to the brief', () => {
    const wrongShare = withLeg(flip, 'hunting', l => ({
      ...l,
      actions: [{ ...firstAction(l), share: '0.25' }, ...l.actions.slice(1)],
    }))
    expect(checkPlan(brief, wrongShare)).toContainEqual(expect.stringContaining('spends 0.25'))
    expect(checkPlan({ ...brief, trade_share: '$5' }, wrongShare)).toEqual([])
  })

  it('requires a take that moves on to route expiry and reject back', () => {
    const noFate = { ...flip, fates: flip.fates.filter(f => f.tag !== 'entry') }
    expect(checkPlan(brief, noFate)).toContainEqual(
      expect.stringContaining('would strand the round')
    )
  })

  it('checks side coverage and entry style against the brief', () => {
    const upOnly = {
      ...flip,
      legs: flip.legs.map(l => ({ ...l, actions: l.actions.filter(a => a.kind !== 'take_down') })),
    }
    const noFlip = {
      ...upOnly,
      legs: upOnly.legs.map(l => ({ ...l, actions: l.actions.filter(a => a.kind !== 'flip') })),
    }
    expect(checkPlan(brief, noFlip)).toContainEqual(
      expect.stringContaining('does not take both up (take_up) and down (take_down)')
    )
    expect(checkPlan({ ...brief, outcome: 'down' }, noFlip)).toContainEqual(
      expect.stringContaining('the plan takes up (take_up)')
    )
    expect(checkPlan({ ...brief, entry_style: 'limit' }, flip)).toContainEqual(
      expect.stringContaining('never rests one')
    )
  })

  it('requires the once-per-round entry to leave, spend or gate', () => {
    const again = withLeg(flip, 'hunting', l => ({
      ...l,
      actions: [{ ...firstAction(l), leads_to: null }, ...l.actions.slice(1)],
    }))
    expect(checkPlan(brief, again)).toContainEqual(
      expect.stringContaining('can place on `up_rich` again')
    )
  })

  it('requires the interval wake and the stop timer the brief names', () => {
    expect(checkPlan({ ...brief, recurrence: 'on_interval', interval: '5s' }, flip)).toContainEqual(
      expect.stringContaining('no interval wake')
    )
    const noStop = withLeg({ ...flip, stop_before_close: null }, 'hunting', l => ({
      ...l,
      wakes: l.wakes.filter(w => w.trigger !== 'last_10s'),
      actions: l.actions.filter(a => a.on !== 'last_10s'),
    }))
    const problems = checkPlan(brief, noStop)
    expect(problems).toContainEqual(expect.stringContaining('stops never before the close'))
    const missingArm = withLeg(flip, 'hunting', l => ({
      ...l,
      wakes: l.wakes.filter(w => w.trigger !== 'last_10s'),
      actions: l.actions.filter(a => a.on !== 'last_10s'),
    }))
    expect(checkPlan(brief, missingArm)).toContainEqual(
      expect.stringContaining('`hunting` places orders but has no stop-before-close wake')
    )
  })

  it('requires the first leg to start flat', () => {
    const [first, ...rest] = flip.legs
    if (!first) throw new Error('the flip plan has legs')
    expect(
      checkPlan(brief, { ...flip, legs: [{ ...first, holds: 'up' }, ...rest] })
    ).toContainEqual(expect.stringContaining('must hold flat'))
  })
})
