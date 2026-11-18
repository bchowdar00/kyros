import { cents } from '@tiny/money'

import { CANDLE_INTERVALS } from '@tiny/ui/charts/lib/candles'
import {
  complementProbabilityPoints,
  createMockPricePoints,
  createMockProbabilityPoints,
} from '@tiny/ui/charts/lib/mock'
import { createSeededRandom } from '@tiny/ui/charts/lib/tape'
import { HOUR, MINUTE, SECOND } from '@tiny/ui/charts/lib/time'
import type { ChartWindow } from '@tiny/ui/charts/chart-window'

import type { MarketDetail } from './types'
import { PUBLIC_STRATEGY_FIXTURES } from './public-strategies.fixture'

const gameTimeZone = 'America/New_York'
/** 7:00 PM ET on August 31, 2026. */
const gameStart = Date.UTC(2026, 7, 31, 23, 0)
const gameEnd = gameStart + 90 * MINUTE
const latestTick = gameStart + 78 * MINUTE
const probabilityPointCount = 10_000

const awayProbabilityPoints = createMockProbabilityPoints({
  count: probabilityPointCount,
  range: { end: latestTick, start: gameStart },
  seed: 0x31b05a1,
})

/** 12:30 PM ET on September 3, 2026; the market resolves five minutes later. */
const btcWindowStart = Date.UTC(2026, 8, 3, 16, 30)
const btcWindowEnd = btcWindowStart + 5 * MINUTE
const btcLatestTick = btcWindowStart + 4 * MINUTE

const btcYesProbabilityPoints = createMockProbabilityPoints({
  // The last anchor pins the final sample to the quoted YES price.
  anchors: [45, 52, 38, 44, 58, 51, 64, 70, 66, 62],
  // A five-minute window carries far fewer ticks than a full game.
  count: 1_500,
  range: { end: btcLatestTick, start: btcWindowStart },
  seed: 0xb7c0135,
})

/**
 * An hour of anchors, dense enough that even the one-minute window shows real
 * movement. The last one pins the close at the quoted price.
 */
function createBtcPriceAnchors() {
  const random = createSeededRandom(0xb7c0135)
  let level = 55

  // One anchor per six seconds: the one-minute window has texture and the hour
  // still reads as a shape rather than noise.
  const anchors = Array.from({ length: 600 }, () => {
    level = Math.min(88, Math.max(12, level + (random() - 0.5) * 16))

    return Math.round(level)
  })

  // The close is quoted, so the last two minutes ease into it instead of jumping.
  const rampStart = anchors.length - 20
  const from = anchors[rampStart] ?? 55

  return anchors.map((anchor, index) => {
    if (index <= rampStart) {
      return anchor
    }

    const progress = (index - rampStart) / (anchors.length - 1 - rampStart)
    const ramp = from + (70 - from) * progress
    // Keep some of the walk's wiggle on the way in, or the run-up reads as a ruler.
    const wiggle = index === anchors.length - 1 ? 0 : (anchor - from) * 0.3

    return Math.round(ramp + wiggle)
  })
}

/** Candidate chart ranges; the price history reaches back an hour to cover them. */
const btcChartWindows: readonly ChartWindow[] = [
  { candleInterval: 5 * SECOND, durationMs: MINUTE },
  { candleInterval: 15 * SECOND, durationMs: 5 * MINUTE },
  { candleInterval: 30 * SECOND, durationMs: 15 * MINUTE },
  { candleInterval: CANDLE_INTERVALS['1m'], durationMs: HOUR },
]

const btcPriceBand = 300
const btcPricePoints = createMockPricePoints({
  anchors: createBtcPriceAnchors(),
  band: btcPriceBand,
  basePrice: 118_000,
  count: 3_600,
  decimals: 2,
  range: { end: btcLatestTick, start: btcLatestTick - HOUR },
  seed: 0xb7c0135,
})

/** The month of rolling 24-hour volume behind the chat answer's chart card. */
const btcVolumeRange = {
  end: Date.UTC(2026, 7, 31, 18, 55),
  start: Date.UTC(2026, 7, 2),
}
const btcVolumePoints = createMockPricePoints({
  band: 22_000_000_000,
  basePrice: 26_530_000_000,
  count: 120,
  decimals: 0,
  range: btcVolumeRange,
  seed: 0xb01d,
})

