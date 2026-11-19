import type { RustGeminiSeries, RustSeriesInfo } from '@kyros/domain'
import { describe, expect, it } from 'vitest'

import { toSiteMode } from './mode.mapper'

function series(
  slug: string,
  env?: RustGeminiSeries['env'],
  orders?: RustGeminiSeries['orders']
): RustSeriesInfo {
  return {
    series: slug,
    duration_secs: 300,
    current_market_id: 1,
    opens_at_ms: 0,
    closes_at_ms: 300_000,
    ...(env === undefined
      ? {}
      : {
          gemini: {
            env,
            series: 'BTC05M',
            phase: 'open',
            book: 'live',
            ...(orders === undefined ? {} : { orders }),
          },
        }),
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

  it('calls real orders against production live orders', () => {
    expect(toSiteMode([series('btc-5m', 'production', 'real')])).toEqual({
      label: 'Live orders',
      description:
        "Strategies run by this server's operator send real orders to Gemini. Manual orders and every balance shown here are pretend money.",
      tone: 'warning',
    })
  })

  it('calls real orders against the sandbox sandbox orders', () => {
    expect(toSiteMode([series('btc-5m', 'sandbox', 'real')])).toEqual({
      label: 'Sandbox orders',
      description:
        "Strategies run by this server's operator send orders to Gemini's sandbox. Manual orders and every balance shown here are pretend money.",
      tone: 'warning',
    })
  })

  it('lets one real-production series make the whole site live orders', () => {
    expect(
      toSiteMode([series('btc-5m', 'production', 'real'), series('eth-5m', 'production')]).label
    ).toBe('Live orders')
  })

  it('leaves paper orders and absent orders at the existing Paper and Sandbox results', () => {
    expect(toSiteMode([series('btc-5m', 'production', 'paper')])).toEqual({
      label: 'Paper',
      description:
        "Pretend money, filled against Gemini's live order book. No order leaves this site.",
      tone: 'neutral',
    })
    expect(toSiteMode([series('btc-5m', 'sandbox', 'paper')])).toEqual({
      label: 'Sandbox',
      description:
        "Pretend money, filled against Gemini's sandbox order book. No order leaves this site.",
      tone: 'warning',
    })
    expect(toSiteMode([series('btc-5m', 'production')]).label).toBe('Paper')
    expect(toSiteMode([series('btc-5m', 'sandbox')]).label).toBe('Sandbox')
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
