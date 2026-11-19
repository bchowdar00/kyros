import { describe, expect, it } from 'vitest'
import { AuthoringStrategySchema } from '@/mastra/capabilities/strategy/authoring-schema'
import {
  DERIVATIONS,
  nearestDerivation,
  renderDerivation,
  renderDerivationReasoning,
} from '@/mastra/capabilities/strategy/derivations'
import { StrategyPlanSchema } from '@/mastra/capabilities/strategy/plan'
import { checkPlan } from '@/mastra/capabilities/strategy/plan-check'
import { projectToIr } from '@/mastra/capabilities/strategy/project-to-ir'

describe('strategy derivations', () => {
  it.each(DERIVATIONS.map(d => [d.id, d] as const))(
    '%s is a valid authoring document that projects cleanly',
    (_id, derivation) => {
      const parsed = AuthoringStrategySchema.safeParse(derivation.document)
      expect(parsed.success, JSON.stringify(parsed.error?.issues)).toBe(true)
      const projection = projectToIr(derivation.document)
      expect(projection.warnings).toEqual([])
      expect(projection.strategy['meta']).toMatchObject({ dsl_version: '0.3' })
      expect(projection.strategy['instruments']).toEqual({
        market: `${derivation.brief.series}.current`,
      })
    }
  )

  it.each(DERIVATIONS.map(d => [d.id, d] as const))(
    '%s has a plan that passes the validator against its own brief',
    (_id, derivation) => {
      expect(StrategyPlanSchema.safeParse(derivation.plan).success).toBe(true)
      expect(checkPlan(derivation.brief, derivation.plan)).toEqual([])
    }
  )

  it.each(DERIVATIONS.map(d => [d.id, d] as const))(
    '%s has seven derivation steps and a description that names its situations',
    (_id, derivation) => {
      expect(derivation.steps).toHaveLength(7)
      expect(derivation.plan.legs.length).toBe(derivation.plan.situations.length)
      for (const leg of derivation.plan.legs)
        expect(derivation.document.legs.map(l => l.name)).toContain(leg.name)
    }
  )

  it('has distinct ids', () => {
    expect(new Set(DERIVATIONS.map(d => d.id)).size).toBe(DERIVATIONS.length)
  })

  it('finds the nearest derivation by leg count and action kinds', () => {
    const flip = DERIVATIONS.find(d => d.id === 'momentum-flip')
    if (!flip) throw new Error('flip derivation exists')
    expect(nearestDerivation(flip.plan).id).toBe('momentum-flip')
    const twoLegFlatten = { ...flip.plan, legs: flip.plan.legs.slice(0, 2) }
    expect(nearestDerivation(twoLegFlatten).id).toBe('momentum-flip')
    const [threshold] = DERIVATIONS
    if (!threshold) throw new Error('derivations exist')
    expect(nearestDerivation(threshold.plan).id).toBe('threshold-entry-exit')
  })

  it('renders a derivation with its reasoning, plan and document', () => {
    const [first] = DERIVATIONS
    if (!first) throw new Error('derivations exist')
    const text = renderDerivation(first)
    expect(text).toContain('Sentence:')
    expect(text).toContain('1. ')
    expect(text).toContain('Leg hunting')
    expect(text).toContain('"blocks"')
    expect(renderDerivationReasoning()).toContain('### momentum-flip')
  })
})
