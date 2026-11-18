'use client'

import { MINUTE } from '@tiny/ui/charts/lib/time'
import { cents, createMoneyFormatter } from '@tiny/money'

import { aggregateCandles } from '@tiny/ui/charts/lib/candles'

import { BarChart } from '../widgets/bar-chart'
import { CandleChart } from '../widgets/candle-chart'
import { DataTable } from '../widgets/data-table'
import { Gauge } from '../widgets/gauge'
import { Histogram } from '../widgets/histogram'
import { Ladder } from '../widgets/ladder'
import { Scatter } from '../widgets/scatter'
import { StatGrid } from '../widgets/stat-grid'
import { TimeSeries } from '../widgets/time-series'
import { DEMO_START, DEMO_TIME_ZONE, demoSamples, demoWalk } from './demo-series'

const PERCENT = { style: 'percent', maximumFractionDigits: 0 } as const
const USD_PRICE = { style: 'currency', currency: 'USD', maximumFractionDigits: 0 } as const
const money = createMoneyFormatter('en-US')
const usd = (amountCents: number) => money.usd(cents(Math.round(amountCents)))

const fairProbability = demoWalk({
  seed: 11,
  count: 240,
  base: 58,
  volatility: 1.1,
  clamp: [1, 99],
})
const marketProbability = demoWalk({
  seed: 12,
  count: 240,
  base: 54,
  volatility: 1.6,
  clamp: [1, 99],
})

export function TimeSeriesExample() {
  return (
    <TimeSeries
      label="Fair vs market probability"
      title="Fair vs market"
      timeZone={DEMO_TIME_ZONE}
      format={PERCENT}
      domain={[30, 80]}
      series={[
        { id: 'fair', label: 'Fair', color: 'var(--foreground)', points: fairProbability },
        { id: 'market', label: 'Market', color: 'var(--action)', points: marketProbability },
      ]}
      band={{ kind: 'between', a: 'fair', b: 'market', color: 'var(--action)' }}
      guides={[{ value: 50, label: 'Even' }]}
      markers={[{ time: DEMO_START + 2 * MINUTE, label: 'Round open' }]}
      windows={[
        { label: '4m', ms: null },
        { label: '2m', ms: 2 * MINUTE },
        { label: '1m', ms: MINUTE },
      ]}
    />
  )
}

const spotTape = demoWalk({ seed: 21, count: 360, base: 72_640, volatility: 28, drift: 0.4 })
const spotCandles = aggregateCandles(spotTape, 15 * 1_000)

export function CandleChartExample() {
  return (
    <CandleChart
      label="Bitcoin price candles"
      title="BTC, 15s candles"
      candles={spotCandles}
      bucketMs={15 * 1_000}
      format={USD_PRICE}
      timeZone={DEMO_TIME_ZONE}
      guide={{ value: 72_640, label: 'Strike' }}
    />
  )
}

/** Round-to-round moves in dollars from strike, 400 rounds, a bell with a fat right tail. */
const strikeMoves = demoSamples(31, 400, random => {
  const bell = (random() + random() + random() + random() - 2) * 60
  return random() < 0.06 ? bell + 140 : bell
})

export function HistogramExample() {
  return (
    <Histogram
      label="Distribution of moves from strike"
      title="Move from strike at close"
      meta="400 rounds"
      values={strikeMoves}
      format={value => `${value >= 0 ? '+' : '−'}$${Math.abs(Math.round(value))}`}
      current={{ value: 84, label: 'Now' }}
      density
    />
  )
}

/** Edge at entry (cents) against realised P&L (dollars), sized by stake, 48 rounds. */
const edgeOutcomes = demoSamples(41, 48, random => {
  const edge = (random() - 0.4) * 12
  const pnl = edge * 2.4 + (random() + random() - 1) * 18
  return { edge, pnl, stake: 20 + random() * 180 }
}).map((round, index) => ({ id: `round-${index}`, x: round.edge, y: round.pnl, size: round.stake }))

export function ScatterExample() {
  return (
    <Scatter
      label="Edge at entry against realised profit"
      title="Edge vs realised P&L"
      meta="48 rounds, sized by stake"
      points={edgeOutcomes}
      xLabel="Edge at entry"
      yLabel="P&L"
      formatX={value => `${value > 0 ? '+' : value < 0 ? '−' : ''}${Math.abs(Math.round(value))}¢`}
      formatY={value => `${value < 0 ? '−' : ''}$${Math.abs(Math.round(value))}`}
      highlight={{ id: 'round-47', label: 'Last round' }}
    />
  )
}

export function GaugeExample() {
  return (
    <Gauge
      label="RSI gauge"
      title="RSI (14)"
      meta="1m bars"
      value={27.4}
      min={0}
      max={100}
      format={value => value.toFixed(value % 1 === 0 ? 0 : 1)}
      zones={[
        { from: 0, to: 30, tone: 'positive', label: 'Oversold' },
        { from: 30, to: 70, tone: 'neutral' },
        { from: 70, to: 100, tone: 'negative', label: 'Overbought' },
      ]}
      reading="Below 30 for the last 40 seconds. Momentum is stretched to the downside."
    />
  )
}

