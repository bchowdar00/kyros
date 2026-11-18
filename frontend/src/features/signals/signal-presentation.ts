import type { RustSignalField } from '@tiny/domain'
import { bps, cents, createMoneyFormatter } from '@tiny/money'
import type { ValueFormat } from '@tiny/ui/charts/lib/types'

const money = createMoneyFormatter('en-US')

/**
 * What a plotted field is, so a chart can pick one axis for values that share a unit and
 * refuse to share one for values that do not. `plain` never shares.
 */
export type SignalUnit =
  | 'cents'
  | 'usd'
  | 'bps'
  | 'pct'
  | 'pp'
  | 'ratio'
  | 'probability'
  | 'count'
  | 'ms'
  | 'plain'

export interface PresentationGuide {
  /** In plotted units, after `scale`. */
  value: number
  label?: string
}

export interface FieldPresentation {
  unit: SignalUnit
  /** Multiply a wire value by this before plotting, so cents and percentages read as such. */
  scale: number
  /** Axis tick format, in plotted units. */
  axisFormat: ValueFormat
  /** A single plotted value for a legend or header. */
  formatValue: (plotted: number) => string
  /** Fixed value domain for bounded fields, in plotted units. */
  domain?: readonly [number, number]
  /** Reference lines a reading is judged against, in plotted units. */
  guides?: readonly PresentationGuide[]
}

const INDICATOR_SEGMENT = 2
const CENTS_PER_UNIT = 100
const BPS_PER_UNIT = 10_000

/** Signals whose price fields are up-side probabilities, read as cents. */
const CENT_PRICED_INDICATORS = new Set(['price', 'trades', 'prediction_quote', 'executable_edge'])
const CENT_PRICED_FIELDS = new Set([
  'up',
  'down',
  'bid',
  'ask',
  'last',
  'price',
  'affirmative_bid',
  'affirmative_ask',
  'affirmative_midpoint',
  'negative_bid',
  'negative_ask',
  'negative_midpoint',
  'last_trade_price',
  'executable_up_price',
  'executable_down_price',
])
/** Fields holding a 0–1 probability, plotted on a 0–100 scale. */
const PROBABILITY_FIELDS = new Set([
  'probability',
  'fair_probability',
  'implied_up_kalshi',
  'implied_up_polymarket',
  'percent_b',
  'up_share_8',
])
/** Fields carrying a BTC price in dollars when their signal is not cent-priced. */
const DOLLAR_FIELDS = new Set([
  'last',
  'bid',
  'ask',
  'midpoint',
  'microprice',
  'price',
  'conf',
  'ema_9',
  'ema_21',
  'vwap_10s',
  'vwap_60s',
  'atr',
  'value',
  // The blended reference and venue comparison publish each venue's mid in dollars.
  'reference_price',
  'coinbase_mid',
  'kraken_mid',
  'book_midpoint',
  'absolute_difference',
])
const STATS_USD_FIELDS = new Set(['volume', 'collateral'])
const STATS_COUNT_FIELDS = new Set(['open_interest', 'largest_trade', 'trades', 'traders'])

const number = (maximumFractionDigits: number): ValueFormat => ({
  style: 'number',
  maximumFractionDigits,
})
const signedDecimal = (value: number, digits: number) =>
  `${value > 0 ? '+' : ''}${money.decimal(value, digits)}`

function present(
  unit: SignalUnit,
  scale: number,
  axisFormat: ValueFormat,
  formatValue: (plotted: number) => string,
  extra: Pick<FieldPresentation, 'domain' | 'guides'> = {}
): FieldPresentation {
  return { unit, scale, axisFormat, formatValue, ...extra }
}

