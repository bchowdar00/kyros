import { exchangeUrl } from '../shared/exchange'
import { defineCapability } from '../types'
import { EXCHANGE_INSTRUCTIONS, EXCHANGE_UNAVAILABLE } from './instructions'
import { createExchangeTools } from './tools'

/** Read-only access to the Rust exchange: rounds, books, positions, fills, prices. */
export const exchange = defineCapability({
  id: 'exchange',
  enabled: env => exchangeUrl(env) !== undefined,
  instructions: EXCHANGE_INSTRUCTIONS,
  unavailable: EXCHANGE_UNAVAILABLE,
  create: env => createExchangeTools(exchangeUrl(env)),
})
