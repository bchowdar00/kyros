import * as z from 'zod/mini'
import { describe, expect, it } from 'vitest'
import { AuthoringStrategySchema } from '@/mastra/capabilities/strategy/authoring-schema'
import { described } from '@/mastra/capabilities/strategy/describe'

interface SchemaNode {
  description?: string
  properties?: Record<string, SchemaNode>
  items?: SchemaNode
}

describe('described', () => {
  it('describes one use of a shared schema, not every use of it', () => {
    const ident = z.string()
    const schema = z.object({ leg: described(ident, 'The leg.'), name: ident })
    const json = z.toJSONSchema(schema) as SchemaNode
    expect(json.properties?.['leg']?.description).toBe('The leg.')
    expect(json.properties?.['name']?.description).toBeUndefined()
  })

  it('keeps the description on the schema it returns', () => {
    const text = described(z.string(), 'Words.')
    expect((z.toJSONSchema(text) as SchemaNode).description).toBe('Words.')
  })

  it('does not tell the writer that every name in the document is a block’s leg', () => {
    const json = z.toJSONSchema(AuthoringStrategySchema) as SchemaNode
    const block = json.properties?.['blocks']?.items?.properties
    const trigger = json.properties?.['triggers']?.items?.properties
    expect(block?.['leg']?.description).toContain('The leg this block belongs to')
    expect(block?.['name']?.description).toBeUndefined()
    expect(trigger?.['name']?.description).toBeUndefined()
  })
})
