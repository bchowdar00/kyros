import { MarketResultTriangleIcon } from '../icons/market-result-triangle-icon'
import { cn } from '../lib/cn'

const MAX_RECENT_RESULTS = 5
const FALLBACK_RESULTS = ['below', 'above', 'below', 'above', 'below'] as const

export type RecentMarketResult = 'above' | 'below'

export interface RecentMarketResultsProps {
  /** Ordered historical outcomes, oldest to newest. Missing results receive a decorative fallback. */
  results?: readonly RecentMarketResult[] | undefined
}

function resolveDisplayedResults(results: readonly RecentMarketResult[] | undefined) {
  const recentResults = results?.slice(-MAX_RECENT_RESULTS)

  return recentResults?.length ? recentResults : FALLBACK_RESULTS
}

/** Compact, overlapping markers for the latest resolved price-against-target markets. */
export function RecentMarketResults({ results }: RecentMarketResultsProps) {
  const displayedResults = resolveDisplayedResults(results)
  const hasResults = results !== undefined && results.length > 0
  const listLabel = hasResults
    ? `Results for the ${displayedResults.length} most recent markets`
    : 'Recent market results unavailable'

  return (
    <ol aria-label={listLabel} className="flex items-center">
      {displayedResults.map((result, index) => {
        const resolvedAboveTarget = result === 'above'
        const markerLabel = hasResults
          ? `Market resolved ${resolvedAboveTarget ? 'above' : 'below'} the price to beat`
          : 'Market result unavailable'

        return (
          <li
            aria-label={markerLabel}
            className={cn(
              'flex size-avatar-md items-center justify-center rounded-full bg-background p-px',
              index === 0 ? '' : '-ml-2'
            )}
            key={`${result}-${index}`}
          >
            <span
              aria-hidden="true"
              className={cn(
                'flex size-full items-center justify-center rounded-full',
                resolvedAboveTarget ? 'bg-positive/25' : 'bg-negative/25'
              )}
            >
              <MarketResultTriangleIcon
                className={cn(
                  'size-3',
                  resolvedAboveTarget ? 'rotate-180 text-positive' : 'text-negative'
                )}
              />
            </span>
          </li>
        )
      })}
    </ol>
  )
}
