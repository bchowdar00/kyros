import type { AuthoringStrategy } from './authoring-schema'
import { briefSides, renderBrief, type StrategyBrief } from './brief'
import type { CompileSummary } from './compile-client'
import { nearestDerivation, renderDerivation, renderDerivationReasoning } from './derivations'
import type { MoneySummary } from './envelope'
import { renderFragments } from './fragments'
import { renderPlan, type StrategyPlan } from './plan'
import type { SignalIndexEntry } from './registry'
import { renderSignalIndex } from './registry'

/** The prompts each workflow step sends the writer. The rulebook is the agent's instructions. */

/**
 * What this market calls its sides. The rulebook reasons in up and down (the quoted
 * side and its complement); the document must carry the market's own labels.
 */
export function renderSides(brief: StrategyBrief) {
  const { quoted, other } = briefSides(brief)
  const same = quoted === 'up' && other === 'down'
  return `## Sides on this market
The quoted side is \`${quoted}\` and the other side is \`${other}\`. \`take_up\` means take ${quoted}, \`take_down\` means take ${other}; a leg that holds \`up\` holds ${quoted} (a positive position), one that holds \`down\` holds ${other} (a negative position). ${
    same
      ? ''
      : `Wherever the rules say up read ${quoted} and wherever they say down read ${other}; every \`outcome\` in the document is written as \`${quoted}\` or \`${other}\`, never up or down, or the compiler refuses it. The book's fields are \`${quoted}\`, \`${other}\`, \`bid\`, \`ask\` and \`last\`, all in the ${quoted} frame.`
  }`
}

export function scoutPrompt(brief: StrategyBrief, index: readonly SignalIndexEntry[]) {
  return `Choose the signals a strategy needs from the registry below. Pick the fewest that
cover the brief; always include the market book (\`<series>.current.price.v1\`) when a price,
bid, ask or spread is involved. Prefer signals the user named. Do not invent ids.

## Brief
${renderBrief(brief)}

## Registry (live signals for this market)
${renderSignalIndex(index)}`
}

/**
 * The plan step: the derivation procedure's first five steps as a structured object,
 * before any document exists. `problems` carries the validator's objections to the
 * previous attempt.
 */
export function planPrompt(
  brief: StrategyBrief,
  definitions: string,
  problems: readonly string[] = []
) {
  const objections = problems.length
    ? `\n## Your previous plan was refused\nFix every problem and return the whole plan.\n${problems.map(p => `- ${p}`).join('\n')}\n`
    : ''
  return `Run the derivation procedure from your instructions on this brief and return the plan:
the situations (one leg each, the first holds flat), what wakes each leg and with which
edge policy, what each wake does and where it leads, what must be remembered, and the
fates of every take. Name triggers and legs as the document will. Use the brief's trade
share as every take's and rest's share. The stop-before-close timer is a window_clock
wake in every leg that places, with a \`none\` action leading to \`done\`. A take that
moves to another leg needs a fate routing expiry and reject back.
${objections}
${renderSides(brief)}

## Brief
${renderBrief(brief)}

## Signal definitions
${definitions}

## Worked derivations (the reasoning to imitate)
${renderDerivationReasoning()}`
}

/** The write step: the plan is fixed; the document is assembled from fragments. */
export function writePrompt(brief: StrategyBrief, plan: StrategyPlan, definitions: string) {
  const kinds = plan.legs.flatMap(leg => leg.actions.map(a => a.kind))
  const sides = briefSides(brief)
  const nearest = nearestDerivation(plan, sides)
  return `Write the strategy document for this plan. The plan is settled: the document must
have exactly these legs in this order, each leg must arm exactly its wakes, every trigger
must carry the plan's edge policy, and every action must become a block on its wake with
the fragment for its kind, its tag, its share and its transition. Follow the rules in your
instructions exactly; the compiler will refuse anything else. Read only the fields the
signal definitions list. The description restates the plan in plain trading language.

${renderSides(brief)}

## Brief
${renderBrief(brief)}

## Plan
${renderPlan(plan)}

## Signal definitions
${definitions}

## Fragments for the action kinds this plan uses
Copy these spellings; change only the level, the share, the tag and the transitions.
${renderFragments(kinds, sides)}

## The nearest worked derivation
${renderDerivation(nearest)}`
}

export function repairPrompt(authored: AuthoringStrategy, problems: readonly string[]) {
  return `The compiler refused this document. Fix every problem listed and return the whole
corrected document. Change nothing else. Paths are into the projected document: legs[i] is
the i-th leg, body[j] the j-th block of that leg, do[k] the k-th item.

## Problems
${problems.map(problem => `- ${problem.trim()}`).join('\n')}

## Document
${JSON.stringify(authored)}`
}

export function reviewPrompt(
  brief: StrategyBrief,
  plan: StrategyPlan,
  authored: AuthoringStrategy,
  summary: CompileSummary,
  money: MoneySummary | null
) {
  const moneyLines = money
    ? `\n## Money (the envelope the document was compiled against)
Allocation ${money.allocation}; each round sees ${money.roundBudget}; one order may spend up to
${money.tradeBudget}, which is ${money.maxOrderContracts} contract${money.maxOrderContracts === 1 ? '' : 's'} at the worst price of $1 and more
when contracts are cheaper; the strategy stops for good after losing ${money.stopAfterLoss}.
State these numbers in the readback in one sentence.\n`
    : ''
  return `A strategy was compiled from the brief below, following the plan. Its structure has
already been checked against the plan mechanically. Write a readback for the user: two to
four plain sentences saying the situations it can be in, what wakes each, what it does,
how it exits, how often it repeats and how much money it uses. No field names or code.
List at most three assumptions the document makes beyond the brief, each one short.
List a mismatch only where the document would trade differently from the brief in an
ordinary round; edge cases of order timing and venue mechanics are not mismatches.
Be brief: the user reads this on a card.

## Brief
${renderBrief(brief)}

## Plan
${renderPlan(plan)}
${moneyLines}
## Compiled strategy
Name: ${summary.name}
Legs: ${summary.legs.join(', ')}
Subscribed signals: ${summary.subscriptions.join(', ')}

## Document
${JSON.stringify(authored)}`
}
