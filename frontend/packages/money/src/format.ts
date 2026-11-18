import { BPS_PER_UNIT, type Bps } from './bps'
import { CENTS_PER_DOLLAR, type Cents } from './cents'

export type SignDisplay = 'always' | 'auto' | 'exceptZero' | 'never'

export interface UsdFormatOptions {
  /** Abbreviate large amounts: $1.2M instead of $1,200,000.00. */
  compact?: boolean
  maximumFractionDigits?: number
  signDisplay?: SignDisplay
}

export interface PercentFormatOptions {
  maximumFractionDigits?: number
  signDisplay?: SignDisplay
}

export interface MoneyFormatter {
  /** Whole-number formatting for shares, trades, participants. */
  count(value: number): string
  /** A plain measurement (an indicator, a ratio) with at most `maximumFractionDigits`. */
  decimal(value: number, maximumFractionDigits?: number): string
  /** A price in cents as the product shows it: `31¢`. */
  price(price: Cents): string
  /** A ratio in basis points as a percentage: `31%`, `4.25%`. */
  percent(ratio: Bps, options?: PercentFormatOptions): string
  /** A USD amount from integer cents. Division happens here and nowhere else. */
  usd(amount: Cents, options?: UsdFormatOptions): string
}

/**
 * Builds the single place where integer money becomes text. `Intl.NumberFormat`
 * instances are expensive to construct, so each option combination is created once per
 * formatter and reused. Create one formatter per locale, not per render.
 */
export function createMoneyFormatter(locale: string): MoneyFormatter {
  const cache = new Map<string, Intl.NumberFormat>()

  function numberFormat(key: string, options: Intl.NumberFormatOptions): Intl.NumberFormat {
    const cached = cache.get(key)

    if (cached) {
      return cached
    }

    const created = new Intl.NumberFormat(locale, options)
    cache.set(key, created)

    return created
  }

  return {
    count(value) {
      return numberFormat('count', { maximumFractionDigits: 0 }).format(value)
    },

    decimal(value, maximumFractionDigits = 4) {
      return numberFormat(`decimal:${maximumFractionDigits}`, { maximumFractionDigits }).format(
        value
      )
    },

    price(price) {
      return `${numberFormat('price', { maximumFractionDigits: 0 }).format(price)}¢`
    },

    percent(ratio, options = {}) {
      const maximumFractionDigits = options.maximumFractionDigits ?? 0
      const signDisplay = options.signDisplay ?? 'auto'

      return numberFormat(`percent:${maximumFractionDigits}:${signDisplay}`, {
        maximumFractionDigits,
        signDisplay,
        style: 'percent',
      }).format(ratio / BPS_PER_UNIT)
    },

    usd(amount, options = {}) {
      const compact = options.compact ?? false
      const maximumFractionDigits = options.maximumFractionDigits ?? (compact ? 1 : 2)
      const signDisplay = options.signDisplay ?? 'auto'

      return numberFormat(
        `usd:${compact ? 'compact' : 'standard'}:${signDisplay}:${maximumFractionDigits}`,
        {
          currency: 'USD',
          signDisplay,
          style: 'currency',
          ...(compact
            ? { maximumFractionDigits, notation: 'compact' }
            : { maximumFractionDigits, minimumFractionDigits: Math.min(2, maximumFractionDigits) }),
        }
      ).format(amount / CENTS_PER_DOLLAR)
    },
  }
}
