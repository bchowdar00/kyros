import { beforeEach, describe, expect, it, vi } from 'vitest'

const { notFoundMock, loadMarketDetailMock } = vi.hoisted(() => ({
  loadMarketDetailMock: vi.fn(),
  notFoundMock: vi.fn((): never => {
    throw new Error('NEXT_NOT_FOUND')
  }),
}))

vi.mock('next/navigation', () => ({ notFound: notFoundMock }))
vi.mock('@/server/market-detail.server', () => ({
  loadMarketDetail: loadMarketDetailMock,
}))

import MarketPage from '@/app/(app)/market/[slug]/page'

describe('market detail route', () => {
  beforeEach(() => {
    notFoundMock.mockClear()
    loadMarketDetailMock.mockReset()
    loadMarketDetailMock.mockResolvedValue(undefined)
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
})
