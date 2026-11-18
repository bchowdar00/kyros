import { CANDLE_INTERVALS } from '@tiny/ui/charts/lib/candles'
import { createMockPricePoints, createMockProbabilityPoints } from '@tiny/ui/charts/lib/mock'
import { createSeededRandom } from '@tiny/ui/charts/lib/tape'
import { HOUR, MINUTE, SECOND } from '@tiny/ui/charts/lib/time'
import type { ChartPoint, TimeRange } from '@tiny/ui/charts/lib/types'
import type { ChartWindow } from '@tiny/ui/charts/chart-window'

export const ETH_WINDOWS: readonly ChartWindow[] = [
  { candleInterval: 15 * SECOND, durationMs: 15 * MINUTE },
  { candleInterval: CANDLE_INTERVALS['1m'], durationMs: HOUR },
  { candleInterval: 2 * MINUTE, durationMs: 2 * HOUR },
]

export interface CryptoAsset {
  color: string
  colorClass: string
  glyph: string
  symbol: string
}

export interface CryptoMarketFixture {
  asset: CryptoAsset
  /** Market close, the right edge of every window. */
  closeTime: number
  /** Absolute price distance represented by the full mock walk band. */
  priceBand: number
  pricePoints: readonly ChartPoint[]
  targetPrice: number
  timeZone: string
  title: string
  windows: readonly ChartWindow[]
}

/** Dense anchors so every window, down to one minute, shows real movement. */
function createCryptoAnchors(seed: number, count: number) {
  const random = createSeededRandom(seed)
  let level = 52

  return Array.from({ length: count }, () => {
    level = Math.min(88, Math.max(12, level + (random() - 0.5) * 22))

    return Math.round(level)
  })
}

/** 9:00 PM PT on May 22, 2026. */
const ethCloseTime = Date.UTC(2026, 4, 23, 4, 0)
const ethHistoryStart = ethCloseTime - 2 * HOUR
const ethLatestTick = ethCloseTime - 41 * MINUTE

export const ETH_MARKET_FIXTURE: CryptoMarketFixture = {
  asset: { color: 'var(--asset-eth)', colorClass: 'bg-asset-eth', glyph: 'Ξ', symbol: 'ETH' },
  closeTime: ethCloseTime,
  priceBand: 18,
  pricePoints: createMockPricePoints({
    anchors: createCryptoAnchors(0x9e7, 200),
    band: 18,
    basePrice: 3_412.5,
    count: Math.round((ethLatestTick - ethHistoryStart) / SECOND),
    range: { end: ethLatestTick, start: ethHistoryStart },
    seed: 0x3e7a,
    volatility: 2,
  }),
  targetPrice: 3_412.5,
  timeZone: 'America/Los_Angeles',
  title: 'ETH Above $3,412.50 at 9:00 PM PT?',
  windows: ETH_WINDOWS,
}

export interface SportsMarketFixture {
  awayLabel: string
  awayPoints: readonly ChartPoint[]
  homeLabel: string
  markers: readonly { label: string; time: number }[]
  range: TimeRange
  subtitle: string
  timeZone: string
  title: string
}

/** 7:00 PM ET on August 31, 2026. */
const gameStart = Date.UTC(2026, 7, 31, 23, 0)

export const SPORTS_MARKET_FIXTURE: SportsMarketFixture = {
  awayLabel: 'BOS',
  awayPoints: createMockProbabilityPoints({
    count: 4_000,
    range: { end: gameStart + 78 * MINUTE, start: gameStart },
    seed: 0x31b05a1,
  }),
  homeLabel: 'LAL',
  markers: [
    { label: 'Halftime', time: gameStart + 42 * MINUTE },
    { label: 'Q4', time: gameStart + 64 * MINUTE },
  ],
  range: { end: gameStart + 90 * MINUTE, start: gameStart },
  subtitle: 'Boston Celtics at Los Angeles Lakers',
  timeZone: 'America/New_York',
  title: 'BOS at LAL',
}
