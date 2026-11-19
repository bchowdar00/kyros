import { Badge, type BadgeVariant } from '@kyros/ui/components/badge'

import type { GeminiBookState } from '../types'

export type GeminiBookSourceProps = GeminiBookState

const GEMINI_PREDICTIONS_URL = 'https://www.gemini.com/predictions'

const bookBadges = {
  live: { label: 'Live', variant: 'positive' },
  stale: { label: 'Stale', variant: 'warning' },
  waiting: { label: 'Waiting', variant: 'neutral' },
} as const satisfies Record<string, { label: string; variant: BadgeVariant }>

const unknownBadge = { label: 'Unknown', variant: 'warning' } as const

/**
 * Where the order book comes from: the exchange mirrors Gemini's public book for the current
 * contract. The badge text carries the state, so colour never stands alone. After a failed
 * poll the state is unknown and the last contract is not shown, since it may have rolled; with
 * no block ever heard, not even the source is known.
 */
export function GeminiBookSource({ status, gemini }: GeminiBookSourceProps) {
  if (gemini === undefined) {
    // Without a block there is no knowing whether Gemini backs this series at all.
    return status === 'polled' ? null : (
      <span className="text-label text-muted-foreground">Book source unknown</span>
    )
  }

  const failed = status === 'poll-failed'
  const badge = failed ? unknownBadge : bookBadges[gemini.book]
  const contract =
    failed || gemini.symbol === undefined || gemini.event_ticker === undefined
      ? undefined
      : { env: gemini.env, symbol: gemini.symbol, eventTicker: gemini.event_ticker }

  return (
    <div
      className="flex flex-wrap items-center gap-sm text-label text-muted-foreground"
      data-slot="gemini-book-source"
    >
      <span>{gemini.env === 'sandbox' ? 'Book from Gemini sandbox' : 'Book from Gemini'}</span>
      <Badge variant={badge.variant}>{badge.label}</Badge>
      {contract === undefined ? null : contract.env === 'production' ? (
        <a
          aria-label={`View ${contract.symbol} on Gemini`}
          className="font-mono text-foreground underline focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-focus-ring"
          href={`${GEMINI_PREDICTIONS_URL}/${encodeURIComponent(contract.eventTicker)}`}
          rel="noopener noreferrer"
          target="_blank"
        >
          {contract.symbol}
        </a>
      ) : (
        <span className="font-mono">{contract.symbol}</span>
      )}
    </div>
  )
}
