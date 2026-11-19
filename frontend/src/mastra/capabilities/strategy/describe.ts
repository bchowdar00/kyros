import * as z from 'zod/mini'

/**
 * Attach a description that survives into the JSON schema the model sees. The description
 * goes on a copy: descriptions are registered per schema object, so describing a shared
 * schema in place (an identifier used by many fields) would describe every one of its uses.
 */
export function described<T extends z.ZodMiniType>(schema: T, description: string): T {
  const copy = schema.clone()
  z.globalRegistry.add(copy, { description })
  return copy
}
