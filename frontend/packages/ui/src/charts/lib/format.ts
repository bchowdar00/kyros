import { DEFAULT_LOCALE } from './time'
import type { ChartKind, ValueFormat } from './types'

const numberFormatters = new Map<string, Intl.NumberFormat>()

function getNumberFormatter(options: Intl.NumberFormatOptions, locale = DEFAULT_LOCALE) {
  const key = `${locale}|${JSON.stringify(options)}`
  let formatter = numberFormatters.get(key)

  if (!formatter) {
    formatter = new Intl.NumberFormat(locale, options)
    numberFormatters.set(key, formatter)
  }

  return formatter
}

export const PERCENT_FORMAT: ValueFormat = { style: 'percent' }
export const USD_FORMAT: ValueFormat = { currency: 'USD', style: 'currency' }

/** Formats a chart value. Percent formats expect values on a 0–100 scale. */
export function formatValue(value: number, format: ValueFormat, locale?: string): string {
  if (!Number.isFinite(value)) {
    return '—'
  }

  switch (format.style) {
    case 'percent': {
      const formatter = getNumberFormatter(
        { maximumFractionDigits: format.maximumFractionDigits ?? 0 },
        locale
      )

      return `${formatter.format(value)}%`
    }
    case 'currency': {
      const maximumFractionDigits = format.maximumFractionDigits ?? 2

      return getNumberFormatter(
        {
          currency: format.currency ?? 'USD',
          maximumFractionDigits,
          minimumFractionDigits: Math.min(
            format.minimumFractionDigits ?? maximumFractionDigits,
            maximumFractionDigits
          ),
          notation: format.notation ?? 'standard',
          style: 'currency',
        },
        locale
      ).format(value)
    }
    case 'number': {
      const maximumFractionDigits = format.maximumFractionDigits ?? 2

      return getNumberFormatter(
        {
          maximumFractionDigits,
          minimumFractionDigits: Math.min(format.minimumFractionDigits ?? 0, maximumFractionDigits),
          notation: format.notation ?? 'standard',
        },
        locale
      ).format(value)
    }
  }
}

/** Formats a difference with an explicit sign, for example `+$20.17` or `-3%`. */
export function formatSignedValue(value: number, format: ValueFormat, locale?: string): string {
  const formatted = formatValue(Math.abs(value), format, locale)

  if (value === 0) {
    return formatted
  }

  return `${value > 0 ? '+' : '-'}${formatted}`
}

/** Approximate rendered width of `text` in pixels for layout reservations. */
export function estimateTextWidth(text: string, fontSize: number): number {
  let width = 0

  for (const character of text) {
    if (character === '.' || character === ',' || character === ':' || character === ' ') {
      width += 0.3
    } else if (character === '%' || character === 'M' || character === 'W') {
      width += 0.85
    } else if (character === '$') {
      width += 0.6
    } else if (/[a-z]/.test(character)) {
      width += 0.55
    } else {
      width += 0.62
    }
  }

  return width * fontSize
}

export const chartKindLabels: Record<ChartKind, string> = {
  candlestick: 'Price candles',
  price: 'Price',
  probability: 'Probability',
}
