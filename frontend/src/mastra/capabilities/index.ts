import type { Mastra } from '@mastra/core/mastra'
import type { Capability, CapabilityEnv, CapabilityTools } from './types'
import { exchange } from './exchange'
import { priceFeeds } from './price-feeds'
import { signals } from './signals'
import { widgets } from './widgets'
import { strategy } from './strategy'

/**
 * Every capability Kyros may have, in the order their instructions appear in the
 * prompt. This list is the one shared file: adding a capability is one import and
 * one entry here. Everything else about it lives in its own folder.
 */
export const CAPABILITIES: readonly Capability[] = [
  exchange,
  signals,
  widgets,
  priceFeeds,
  strategy,
]

/** Split the registry by whether each capability is configured in this environment. */
export function partitionCapabilities(env: CapabilityEnv) {
  const enabled: Capability[] = []
  const disabled: Capability[] = []
  for (const capability of CAPABILITIES) {
    ;(capability.enabled(env) ? enabled : disabled).push(capability)
  }
  return { enabled, disabled }
}

/** The tool map shape both the Mastra instance and the Agent accept. */
export type RegisteredTools = NonNullable<
  NonNullable<ConstructorParameters<typeof Mastra>[0]>['tools']
>

/**
 * Build every enabled capability's tools into one flat map. Tool names must be
 * unique across capabilities: a collision is a startup error, never a silent
 * override, because the model addresses tools by name.
 */
export function collectTools(env: CapabilityEnv, capabilities: readonly Capability[]) {
  const tools: CapabilityTools = {}
  const owners = new Map<string, string>()
  for (const capability of capabilities) {
    for (const [name, tool] of Object.entries(capability.create(env))) {
      const owner = owners.get(name)
      if (owner)
        throw new Error(`Tool "${name}" is defined by both "${owner}" and "${capability.id}".`)
      owners.set(name, capability.id)
      tools[name] = tool
    }
  }
  // Mastra's registry type omits explicit undefined on optional Tool properties.
  // createTool returns valid runtime tools; bridge that SDK exactOptionalPropertyTypes mismatch.
  return tools as RegisteredTools
}
