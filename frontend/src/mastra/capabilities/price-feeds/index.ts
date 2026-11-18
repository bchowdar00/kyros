import { type CapabilityEnv, defineCapability } from '../types'
import { PRICE_FEEDS_INSTRUCTIONS, PRICE_FEEDS_UNAVAILABLE } from './instructions'
import { createPriceFeedTools } from './tools'

function feedsUrl(env: CapabilityEnv) {
  return env.PRICE_FEEDS_API_URL ?? env.NEXT_PUBLIC_PRICE_FEEDS_URL
}

/** Read-only access to Tiny Price Feeds: asset discovery, live quotes, history. */
export const priceFeeds = defineCapability({
  id: 'price-feeds',
  enabled: env => feedsUrl(env) !== undefined,
  instructions: PRICE_FEEDS_INSTRUCTIONS,
  unavailable: PRICE_FEEDS_UNAVAILABLE,
  create: env => createPriceFeedTools(feedsUrl(env)),
})
