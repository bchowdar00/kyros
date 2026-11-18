import type { ChartPoint, TimeRange } from '@tiny/ui/charts/lib/types'

export type HomeMarketSide = 'away' | 'home'

export interface HomeMarketOutcome {
  label: string
  priceCents?: number
  side: HomeMarketSide
}

export interface HomeMarketSeries {
  label: string
  points: readonly ChartPoint[]
  side: HomeMarketSide
}

export interface MarketCardArtwork {
  images: readonly MarketCardImage[]
  label: string
}

export interface MarketCardImage {
  presentation?: 'full' | 'inset'
  src: string
  tone?: 'default' | 'lakers'
}

export interface HomeMarketPriceChart {
  /** Series label, for example the asset symbol. */
  label: string
  points: readonly ChartPoint[]
  targetLabel: string
  /** Strike the market resolves against; drawn as a centred reference line. */
  targetPrice: number
}

export interface HomeMarketCardModel {
  category: string
  live: boolean
  /** A live sporting event's current period and clock; absent for non-sport markets. */
  marketStatus?: string
  outcomes: readonly [HomeMarketOutcome, HomeMarketOutcome]
  /** A strike market charts price against its strike; without it the card charts probability. */
  priceChart?: HomeMarketPriceChart
  range: TimeRange
  series?: readonly [HomeMarketSeries, HomeMarketSeries]
  slug: string
  subtitle: string
  timeZone: string
  title: string
}
