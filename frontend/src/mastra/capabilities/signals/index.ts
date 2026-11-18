import { exchangeUrl } from '../shared/exchange'
import { defineCapability } from '../types'
import { SIGNALS_INSTRUCTIONS, SIGNALS_UNAVAILABLE } from './instructions'
import { createSignalTools } from './tools'

/** Search the signal catalogue and read retained signal history. */
export const signals = defineCapability({
  id: 'signals',
  enabled: env => exchangeUrl(env) !== undefined,
  instructions: SIGNALS_INSTRUCTIONS,
  unavailable: SIGNALS_UNAVAILABLE,
  create: env => createSignalTools(exchangeUrl(env)),
})