const CENTS = present(
  'cents',
  CENTS_PER_UNIT,
  number(0),
  plotted => `${money.decimal(plotted, 1)}¢`,
  { domain: [0, 100] }
)
const USD = present(
  'usd',
  1,
  { style: 'currency', currency: 'USD', maximumFractionDigits: 0 },
  plotted => money.usd(cents(Math.round(plotted * CENTS_PER_UNIT)))
)
const BPS = present('bps', 1, number(0), plotted => `${signedDecimal(plotted, 1)} bps`, {
  guides: [{ value: 0 }],
})
const PCT = present('pct', 1, number(2), plotted => `${signedDecimal(plotted, 2)}%`, {
  guides: [{ value: 0 }],
})
const PP = present('pp', 1, number(2), plotted => `${signedDecimal(plotted, 2)} pp`, {
  guides: [{ value: 0 }],
})
const RATIO = present('ratio', 1, number(2), plotted => `${money.decimal(plotted, 2)}×`, {
  guides: [{ value: 1 }],
})
const PROBABILITY = present(
  'probability',
  CENTS_PER_UNIT,
  { style: 'percent', maximumFractionDigits: 0 },
  plotted =>
    money.percent(bps(Math.round(plotted * (BPS_PER_UNIT / CENTS_PER_UNIT))), {
      maximumFractionDigits: 1,
    }),
  { domain: [0, 100] }
)
const COUNT = present('count', 1, number(0), plotted => money.count(Math.round(plotted)))
const MS = present('ms', 1, number(0), plotted => `${money.count(Math.round(plotted))} ms`)
const SIGNED = present('plain', 1, number(2), plotted => signedDecimal(plotted, 2), {
  guides: [{ value: 0 }],
})
const PLAIN = present('plain', 1, number(2), plotted =>
  money.decimal(plotted, Math.abs(plotted) >= 100 ? 2 : 4)
)

/** Bounded indicators with the levels a reading is judged against, by field name. */
const BOUNDED: Readonly<Record<string, Pick<FieldPresentation, 'domain' | 'guides'>>> = {
  rsi_14: {
    domain: [0, 100],
    guides: [
      { value: 30, label: 'Oversold' },
      { value: 70, label: 'Overbought' },
    ],
  },
  percent_b: {
    domain: [-25, 125],
    guides: [
      { value: 0, label: 'Lower band' },
      { value: 100, label: 'Upper band' },
    ],
  },
  imbalance: { domain: [-1, 1], guides: [{ value: 0 }] },
  combined_imbalance: { domain: [-1, 1], guides: [{ value: 0 }] },
}

function decimalPresentation(indicator: string, name: string): FieldPresentation {
  if (CENT_PRICED_INDICATORS.has(indicator) && CENT_PRICED_FIELDS.has(name)) return CENTS
  if (PROBABILITY_FIELDS.has(name)) return PROBABILITY
  if (name.includes('_usd')) return USD
  if (name.endsWith('_bps') || name.includes('_bps_')) return BPS
  if (name.includes('_pp_')) return PP
  if (name.includes('_pct')) return PCT
  if (name.includes('ratio')) return RATIO
  if (name.includes('imbalance') || name.startsWith('z_') || name.includes('delta')) return SIGNED
  if (DOLLAR_FIELDS.has(name)) return USD
  return PLAIN
}

/**
 * How a field plots: its unit, the scale from wire to plotted values, the formats for
 * axis and legend, and any fixed domain or guides. Keyed the way the allowlist is keyed,
 * by the id's indicator segment, so one rule covers every venue publishing an indicator.
 */
export function fieldPresentation(
  signalId: string,
  field: Pick<RustSignalField, 'name' | 'type'>
): FieldPresentation {
  const { name } = field
  const indicator = signalId.split('.')[INDICATOR_SEGMENT] ?? ''
  const bounded = BOUNDED[name]
  const withBounds = (base: FieldPresentation): FieldPresentation =>
    bounded === undefined ? base : { ...base, ...bounded }

  if (indicator === 'stats') {
    if (STATS_USD_FIELDS.has(name)) return USD
    if (STATS_COUNT_FIELDS.has(name)) return COUNT
  }
  switch (field.type) {
    case 'decimal':
      return withBounds(decimalPresentation(indicator, name))
    case 'u8':
    case 'u16':
    case 'u32':
    case 'u64':
    case 'i64':
      return withBounds(COUNT)
    case 'duration_ms':
      return MS
    default:
      return PLAIN
  }
}

/** Two fields share one axis only when they are the same kind of number. */
export function shareAxis(a: FieldPresentation, b: FieldPresentation): boolean {
  return a.unit === b.unit && a.unit !== 'plain'
}
