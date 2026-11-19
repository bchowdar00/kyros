import { describe, expect, it } from 'vitest'
import { AuthoringStrategySchema } from '@/mastra/capabilities/strategy/authoring-schema'
import { fragmentHost } from '@/mastra/capabilities/strategy/fragment-host'
import { FRAGMENTS, flip, renderFragments } from '@/mastra/capabilities/strategy/fragments'
import { projectToIr } from '@/mastra/capabilities/strategy/project-to-ir'

const kinds = Object.keys(FRAGMENTS) as (keyof typeof FRAGMENTS)[]

describe('strategy fragments', () => {
  it.each(kinds)('%s lives in a host document that parses and projects cleanly', kind => {
    const host = fragmentHost('btc-5m', FRAGMENTS[kind].items)
    const parsed = AuthoringStrategySchema.safeParse(host)
    expect(parsed.success, JSON.stringify(parsed.error?.issues)).toBe(true)
    const projection = projectToIr(host)
    expect(projection.warnings).toEqual([])
  })

  it('flattens a long with a sell and a short with a buy, both reduce-only', () => {
    const [long, short] = FRAGMENTS.flatten.items
    expect(long).toMatchObject({ side: 'sell', reduce_only: true, order_type: 'market' })
    expect(short).toMatchObject({ side: 'buy', reduce_only: true, order_type: 'market' })
  })

  it('flips with a flatten of the held side then a take of the other, sized off the released dollars', () => {
    const [flatten, buy] = flip('down', 'flatten', 'flip', { reject: 'hunting', expiry: 'hunting' })
    expect(flatten).toMatchObject({ side: 'buy', reduce_only: true })
    expect(buy).toMatchObject({ outcome: 'up', side: 'buy', tif: 'ioc', on_expiry: 'hunting' })
    expect(buy?.size).toContain('abs(platform.position.size) * (1 - platform.position.avg_cost)')
    expect(buy?.size).toContain('signal.book.ask + 0.01')
  })

  it('renders only the fragments a plan uses', () => {
    const text = renderFragments(['take_up', 'flatten', 'none', 'take_up'])
    expect(text).toContain('### take_up')
    expect(text).toContain('### flatten')
    expect(text).not.toContain('### take_down')
    expect(text.match(/### take_up/g)).toHaveLength(1)
  })
})
