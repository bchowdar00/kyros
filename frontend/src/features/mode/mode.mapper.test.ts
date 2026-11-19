import type { RustGeminiSeries, RustSeriesInfo } from '@kyros/domain'
import { describe, expect, it } from 'vitest'

import { toSiteMode } from './mode.mapper'

function series(slug: string, env?: RustGeminiSeries['env']): RustSeriesInfo {
  return {
    series: slug,
    duration_secs: 300,
    current_market_id: 1,
    opens_at_ms: 0,
    closes_at_ms: 300_000,
    ...(env === undefined
      ? {}
      : { gemini: { env, series: 'BTC05M', phase: 'open', book: 'live' } }),
  }
}

describe('toSiteMode', () => {
  it('says the mode is unknown when the exchange did not answer', () => {
    expect(toSiteMode(undefined)).toEqual({
      label: 'Mode unknown',
      description:
        'The exchange did not answer, so this page cannot say which books it is reading.',
      tone: 'warning',
    })
  })

  it('calls a production-following server paper trading on the live book', () => {
    expect(toSiteMode([series('btc-5m', 'production')])).toEqual({
      label: 'Paper',
      description:
        "Pretend money, filled against Gemini's live order book. No order leaves this site.",
      tone: 'neutral',
    })
  })

  it('calls a sandbox-following server sandbox', () => {
    expect(toSiteMode([series('btc-5m', 'sandbox')])).toEqual({
      label: 'Sandbox',
      description:
        "Pretend money, filled against Gemini's sandbox order book. No order leaves this site.",
      tone: 'warning',
    })
  })

  it('lets one sandbox series win over production ones', () => {
    expect(toSiteMode([series('btc-5m', 'production'), series('eth-5m', 'sandbox')]).label).toBe(
      'Sandbox'
    )
  })

  it('warns when no series follows Gemini, including an empty list', () => {
    const notFollowing = {
      label: 'Paper',
      description: 'Pretend money. This server is not following Gemini.',
      tone: 'warning',
    }
    expect(toSiteMode([series('btc-5m')])).toEqual(notFollowing)
    expect(toSiteMode([])).toEqual(notFollowing)
  })
})
