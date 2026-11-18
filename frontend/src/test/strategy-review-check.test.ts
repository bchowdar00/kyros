import { describe, expect, it } from 'vitest'
import type { AuthoringStrategy } from '@/mastra/capabilities/strategy/authoring-schema'
import { briefSides } from '@/mastra/capabilities/strategy/brief'
import { DERIVATIONS } from '@/mastra/capabilities/strategy/derivations'
import { checkDocumentAgainstPlan } from '@/mastra/capabilities/strategy/review-check'

function derivation(id: string) {
  const found = DERIVATIONS.find(d => d.id === id)
  if (!found) throw new Error(`derivation ${id} exists`)
  return found
}

function withBlocks(
  document: AuthoringStrategy,
  patch: (blocks: AuthoringStrategy['blocks']) => AuthoringStrategy['blocks']
) {
  return { ...document, blocks: patch(document.blocks) }
}

/** The BTC labels a derivation is written in, as an NBA market would name them. */
const relabel = (outcome: string | null) =>
  outcome === 'up' ? 'home' : outcome === 'down' ? 'away' : outcome

describe('checkDocumentAgainstPlan', () => {
  it.each(DERIVATIONS.map(d => [d.id, d] as const))('%s matches its own plan', (_id, d) => {
    expect(
      checkDocumentAgainstPlan(
        d.plan,
        d.document,
        { legs: d.plan.legs.map(l => l.name) },
        briefSides(d.brief)
      )
    ).toEqual([])
  })

  it('matches a document written in the market’s own labels when told the sides', () => {
    const { plan, document } = derivation('threshold-entry-exit')
    const nba = withBlocks(document, blocks =>
      blocks.map(b => ({ ...b, items: b.items.map(i => ({ ...i, outcome: relabel(i.outcome) })) }))
    )
    const sides = { quoted: 'home', other: 'away' }
    expect(checkDocumentAgainstPlan(plan, nba, undefined, sides)).toEqual([])
    // In BTC's labels the same document does not do what the plan says.
    expect(checkDocumentAgainstPlan(plan, nba)).toContainEqual(
      expect.stringContaining('`hunting` has no block that does `take_up`')
    )
  })

  it('catches a three-state plan written as two states', () => {
    const { plan, document } = derivation('momentum-flip')
    const twoState: AuthoringStrategy = {
      ...document,
      legs: document.legs.filter(l => l.name !== 'holding_down'),
      blocks: document.blocks.filter(b => b.leg !== 'holding_down'),
    }
    const mismatches = checkDocumentAgainstPlan(plan, twoState, {
      legs: ['hunting', 'holding_up', 'done'],
    })
    expect(mismatches).toContainEqual(
      expect.stringContaining('the plan has legs hunting, holding_up, holding_down, done')
    )
    expect(mismatches).toContainEqual(expect.stringContaining('the compiled program has legs'))
  })

  it('catches a flatten on the wrong side', () => {
    const { plan, document } = derivation('momentum-flip')
    const wrongSide = withBlocks(document, blocks =>
      blocks.map(b =>
        b.name === 'flip_to_up'
          ? { ...b, items: b.items.map(i => (i.reduce_only ? { ...i, side: 'sell' as const } : i)) }
          : b
      )
    )
    expect(checkDocumentAgainstPlan(plan, wrongSide)).toContainEqual(
      expect.stringContaining('`holding_down` has no block that does `flip` on `down_cheap`')
    )
  })

  it('catches a trigger the leg does not arm and a changed edge policy', () => {
    const { plan, document } = derivation('momentum-flip')
    const unarmed = {
      ...document,
      legs: document.legs.map(l => (l.name === 'holding_up' ? { ...l, arm: ['last_10s'] } : l)),
    }
    expect(checkDocumentAgainstPlan(plan, unarmed)).toContainEqual(
      expect.stringContaining(
        'leg `holding_up` arms last_10s but the plan wakes it on up_cheap, last_10s'
      )
    )
    const firehose = {
      ...document,
      triggers: document.triggers.map(t =>
        t.name === 'up_cheap' ? { ...t, fire: 'if_true' as const, rearm: 'immediate' as const } : t
      ),
    }
    expect(checkDocumentAgainstPlan(plan, firehose)).toContainEqual(
      expect.stringContaining(
        'trigger `up_cheap` is condition if_true/immediate in the document but condition on_crossing/fresh_crossing in the plan'
      )
    )
  })

  it('catches a transition, a share and a fate that drifted from the plan', () => {
    const { plan, document } = derivation('threshold-entry-exit')
    const stays = withBlocks(document, blocks =>
      blocks.map(b => (b.name === 'buy_up' ? { ...b, next_leg: null } : b))
    )
    expect(checkDocumentAgainstPlan(plan, stays)).toContainEqual(
      expect.stringContaining('moves to nowhere but the plan says holding')
    )
    const halfSize = withBlocks(document, blocks =>
      blocks.map(b =>
        b.name === 'buy_up'
          ? {
              ...b,
              items: b.items.map(i => ({ ...i, size: (i.size ?? '').replace('0.25', '0.5') })),
            }
          : b
      )
    )
    expect(checkDocumentAgainstPlan(plan, halfSize)).toContainEqual(
      expect.stringContaining('but the plan spends 0.25')
    )
    const noWalkBack = withBlocks(document, blocks =>
      blocks.map(b =>
        b.name === 'buy_up' ? { ...b, items: b.items.map(i => ({ ...i, on_expiry: null })) } : b
      )
    )
    expect(checkDocumentAgainstPlan(plan, noWalkBack)).toContainEqual(
      expect.stringContaining('tag `entry` routes expiry to stay but the plan says hunting')
    )
  })

  it('catches a missing stop timer and an undeclared memory slot', () => {
    const { plan, document } = derivation('scale-in')
    const noStop = { ...document, triggers: document.triggers.filter(t => t.name !== 'last_10s') }
    expect(checkDocumentAgainstPlan(plan, noStop)).toContainEqual(
      expect.stringContaining('no window_clock trigger stops 10s')
    )
    const noState = { ...document, state: document.state.filter(s => s.name !== 'adds') }
    expect(checkDocumentAgainstPlan(plan, noState)).toContainEqual(
      expect.stringContaining('memory `adds` is in the plan but not declared')
    )
    const ungated = withBlocks(document, blocks =>
      blocks.map(b =>
        b.name === 'add'
          ? { ...b, guard: 'triggered(book_tick) and signal.book.ask <= state.last_entry - 0.05' }
          : b
      )
    )
    expect(checkDocumentAgainstPlan(plan, ungated)).toContainEqual(
      expect.stringContaining('is not gated on state.adds')
    )
  })
})
