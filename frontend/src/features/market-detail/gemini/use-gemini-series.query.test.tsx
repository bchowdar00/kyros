import type { RustGeminiSeries } from '@kyros/domain'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { renderHook, waitFor } from '@testing-library/react'
import { http, HttpResponse } from 'msw'
import { setupServer } from 'msw/node'
import type { ReactNode } from 'react'
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest'

import { useGeminiSeries } from './use-gemini-series.query'

const server = setupServer()
const apiUrl = 'http://exchange.test'

function gemini(series: string, book: RustGeminiSeries['book']): RustGeminiSeries {
  return { env: 'production', series, phase: 'open', book }
}

function row(series: string, block: RustGeminiSeries | undefined) {
  return {
    series,
    duration_secs: 300,
    current_market_id: 7,
    opens_at_ms: 1_000,
    closes_at_ms: 301_000,
    ...(block === undefined ? {} : { gemini: block }),
  }
}

function setup(initial: RustGeminiSeries | undefined) {
  const client = new QueryClient()
  const wrapper = ({ children }: { children: ReactNode }) => (
    <QueryClientProvider client={client}>{children}</QueryClientProvider>
  )
  const hook = renderHook(() => useGeminiSeries(apiUrl, 'btc-5m', initial), { wrapper })
  return { client, hook }
}

beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => server.resetHandlers())
afterAll(() => server.close())

describe('useGeminiSeries', () => {
  it("picks the slug's row from the series list", async () => {
    server.use(
      http.get('*/api/series', () =>
        HttpResponse.json([
          row('btc-15m', gemini('BTC15M', 'waiting')),
          row('btc-5m', gemini('BTC05M', 'stale')),
        ])
      )
    )
    const { hook } = setup(gemini('BTC05M', 'live'))
    expect(hook.result.current).toEqual({ status: 'polled', gemini: gemini('BTC05M', 'live') })
    await waitFor(() =>
      expect(hook.result.current).toEqual({ status: 'polled', gemini: gemini('BTC05M', 'stale') })
    )
  })

  it('reports a failed poll and keeps the last value', async () => {
    server.use(
      http.get('*/api/series', () => HttpResponse.json([row('btc-5m', gemini('BTC05M', 'stale'))]))
    )
    const { client, hook } = setup(undefined)
    await waitFor(() =>
      expect(hook.result.current).toEqual({ status: 'polled', gemini: gemini('BTC05M', 'stale') })
    )
    server.use(http.get('*/api/series', () => HttpResponse.json({}, { status: 503 })))
    await client.refetchQueries()
    await waitFor(() =>
      expect(hook.result.current).toEqual({
        status: 'poll-failed',
        gemini: gemini('BTC05M', 'stale'),
      })
    )
  })
})
