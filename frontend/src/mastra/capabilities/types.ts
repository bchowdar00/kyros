import type { Tool } from '@mastra/core/tools'
import type { env } from '../../env'

/** The validated server environment a capability reads its configuration from. */
export type CapabilityEnv = typeof env

/**
 * The tools a capability contributes, keyed by the name the model calls them by.
 * Each is whatever `createTool()` returned; the schemas stay the tool's own business.
 */
// oxlint-disable-next-line typescript/no-explicit-any -- erases createTool's per-tool generics
export type CapabilityTools = Record<string, Tool<any, any, any, any, any, any, any>>

/**
 * One thing Teeny can do: a named bundle of tools plus the prompt fragment that
 * teaches the model when and how to use them.
 *
 * A capability owns its whole surface. Its tools, its instructions, its
 * configuration keys and its tests live together in one folder under
 * `capabilities/<id>/`, so two engineers building two capabilities never edit the
 * same file. The registry in `capabilities/index.ts` is the only shared list.
 */
export interface Capability {
  /** Stable kebab-case identifier; also the folder name. */
  id: string
  /**
   * Whether the capability is usable with this environment. A capability whose
   * configuration is missing still ships its `unavailable` line so the model can
   * say so instead of inventing an answer.
   */
  enabled: (env: CapabilityEnv) => boolean
  /**
   * The prompt fragment appended to Teeny's base instructions when the capability
   * is enabled. Describe what the tools can do, their limits, and what they must
   * never claim. Keep the base persona out of it.
   */
  instructions: string
  /** The one-line prompt fragment used when the capability is disabled. */
  unavailable: string
  /** Build the tools from the environment. Called once at startup. */
  create: (env: CapabilityEnv) => CapabilityTools
}

/** Identity helper that keeps capability definitions type-checked and uniform. */
export function defineCapability(capability: Capability): Capability {
  return capability
}
