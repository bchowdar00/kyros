import * as z from 'zod/mini'

/** Attach a description that survives into the JSON schema the model sees. */
export function described<T extends z.ZodMiniType>(schema: T, description: string): T {
  z.globalRegistry.add(schema, { description })
  return schema
}
