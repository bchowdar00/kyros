import { notFound } from 'next/navigation'

import { loadMarketDetail } from '@/server/market-detail.server'
import { loadGameSeriesBySlug } from '@/server/silk-road.server'

// Resolves the series above the page's loading boundary. Below it the shell has already
// streamed with a 200, and an unknown slug would answer 200 + noindex instead of a real 404.
// A game series between games is not a 404: the page shows a waiting screen for it.
export default async function MarketLayout({ children, params }: LayoutProps<'/market/[slug]'>) {
  const { slug } = await params

  if ((await loadMarketDetail(slug)) || (await loadGameSeriesBySlug(slug))) {
    return children
  }

  notFound()
}
