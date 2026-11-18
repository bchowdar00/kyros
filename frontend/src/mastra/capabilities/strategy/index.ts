import { type CapabilityEnv, defineCapability } from '../types'
import { STRATEGY_INSTRUCTIONS, STRATEGY_UNAVAILABLE } from './instructions'
import { createStrategyTools } from './tools'

function exchangeUrl(env: CapabilityEnv) {
  return env.EXCHANGE_API_URL ?? env.NEXT_PUBLIC_API_URL
}

/** Designing a strategy in conversation and handing it to the compile workflow. */
export const strategy = defineCapability({
  id: 'strategy',
  enabled: env => exchangeUrl(env) !== undefined && env.ANTHROPIC_API_KEY !== undefined,
  instructions: STRATEGY_INSTRUCTIONS,
  unavailable: STRATEGY_UNAVAILABLE,
  create: env => createStrategyTools(exchangeUrl(env) ?? ''),
})