export const MARKET_DETAIL_FIXTURES = {
  'nba-bos-la-083126': {
    category: 'NBA',
    header: {
      outcomes: [
        { label: 'BOS', priceCents: 31, probability: 31, side: 'away' },
        { label: 'LAL', priceCents: 69, probability: 69, side: 'home' },
      ],
      surfaceImage: {
        alt: '',
        src: '/assets/market/header-surface.png',
      },
      subtitle: 'Boston Celtics at Los Angeles Lakers',
      teams: [
        {
          abbreviation: 'BOS',
          id: 'boston-celtics',
          logo: {
            alt: 'Boston Celtics logo',
            src: '/assets/market/teams/boston-celtics.svg',
          },
          name: 'Boston Celtics',
          side: 'away',
        },
        {
          abbreviation: 'LAL',
          id: 'los-angeles-lakers',
          logo: {
            alt: 'Los Angeles Lakers logo',
            src: '/assets/market/teams/los-angeles-lakers.svg',
          },
          logoInset: true,
          name: 'Los Angeles Lakers',
          side: 'home',
        },
      ],
      title: 'Celtics at Lakers',
    },
    myStrategies: {
      rows: [
        {
          id: 'manual-trades',
          name: 'Manual Trades',
          position: { valueCents: cents(2920), label: '40 LAL', shares: 40, side: 'home' },
          status: 'active',
          totalPnlCents: cents(200),
          trades: 1,
        },
        {
          id: 'momentum-flips',
          name: 'Buy the leader every time momentum flips',
          position: { valueCents: cents(8840), label: '65 BOS', shares: 65, side: 'away' },
          status: 'active',
          totalPnlCents: cents(21084),
          trades: 4,
        },
        {
          id: 'halftime-leaders',
          name: 'Buy halftime leaders, hold to buzzer',
          status: 'paused',
          totalPnlCents: cents(-910),
          trades: 3,
        },
        {
          id: 'final-quarter-dips',
          name: 'Buy every dip below 30¢ in the final quarter',
          status: 'active',
          totalPnlCents: cents(-10),
          trades: 2,
        },
      ],
      tabs: [
        { id: 'strategies', label: 'My Accounts' },
        { id: 'open-orders', label: 'Open Orders' },
        { id: 'trade-history', label: 'Trade History' },
      ],
      title: 'My Accounts',
    },
    orderBook: {
      actionLabel: 'Center order book on spread',
      asks: [
        { priceCents: 58, shares: 195.33, totalUsd: 1903.58 },
        { priceCents: 57, shares: 210.22, totalUsd: 1724.88 },
        { priceCents: 56, shares: 89.66, totalUsd: 1088.81 },
        { priceCents: 55, shares: 116.95, totalUsd: 1030.04 },
        { priceCents: 54, shares: 446.2, totalUsd: 760 },
        { priceCents: 53, shares: 661, totalUsd: 519.05 },
        { priceCents: 52, shares: 151.79, totalUsd: 168.72 },
        { priceCents: 51, shares: 119.78, totalUsd: 89.79 },
        { priceCents: 50, shares: 57.4, totalUsd: 28.7 },
      ],
      bids: [
        { priceCents: 49, shares: 50, totalUsd: 25.5 },
        { priceCents: 48, shares: 445.8, totalUsd: 238.58 },
        { priceCents: 47, shares: 519.38, totalUsd: 482.79 },
        { priceCents: 46, shares: 420.3, totalUsd: 676.13 },
        { priceCents: 45, shares: 289.9, totalUsd: 806.37 },
        { priceCents: 44, shares: 531.51, totalUsd: 1289.66 },
      ],
      columnLabels: ['Price', 'Shares', 'Total'],
      spreadCents: 1,
      spreadLabel: 'Spread',
      spreadPercentage: 1,
      title: 'Order Book',
    },
    probabilityChart: {
      commentary: 'Lebron James scores a 3 pointer from 30 feet.',
      live: true,
      range: { end: gameEnd, start: gameStart },
      series: [
        { label: 'BOS', points: awayProbabilityPoints, side: 'away' },
        { label: 'LAL', points: complementProbabilityPoints(awayProbabilityPoints), side: 'home' },
      ],
      timeZone: gameTimeZone,
    },
    scoreboard: {
      awayScore: 90,
      clock: '01:58',
      homeScore: 95,
      period: 'Q4',
    },
    signals: {
      browseLabel: 'Browse All',
      items: [
        { id: 'market-volume', label: 'Market Volume', value: '$988,463.38' },
        { id: 'open-interest', label: 'Open Interest', value: '$72,680' },
        { id: 'active-strategies', label: 'Active Strategies', value: '201' },
        { id: 'largest-lead', label: 'Largest Lead', side: 'home', value: 'LAL +14' },
        { id: 'largest-run', label: 'Largest Run', side: 'away', value: 'BOS 6-0' },
        { id: 'lead-changes', label: 'Lead Changes', value: '11' },
        { id: 'points-last-five', label: 'Points Last 5m', side: 'home', value: 'LAL +7' },
        { id: 'lead-duration', label: 'Lead Duration', side: 'home', value: '1:42' },
      ],
      title: 'Signals',
      total: 167,
    },
    slug: 'nba-bos-la-083126',
    chatPanel: {
      composer: {
        addContextLabel: 'Add strategy context',
        dismissTipLabel: 'Dismiss strategy tip',
        inputLabel: 'Strategy instructions',
        placeholder: 'Create a new strategy....',
        submitLabel: 'Create strategy',
        tipLinkLabel: 'signals',
        tipPrefix: 'Tip: your strategy can reference all 167 live',
      },
      defaultSortId: 'most-running',
      defaultSortLabel: 'Most Running',
      publicStrategies: PUBLIC_STRATEGY_FIXTURES,
      sectionTitle: 'Public Strategies',
      sortLabel: 'Sort',
      sortOptions: [
        { id: 'most-running', label: 'Most Running' },
        { id: 'most-deployed', label: 'Most Deployed' },
      ],
      subtitle: 'Start with a proven one or write your own below',
      title: 'Deploy an Automated Trading Strategy',
      view: 'strategies',
    },
  },
  'btc-above-118k-0903': {
    category: 'Crypto',
    header: {
      outcomes: [
        { label: 'YES', priceCents: 62, probability: 62, side: 'home' },
        { label: 'NO', priceCents: 38, probability: 38, side: 'away' },
      ],
      surfaceImage: {
        alt: '',
        src: '/assets/market/header-surface.png',
      },
      subtitle: 'Bitcoin above $118,000 at 12:35 PM ET',
      title: 'BTC above $118K',
    },
    myStrategies: {
      rows: [
        {
          id: 'manual-trades',
          name: 'Manual Trades',
          position: { valueCents: cents(7800), label: '120 YES', shares: 120, side: 'home' },
          status: 'active',
          totalPnlCents: cents(620),
          trades: 2,
        },
        {
          id: 'funding-flips',
          name: 'Buy YES whenever funding flips positive',
          position: { valueCents: cents(5525), label: '85 YES', shares: 85, side: 'home' },
          status: 'active',
          totalPnlCents: cents(3812),
          trades: 5,
        },
        {
          id: 'fade-late-breakouts',
          name: 'Fade breakouts above the strike in the last minute',
          status: 'paused',
          totalPnlCents: cents(-405),
          trades: 3,
        },
      ],
      tabs: [
        { id: 'strategies', label: 'My Accounts' },
        { id: 'open-orders', label: 'Open Orders' },
        { id: 'trade-history', label: 'Trade History' },
      ],
      title: 'My Accounts',
    },
    orderBook: {
      actionLabel: 'Center order book on spread',
      asks: [
        { priceCents: 68, shares: 240.11, totalUsd: 1633.75 },
        { priceCents: 67, shares: 188.4, totalUsd: 1262.28 },
        { priceCents: 66, shares: 310.55, totalUsd: 1024.82 },
        { priceCents: 65, shares: 96.2, totalUsd: 625.3 },
        { priceCents: 64, shares: 402.75, totalUsd: 515.52 },
        { priceCents: 63, shares: 155.6, totalUsd: 98.03 },
      ],
      bids: [
        { priceCents: 62, shares: 84.3, totalUsd: 52.27 },
        { priceCents: 61, shares: 268.9, totalUsd: 164.03 },
        { priceCents: 60, shares: 501.45, totalUsd: 300.87 },
        { priceCents: 59, shares: 351.02, totalUsd: 493.1 },
        { priceCents: 58, shares: 220.68, totalUsd: 921.19 },
        { priceCents: 57, shares: 480.33, totalUsd: 1548.79 },
      ],
      columnLabels: ['Price', 'Shares', 'Total'],
      spreadCents: 1,
      spreadLabel: 'Spread',
      spreadPercentage: 1.6,
      title: 'Order Book',
    },
    priceChart: {
      commentary: 'BTC reclaims $118,120 after a sharp dip below the strike.',
      label: 'BTC',
      live: true,
      points: btcPricePoints,
      priceSymbol: 'Crypto.BTC/USD',
      priceBand: btcPriceBand,
      range: { end: btcWindowEnd, start: btcWindowStart },
      targetLabel: 'Price to beat',
      targetPrice: 118_000,
      timeZone: gameTimeZone,
      windows: btcChartWindows,
    },
    probabilityChart: {
      live: true,
      range: { end: btcWindowEnd, start: btcWindowStart },
      series: [
        {
          label: 'NO',
          points: complementProbabilityPoints(btcYesProbabilityPoints),
          side: 'away',
        },
        { label: 'YES', points: btcYesProbabilityPoints, side: 'home' },
      ],
      timeZone: gameTimeZone,
    },
    signals: {
      browseLabel: 'Browse All',
      items: [
        { id: 'market-volume', label: 'Market Volume', value: '$412,908.11' },
        { id: 'open-interest', label: 'Open Interest', value: '$38,450' },
        { id: 'active-strategies', label: 'Active Strategies', value: '96' },
        { id: 'btc-volume-24h', label: '24h BTC Volume', value: '$31.46B' },
        { id: 'distance-to-strike', label: 'To Strike', side: 'home', value: '+$412' },
        { id: 'funding-rate', label: 'Funding Rate', side: 'home', value: '+0.012%' },
        { id: 'last-minute-move', label: 'Last 1m Move', side: 'away', value: '-$96' },
        { id: 'time-to-expiry', label: 'Time to Expiry', value: '1:07' },
      ],
      title: 'Signals',
      total: 167,
    },
    slug: 'btc-above-118k-0903',
    chatPanel: {
      chat: {
        answerParagraphs: [
          [
            {
              text: "BTC's latest global aggregate trading volume is $31.46B over the trailing 24 hours, measured at 2026-08-31 18:55:50 UTC. This covers reported activity across tracked markets, not just one exchange or BTC/USD order book.",
            },
          ],
          [
            { text: "That's " },
            { emphasis: true, text: 'elevated but not extreme' },
            {
              text: ': roughly the 76th percentile of the past month 75.87%, above the 30-day median of $21.5B and mean of $26.53B, and only slightly above the 75th-percentile level of $31.13B. Figures vary by provider because exchange coverage, derivatives inclusion, stablecoin pairs, and data-quality filters differ.',
            },
          ],
        ],
        chartCard: {
          caption:
            'Shows the latest aggregate volume against its directly comparable recent history.',
          points: btcVolumePoints,
          range: btcVolumeRange,
          timeZone: 'UTC',
          title: 'BTC rolling 24-hour global trading volume, past month',
        },
        question: 'What is BTC volume like?',
      },
      composer: {
        addContextLabel: 'Add strategy context',
        dismissTipLabel: 'Dismiss strategy tip',
        inputLabel: 'Strategy instructions',
        placeholder: 'Create a new strategy....',
        submitLabel: 'Create strategy',
        tipLinkLabel: 'signals',
        tipPrefix: 'Tip: your strategy can reference all 167 live',
      },
      defaultSortId: 'most-running',
      defaultSortLabel: 'Most Running',
      publicStrategies: PUBLIC_STRATEGY_FIXTURES,
      sectionTitle: 'Public Strategies',
      sortLabel: 'Sort',
      sortOptions: [
        { id: 'most-running', label: 'Most Running' },
        { id: 'most-deployed', label: 'Most Deployed' },
      ],
      subtitle: 'Start with a proven one or write your own below',
      title: 'Deploy an Automated Trading Strategy',
      view: 'chat',
    },
  },
} as const

export type MarketDetailFixtureSlug = keyof typeof MARKET_DETAIL_FIXTURES

export function isMarketDetailFixtureSlug(slug: string): slug is MarketDetailFixtureSlug {
  return Object.hasOwn(MARKET_DETAIL_FIXTURES, slug)
}

export function getMarketDetailFixture(slug: string): MarketDetail | undefined {
  if (!isMarketDetailFixtureSlug(slug)) {
    return undefined
  }

  return MARKET_DETAIL_FIXTURES[slug]
}
