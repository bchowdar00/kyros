'use client'

import { EdgeGauge } from '../presets/edge-gauge'
import { FillLog } from '../presets/fill-log'
import { MarketCard } from '../presets/market-card'
import { OrderBook } from '../presets/order-book'
import { PositionCard } from '../presets/position-card'
import { PriceVsStrike } from '../presets/price-vs-strike'
import { RoundResults } from '../presets/round-results'
import { SignalChart } from '../presets/signal-chart'
import { SignalCompare } from '../presets/signal-compare'
import { StatusList } from '../presets/status-list'
import { DEMO_ASKS, DEMO_BIDS } from './data-examples'
import { DEMO_START, DEMO_TIME_ZONE, demoWalk } from './demo-series'

export function MarketCardExample() {
  return (
    <MarketCard
      series="BTC 2m"
      secondsRemaining={47}
      spotCents={7_278_305}
      strikeCents={7_275_414}
      yesCents={62}
      spreadCents={2}
      fairCents={65}
      marketCents={62}
    />
  )
}

export function PositionCardExample() {
  return (
    <PositionCard
      series="BTC 2m"
      contracts={100}
      entryCents={58}
      markCents={62}
      lockedCents={5_800}
      secondsRemaining={47}
    />
  )
}

const rsi = demoWalk({ seed: 61, count: 240, base: 38, volatility: 2.2, clamp: [0, 100] })

export function SignalChartExample() {
  return (
    <SignalChart
      signalId="rsi_14"
      signalLabel="RSI (14)"
      points={rsi}
      format={{ style: 'number', maximumFractionDigits: 0 }}
      timeZone={DEMO_TIME_ZONE}
    />
  )
}

/** A slow and a fast EMA of the same tape: the fast one crosses over two minutes in. */
const ema21 = demoWalk({ seed: 71, count: 240, base: 72_700, volatility: 4, drift: 0.25 })
const ema9 = ema21.map((point, index) => ({
  time: point.time,
  value: point.value + (index - 120) * 0.18 + Math.sin(index / 12) * 6,
}))

export function SignalCompareExample() {
  return (
    <SignalCompare
      title="EMA 9 vs EMA 21"
      signals={[
        { id: 'ema_9', label: 'EMA 9', points: ema9 },
        { id: 'ema_21', label: 'EMA 21', points: ema21 },
      ]}
      format={{ style: 'currency', currency: 'USD', maximumFractionDigits: 0 }}
      timeZone={DEMO_TIME_ZONE}
      markers={[{ time: DEMO_START + 120_000, label: 'Cross', seriesId: 'ema_9' }]}
    />
  )
}

const spotSinceRunUp = demoWalk({ seed: 81, count: 200, base: 72_754, volatility: 9, drift: 0.3 })

export function PriceVsStrikeExample() {
  return (
    <PriceVsStrike
      points={spotSinceRunUp}
      strike={72_754.14}
      roundOpenedAt={DEMO_START + 80_000}
      timeZone={DEMO_TIME_ZONE}
    />
  )
}

export function OrderBookExample() {
  return <OrderBook series="BTC 2m" asks={DEMO_ASKS} bids={DEMO_BIDS} />
}

export function StatusListExample() {
  return (
    <StatusList
      title="Mean reversion v2 rules"
      items={[
        {
          id: 'drop',
          label: 'Drop ≥ 12¢ in 3m',
          value: '−14¢',
          state: 'on',
          stateLabel: 'Met',
          since: '0:38',
        },
        {
          id: 'score',
          label: 'Score within 6',
          value: '4',
          state: 'on',
          stateLabel: 'Met',
          since: '3:10',
        },
        {
          id: 'rsi',
          label: 'RSI below 50',
          value: '52.1',
          state: 'warn',
          stateLabel: 'Near',
          since: '0:06',
        },
        {
          id: 'depth',
          label: 'Depth ≥ 500',
          value: '333',
          state: 'off',
          stateLabel: 'Not met',
          since: '1:22',
        },
        { id: 'time', label: 'More than 30s left', value: '0:47', state: 'on', stateLabel: 'Met' },
      ]}
    />
  )
}

const RECENT_ROUNDS = [-31, 12, 48, -9, 22, 67, -54, -18, 5, 39, 27, 14].map((move, index) => ({
  id: `round-${index}`,
  outcome: move >= 0 ? ('up' as const) : ('down' as const),
  moveDollars: move,
}))

export function RoundResultsExample() {
  return <RoundResults series="BTC 2m" rounds={RECENT_ROUNDS} />
}

export function FillLogExample() {
  return (
    <FillLog
      series="BTC 2m"
      fills={[
        { id: 'f6', side: 'YES', action: 'sell', priceCents: 64, size: 40, time: '2:32:51' },
        { id: 'f5', side: 'YES', action: 'buy', priceCents: 58, size: 60, time: '2:32:04' },
        { id: 'f4', side: 'YES', action: 'buy', priceCents: 58, size: 40, time: '2:32:03' },
        { id: 'f3', side: 'NO', action: 'sell', priceCents: 41, size: 25, time: '2:31:20' },
        { id: 'f2', side: 'NO', action: 'buy', priceCents: 43, size: 25, time: '2:30:48' },
        { id: 'f1', side: 'YES', action: 'buy', priceCents: 55, size: 100, time: '2:30:12' },
      ]}
    />
  )
}

export function EdgeGaugeExample() {
  return <EdgeGauge series="BTC 2m" fairCents={65} marketCents={62} />
}
