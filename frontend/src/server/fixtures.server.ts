import 'server-only'

import { env } from '@/env'

/**
 * Static fixtures stand in for the exchange only in local development with no backend
 * configured, so a production route can never reach them (AGENTS.md §0.4).
 */
export function fixturesEnabled(): boolean {
  return env.NODE_ENV === 'development' && env.EXCHANGE_API_URL === undefined
}
