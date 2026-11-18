import * as z from 'zod/mini'
import { describe, expect, it } from 'vitest'
import { AuthoringStrategySchema } from '@/mastra/capabilities/strategy/authoring-schema'

interface SchemaNode {
  type?: string
  properties?: Record<string, SchemaNode>
  items?: SchemaNode
  anyOf?: SchemaNode[]
  required?: string[]
  additionalProperties?: boolean
}

// OpenAI strict mode refuses schemas nested deeper than five object levels.
const STRICT_MODE_MAX_DEPTH = 5

function objectDepth(node: SchemaNode): number {
  if (node.type === 'object' && node.properties)
    return 1 + Math.max(0, ...Object.values(node.properties).map(objectDepth))
  if (node.type === 'array' && node.items) return objectDepth(node.items)
  if (node.anyOf) return Math.max(0, ...node.anyOf.map(objectDepth))
  return 0
}

function everyObjectIsStrict(node: SchemaNode): boolean {
  if (node.type === 'object' && node.properties) {
    const keys = Object.keys(node.properties)
    const closed = node.additionalProperties === false
    const allRequired = keys.every(key => node.required?.includes(key))
    return closed && allRequired && Object.values(node.properties).every(everyObjectIsStrict)
  }
  if (node.type === 'array' && node.items) return everyObjectIsStrict(node.items)
  if (node.anyOf) return node.anyOf.every(everyObjectIsStrict)
  return true
}

describe('authoring schema', () => {
  const schema = z.toJSONSchema(AuthoringStrategySchema) as SchemaNode

  it('stays within strict-mode nesting depth', () => {
    expect(objectDepth(schema)).toBeLessThanOrEqual(STRICT_MODE_MAX_DEPTH)
  })

  it('closes every object and requires every field', () => {
    expect(everyObjectIsStrict(schema)).toBe(true)
  })

  it('uses no unions other than nullable fields', () => {
    const unions: SchemaNode[][] = []
    function walk(node: SchemaNode) {
      if (node.anyOf) unions.push(node.anyOf)
      for (const child of Object.values(node.properties ?? {})) walk(child)
      if (node.items) walk(node.items)
      node.anyOf?.forEach(walk)
    }
    walk(schema)
    for (const branches of unions) {
      expect(branches).toHaveLength(2)
      expect(branches.some(branch => branch.type === 'null')).toBe(true)
    }
  })
})
