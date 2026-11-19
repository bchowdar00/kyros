import type { RustSeriesInfo } from '@kyros/domain'

/** What the nav badge says about the money and the books behind this site. */
export interface SiteMode {
  label: 'Paper' | 'Sandbox' | 'Live orders' | 'Sandbox orders' | 'Mode unknown'
  description: string
  tone: 'neutral' | 'warning'
}

/**
 * `undefined` means the exchange did not answer. One sandbox series is enough to call the
 * whole site sandbox: a mixed deployment must not read as live.
 */
export function toSiteMode(series: readonly RustSeriesInfo[] | undefined): SiteMode {
  if (series === undefined) {
    return {
      label: 'Mode unknown',
      description:
        'The exchange did not answer, so this page cannot say which books it is reading.',
      tone: 'warning',
    }
  }

  const real = series.filter(info => info.gemini?.orders === 'real')
  if (real.some(info => info.gemini?.env === 'production')) {
    return {
      label: 'Live orders',
      description:
        "Strategies run by this server's operator send real orders to Gemini. Manual orders and every balance shown here are pretend money.",
      tone: 'warning',
    }
  }
  if (real.length > 0) {
    return {
      label: 'Sandbox orders',
      description:
        "Strategies run by this server's operator send orders to Gemini's sandbox. Manual orders and every balance shown here are pretend money.",
      tone: 'warning',
    }
  }

  if (series.some(info => info.gemini?.env === 'sandbox')) {
    return {
      label: 'Sandbox',
      description:
        "Pretend money, filled against Gemini's sandbox order book. No order leaves this site.",
      tone: 'warning',
    }
  }

  if (series.some(info => info.gemini?.env === 'production')) {
    return {
      label: 'Paper',
      description:
        "Pretend money, filled against Gemini's live order book. No order leaves this site.",
      tone: 'neutral',
    }
  }

  return {
    label: 'Paper',
    description: 'Pretend money. This server is not following Gemini.',
    tone: 'warning',
  }
}
