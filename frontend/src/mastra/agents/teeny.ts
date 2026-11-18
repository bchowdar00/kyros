import { Agent } from '@mastra/core/agent'
import { Memory } from '@mastra/memory'
import { env } from '../../env'
import { collectTools, partitionCapabilities } from '../capabilities'
import { buildInstructions } from '../prompt'

/** The id the chat routes look the agent up by. */
export const TEENY_AGENT_ID = 'teeny'

/**
 * Tool calls per turn: search a signal, read it, show it and still have room to read the
 * market and answer. Five was enough for price feeds alone.
 */
export const TEENY_MAX_STEPS = 8

const { enabled, disabled } = partitionCapabilities(env)

/** Every tool Teeny has in this environment, assembled from the capability registry. */
export const teenyTools = collectTools(env, enabled)

/**
 * Teeny knows nothing about individual capabilities. Its prompt and its tool map
 * are both derived from `capabilities/index.ts`, so a new capability changes this
 * file's behavior without changing this file.
 */
export const teeny = new Agent({
  id: TEENY_AGENT_ID,
  name: 'Teeny',
  description: "Tiny's conversational assistant.",
  instructions: buildInstructions(enabled, disabled),
  model: 'openai/gpt-5.6-terra',
  tools: teenyTools,
  defaultOptions: { maxSteps: TEENY_MAX_STEPS },
  memory: new Memory({ options: { lastMessages: 30 } }),
})
