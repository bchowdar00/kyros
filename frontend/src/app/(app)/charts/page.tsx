import type { Metadata } from 'next'

import { ChartPlaygroundScreen } from '@/features/chart-playground/chart-playground-screen'

export const metadata: Metadata = {
  description: 'Every market chart kind on one page: price, candlesticks and probability.',
  // The playground renders simulated series (AGENTS.md §0.4 waiver), so it stays unindexed.
  robots: { follow: false, index: false },
  title: 'Charts',
}

export default function ChartsPage() {
  return <ChartPlaygroundScreen />
}
