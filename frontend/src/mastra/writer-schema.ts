/**
 * Claude's structured outputs compile a format schema into a grammar, and the API refuses
 * (HTTP 400) a schema with more than 16 union-typed parameters or whose grammar comes out
 * too large. Every nullable field is a union, and the strategy document has dozens; with
 * its unions cut to four the API still answered "The compiled grammar is too large". So a
 * schema over the union limit is not sent as a format at all: it travels as text in the
 * prompt and the answer is validated in this process (see `writer-model.ts`).
 */

/** The API's limit on parameters that use `anyOf` or a type array, per request. */
export const MAX_UNION_PARAMETERS = 16

type SchemaNode = Record<string, unknown>

function isNode(value: unknown): value is SchemaNode {
  return value !== null && typeof value === 'object' && !Array.isArray(value)
}

/** How many parameters in `schema` use `anyOf` or a type array. */
export function countUnionParameters(schema: unknown): number {
  if (Array.isArray(schema))
    return schema.reduce((sum: number, child) => sum + countUnionParameters(child), 0)
  if (!isNode(schema)) return 0
  const own = 'anyOf' in schema || Array.isArray(schema['type']) ? 1 : 0
  return (
    own + Object.values(schema).reduce((sum: number, child) => sum + countUnionParameters(child), 0)
  )
}

/**
 * The non-null schema of a "this or null" node, in either spelling JSON Schema allows
 * (`anyOf: [X, { type: 'null' }]` or `type: [T, 'null']`); `undefined` for any other node.
 */
function nonNullPart(node: SchemaNode): SchemaNode | undefined {
  const { anyOf, type } = node
  if (Array.isArray(anyOf)) {
    const members = anyOf.filter(isNode)
    if (members.some(member => member['type'] === 'null'))
      return members.find(member => member['type'] !== 'null')
  }
  if (Array.isArray(type) && type.includes('null')) {
    const inner = type.find(name => name !== 'null')
    if (typeof inner === 'string') return { type: inner }
  }
  return undefined
}

/**
 * `value` with `null` written for every nullable field of `schema` that the answer left
 * out. Without a grammar holding it to the format, a model tends to omit a field that does
 * not apply instead of writing `null`; the two mean the same thing here. A field that
 * cannot be null is never invented, and nothing the answer did write is changed.
 */
export function withMissingNulls(value: unknown, schema: unknown): unknown {
  if (!isNode(schema) || value === null) return value
  const inner = nonNullPart(schema)
  if (inner) return withMissingNulls(value, inner)
  if (Array.isArray(value)) return value.map(entry => withMissingNulls(entry, schema['items']))
  const properties = schema['properties']
  if (!isNode(value) || !isNode(properties)) return value
  const filled: SchemaNode = {}
  for (const [key, entry] of Object.entries(value))
    filled[key] = withMissingNulls(entry, properties[key])
  for (const [key, property] of Object.entries(properties))
    if (!(key in value) && isNode(property) && nonNullPart(property)) filled[key] = null
  return filled
}
