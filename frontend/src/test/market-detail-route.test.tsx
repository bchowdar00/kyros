import { beforeEach, describe, expect, it, vi } from 'vitest'

const { notFoundMock, loadMarketDetailMock, loadGameSeriesBySlugMock } = vi.hoisted(() => ({
  loadMarketDetailMock: vi.fn(),
  loadGameSeriesBySlugMock: vi.fn(),
  notFoundMock: vi.fn((): never => {
    throw new Error('NEXT_NOT_FOUND')
  }),
}))

vi.mock('next/navigation', () => ({ notFound: notFoundMock }))
vi.mock('@/server/market-detail.server', () => ({
  loadMarketDetail: loadMarketDetailMock,
}))
vi.mock('@/server/silk-road.server', () => ({
  loadGameSeriesBySlug: loadGameSeriesBySlugMock,
}))

import MarketPage from '@/app/(app)/market/[slug]/page'
import { MarketWaitingScreen } from '@/features/market-detail/market-waiting-screen'

describe('market detail route', () => {
  beforeEach(() => {
    notFoundMock.mockClear()
    loadMarketDetailMock.mockReset()
    loadMarketDetailMock.mockResolvedValue(undefined)
    loadGameSeriesBySlugMock.mockReset()
    loadGameSeriesBySlugMock.mockResolvedValue(undefined)
  })

  it('uses the not-found boundary for the retired fixture slugs', async () => {
    await expect(
      MarketPage({
        params: Promise.resolve({ slug: 'btc-above-118k-0903' }),
        searchParams: Promise.resolve({}),
      })
    ).rejects.toThrow('NEXT_NOT_FOUND')

    expect(notFoundMock).toHaveBeenCalledOnce()
    expect(loadMarketDetailMock).toHaveBeenCalledWith('btc-above-118k-0903')
  })

  it('uses the not-found boundary for an unknown market slug', async () => {
    await expect(
      MarketPage({
        params: Promise.resolve({ slug: 'unknown-market' }),
        searchParams: Promise.resolve({}),
      })
    ).rejects.toThrow('NEXT_NOT_FOUND')

    expect(notFoundMock).toHaveBeenCalledOnce()
  })

  it('shows the waiting screen for a game series with no open round', async () => {
    loadGameSeriesBySlugMock.mockResolvedValue({
      series: {
        id: 100_003,
        slug: 'nba-h2h',
        status: 'SERIES_STATUS_ACTIVE',
        title: 'NBA game winner',
      },
    })

    const element = await MarketPage({
      params: Promise.resolve({ slug: 'nba-h2h' }),
      searchParams: Promise.resolve({}),
    })

    expect(notFoundMock).not.toHaveBeenCalled()
    expect(element.type).toBe(MarketWaitingScreen)
    expect(element.props).toEqual({ title: 'NBA game winner' })
  })
})
