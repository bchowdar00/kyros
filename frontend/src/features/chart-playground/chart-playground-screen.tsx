'use client'

import { useState } from 'react'
import dynamic from 'next/dynamic'
import { Button } from '@kyros/ui/components/button'
import { BitcoinPlayground } from './bitcoin-playground'

const ComparisonCharts = dynamic(
  () => import('./comparison-charts').then(module => module.ComparisonCharts),
  {
    ssr: false,
    loading: () => (
      <output className="block h-chart-md bg-surface">Loading comparison charts</output>
    ),
  }
)

export function ChartPlaygroundScreen() {
  const [showComparisons, setShowComparisons] = useState(false)
  return (
    <main className="mx-auto flex w-full max-w-7xl flex-col gap-4xl p-xl pb-4xl">
      <BitcoinPlayground />
      {showComparisons ? (
        <ComparisonCharts />
      ) : (
        <Button variant="secondary" onClick={() => setShowComparisons(true)}>
          Show probability and candlestick demos
        </Button>
      )}
      <p className="text-label font-medium text-muted-foreground">
        Hover to scrub. Focus a chart and use the arrow keys to step through observed samples. All
        demo feeds are simulated.
      </p>
    </main>
  )
}
