import type { ActionKind, StrategyPlan } from '../plan'
import { renderPlan } from '../plan'
import { BTC_SIDES, type Sides } from '../sides'
import { endgame } from './endgame'
import { everyTime } from './every-time'
import { momentumFlip } from './flip'
import { quoter } from './quoter'
import { reEntry } from './re-entry'
import { scaleIn } from './scale-in'
import type { Derivation } from './shared'
import { signalFollow } from './signal-follow'
import { threshold } from './threshold'

export type { Derivation } from './shared'

/** Every worked derivation, one per shape a user is likely to describe. */
export const DERIVATIONS: readonly Derivation[] = [
  threshold,
  momentumFlip,
  scaleIn,
  quoter,
  reEntry,
  everyTime,
  endgame,
  signalFollow,
]

function kinds(plan: StrategyPlan): Set<ActionKind> {
  return new Set(plan.legs.flatMap(leg => leg.actions.map(a => a.kind)))
}

/** Whether a derivation's document is written in these sides' labels. */
function speaks(derivation: Derivation, sides: Sides) {
  return derivation.document.blocks.some(block =>
    block.items.some(item => item.outcome === sides.quoted || item.outcome === sides.other)
  )
}

/**
 * The derivation whose plan looks most like `plan`: shared action kinds weigh most,
 * a different leg count costs a little, and one written in the market's own labels
 * edges out an equal shape in another market's. Ties go to the earlier, simpler shape.
 */
export function nearestDerivation(plan: StrategyPlan, sides: Sides = BTC_SIDES): Derivation {
  const wanted = kinds(plan)
  let best: { derivation: Derivation; score: number } | undefined
  for (const derivation of DERIVATIONS) {
    const shared = [...kinds(derivation.plan)].filter(kind => wanted.has(kind)).length
    const legGap = Math.abs(derivation.plan.legs.length - plan.legs.length)
    const score = 3 * shared - legGap + (speaks(derivation, sides) ? 1 : 0)
    if (!best || score > best.score) best = { derivation, score }
  }
  if (!best) throw new Error('no derivations')
  return best.derivation
}

export function renderDerivation(derivation: Derivation) {
  return [
    `### ${derivation.id}`,
    `Sentence: ${derivation.sentence}`,
    'Reasoning:',
    ...derivation.steps.map((step, index) => `${index + 1}. ${step}`),
    'Plan:',
    renderPlan(derivation.plan),
    'Document:',
    JSON.stringify(derivation.document),
  ].join('\n')
}

/** The sentence and reasoning of every derivation, for the planner. */
export function renderDerivationReasoning() {
  return DERIVATIONS.map(derivation =>
    [
      `### ${derivation.id} — ${derivation.when}`,
      `Sentence: ${derivation.sentence}`,
      ...derivation.steps.map((step, index) => `${index + 1}. ${step}`),
      'Plan:',
      renderPlan(derivation.plan),
    ].join('\n')
  ).join('\n\n')
}
