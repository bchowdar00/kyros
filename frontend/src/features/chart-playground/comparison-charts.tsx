'use client'

import { useState } from 'react'
import { createPriceTickStep, createProbabilityTickStep } from '@kyros/ui/charts/lib/tape'
import { SimulationDisclosure } from '@kyros/ui/charts/simulation-disclosure'
import { useSimulatedTape } from './use-simulated-tape'
import { PriceMarketCard } from '@kyros/ui/charts/price-market-card'
import { ProbabilityMarketCard } from '@kyros/ui/charts/probability-market-card'
import { ETH_MARKET_FIXTURE, SPORTS_MARKET_FIXTURE } from './chart-playground.fixture'

const PRICE_TAPE = { intervalMs: 250, seed: 0x51ee }
const SPORTS_TAPE = { intervalMs: 250, seed: 0xb05 }

export function ComparisonCharts() {
  const [priceStep] = useState(() =>
    createPriceTickStep({
      band: ETH_MARKET_FIXTURE.priceBand,
      target: ETH_MARKET_FIXTURE.targetPrice,
    })
  )
  const [probabilityStep] = useState(() => createProbabilityTickStep())
  const priceTape = useSimulatedTape({
    ...PRICE_TAPE,
    base: ETH_MARKET_FIXTURE.pricePoints,
    step: priceStep,
  })
  const probabilityTape = useSimulatedTape({
    ...SPORTS_TAPE,
    base: SPORTS_MARKET_FIXTURE.awayPoints,
    step: probabilityStep,
  })
  return (
    <>
      <ProbabilityMarketCard
        markers={SPORTS_MARKET_FIXTURE.markers}
        outcomes={[
          {
            color: 'var(--market-away)',
            id: 'away',
            label: SPORTS_MARKET_FIXTURE.awayLabel,
            points: probabilityTape.points,
          },
          { color: 'var(--market-home)', id: 'home', label: SPORTS_MARKET_FIXTURE.homeLabel },
        ]}
        range={SPORTS_MARKET_FIXTURE.range}
        subtitle={SPORTS_MARKET_FIXTURE.subtitle}
        live
        timeZone={SPORTS_MARKET_FIXTURE.timeZone}
        title={SPORTS_MARKET_FIXTURE.title}
      />

      <PriceMarketCard
        asset={ETH_MARKET_FIXTURE.asset}
        fixedKind="candlestick"
        points={priceTape.points}
        priceBand={ETH_MARKET_FIXTURE.priceBand}
        clock="sample"
        feed={{ state: 'live' }}
        targetLabel="Price to beat"
        targetPrice={ETH_MARKET_FIXTURE.targetPrice}
        timeZone={ETH_MARKET_FIXTURE.timeZone}
        title={ETH_MARKET_FIXTURE.title}
        windows={ETH_MARKET_FIXTURE.windows}
      />
      <SimulationDisclosure />
    </>
  )
}
