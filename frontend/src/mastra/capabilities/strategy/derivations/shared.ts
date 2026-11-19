import type { AuthoringStrategy } from '../authoring-schema'
import type { StrategyBrief } from '../brief'
import type { PlanAction, PlanLeg, PlanWake, StrategyPlan } from '../plan'

/**
 * A worked derivation: the user's sentence, the brief Kyros would fill, the seven
 * derivation steps written out, the plan they produce, and the document built from
 * fragments. The writer is shown the closest one as reasoning to imitate, never as a
 * template to edit.
 */
export interface Derivation {
  id: string
  /** When this shape is the nearest one. */
  when: string
  sentence: string
  brief: StrategyBrief
  steps: string[]
  plan: StrategyPlan
  document: AuthoringStrategy
}

export const SERIES = 'btc-5m'
export const MARKET = `${SERIES}.current`
export const BOOK = `${SERIES}.current.price.v1`
export const RSI = 'btc.kraken.rsi.v1'

/** The worked money: $100 in, $10 a round, the trade share set per shape. */
export function brief(
  fields: Partial<StrategyBrief> & Pick<StrategyBrief, 'summary' | 'goal' | 'entry' | 'exit'>
): StrategyBrief {
  return {
    series: SERIES,
    signal_ids: [],
    outcome: 'up',
    recurrence: 'once_per_round',
    interval: null,
    entry_style: 'market',
    allocation: '100',
    round_share: '10%',
    trade_share: '25%',
    stop_after_loss: '100',
    stop_before_close: '10s',
    assumptions: [],
    ...fields,
  }
}

export function wake(fields: Partial<PlanWake> & Pick<PlanWake, 'trigger' | 'phrase'>): PlanWake {
  return {
    source: 'condition',
    fire: 'on_crossing',
    rearm: 'fresh_crossing',
    cooldown: null,
    ...fields,
  }
}

export function tick(phrase: string): PlanWake {
  return wake({ trigger: 'book_tick', phrase, fire: 'if_true', rearm: 'immediate' })
}

export function stopWake(remaining: string): PlanWake {
  return wake({
    trigger: `last_${remaining}`,
    phrase: `${remaining} before the close`,
    source: 'window_clock',
  })
}

export function act(fields: Partial<PlanAction> & Pick<PlanAction, 'on' | 'kind'>): PlanAction {
  return { tag: null, share: null, gate: null, leads_to: null, ...fields }
}

export function stopAction(remaining: string): PlanAction {
  return act({ on: `last_${remaining}`, kind: 'none', leads_to: 'done' })
}

export function leg(fields: Partial<PlanLeg> & Pick<PlanLeg, 'name'>): PlanLeg {
  return { holds: 'flat', wakes: [], actions: [], ...fields }
}

export const DONE_LEG: PlanLeg = leg({ name: 'done' })
export const DONE = { name: 'done', resident: true, arm: [] as string[] }

/** The empty sections every authoring document carries. */
export function noExtras(): Pick<
  AuthoringStrategy,
  'state' | 'params' | 'named_expressions' | 'tables'
> {
  return { state: [], params: [], named_expressions: [], tables: [] }
}

export function bookOnly() {
  return {
    instruments: [{ nick: 'market', id: MARKET }],
    signals: [{ nick: 'book', id: BOOK }],
  }
}
