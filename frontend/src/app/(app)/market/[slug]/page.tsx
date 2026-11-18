import type { Metadata } from 'next'
import { notFound } from 'next/navigation'

import { RustExchangeMarketScreen } from '@/features/market-detail/exchange/rust-market-screen'
import { MarketDetailScreen } from '@/features/market-detail/market-detail-screen'
import { loadExchangeConfig } from '@/server/exchange.server'
import { loadMarketDetail } from '@/server/market-detail.server'

type MarketPageProps = PageProps<'/market/[slug]'>

export async function generateMetadata({ params }: MarketPageProps): Promise<Metadata> {
  const { slug } = await params
  const market = await loadMarketDetail(slug)

  if (!market) {
    return {}
  }

  return {
    alternates: { canonical: `/market/${slug}` },
    description: market.header.subtitle,
    openGraph: { description: market.header.subtitle, title: market.header.title, type: 'website' },
    title: market.header.title,
    twitter: { card: 'summary_large_image' },
  }
}

export default async function MarketPage({ params }: MarketPageProps) {
  const { slug } = await params
  const market = await loadMarketDetail(slug)

  if (!market) {
    notFound()
  }

  // Only a live series trades; the static fixtures have no exchange behind them.
  const exchange = market.liveEvent === undefined ? undefined : loadExchangeConfig(market.slug)
  if (exchange !== undefined) return <RustExchangeMarketScreen market={market} config={exchange} />
  return (
    <MarketDetailScreen
      market={market}
      exchangeStatus={
        <p className="py-lg text-label text-muted-foreground">
          Exchange unavailable. Live order-book data and trading require a connected exchange.
        </p>
      }
    />
  )
}