export function StatGridExample() {
  return (
    <StatGrid
      label="Window statistics"
      title="This window"
      meta={<span className="font-mono">T−0:47</span>}
      stats={[
        { id: 'fair', label: 'Fair probability', value: '61.4¢', delta: '+2.1¢' },
        { id: 'market', label: 'Market probability', value: '58.0¢', delta: '−0.5¢' },
        { id: 'edge', label: 'Edge', value: '+3.4¢', direction: 'up' },
        { id: 'vol', label: 'Realised vol', value: '41.6%', delta: '+3.9%' },
        { id: 'depth', label: 'Top of book', value: '5,554' },
        { id: 'remaining', label: 'Time remaining', value: '0:47' },
      ]}
    />
  )
}

const signalTrend = (seed: number, base: number, volatility: number) =>
  demoWalk({ seed, count: 24, base, volatility }).map(point => point.value)

export function DataTableExample() {
  return (
    <DataTable
      label="Signals with a live value and trend"
      title="Momentum signals"
      meta="1m"
      columns={[
        { id: 'signal', label: 'Signal', sortable: true },
        { id: 'value', label: 'Value', align: 'right', sortable: true },
        { id: 'trend', label: 'Trend', align: 'right' },
        { id: 'state', label: 'State' },
      ]}
      defaultSort={{ column: 'value', direction: 'descending' }}
      rows={[
        {
          id: 'rsi',
          cells: {
            signal: { kind: 'text', text: 'RSI (14)' },
            value: { kind: 'number', text: '27.4', value: 27.4 },
            trend: { kind: 'sparkline', values: signalTrend(51, 40, 3), tone: 'negative' },
            state: { kind: 'pip', state: 'warn', text: 'Oversold' },
          },
        },
        {
          id: 'macd',
          cells: {
            signal: { kind: 'text', text: 'MACD' },
            value: { kind: 'number', text: '+3.36', value: 3.36, tone: 'positive' },
            trend: { kind: 'sparkline', values: signalTrend(52, 1, 0.8), tone: 'positive' },
            state: { kind: 'pip', state: 'on', text: 'Bullish' },
          },
        },
        {
          id: 'roc',
          cells: {
            signal: { kind: 'text', text: 'ROC' },
            value: { kind: 'number', text: '−0.01%', value: -0.01, tone: 'negative' },
            trend: { kind: 'sparkline', values: signalTrend(53, 0, 0.3) },
            state: { kind: 'pip', state: 'off', text: 'Flat' },
          },
        },
        {
          id: 'bb',
          cells: {
            signal: { kind: 'text', text: 'Bollinger %B' },
            value: { kind: 'number', text: '0.84', value: 0.84 },
            trend: { kind: 'sparkline', values: signalTrend(54, 0.5, 0.08) },
            state: { kind: 'pip', state: 'on', text: 'Upper band' },
          },
        },
        {
          id: 'z',
          cells: {
            signal: { kind: 'text', text: 'Z-score' },
            value: { kind: 'number', text: '+1.40σ', value: 1.4, tone: 'positive' },
            trend: { kind: 'sparkline', values: signalTrend(55, 0.6, 0.4), tone: 'positive' },
            state: { kind: 'pip', state: 'off', text: 'Within 2σ' },
          },
        },
      ]}
    />
  )
}

/** A YES book in cents: depth thins away from the touch with a couple of resting walls. */
export const DEMO_ASKS = [
  { price: 62, size: 1_747 },
  { price: 63, size: 1_080 },
  { price: 64, size: 399 },
  { price: 65, size: 333 },
  { price: 66, size: 518 },
  { price: 67, size: 483 },
  { price: 68, size: 276 },
]
export const DEMO_BIDS = [
  { price: 61, size: 3_142 },
  { price: 60, size: 1_540 },
  { price: 59, size: 558 },
  { price: 58, size: 884 },
  { price: 57, size: 321 },
  { price: 56, size: 268 },
  { price: 55, size: 342 },
]

export function LadderExample() {
  return (
    <Ladder
      label="YES order book"
      title="YES book"
      meta={<span className="font-mono">Imbalance +0.17</span>}
      asks={DEMO_ASKS}
      bids={DEMO_BIDS}
      formatPrice={price => `${price}¢`}
      formatSize={money.count}
      midLabel="Spread 1¢"
    />
  )
}

/** Per-round profit and loss in cents, the last twelve rounds of the 2-minute series. */
const ROUND_PNL_CENTS = [1240, -380, 610, 2150, -1120, 90, 1480, -640, 330, 2790, -210, 940]

export function BarChartExample() {
  return (
    <BarChart
      label="Profit and loss per round"
      title="P&L per round"
      meta="Last 12 rounds"
      signed
      format={usd}
      categories={ROUND_PNL_CENTS.map((_, index) => `${index + 1}`)}
      series={[{ id: 'pnl', label: 'P&L', color: 'var(--foreground)', values: ROUND_PNL_CENTS }]}
    />
  )
}
