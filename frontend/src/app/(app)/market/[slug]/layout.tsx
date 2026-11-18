import { notFound } from 'next/navigation'

import { loadMarketDetail } from '@/server/market-detail.server'

// Resolves the market above the page's loading boundary. Below it the shell has already
// streamed with a 200, and an unknown slug would answer 200 + noindex instead of a real 404.
export default async function MarketLayout({ children, params }: LayoutProps<'/market/[slug]'>) {
  const { slug } = await params

  if (!(await loadMarketDetail(slug))) {
    notFound()
  }

  return children
}
