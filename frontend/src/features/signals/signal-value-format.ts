import type { RustSignal, RustSignalField } from '@tiny/domain'
import { bps, type Cents, cents, createMoneyFormatter } from '@tiny/money'

const money = createMoneyFormatter('en-US')
const clock = new Intl.DateTimeFormat('en-US', {
  hour: '2-digit',
  minute: '2-digit',
  second: '2-digit',
})

const MS_PER_SECOND = 1000
const MS_PER_MINUTE = 60_000
const BPS_PER_UNIT = 10_000
const CENTS_PER_UNIT = 100
/** Above this a reading is a dollar price and two decimals are enough. */
const COARSE_DECIMAL_FROM = 100
const INDICATOR_SEGMENT = 2

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
/** Fields holding a 0–1 probability, read as a percentage. */
const PROBABILITY_FIELDS = new Set([
  'probability',
  'fair_probability',
  'implied_up_kalshi',
  'implied_up_polymarket',
  'percent_b',
  'up_share_8',
])
const DIRECTION_WORDS: Readonly<Record<string, string>> = {
  up: '↑ Up',
  down: '↓ Down',
  buy: '↑ Buy',
  sell: '↓ Sell',
  long: '↑ Long',
  short: '↓ Short',
}

/** Units the registry does not carry: the stats strip's dollars and lots. */
function statsUnit(field: string): 'usd' | 'lots' | undefined {
  switch (field) {
    case 'volume':
    case 'collateral':
      return 'usd'
    case 'open_interest':
    case 'largest_trade':
      return 'lots'
    default:
      return undefined
  }
}

/** `"88650.000"` → 8865000 cents, read lexically so no float carries money. */
function decimalStringToCents(text: string): Cents | undefined {
  const match = /^(-?)(\d+)(?:\.(\d*))?$/.exec(text)
  if (match === null) return undefined
  const [, sign, whole = '0', fraction = ''] = match
  const centsPart = `${fraction}00`.slice(0, 2)
  const rounded = Number(whole) * 100 + Number(centsPart)
  return cents(sign === '-' ? -rounded : rounded)
}

function formatDuration(ms: number): string {
  if (ms < MS_PER_SECOND) return `${money.count(ms)} ms`
  if (ms < MS_PER_MINUTE) return `${money.decimal(ms / MS_PER_SECOND, 1)} s`
  const total = Math.round(ms / MS_PER_SECOND)
  const minutes = Math.floor(total / 60)
  const seconds = total % 60
  return `${minutes}:${String(seconds).padStart(2, '0')}`
}

function shortAddress(value: string): string {
  return /^0x[0-9a-f]{40}$/.test(value) ? `${value.slice(0, 6)}…${value.slice(-4)}` : value
}

function signed(text: string, amount: number): string {
  return amount > 0 ? `+${text}` : text
}

/** `within_guard` → `Within guard`; directions get their arrow. */
function stateWords(value: string): string {
  const direction = DIRECTION_WORDS[value]
  if (direction !== undefined) return direction
  const words = value.replaceAll('_', ' ')
  return words.charAt(0).toUpperCase() + words.slice(1)
}

function formatDecimal(signal: RustSignal, field: RustSignalField, amount: number): string {
  const indicator = signal.signal_id.split('.')[INDICATOR_SEGMENT] ?? ''
  const { name } = field
  if (CENT_PRICED_INDICATORS.has(indicator) && CENT_PRICED_FIELDS.has(name)) {
    return `${money.decimal(amount * CENTS_PER_UNIT, 1)}¢`
  }
  if (PROBABILITY_FIELDS.has(name)) {
    return money.percent(bps(Math.round(amount * BPS_PER_UNIT)), { maximumFractionDigits: 1 })
  }
  if (name.includes('_usd')) {
    const asCents = decimalStringToCents(String(field.value))
    if (asCents !== undefined) return money.usd(asCents)
  }
  if (name.endsWith('_bps') || name.includes('_bps_')) {
    return signed(`${money.decimal(amount, 2)} bps`, amount)
  }
  if (name.includes('_pp_')) {
    return signed(`${money.decimal(amount, 2)} pp`, amount)
  }
  if (name.includes('_pct')) {
    return signed(`${money.decimal(amount, 2)}%`, amount)
  }
  if (name.includes('ratio')) {
    const change = amount - 1
    return money.percent(bps(Math.round(change * BPS_PER_UNIT)), {
      maximumFractionDigits: 1,
      signDisplay: 'exceptZero',
    })
  }
  if (name.includes('imbalance') || name.startsWith('z_') || name.includes('delta')) {
    return signed(money.decimal(amount, 2), amount)
  }
  return money.decimal(amount, Math.abs(amount) >= COARSE_DECIMAL_FROM ? 2 : 4)
}

/** The text a card shows for a field; `—` until the signal has published. */
export function formatSignalValue(signal: RustSignal, field: RustSignalField): string {
  const { value } = field
  if (value === undefined) return '—'

  if (signal.signal_id.endsWith('.current.stats.v1')) {
    const unit = statsUnit(field.name)
    if (unit === 'usd') {
      const amount = decimalStringToCents(String(value))
      return amount === undefined ? String(value) : money.usd(amount)
    }
    if (unit === 'lots') return `${money.count(Number(value))} lots`
  }

  switch (field.type) {
    case 'decimal': {
      const amount = Number(value)
      return Number.isFinite(amount) ? formatDecimal(signal, field, amount) : String(value)
    }
    case 'u8':
    case 'u16':
    case 'u32':
    case 'u64':
    case 'i64':
      return money.count(Number(value))
    case 'duration_ms':
      return formatDuration(Number(value))
    case 'timestamp': {
      const at = typeof value === 'number' ? new Date(value) : new Date(String(value))
      return Number.isNaN(at.getTime()) ? String(value) : clock.format(at)
    }
    case 'enum':
      return stateWords(String(value))
    case 'bool':
      return value === true ? 'Yes' : 'No'
    case 'string':
      return shortAddress(String(value))
    default:
      return String(value)
  }
}

/** Whether a field is a number a sparkline can trace, rather than a state or a label. */
export function isNumericField(field: RustSignalField): boolean {
  switch (field.type) {
    case 'decimal':
    case 'u8':
    case 'u16':
    case 'u32':
    case 'u64':
    case 'i64':
    case 'duration_ms':
      return true
    default:
      return false
  }
}
